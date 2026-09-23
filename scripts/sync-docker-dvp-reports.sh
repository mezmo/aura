#!/usr/bin/env bash
# Snapshot Docker Verified Publisher pull reports into PostHog.
#
# Docker publishes DVP analytics as CSV reports covering a whole week or month,
# and retains only the last few. Nothing reconstructs a report once it ages
# out, so this reads the catalogue on every run and files what is there.
#
# Unlike the other snapshots, the numbers here are not a cumulative counter
# read at run time: each report states the counts for a closed period, so a
# period's value is final and no day-over-day differencing is needed. Events
# are keyed by (repository, granularity, period start) and timestamped at the
# end of the period they describe, which makes a re-run of any report
# byte-identical and therefore free.
#
# These counts are not comparable with the public pull counter that
# sync-docker-downloads.sh records. A week of DVP events annualises far above
# that counter's all-time total, because the two measure different things.
# Keep the two series apart; never add them together.
#
# DATA_DOWNLOADS counts image layer transfers. VERSION_CHECKS counts manifest
# requests that transferred no layers, and EVENT_COUNT is their sum.
#
# Usage: sync-docker-dvp-reports.sh [--dry-run] [--period YYYY-MM-DD]
#                                   [--report trend|technographic] [--selftest]
#   --report     - which report to read (default: trend)
#   --period     - process only the report starting on this date
#   --dry-run    - fetch and build payloads, print a summary, send nothing
#   --selftest   - run the built-in assertions and exit
#
# Environment:
#   DOCKER_USERNAME         - Docker Hub account the token belongs to (required)
#   DOCKER_TOKEN            - Docker Hub personal access token (required)
#   POSTHOG_PROJECT_API_KEY - PostHog project write token (required unless --dry-run)
#   POSTHOG_API_READ_KEY    - personal API key used to read the snapshot back
#   POSTHOG_PROJECT_ID      - numeric project id the read-back queries (default: 443794)
#   POSTHOG_HOST            - PostHog ingest host (default: https://us.i.posthog.com)
#   POSTHOG_API_HOST        - PostHog query host (default: https://us.posthog.com)
#   VERIFY_TIMEOUT          - seconds to wait for ingestion (default: 600)
#   SKIP_VERIFY             - 1 sends without reading the snapshot back
#   DOCKER_NAMESPACE        - publisher namespace to read (default: mezmo)
#   DOCKER_IMAGES           - space-separated repositories to keep (default: mezmo/aura)
#   DVP_GRANULARITY         - weekly or monthly (default: weekly)
#   REPORT_TYPE             - same as --report
#   DOCKER_HUB_HOST         - Docker Hub API host (default: https://hub.docker.com)
#   DRY_RUN                 - 1 is the same as --dry-run
#   BATCH_SIZE              - events per PostHog /batch request (default: 1000)
set -euo pipefail

USAGE="usage: $0 [--dry-run] [--period YYYY-MM-DD] [--report trend|technographic] [--selftest]"

# Namespace for the version-5 event UUIDs. Permanent: it is part of every
# event key ever sent.
readonly UUID_NAMESPACE="6d3cea6d-9fef-46af-aec1-e9c705245832"
readonly SUBJECT_PREFIX="docker-dvp"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/posthog-snapshot.sh
. "${SCRIPT_DIR}/lib/posthog-snapshot.sh"

REPORT_TYPE="${REPORT_TYPE:-trend}"
SELFTEST=0
PERIOD=""
DOCKER_NAMESPACE="${DOCKER_NAMESPACE:-mezmo}"
DOCKER_IMAGES="${DOCKER_IMAGES:-mezmo/aura}"
DVP_GRANULARITY="${DVP_GRANULARITY:-weekly}"
DOCKER_HUB_HOST="${DOCKER_HUB_HOST:-https://hub.docker.com}"
ROOT_DIR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --selftest) SELFTEST=1; shift ;;
        --period)
            if [ $# -lt 2 ]; then echo "error: --period needs a value" >&2; exit 1; fi
            PERIOD="$2"; shift 2 ;;
        --period=*) PERIOD="${1#--period=}"; shift ;;
        --report)
            if [ $# -lt 2 ]; then echo "error: --report needs a value" >&2; exit 1; fi
            REPORT_TYPE="$2"; shift 2 ;;
        --report=*) REPORT_TYPE="${1#--report=}"; shift ;;
        -h|--help) echo "${USAGE}" >&2; exit 0 ;;
        *) echo "${USAGE}" >&2; exit 1 ;;
    esac
