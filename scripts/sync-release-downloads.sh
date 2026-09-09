#!/usr/bin/env bash
# Snapshot cumulative GitHub release-asset download totals into PostHog.
#
# Sends one PostHog event per release asset carrying that asset's cumulative
# download count as of a snapshot date. Retries are safe: the event UUID is
# derived from (repository, asset ID, snapshot date) and the timestamp is
# pinned to 23:59:59Z on the snapshot date, so re-running a date re-sends
# byte-identical events that PostHog deduplicates.
#
# Reporting should still aggregate with max(download_count) per asset and
# snapshot date. PostHog deduplication is eventual, and cumulative counts
# only ever rise, so max() is correct while duplicates remain visible.
#
# Draft releases are skipped; their assets are not publicly downloadable.
# GitHub-generated source archives are not release assets and never appear.
#
# Usage: sync-release-downloads.sh [--dry-run] [--date YYYY-MM-DD] [--selftest]
#   --date       - snapshot date (default: yesterday, UTC)
#   --dry-run    - collect and build the payload, print a summary, send nothing
#   --selftest   - run the built-in assertions and exit
#
# Environment:
#   POSTHOG_PROJECT_API_KEY - PostHog project write token (required unless --dry-run)
#   POSTHOG_HOST            - PostHog ingest host (default: https://us.i.posthog.com)
#   POSTHOG_API_READ_KEY    - personal API key used to read the snapshot back
#   POSTHOG_PROJECT_ID      - numeric project id the read-back queries (default: 443794)
#   POSTHOG_API_HOST        - PostHog query host (default: https://us.posthog.com)
#   VERIFY_TIMEOUT          - seconds to wait for ingestion (default: 300)
#   SKIP_VERIFY             - 1 sends without reading the snapshot back
#   GITHUB_REPOS            - space-separated owner/repo list (default: mezmo/aura)
#   SNAPSHOT_DATE           - same as --date
#   DRY_RUN                 - 1 is the same as --dry-run
#   BATCH_SIZE              - events per PostHog /batch request (default: 1000)
#   GH_TOKEN / GITHUB_TOKEN - token gh authenticates with
set -euo pipefail

USAGE="usage: $0 [--dry-run] [--date YYYY-MM-DD] [--selftest]"

# Namespace for the version-5 event UUIDs. Permanent: it is part of every
# event key ever sent.
readonly UUID_NAMESPACE="6d3cea6d-9fef-46af-aec1-e9c705245832"
readonly EVENT_NAME="github_release_asset_downloads"

DRY_RUN="${DRY_RUN:-0}"
SELFTEST=0
SNAPSHOT_DATE="${SNAPSHOT_DATE:-}"
POSTHOG_HOST="${POSTHOG_HOST:-https://us.i.posthog.com}"
GITHUB_REPOS="${GITHUB_REPOS:-mezmo/aura}"
BATCH_SIZE="${BATCH_SIZE:-1000}"
POSTHOG_PROJECT_ID="${POSTHOG_PROJECT_ID:-443794}"
POSTHOG_API_HOST="${POSTHOG_API_HOST:-https://us.posthog.com}"
VERIFY_TIMEOUT="${VERIFY_TIMEOUT:-300}"
SKIP_VERIFY="${SKIP_VERIFY:-0}"
WORK_DIR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --selftest) SELFTEST=1; shift ;;
        --date)
            if [ $# -lt 2 ]; then echo "error: --date needs a value" >&2; exit 1; fi
            SNAPSHOT_DATE="$2"; shift 2 ;;
        --date=*) SNAPSHOT_DATE="${1#--date=}"; shift ;;
        -h|--help) echo "${USAGE}" >&2; exit 0 ;;
        *) echo "${USAGE}" >&2; exit 1 ;;
    esac
done

