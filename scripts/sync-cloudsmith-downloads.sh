#!/usr/bin/env bash
# Snapshot cumulative Cloudsmith package download totals into PostHog.
#
# Sends one PostHog event per Cloudsmith package carrying that package's
# cumulative download count as of a snapshot date. Retries cannot add a second
# snapshot: the event UUID is derived from (repository, package identifier,
# snapshot date) and the timestamp is pinned to 23:59:59Z on the snapshot date,
# which is what PostHog deduplicates on.
#
# Reporting must aggregate with max(download_count) per package and snapshot
# date. Deduplication is eventual, so a retry's rows stay visible in the
# meantime, and a retry taken after the counts moved carries a higher count
# under the same key. Cumulative counts only rise, which makes max() right in
# both cases.
#
# The count is observed when the run happens, not at the instant it is filed
# under. A 01:23 UTC run reads totals that already include that morning's
# downloads and attributes them to 23:59:59Z the day before, so this is an
# approximate daily snapshot; running early keeps the overlap small. The
# package list exposes only a current counter, so no exact figure for a past
# instant is available to use instead.
#
# Cloudsmith identifies a package by its permanent identifier rather than by
# name and version: the same version can be uploaded to several distributions
# and architectures, each with its own download count.
#
# Usage: sync-cloudsmith-downloads.sh [--dry-run] [--date YYYY-MM-DD] [--selftest]
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
#   CLOUDSMITH_REPOS        - space-separated owner/repo list (default: mezmo/aura)
#   CLOUDSMITH_API_KEY      - Cloudsmith API key; unset reads the public package list
#   CLOUDSMITH_HOST         - Cloudsmith API host (default: https://api.cloudsmith.io)
#   PAGE_SIZE               - packages per Cloudsmith page (default: 500, its maximum)
#   SNAPSHOT_DATE           - same as --date
#   DRY_RUN                 - 1 is the same as --dry-run
#   BATCH_SIZE              - events per PostHog /batch request (default: 1000)
set -euo pipefail

USAGE="usage: $0 [--dry-run] [--date YYYY-MM-DD] [--selftest]"

# Namespace for the version-5 event UUIDs. Permanent: it is part of every
# event key ever sent.
readonly UUID_NAMESPACE="0d08662c-b474-467e-b476-20688fdb1f2c"
readonly EVENT_NAME="cloudsmith_package_downloads"

DRY_RUN="${DRY_RUN:-0}"
SELFTEST=0
SNAPSHOT_DATE="${SNAPSHOT_DATE:-}"
POSTHOG_HOST="${POSTHOG_HOST:-https://us.i.posthog.com}"
CLOUDSMITH_REPOS="${CLOUDSMITH_REPOS:-mezmo/aura}"
CLOUDSMITH_HOST="${CLOUDSMITH_HOST:-https://api.cloudsmith.io}"
PAGE_SIZE="${PAGE_SIZE:-500}"
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

day_before() {
    jq -rn --arg d "$1" '($d + "T00:00:00Z" | fromdateiso8601) - 86400 | strftime("%Y-%m-%d")'
}

# A shell glob accepts digit-shaped nonsense like 2026-99-99, and an invalid
# date reaches the wire as a malformed event timestamp. Round-trip the value
# through jq's calendar and require it back unchanged: that rejects impossible
# dates outright and catches the ones a parser silently rolls over, such as
# 2026-02-30 becoming 2026-03-02. The glob also keeps a quote out of the date,
# which the read-back interpolates into a SQL literal.
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