done

# The two reports answer different questions and so become different events:
# trend counts pulls, technographic counts the people and organisations behind
# them. Both share everything else about how a report is fetched and filed.
case "${REPORT_TYPE}" in
    trend)          EVENT_NAME="docker_dvp_pulls";    COUNT_PROPERTY="data_downloads" ;;
    technographic)  EVENT_NAME="docker_dvp_audience"; COUNT_PROPERTY="total_pullers" ;;
    *) echo "error: --report must be trend or technographic (got '${REPORT_TYPE}')" >&2; exit 1 ;;
esac

require_tools() {
    local tool
    for tool in jq curl openssl; do
        if ! command -v "${tool}" >/dev/null 2>&1; then
            echo "error: ${tool} not found" >&2
            exit 1
        fi
    done
    # The report catalogue is not public, so even a dry run has to authenticate.
    if [ -z "${DOCKER_USERNAME:-}" ] || [ -z "${DOCKER_TOKEN:-}" ]; then
        echo "error: DOCKER_USERNAME and DOCKER_TOKEN are required" >&2
        exit 1
    fi
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

# Exchange the personal access token for a session JWT.
#
# The token is not a bearer credential: passing it to the analytics API is
# rejected. The JWT it buys is short lived, which is why one is minted per run
# rather than cached anywhere.
docker_jwt() {
    local body status
    body=$(jq -n --arg u "${DOCKER_USERNAME}" --arg p "${DOCKER_TOKEN}" \
        '{username: $u, password: $p}')
    status=$(printf '%s' "${body}" \
        | curl --silent --show-error --retry 3 --retry-delay 2 --max-time 30 \
            --write-out '%{http_code}' --output "${ROOT_DIR}/login.json" \
            --header 'Content-Type: application/json' --data-binary @- \
            "${DOCKER_HUB_HOST%/}/v2/users/login")
    if [ "${status}" != 200 ]; then
        echo "error: Docker Hub login returned ${status} for ${DOCKER_USERNAME}" >&2
        return 1
    fi
    jq -r '.token' "${ROOT_DIR}/login.json"
}

# List the summary reports for the configured granularity, oldest first, as
# "period<TAB>url".
#
# Docker's documented analytics API answers this namespace with no data at all;
# the publisher dashboard reads proxylytics, which is what this uses.
list_reports() {
    local jwt=$1 status
    status=$(curl --silent --show-error --retry 3 --retry-delay 2 --max-time 60 \
        --write-out '%{http_code}' --output "${ROOT_DIR}/reports.json" \
        --header "Authorization: Bearer ${jwt}" \
        "${DOCKER_HUB_HOST%/}/api/publisher/proxylytics/v1/namespaces/${DOCKER_NAMESPACE}/reports")
    if [ "${status}" != 200 ]; then
        echo "error: report catalogue returned ${status} for ${DOCKER_NAMESPACE}" >&2
        return 1
    fi
    jq -r --arg gran "${DVP_GRANULARITY}" --arg type "${REPORT_TYPE}" '
        .reports[]
        | select(.type == $type)
        | (.url | split("/") | last) as $file
        | select($file | contains("_" + $gran + "_"))
        | ($file | capture("_(?<d>[0-9]{4})_(?<m>[0-9]{2})_(?<day>[0-9]{2})\\.csv")) as $p
        | [$p.d + "-" + $p.m + "-" + $p.day, .url] | @tsv' \
        "${ROOT_DIR}/reports.json" | sort
}

# The last instant the report describes: a weekly report is named for the
# Monday it starts on and covers through the Sunday, a monthly report for the
# first of its month.
period_end() {
    local start=$1
    case "${DVP_GRANULARITY}" in
        weekly)
            jq -rn --arg d "${start}" \
                '($d + "T00:00:00Z" | fromdateiso8601) + 6*86400 | strftime("%Y-%m-%d")' ;;
        monthly)
            jq -rn --arg d "${start}" \
                '($d[0:8] + "01" + "T00:00:00Z" | fromdateiso8601) as $s
                 | ($s + 32*86400) | strftime("%Y-%m-01") | . + "T00:00:00Z"
                 | fromdateiso8601 - 86400 | strftime("%Y-%m-%d")' ;;
        *) echo "error: DVP_GRANULARITY must be weekly or monthly (got '${DVP_GRANULARITY}')" >&2
           return 1 ;;
    esac
}