# RFC 4122 version-5 UUID: sha1(namespace bytes || name), with the version and
# variant nibbles forced. Matches Python's uuid.uuid5 byte for byte.
uuid5() {
    local ns_hex=${1//-/} name=$2 h b6 b8
    h=$( { printf '%b' "$(printf '%s' "${ns_hex}" | sed 's/../\\x&/g')"; printf '%s' "${name}"; } \
         | openssl dgst -sha1 -r | cut -d' ' -f1 )
    printf -v b6 '%02x' $(( 0x${h:12:2} & 0x0f | 0x50 ))
    printf -v b8 '%02x' $(( 0x${h:16:2} & 0x3f | 0x80 ))
    printf '%s-%s-%s%s-%s%s-%s\n' \
        "${h:0:8}" "${h:8:4}" "${b6}" "${h:14:2}" "${b8}" "${h:18:2}" "${h:20:12}"
}

# Dates go through jq rather than date(1): GNU and BSD date disagree on both
# relative-date syntaxes, and jq is already a hard dependency here. jq's
# strftime formats UTC regardless of the runner's timezone.
yesterday_utc() {
    jq -rn 'now - 86400 | strftime("%Y-%m-%d")'
}

# A shell glob accepts digit-shaped nonsense like 2026-99-99, and an invalid
# date reaches the wire as a malformed event timestamp. Round-trip the value
# through jq's calendar and require it back unchanged: that rejects impossible
# dates outright and catches the ones a parser silently rolls over, such as
# 2026-02-30 becoming 2026-03-02.
valid_date() {
    local d=$1 normalized
    case "${d}" in
        [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]) ;;
        *) return 1 ;;
    esac
    normalized=$(jq -rn --arg d "${d}" \
        'try ($d + "T00:00:00Z" | fromdateiso8601 | strftime("%Y-%m-%d")) catch ""')
    [ "${normalized}" = "${d}" ]
}

day_before() {
    jq -rn --arg d "$1" '($d + "T00:00:00Z" | fromdateiso8601) - 86400 | strftime("%Y-%m-%d")'
}

require_tools() {
    local tool
    for tool in gh jq curl openssl; do
        if ! command -v "${tool}" >/dev/null 2>&1; then
            echo "error: ${tool} not found" >&2
            exit 1
        fi
    done
    if [ "${DRY_RUN}" != 1 ] && [ -z "${POSTHOG_PROJECT_API_KEY:-}" ]; then
        echo "error: POSTHOG_PROJECT_API_KEY is required (or use --dry-run)" >&2
        exit 1
    fi
    # PostHog answers 200 OK to a batch sent with an invalid project token, so
    # a send that is never verified cannot tell success from silent discard.
    if [ "${DRY_RUN}" != 1 ] && [ "${SKIP_VERIFY}" != 1 ] && [ -z "${POSTHOG_API_READ_KEY:-}" ]; then
        echo "error: POSTHOG_API_READ_KEY is required to verify the send (or set SKIP_VERIFY=1)" >&2
        exit 1
    fi
}

# Emit one compact JSON object per asset, for every non-draft release.
#
# The release list lands in a file rather than a process substitution: a
# partial pagination failure inside `<(...)` would truncate the loop's input
# and silently under-report, where a redirect makes it fatal under `set -e`.
collect_assets() {
    local repo=$1 releases="${WORK_DIR}/releases.tsv" release_id tag
    gh api --paginate "repos/${repo}/releases" \
        --jq '.[] | select(.draft | not) | [.id, .tag_name] | @tsv' > "${releases}"
    while IFS=$'\t' read -r release_id tag; do
        gh api --paginate "repos/${repo}/releases/${release_id}/assets" --jq '.[]' \
            | jq -c --arg repo "${repo}" --argjson rid "${release_id}" --arg tag "${tag}" \
                '{repository: $repo, release_id: $rid, release_tag: $tag,
                  asset_id: .id, asset_name: .name, download_count: .download_count}'
    done < "${releases}"
}

# Prefix each asset record with its event UUID, tab separated. The record stays
# JSON throughout, so asset names are never parsed out of a delimited field.
key_assets() {
    local assets=$1 date=$2 uuids=$3 key
    local keys="${uuids}.keys"
    jq -r '.repository + "|" + (.asset_id | tostring)' "${assets}" > "${keys}"
    while IFS= read -r key; do
        uuid5 "${UUID_NAMESPACE}" "${key}|${date}"
    done < "${keys}" > "${uuids}"
    paste "${uuids}" "${assets}"
}

