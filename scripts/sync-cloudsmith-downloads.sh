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
#   VERIFY_TIMEOUT          - seconds to wait for ingestion (default: 600)
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
readonly SUBJECT_PREFIX="cloudsmith"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/posthog-snapshot.sh
. "${SCRIPT_DIR}/lib/posthog-snapshot.sh"

SELFTEST=0
CLOUDSMITH_REPOS="${CLOUDSMITH_REPOS:-mezmo/aura}"
CLOUDSMITH_HOST="${CLOUDSMITH_HOST:-https://api.cloudsmith.io}"
PAGE_SIZE="${PAGE_SIZE:-500}"

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

    # An upload landing after the final page was read cannot be seen by this
    # run, however the loop is written: the list is live and the snapshot is
    # not atomic. Say so rather than omitting the package silently. Cumulative
    # totals mean the next run picks it up, so this is not a failure.
    if [ "${collected}" -lt "${count}" ]; then
        echo "::warning::${repo}: $(( count - collected )) package(s) appeared during collection and are not in this snapshot; the next run includes them" >&2
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

    # The probe UUID must be well formed and never repeat.
    got=$(uuid4)
    case "${got}" in
        [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]-*-4*-[89ab]*-*) ;;
        *) echo "selftest: uuid4 produced '${got}'" >&2; exit 1 ;;
    esac
    [ "${got}" != "$(uuid4)" ] || { echo "selftest: uuid4 repeated" >&2; exit 1; }

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

    # Every batch carries its own probe. One probe in the first batch cannot
    # speak for a later batch that was discarded, because the deterministic
    # events a retry re-sends are already present from the earlier run.
    local probe payload probes=0
    : > "${WORK_DIR}/probes"
    for payload in "${WORK_DIR}"/payload.*.json; do
        probe=$(uuid4)
        printf '%s\n' "${probe}" >> "${WORK_DIR}/probes"
        add_probe "${payload}" "${probe}"
        post_batch "${payload}"
        probes=$(( probes + 1 ))
    done

    echo "Sent ${packages} event(s) for ${date} to ${POSTHOG_HOST%/}"

    if [ "${SKIP_VERIFY}" = 1 ]; then
        echo "Skipping read-back verification (SKIP_VERIFY=1)"
        return 0
    fi
    verify_snapshot "${date}" "${packages}" "${downloads}" "${probes}"
    # Advisory only: a missing earlier day is worth reporting but is not a
    # failure of this run, and cannot be repaired by retrying it.
    warn_on_gap "${date}" || true
}

main "$@"