# Emit one compact JSON object per (repository, tag) in a report.
#
# The trend report breaks each repository down by tag, country, cloud provider
# and client. Country is summed back up; the rest are kept. Summing every tag returns the
# repository totals the summary report states, so nothing is lost by reading
# the trend report instead.
#
# A pull by digest carries no tag and the export writes the literal \\N
# there, which becomes a null tag and by_digest true rather than a row named
# after an escape sequence. These are the majority of pulls, so dropping them
# would understate the repository badly.
# Split one CSV line, honouring quoted fields.
#
# A plain split(",") shifts every later column the moment a field contains a
# comma, which reads as a valid row with the wrong values rather than as an
# error. Docker normalises the text fields today, so this changes nothing for
# the reports as they stand; it stops a future comma from silently rewriting
# the counts.
readonly CSV_SPLIT='def csvsplit: [ match("(\"(?:[^\"]|\"\")*\"|[^,]*)(,|$)"; "g") | .captures[0].string | if startswith("\"") then .[1:-1] | gsub("\"\"";"\"") else . end ] | if (length > 0 and .[-1] == "") then .[0:-1] else . end;'

report_records() {
    local url=$1 jwt=$2 period=$3 csv="${WORK_DIR}/report.csv"
    curl --silent --show-error --location --retry 3 --retry-delay 2 --max-time 120 \
        --fail --header "Authorization: Bearer ${jwt}" --output "${csv}" "${url}"
    case "${REPORT_TYPE}" in
        trend)         trend_records "${csv}" "${period}" ;;
        technographic) technographic_records "${csv}" "${period}" ;;
    esac
}

trend_records() {
    local csv=$1 period=$2 wanted=" ${DOCKER_IMAGES} "
    jq -Rs -c --arg wanted "${wanted}" --arg period "${period}" \
           --arg gran "${DVP_GRANULARITY}" "${CSV_SPLIT}"'
        split("\n")[1:]
        | map(select(length > 0) | csvsplit)
        | map(. as $row | select($wanted | contains(" " + $row[3] + " ")))
        | group_by(.[3] + "\u0000" + .[8] + "\u0000" + .[7] + "\u0000" + .[6])
        | map({repository: .[0][3],
               namespace: (.[0][3] | split("/")[0]),
               image: (.[0][3] | split("/")[1]),
               tag: (if .[0][8] == "\\\\N" then null else .[0][8] end),
               by_digest: (.[0][8] == "\\\\N"),
               user_agent: .[0][7],
               cloud_service_provider: .[0][6],
               granularity: $gran,
               period_start: $period,
               data_downloads: (map(.[9] | tonumber) | add),
               version_checks: (map(.[10] | tonumber) | add),
               pulls: (map(.[11] | tonumber) | add)})
        | .[]' "${csv}"
}

# One event per image carrying its distinct puller and domain counts, plus one
# per image it shares pullers with.
#
# TOTAL_PULLERS and TOTAL_DOMAINS are the deduplicated counts for the whole
# period, which is the only place Docker reports them: the per-row unique
# counts in the trend report cannot be added up, because one person pulling two
# tags appears in both rows. They repeat on every row of this report, so the
# image-level event carries them once and reporting takes max() rather than a
# sum.
technographic_records() {
    local csv=$1 period=$2 wanted=" ${DOCKER_IMAGES} "
    jq -Rs -c --arg wanted "${wanted}" --arg period "${period}" \
           --arg gran "${DVP_GRANULARITY}" "${CSV_SPLIT}"'
        (split("\n")[1:]
         | map(select(length > 0) | csvsplit)
         | map(. as $row | select($wanted | contains(" " + $row[3] + " ")))) as $rows
        | ($rows | group_by(.[3]) | map({
              repository: .[0][3],
              namespace: (.[0][3] | split("/")[0]),
              image: (.[0][3] | split("/")[1]),
              paired_image: null,
              granularity: $gran,
              period_start: $period,
              total_pullers: (.[0][6] | tonumber),
              total_domains: (.[0][9] | tonumber)}))
          + ($rows | map({
              repository: .[3],
              namespace: (.[3] | split("/")[0]),
              image: (.[3] | split("/")[1]),
              paired_image: .[4],
              granularity: $gran,
              period_start: $period,
              total_pullers: (.[6] | tonumber),
              total_domains: (.[9] | tonumber),
              paired_users: (.[5] | tonumber),
              paired_domains: (.[8] | tonumber),
              pct_users: (.[7] | tonumber),
              pct_domains: (.[10] | tonumber)}))
        | .[]' "${csv}"
}