build_batch() {
    local chunk=$1 date=$2 api_key=$3
    jq -R -n --arg key "${api_key}" --arg date "${date}" --arg event "${EVENT_NAME}" '
        {api_key: $key,
         batch: [inputs
           | index("\t") as $i
           | (.[:$i]) as $uuid
           | (.[$i+1:] | fromjson) as $r
           | {uuid: $uuid,
              event: $event,
              distinct_id: ("github:" + $r.repository),
              timestamp: ($date + "T23:59:59Z"),
              properties: ($r + {snapshot_date: $date, "$process_person_profile": false})}]}
    ' "${chunk}"
}

post_batch() {
    local payload=$1
    curl --silent --show-error --fail-with-body \
        --retry 5 --retry-delay 2 \
        --max-time 60 \
        --header 'Content-Type: application/json' \
        --data-binary "@${payload}" \
        "${POSTHOG_HOST%/}/batch/" >/dev/null
}

# Run a HogQL query and print its first scalar result.
#
# Plain --retry covers the transient cases (timeouts, 429, 5xx) and leaves
# 4xx alone: an auth or project error repeated four times is noise, and the
# status is worth naming because every likely cause is a misconfiguration
# rather than an outage.
hogql_scalar() {
    local query=$1 response status body
    response=$(jq -n --arg q "${query}" '{query: {kind: "HogQLQuery", query: $q}}' \
        | curl --silent --show-error --retry 3 --retry-delay 2 --max-time 60 \
            --write-out '\n%{http_code}' \
            --header "Authorization: Bearer ${POSTHOG_API_READ_KEY}" \
            --header 'Content-Type: application/json' \
            --data-binary @- \
            "${POSTHOG_API_HOST%/}/api/projects/${POSTHOG_PROJECT_ID}/query/")
    status=${response##*$'\n'}
    body=${response%$'\n'*}
    if [ "${status}" != 200 ]; then
        echo "error: PostHog query API returned ${status} for project ${POSTHOG_PROJECT_ID}" >&2
        echo "       ${body}" >&2
        echo "       POSTHOG_PROJECT_ID must name the project POSTHOG_PROJECT_API_KEY writes to," >&2
        echo "       and POSTHOG_API_READ_KEY must hold query:read on that project" >&2
        return 1
    fi
    jq -r '.results[0][0]' <<<"${body}"
}

# Count what actually landed. Distinct UUIDs, not rows: PostHog deduplicates
# lazily during background merges, so a re-run's rows stay visible until then.
#
# printf builds the literals rather than nested shell quoting, which is easy to
# get wrong in a way that still returns a well-formed answer: a quote stray
# inside the string literal matches no rows and reads as "nothing ingested".
snapshot_count_query() {
    printf "SELECT count(DISTINCT uuid) FROM events WHERE event = '%s' AND properties.snapshot_date = '%s'" \
        "${EVENT_NAME}" "$1"
}

distinct_events_on() {
    hogql_scalar "$(snapshot_count_query "$1")"
}

# Count how many of this run's own event UUIDs are queryable at this run's
# timestamp. A date-wide count would also match UUIDs left by an earlier run
# whose asset set differed, and those extras can satisfy the threshold while an
# event from the current payload is still missing. Pinning the timestamp too
# means a snapshot attributed to the wrong instant cannot read as verified.
count_ingested() {
    local date=$1 chunk list total=0 found
    for chunk in "${WORK_DIR}"/uchunk.*; do
        list=$(sed "s/^/'/; s/$/'/" "${chunk}" | paste -sd, -)
        found=$(hogql_scalar "SELECT count(DISTINCT uuid) FROM events \
            WHERE event = '${EVENT_NAME}' \
              AND timestamp = toDateTime('${date} 23:59:59') \
              AND uuid IN (${list})")
        total=$((total + found))
    done
    printf '%s\n' "${total}"
}

# Poll until the snapshot is queryable, since ingestion lags the send.
verify_snapshot() {
    local date=$1 expected=$2 deadline got
    split -l 500 "${WORK_DIR}/uuids" "${WORK_DIR}/uchunk."
    deadline=$(( $(date +%s) + VERIFY_TIMEOUT ))
    while :; do
        got=$(count_ingested "${date}")
        if [ "${got}" -ge "${expected}" ]; then
            echo "Verified ${got} of ${expected} event(s) queryable in PostHog for ${date}"
            return 0
        fi
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            echo "error: PostHog holds ${got} of ${expected} event(s) for ${date} after ${VERIFY_TIMEOUT}s" >&2
            return 1
        fi
        sleep 5
    done
}

# A dropped or disabled scheduled run leaves a hole no failure reports, since
# nothing ran to fail. Surface it on the next run that does happen.
warn_on_gap() {
    local date=$1 previous got
    previous=$(day_before "${date}")
    got=$(distinct_events_on "${previous}")
    if [ "${got}" -eq 0 ]; then
        echo "::warning::No release download snapshot in PostHog for ${previous}; GitHub cannot reconstruct a past day's totals, so that day stays missing"
    fi
}

selftest() {
    local tmp=$1 got want payload

    # Well-known RFC 4122 vector, an empty name, and a real event key.
    got=$(uuid5 6ba7b810-9dad-11d1-80b4-00c04fd430c8 "python.org")
    want="886313e1-3b8a-5372-9b90-0c9aee199e5d"
    [ "${got}" = "${want}" ] || { echo "selftest: uuid5(python.org) = ${got}, want ${want}" >&2; exit 1; }

    got=$(uuid5 6ba7b810-9dad-11d1-80b4-00c04fd430c8 "")
    want="4ebd0208-8328-5d69-8c44-ec50939c0967"
    [ "${got}" = "${want}" ] || { echo "selftest: uuid5(empty) = ${got}, want ${want}" >&2; exit 1; }

    # The same key must key the same event on every run, and differ per date.
    got=$(uuid5 "${UUID_NAMESPACE}" "mezmo/aura|541570506|2026-09-01")
    [ "${got}" = "$(uuid5 "${UUID_NAMESPACE}" "mezmo/aura|541570506|2026-09-01")" ] \
        || { echo "selftest: uuid5 is not deterministic" >&2; exit 1; }
    [ "${got}" != "$(uuid5 "${UUID_NAMESPACE}" "mezmo/aura|541570506|2026-09-02")" ] \
        || { echo "selftest: uuid5 collides across snapshot dates" >&2; exit 1; }

    # An asset name carrying a tab and a quote must survive into the payload.
    printf '%s\t%s\n' "3538a427-3e7d-5170-bf7d-e9560ebb3468" \
        '{"repository":"mezmo/aura","release_id":1,"release_tag":"v0.0.1","asset_id":2,"asset_name":"a\tb\"c","download_count":7}' \
        > "${tmp}/chunk"
    payload=$(build_batch "${tmp}/chunk" "2026-09-01" "phc_test")

    got=$(jq -r '.batch[0].properties.asset_name' <<<"${payload}")
    [ "${got}" = "$(printf 'a\tb"c')" ] || { echo "selftest: asset name mangled: ${got}" >&2; exit 1; }
    got=$(jq -r '.batch[0].timestamp' <<<"${payload}")
    [ "${got}" = "2026-09-01T23:59:59Z" ] || { echo "selftest: timestamp = ${got}" >&2; exit 1; }
    got=$(jq -r '.batch[0].properties.download_count | type' <<<"${payload}")
    [ "${got}" = "number" ] || { echo "selftest: download_count is ${got}, want number" >&2; exit 1; }
    got=$(jq -r '.batch[0].properties["$process_person_profile"]' <<<"${payload}")
    [ "${got}" = "false" ] || { echo "selftest: person profiles not suppressed" >&2; exit 1; }
    got=$(jq -r '.batch[0].distinct_id' <<<"${payload}")
    [ "${got}" = "github:mezmo/aura" ] || { echo "selftest: distinct_id = ${got}" >&2; exit 1; }

    # The read-back query must quote its literals exactly once. Nested quoting
    # bugs here return 0 rows, which is indistinguishable from a failed send.
    got=$(snapshot_count_query "2026-08-20")
    want="SELECT count(DISTINCT uuid) FROM events WHERE event = 'github_release_asset_downloads' AND properties.snapshot_date = '2026-08-20'"
    [ "${got}" = "${want}" ] || { echo "selftest: query is\n  ${got}\nwant\n  ${want}" >&2; exit 1; }

    # Calendar validation must reject digit-shaped non-dates, rollovers, and
    # anything that could carry a quote into the read-back query.
    local d
    for d in 2026-09-01 2024-02-29 2026-12-31 2026-01-01; do
        valid_date "${d}" || { echo "selftest: rejected valid date ${d}" >&2; exit 1; }
    done
    for d in 2026-99-99 2026-13-01 2026-00-00 2026-02-30 2025-02-29 2026-9-1 "" "2026-09-0'"; do
        if valid_date "${d}"; then echo "selftest: accepted invalid date '${d}'" >&2; exit 1; fi
    done

    # Date arithmetic across month, year, and leap-day boundaries.
    got=$(day_before "2026-03-01"); want="2026-02-28"
    [ "${got}" = "${want}" ] || { echo "selftest: day_before(2026-03-01) = ${got}, want ${want}" >&2; exit 1; }
    got=$(day_before "2027-01-01"); want="2026-12-31"
    [ "${got}" = "${want}" ] || { echo "selftest: day_before(2027-01-01) = ${got}, want ${want}" >&2; exit 1; }
    got=$(TZ=Pacific/Auckland day_before "2024-03-01"); want="2024-02-29"
    [ "${got}" = "${want}" ] || { echo "selftest: day_before is timezone-sensitive: ${got}, want ${want}" >&2; exit 1; }

    echo "selftest: ok"
}

main() {
    WORK_DIR=$(mktemp -d)
    trap 'rm -rf "${WORK_DIR}"' EXIT

    if [ "${SELFTEST}" = 1 ]; then
        selftest "${WORK_DIR}"
        return 0
    fi

    require_tools

    local date="${SNAPSHOT_DATE}"
    [ -n "${date}" ] || date=$(yesterday_utc)
    if ! valid_date "${date}"; then
        echo "error: --date must be a real calendar date as YYYY-MM-DD (got '${date}')" >&2
        exit 1
    fi

    local repo
    for repo in ${GITHUB_REPOS}; do
        echo "Collecting ${repo} release assets"
        collect_assets "${repo}" >> "${WORK_DIR}/assets.ndjson"
    done

    local assets
    assets=$(wc -l < "${WORK_DIR}/assets.ndjson" | tr -d ' ')
    if [ "${assets}" -eq 0 ]; then
        echo "error: no release assets found in ${GITHUB_REPOS}" >&2
        exit 1
    fi

    key_assets "${WORK_DIR}/assets.ndjson" "${date}" "${WORK_DIR}/uuids" > "${WORK_DIR}/keyed"
    split -l "${BATCH_SIZE}" "${WORK_DIR}/keyed" "${WORK_DIR}/chunk."

    local chunk chunks=0
    for chunk in "${WORK_DIR}"/chunk.*; do
        chunks=$((chunks + 1))
        build_batch "${chunk}" "${date}" "${POSTHOG_PROJECT_API_KEY:-phc_dry_run}" \
            > "${WORK_DIR}/payload.${chunks}.json"
    done

    # Every collected asset must reach a payload. A mismatch means the keying
    # or chunking dropped rows, which would silently under-report.
    local events
    events=$(jq -s '[.[].batch | length] | add' "${WORK_DIR}"/payload.*.json)
    if [ "${events}" != "${assets}" ]; then
        echo "error: built ${events} event(s) from ${assets} asset(s)" >&2
        exit 1
    fi

    echo "Snapshot ${date}: ${assets} assets in ${chunks} batch(es)"

    if [ "${DRY_RUN}" = 1 ]; then
        jq -c '.batch[0]' "${WORK_DIR}/payload.1.json"
        echo "Dry run: nothing sent to ${POSTHOG_HOST%/}"
        return 0
    fi

    local payload
    for payload in "${WORK_DIR}"/payload.*.json; do
        post_batch "${payload}"
    done

    echo "Sent ${assets} event(s) for ${date} to ${POSTHOG_HOST%/}"

    if [ "${SKIP_VERIFY}" = 1 ]; then
        echo "Skipping read-back verification (SKIP_VERIFY=1)"
        return 0
    fi
    verify_snapshot "${date}" "${assets}"
    # Advisory only: a missing earlier day is worth reporting but is not a
    # failure of this run, and cannot be repaired by retrying it.
    warn_on_gap "${date}" || true
}

main "$@"