require_tools() {
    local tool
    for tool in jq curl openssl; do
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

# Last value of a header, matched case-insensitively. HTTP/2 lowercases header
# names and HTTP/1.1 does not, and curl appends a block per redirect and retry,
# so only the final block describes the response the body came from.
header_value() {
    local file=$1 name=$2
    tr -d '\r' < "${file}" \
        | awk -F': ' -v name="${name}" 'tolower($1) == name { value = $2 } END { print value }'
}

# Fetch one page of a repository's package list.
#
# sort=date orders oldest first, which keeps pagination stable against uploads:
# a package uploaded mid-run lands on the last page instead of shifting every
# later package one position down, past a boundary already read.
cloudsmith_page() {
    local repo=$1 page=$2 headers=$3 body=$4
    local args=(--silent --show-error --fail-with-body
                --retry 5 --retry-delay 2 --max-time 60
                --dump-header "${headers}" --output "${body}")
    if [ -n "${CLOUDSMITH_API_KEY:-}" ]; then
        args+=(--header "X-Api-Key: ${CLOUDSMITH_API_KEY}")
    fi
    # The response body names the cause on every likely failure — an unknown
    # repository, a private one read without a key, a rejected key — so print
    # it rather than leaving curl's exit status to speak for it.
    if ! curl "${args[@]}" \
        "${CLOUDSMITH_HOST%/}/v1/packages/${repo}/?page=${page}&page_size=${PAGE_SIZE}&sort=date"
    then
        echo "error: Cloudsmith returned no package list for ${repo} page ${page}" >&2
        if [ -s "${body}" ]; then
            head -c 500 "${body}" >&2
            echo >&2
        fi
        return 1
    fi
}

# Emit one compact JSON object per package in a page of the API response.
package_records() {
    jq -c --arg repo "$2" '
        .[] | {repository: $repo,
               package_id: .identifier_perm,
               package_name: .name,
               package_version: .version,
               package_format: .format,
               filename: .filename,
               architecture: ([.architectures[]?.name] | join(",")),
               distribution: .type_display,
               download_count: .downloads}' "$1"
}

collect_packages() {
    local repo=$1
    local headers="${WORK_DIR}/page.headers" body="${WORK_DIR}/page.json"
    local page=1 pages count first_count=0 got collected=0 value

    # Every response supplies the page total afresh rather than page one
    # settling it for the run. An upload mid-run can append a page beyond the
    # total page one reported, and stopping at that stale total would drop the
    # new package while the count check below, comparing against an equally
    # stale count, still passed.
    while :; do
        cloudsmith_page "${repo}" "${page}" "${headers}" "${body}"
        pages=$(header_value "${headers}" x-pagination-pagetotal)
        count=$(header_value "${headers}" x-pagination-count)
        for value in "${pages}" "${count}"; do
            case "${value}" in
                '' | *[!0-9]*)
                    echo "error: ${repo}: Cloudsmith returned unusable pagination headers on page ${page} (pagetotal='${pages}' count='${count}')" >&2
                    return 1 ;;
            esac
        done
        [ "${page}" -ne 1 ] || first_count="${count}"

        got=$(jq 'length' "${body}")
        collected=$(( collected + got ))
        package_records "${body}" "${repo}"

        [ "${page}" -lt "${pages}" ] || break
        page=$(( page + 1 ))
    done

    # A page that came back short would under-report without failing anything.
    # Uploads only ever append under sort=date, so ending up above the count
    # page one reported is expected; ending up below it means a page was lost
    # between the header and the records.
    if [ "${collected}" -lt "${first_count}" ]; then
        echo "error: ${repo}: collected ${collected} of ${first_count} package(s)" >&2
        return 1
    fi
    echo "Collected ${collected} ${repo} package(s)" >&2
}