# Prefix each row with its event UUID, tab separated. Keyed by the period the
# report describes rather than by when it was fetched, so re-reading a report
# on any later day produces the same events.
key_rows() {
    local rows=$1 uuids=$2 key
    local keys="${uuids}.keys"
    case "${REPORT_TYPE}" in
        trend)
            jq -r '"docker-dvp|" + .repository + "|" + (.tag // "<digest>")
                   + "|" + .user_agent + "|" + .cloud_service_provider
                   + "|" + .granularity + "|" + .period_start' "${rows}" > "${keys}" ;;
        technographic)
            jq -r '"docker-dvp-audience|" + .repository + "|" + (.paired_image // "<all>")
                   + "|" + .granularity + "|" + .period_start' "${rows}" > "${keys}" ;;
    esac
    while IFS= read -r key; do
        uuid5 "${UUID_NAMESPACE}" "${key}"
    done < "${keys}" > "${uuids}"
    paste "${uuids}" "${rows}"
}

selftest() {
    local tmp=$1 got want payload

    got=$(uuid5 6ba7b810-9dad-11d1-80b4-00c04fd430c8 "python.org")
    want="886313e1-3b8a-5372-9b90-0c9aee199e5d"
    [ "${got}" = "${want}" ] || { echo "selftest: uuid5(python.org) = ${got}, want ${want}" >&2; exit 1; }

    # A period keys the same event forever, and different periods do not collide.
    got=$(uuid5 "${UUID_NAMESPACE}" "docker-dvp|mezmo/aura|weekly|2026-08-31")
    [ "${got}" = "$(uuid5 "${UUID_NAMESPACE}" "docker-dvp|mezmo/aura|weekly|2026-08-31")" ] \
        || { echo "selftest: uuid5 is not deterministic" >&2; exit 1; }
    [ "${got}" != "$(uuid5 "${UUID_NAMESPACE}" "docker-dvp|mezmo/aura|weekly|2026-08-24")" ] \
        || { echo "selftest: uuid5 collides across periods" >&2; exit 1; }
    [ "${got}" != "$(uuid5 "${UUID_NAMESPACE}" "docker-dvp|mezmo/aura|monthly|2026-08-31")" ] \
        || { echo "selftest: uuid5 collides across granularities" >&2; exit 1; }

    got=$(uuid4)
    case "${got}" in
        [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]-*-4*-[89ab]*-*) ;;
        *) echo "selftest: uuid4 produced '${got}'" >&2; exit 1 ;;
    esac
    [ "${got}" != "$(uuid4)" ] || { echo "selftest: uuid4 repeated" >&2; exit 1; }

    # A weekly report names its Monday and runs through the Sunday.
    got=$(period_end "2026-08-31"); want="2026-09-06"
    [ "${got}" = "${want}" ] || { echo "selftest: period_end(weekly) = ${got}, want ${want}" >&2; exit 1; }
    got=$(DVP_GRANULARITY=monthly period_end "2026-08-01"); want="2026-08-31"
    [ "${got}" = "${want}" ] || { echo "selftest: period_end(monthly) = ${got}, want ${want}" >&2; exit 1; }
    got=$(DVP_GRANULARITY=monthly period_end "2026-02-01"); want="2026-02-28"
    [ "${got}" = "${want}" ] || { echo "selftest: period_end(feb) = ${got}, want ${want}" >&2; exit 1; }

    got=$(snapshot_count_query "2026-08-20")
    want="SELECT count(DISTINCT uuid) FROM events WHERE event = 'docker_dvp_pulls' AND properties.snapshot_date = '2026-08-20'"
    [ "${got}" = "${want}" ] || { echo "selftest: query is"$'\n'"  ${got}"$'\n'"want"$'\n'"  ${want}" >&2; exit 1; }

    # Exercise the real parser, not a copy of it: rows collapse to one group
    # per tag, client and provider, a digest pull keeps its counts, and a
    # quoted field carrying a comma does not shift the columns after it.
    WORK_DIR="${tmp}"
    cat > "${tmp}/fixture.csv" <<'CSV'
DATE_GRANULARITY,DATE_REFERENCE,PUBLISHER_NAME,IMAGE_REPOSITORY,NAMESPACE,IP_COUNTRY,CLOUD_SERVICE_PROVIDER,USER_AGENT,TAG,DATA_DOWNLOADS,VERSION_CHECKS,PULLS,UNIQUE_AUTHENTICATED_USERS,UNIQUE_UNAUTHENTICATED_USERS
week,2026-08-31,mezmo,mezmo/aura,mezmo,DE,no csp,docker,latest,3,1,4,0,1
week,2026-08-31,mezmo,mezmo/aura,mezmo,SE,no csp,docker,latest,5,2,7,0,1
week,2026-08-31,mezmo,mezmo/aura,mezmo,US,no csp,docker,\\N,9,0,9,0,1
week,2026-08-31,mezmo,mezmo/aura,mezmo,GB,no csp,"curl, 8.4",latest,2,0,2,0,1
week,2026-08-31,mezmo,mezmo/vector,mezmo,US,no csp,docker,latest,99,9,108,0,1
CSV
    got=$(DOCKER_IMAGES="mezmo/aura" DVP_GRANULARITY="weekly" \
          trend_records "${tmp}/fixture.csv" "2026-08-31")

    [ "$(printf '%s\n' "${got}" | wc -l | tr -d ' ')" = "3" ] \
        || { echo "selftest: expected 3 groups, got:"$'\n'"${got}" >&2; exit 1; }
    [ "$(printf '%s\n' "${got}" | jq -r 'select(.by_digest) | .pulls')" = "9" ] \
        || { echo "selftest: digest group lost its pulls" >&2; exit 1; }
    [ "$(printf '%s\n' "${got}" | jq -r 'select(.by_digest) | .tag')" = "null" ] \
        || { echo "selftest: digest row kept the escape marker as a tag" >&2; exit 1; }
    [ "$(printf '%s\n' "${got}" | jq -r 'select(.user_agent == "docker" and .tag == "latest") | .pulls')" = "11" ] \
        || { echo "selftest: latest did not sum across countries" >&2; exit 1; }

    # The quoted user agent keeps its comma, and the columns after it survive.
    [ "$(printf '%s\n' "${got}" | jq -r 'select(.user_agent | test(",")) | .user_agent')" = "curl, 8.4" ] \
        || { echo "selftest: quoted field lost its comma" >&2; exit 1; }
    [ "$(printf '%s\n' "${got}" | jq -r 'select(.user_agent | test(",")) | .pulls')" = "2" ] \
        || { echo "selftest: a quoted comma shifted the numeric columns" >&2; exit 1; }
    [ -z "$(printf '%s\n' "${got}" | jq -r 'select(.repository != "mezmo/aura")')" ] \
        || { echo "selftest: another repository leaked through the filter" >&2; exit 1; }

    printf '%s\t%s\n' "3538a427-3e7d-5170-bf7d-e9560ebb3468" \
        '{"repository":"mezmo/aura","namespace":"mezmo","image":"aura","tag":"latest","by_digest":false,"granularity":"weekly","period_start":"2026-08-31","data_downloads":1158,"version_checks":456,"pulls":1614}' \
        > "${tmp}/chunk"
    payload=$(build_batch "${tmp}/chunk" "2026-09-06" "phc_test")
    got=$(jq -r '.batch[0].properties.data_downloads | type' <<<"${payload}")
    [ "${got}" = "number" ] || { echo "selftest: data_downloads is ${got}, want number" >&2; exit 1; }
    got=$(jq -r '.batch[0].timestamp' <<<"${payload}")
    [ "${got}" = "2026-09-06T23:59:59Z" ] || { echo "selftest: timestamp = ${got}" >&2; exit 1; }
    got=$(jq -r '.batch[0].distinct_id' <<<"${payload}")
    [ "${got}" = "docker-dvp:mezmo/aura" ] || { echo "selftest: distinct_id = ${got}" >&2; exit 1; }

    echo "selftest: ok"
}