# Prefix each package record with its event UUID, tab separated. The record
# stays JSON throughout, so package names are never parsed out of a delimited
# field.
key_packages() {
    local packages=$1 date=$2 uuids=$3 key
    local keys="${uuids}.keys"
    jq -r '.repository + "|" + .package_id' "${packages}" > "${keys}"
    while IFS= read -r key; do
        uuid5 "${UUID_NAMESPACE}" "${key}|${date}"
    done < "${keys}" > "${uuids}"
    paste "${uuids}" "${packages}"
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
              distinct_id: ("cloudsmith:" + $r.repository),
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

# Count how many of this run's own event UUIDs are queryable at this run's
# timestamp. A date-wide count would also match UUIDs left by an earlier run
# whose package set differed, and those extras can satisfy the threshold while
# an event from the current payload is still missing. Pinning the timestamp too
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

# The download total the stored snapshot accounts for.
#
# Scoping to this run's UUIDs cannot answer whether a retry landed: those UUIDs
# are derived from the snapshot date, so an earlier run for the same date has
# already ingested every one of them, and they stay present however completely
# the retry was discarded. Only the total moves when the counts do, and
# cumulative counts only rise, so a total at least as high as the one just
# collected is a bar a staler snapshot cannot clear.
#
# max() per package mirrors what reporting must do, since a re-run's rows stay
# visible until PostHog's background merges collapse them.
#
# printf builds the literals rather than nested shell quoting, which is easy to
# get wrong in a way that still returns a well-formed answer: a quote stray
# inside the string literal matches no rows and reads as "nothing ingested".
downloads_query() {
    printf "SELECT sum(dl) FROM (SELECT properties.repository AS repo, properties.package_id AS pid, max(toIntOrZero(toString(properties.download_count))) AS dl FROM events WHERE event = '%s' AND timestamp = toDateTime('%s 23:59:59') GROUP BY repo, pid)" \
        "${EVENT_NAME}" "$1"
}

# A date-wide count, for asking whether a day holds any snapshot at all. This
# run has no UUIDs for an earlier date to scope by.
snapshot_count_query() {
    printf "SELECT count(DISTINCT uuid) FROM events WHERE event = '%s' AND properties.snapshot_date = '%s'" \
        "${EVENT_NAME}" "$1"
}

# Poll until the snapshot is queryable, since ingestion lags the send.
verify_snapshot() {
    local date=$1 want_events=$2 want_downloads=$3 deadline events downloads value
    deadline=$(( $(date +%s) + VERIFY_TIMEOUT ))
    while :; do
        events=$(count_ingested "${date}")
        downloads=$(hogql_scalar "$(downloads_query "${date}")")
        for value in "${events}" "${downloads}"; do
            case "${value}" in
                '' | *[!0-9]*)
                    echo "error: PostHog answered the read-back with '${events}' event(s) and '${downloads}' download(s), which are not both counts" >&2
                    return 1 ;;
            esac
        done
        if [ "${events}" -ge "${want_events}" ] && [ "${downloads}" -ge "${want_downloads}" ]; then
            echo "Verified ${events} of ${want_events} event(s) and ${downloads} download(s) queryable in PostHog for ${date}"
            return 0
        fi
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            echo "error: PostHog holds ${events} of ${want_events} event(s) and ${downloads} of ${want_downloads} download(s) for ${date} after ${VERIFY_TIMEOUT}s" >&2
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
    got=$(hogql_scalar "$(snapshot_count_query "${previous}")")
    if [ "${got}" = "0" ]; then
        echo "::warning::No Cloudsmith download snapshot in PostHog for ${previous}; the package list reports only current totals, so re-running that date would file today's counts under it"
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
    got=$(uuid5 "${UUID_NAMESPACE}" "mezmo/aura|9yGBoK4cRlDo|2026-09-01")
    [ "${got}" = "$(uuid5 "${UUID_NAMESPACE}" "mezmo/aura|9yGBoK4cRlDo|2026-09-01")" ] \
        || { echo "selftest: uuid5 is not deterministic" >&2; exit 1; }
    [ "${got}" != "$(uuid5 "${UUID_NAMESPACE}" "mezmo/aura|9yGBoK4cRlDo|2026-09-02")" ] \
        || { echo "selftest: uuid5 collides across snapshot dates" >&2; exit 1; }

    # Pagination headers arrive CRLF terminated, and their case depends on the
    # negotiated HTTP version.
    printf 'HTTP/2 200 \r\nX-Pagination-Count: 150\r\nx-pagination-pagetotal: 2\r\n\r\n' \
        > "${tmp}/headers"
    got=$(header_value "${tmp}/headers" x-pagination-count)
    [ "${got}" = "150" ] || { echo "selftest: pagination count = '${got}', want 150" >&2; exit 1; }
    got=$(header_value "${tmp}/headers" x-pagination-pagetotal)
    [ "${got}" = "2" ] || { echo "selftest: pagination pagetotal = '${got}', want 2" >&2; exit 1; }

    # A page maps onto the properties reporting reads, and a package carrying
    # no architecture is still a package worth counting.
    cat > "${tmp}/page.json" <<'JSON'
[{"identifier_perm": "9yGBoK4cRlDo", "name": "aura", "version": "0.2.15-1", "format": "rpm",
  "filename": "aura-0.2.15-1.x86_64.rpm", "architectures": [{"name": "x86_64"}],
  "type_display": "any-distro/any-version", "downloads": 7, "size": 19359632},
 {"identifier_perm": "3uq7JmasvnKF", "name": "aura", "version": "0.1.18", "format": "raw",
  "filename": "aura.tar.gz", "architectures": null,
  "type_display": null, "downloads": 0}]
JSON
    got=$(package_records "${tmp}/page.json" "mezmo/aura" \
        | jq -r '[.repository, .package_id, .package_format, .architecture,
                  (.distribution | tostring), (.download_count | tostring)] | join("|")')
    want="mezmo/aura|9yGBoK4cRlDo|rpm|x86_64|any-distro/any-version|7
mezmo/aura|3uq7JmasvnKF|raw||null|0"
    [ "${got}" = "${want}" ] || { echo "selftest: package records are"$'\n'"${got}"$'\n'"want"$'\n'"${want}" >&2; exit 1; }

    # A package name carrying a tab and a quote must survive into the payload.
    printf '%s\t%s\n' "3538a427-3e7d-5170-bf7d-e9560ebb3468" \
        '{"repository":"mezmo/aura","package_id":"9yGBoK4cRlDo","package_name":"a\tb\"c","package_version":"0.2.15-1","package_format":"rpm","download_count":7}' \
        > "${tmp}/chunk"
    payload=$(build_batch "${tmp}/chunk" "2026-09-01" "phc_test")

    got=$(jq -r '.batch[0].properties.package_name' <<<"${payload}")
    [ "${got}" = "$(printf 'a\tb"c')" ] || { echo "selftest: package name mangled: ${got}" >&2; exit 1; }
    got=$(jq -r '.batch[0].timestamp' <<<"${payload}")
    [ "${got}" = "2026-09-01T23:59:59Z" ] || { echo "selftest: timestamp = ${got}" >&2; exit 1; }
    got=$(jq -r '.batch[0].properties.download_count | type' <<<"${payload}")
    [ "${got}" = "number" ] || { echo "selftest: download_count is ${got}, want number" >&2; exit 1; }
    got=$(jq -r '.batch[0].properties["$process_person_profile"]' <<<"${payload}")
    [ "${got}" = "false" ] || { echo "selftest: person profiles not suppressed" >&2; exit 1; }
    got=$(jq -r '.batch[0].distinct_id' <<<"${payload}")
    [ "${got}" = "cloudsmith:mezmo/aura" ] || { echo "selftest: distinct_id = ${got}" >&2; exit 1; }

    # The read-back queries must quote their literals exactly once. Nested
    # quoting bugs here return 0 rows, which is indistinguishable from a failed
    # send.
    got=$(downloads_query "2026-08-20")
    want="SELECT sum(dl) FROM (SELECT properties.repository AS repo, properties.package_id AS pid, max(toIntOrZero(toString(properties.download_count))) AS dl FROM events WHERE event = 'cloudsmith_package_downloads' AND timestamp = toDateTime('2026-08-20 23:59:59') GROUP BY repo, pid)"
    [ "${got}" = "${want}" ] || { echo "selftest: downloads query is"$'\n'"  ${got}"$'\n'"want"$'\n'"  ${want}" >&2; exit 1; }

    got=$(snapshot_count_query "2026-08-20")
    want="SELECT count(DISTINCT uuid) FROM events WHERE event = 'cloudsmith_package_downloads' AND properties.snapshot_date = '2026-08-20'"
    [ "${got}" = "${want}" ] || { echo "selftest: count query is"$'\n'"  ${got}"$'\n'"want"$'\n'"  ${want}" >&2; exit 1; }

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

    if [ -z "${CLOUDSMITH_API_KEY:-}" ]; then
        echo "CLOUDSMITH_API_KEY unset: reading the public package list anonymously"
    fi

    local repo
    for repo in ${CLOUDSMITH_REPOS}; do
        echo "Collecting ${repo} packages"
        collect_packages "${repo}" >> "${WORK_DIR}/packages.ndjson"
    done

    local packages
    packages=$(wc -l < "${WORK_DIR}/packages.ndjson" | tr -d ' ')
    if [ "${packages}" -eq 0 ]; then
        echo "error: no packages found in ${CLOUDSMITH_REPOS}" >&2
        exit 1
    fi

    # A record missing its identifier or its count would ingest as a
    # well-formed event that reports nothing, and the read-back would accept
    # it, so reject the snapshot instead of sending it.
    local unusable
    unusable=$(jq -n '[inputs | select((.package_id | type) != "string"
                                       or (.download_count | type) != "number")] | length' \
        "${WORK_DIR}/packages.ndjson")
    if [ "${unusable}" != 0 ]; then
        echo "error: ${unusable} of ${packages} package(s) lack an identifier or a download count" >&2
        exit 1
    fi

    key_packages "${WORK_DIR}/packages.ndjson" "${date}" "${WORK_DIR}/uuids" > "${WORK_DIR}/keyed"
    split -l "${BATCH_SIZE}" "${WORK_DIR}/keyed" "${WORK_DIR}/chunk."
    # The read-back names this run's UUIDs explicitly, in query-sized groups.
    split -l 500 "${WORK_DIR}/uuids" "${WORK_DIR}/uchunk."

    local chunk chunks=0
    for chunk in "${WORK_DIR}"/chunk.*; do
        chunks=$((chunks + 1))
        build_batch "${chunk}" "${date}" "${POSTHOG_PROJECT_API_KEY:-phc_dry_run}" \
            > "${WORK_DIR}/payload.${chunks}.json"
    done

    # Every collected package must reach a payload. A mismatch means the keying
    # or chunking dropped rows, which would silently under-report.
    local events
    events=$(jq -s '[.[].batch | length] | add' "${WORK_DIR}"/payload.*.json)
    if [ "${events}" != "${packages}" ]; then
        echo "error: built ${events} event(s) from ${packages} package(s)" >&2
        exit 1
    fi

    # The total the read-back has to reach. Taken from what was collected, not
    # from the payload, so it measures the snapshot rather than the encoding.
    local downloads
    downloads=$(jq -n '[inputs.download_count] | add // 0' "${WORK_DIR}/packages.ndjson")

    echo "Snapshot ${date}: ${packages} packages, ${downloads} downloads, in ${chunks} batch(es)"

    if [ "${DRY_RUN}" = 1 ]; then
        jq -c '.batch[0]' "${WORK_DIR}/payload.1.json"
        echo "Dry run: nothing sent to ${POSTHOG_HOST%/}"
        return 0
    fi

    local payload
    for payload in "${WORK_DIR}"/payload.*.json; do
        post_batch "${payload}"
    done

    echo "Sent ${packages} event(s) for ${date} to ${POSTHOG_HOST%/}"

    if [ "${SKIP_VERIFY}" = 1 ]; then
        echo "Skipping read-back verification (SKIP_VERIFY=1)"
        return 0
    fi
    verify_snapshot "${date}" "${packages}" "${downloads}"
    # Advisory only: a missing earlier day is worth reporting but is not a
    # failure of this run, and cannot be repaired by retrying it.
    warn_on_gap "${date}" || true
}

main "$@"