# Collect, send and verify one report as its own unit, so each period's events
# carry the timestamp of the period they describe.
sync_report() {
    local period=$1 url=$2 jwt=$3 ends rows chunk chunks=0 events probe payload probes=0 counted

    ends=$(period_end "${period}")
    report_records "${url}" "${jwt}" "${period}" > "${WORK_DIR}/rows.ndjson"
    rows=$(wc -l < "${WORK_DIR}/rows.ndjson" | tr -d ' ')
    if [ "${rows}" -eq 0 ]; then
        echo "  ${period}: no rows for ${DOCKER_IMAGES}, skipping"
        return 0
    fi

    key_rows "${WORK_DIR}/rows.ndjson" "${WORK_DIR}/uuids" > "${WORK_DIR}/keyed"
    split -l "${BATCH_SIZE}" "${WORK_DIR}/keyed" "${WORK_DIR}/chunk."
    for chunk in "${WORK_DIR}"/chunk.*; do
        chunks=$((chunks + 1))
        build_batch "${chunk}" "${ends}" "${POSTHOG_PROJECT_API_KEY:-phc_dry_run}" \
            > "${WORK_DIR}/payload.${chunks}.json"
    done

    events=$(jq -s '[.[].batch | length] | add' "${WORK_DIR}"/payload.*.json)
    if [ "${events}" != "${rows}" ]; then
        echo "error: built ${events} event(s) from ${rows} row(s) for ${period}" >&2
        return 1
    fi

    # The counter differs per report, so the read-back total is summed over
    # whichever property this report's events carry.
    local counted
    counted=$(jq -s --arg k "${COUNT_PROPERTY}" '[.[][$k]] | add' "${WORK_DIR}/rows.ndjson")
    echo "  ${period} through ${ends}: ${rows} row(s), ${counted} ${COUNT_PROPERTY}"

    if [ "${DRY_RUN}" = 1 ]; then
        jq -c '.batch[0]' "${WORK_DIR}/payload.1.json"
        return 0
    fi

    : > "${WORK_DIR}/probes"
    for payload in "${WORK_DIR}"/payload.*.json; do
        probe=$(uuid4)
        printf '%s\n' "${probe}" >> "${WORK_DIR}/probes"
        add_probe "${payload}" "${probe}"
        post_batch "${payload}"
        probes=$(( probes + 1 ))
    done

    if [ "${SKIP_VERIFY}" = 1 ]; then
        return 0
    fi
    verify_snapshot "${ends}" "${rows}" "${counted}" "${probes}"
}

main() {
    ROOT_DIR=$(mktemp -d)
    trap 'rm -rf "${ROOT_DIR}"' EXIT

    if [ "${SELFTEST}" = 1 ]; then
        selftest "${ROOT_DIR}"
        return 0
    fi

    require_tools

    if [ -n "${PERIOD}" ] && ! valid_date "${PERIOD}"; then
        echo "error: --period must be a real calendar date as YYYY-MM-DD (got '${PERIOD}')" >&2
        exit 1
    fi

    local jwt
    jwt=$(docker_jwt)

    list_reports "${jwt}" > "${ROOT_DIR}/reports.tsv"
    local available
    available=$(wc -l < "${ROOT_DIR}/reports.tsv" | tr -d ' ')
    if [ "${available}" -eq 0 ]; then
        echo "error: no ${DVP_GRANULARITY} summary reports for ${DOCKER_NAMESPACE}" >&2
        exit 1
    fi
    echo "Docker DVP: ${available} ${DVP_GRANULARITY} report(s) available for ${DOCKER_NAMESPACE}"

    local period url synced=0
    while IFS=$'\t' read -r period url; do
        [ -z "${PERIOD}" ] || [ "${PERIOD}" = "${period}" ] || continue
        WORK_DIR="${ROOT_DIR}/${period}"
        mkdir -p "${WORK_DIR}"
        sync_report "${period}" "${url}" "${jwt}"
        synced=$((synced + 1))
    done < "${ROOT_DIR}/reports.tsv"

    if [ "${synced}" -eq 0 ]; then
        echo "error: no report matches --period ${PERIOD}" >&2
        exit 1
    fi
    if [ "${DRY_RUN}" = 1 ]; then
        echo "Dry run: nothing sent to ${POSTHOG_HOST%/}"
        return 0
    fi
    echo "Synced ${synced} ${DVP_GRANULARITY} report(s) to ${POSTHOG_HOST%/}"
}

main "$@"
