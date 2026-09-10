#!/usr/bin/env bash
# Snapshot cumulative Docker Hub pull totals into PostHog.
#
# Sends one PostHog event per image carrying that image's cumulative pull
# count. Retries are safe: the event UUID is derived from (image, snapshot
# date) and the timestamp is pinned to 23:59:59Z on the snapshot date, so
# re-running a date re-sends byte-identical events that PostHog deduplicates.
#
# The count is approximate for the day it names. Docker Hub publishes only a
# live cumulative counter, never a historical one, so a run reads the counter
# at the moment it executes and attributes it to the previous UTC day. Every
# snapshot carries the same offset, so day-over-day differences still cover a
# true 24 hours; a single day's cumulative value runs slightly ahead of its
# label. Naming an older date does not reconstruct it.
#
# Reporting should aggregate with max(pull_count) per image and snapshot date.
# PostHog deduplication is eventual, and cumulative counts only ever rise, so
# max() is correct while duplicates remain visible.
#
# This is the public counter, which is not the same measure as the Docker
# Verified Publisher reports: a week of DVP events annualises far above this
# counter's all-time total, because the two count different things. Keep the
# two series apart; never add them together.
#
# Usage: sync-docker-downloads.sh [--dry-run] [--date YYYY-MM-DD] [--selftest]
#   --date       - snapshot date (default: yesterday, UTC)
#   --dry-run    - collect and build the payload, print a summary, send nothing
#   --selftest   - run the built-in assertions and exit
#
# Environment:
#   POSTHOG_PROJECT_API_KEY - PostHog project write token (required unless --dry-run)
#   POSTHOG_API_READ_KEY    - personal API key used to read the snapshot back
#   POSTHOG_PROJECT_ID      - numeric project id the read-back queries (default: 443794)
#   POSTHOG_HOST            - PostHog ingest host (default: https://us.i.posthog.com)
#   POSTHOG_API_HOST        - PostHog query host (default: https://us.posthog.com)
#   VERIFY_TIMEOUT          - seconds to wait for ingestion (default: 600)
#   SKIP_VERIFY             - 1 sends without reading the snapshot back
#   DOCKER_IMAGES           - space-separated namespace/image list (default: mezmo/aura)
#   DOCKER_HUB_HOST         - Docker Hub API host (default: https://hub.docker.com)
#   SNAPSHOT_DATE           - same as --date
#   DRY_RUN                 - 1 is the same as --dry-run
#   BATCH_SIZE              - events per PostHog /batch request (default: 1000)
set -euo pipefail

USAGE="usage: $0 [--dry-run] [--date YYYY-MM-DD] [--selftest]"

# Namespace for the version-5 event UUIDs. Permanent: it is part of every
# event key ever sent.
readonly UUID_NAMESPACE="6d3cea6d-9fef-46af-aec1-e9c705245832"
readonly EVENT_NAME="docker_image_pulls"
readonly SUBJECT_PREFIX="docker"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/posthog-snapshot.sh
. "${SCRIPT_DIR}/lib/posthog-snapshot.sh"

COUNT_PROPERTY="pull_count"
SELFTEST=0
DOCKER_IMAGES="${DOCKER_IMAGES:-mezmo/aura}"
DOCKER_HUB_HOST="${DOCKER_HUB_HOST:-https://hub.docker.com}"

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

# Emit one compact JSON object per image.
#
# The repository endpoint answers for a single image, so there is nothing to
# paginate and no page-total race: each image costs one request whose absence
# is fatal rather than silently short.
collect_pulls() {
    local image=$1 body="${WORK_DIR}/image.json" status
    status=$(curl --silent --show-error --retry 3 --retry-delay 2 --max-time 30 \
        --write-out '%{http_code}' --output "${body}" \
        "${DOCKER_HUB_HOST%/}/v2/repositories/${image}/")
    if [ "${status}" != 200 ]; then
        echo "error: Docker Hub returned ${status} for ${image}" >&2
        return 1
    fi
    jq -c '{repository: (.namespace + "/" + .name), namespace: .namespace,
            image: .name, pull_count: .pull_count, star_count: .star_count}' "${body}"
}

# Prefix each image record with its event UUID, tab separated. The record stays
# JSON throughout, so no field is parsed out of a delimited string.
key_images() {
    local images=$1 date=$2 uuids=$3 key
    local keys="${uuids}.keys"
    jq -r '"docker|" + .repository' "${images}" > "${keys}"
    while IFS= read -r key; do
        uuid5 "${UUID_NAMESPACE}" "${key}|${date}"
    done < "${keys}" > "${uuids}"
    paste "${uuids}" "${images}"
}

# A dropped or disabled scheduled run leaves a hole no failure reports, since
# nothing ran to fail. Surface it on the next run that does happen.
warn_on_gap() {
    local date=$1 previous got
    previous=$(day_before "${date}")
    got=$(hogql_scalar "$(snapshot_count_query "${previous}")")
    if [ "${got}" = "0" ]; then
        echo "::warning::No Docker pull snapshot in PostHog for ${previous}; Docker Hub reports only current totals, so re-running that date would file today's counts under it"
    fi
}

selftest() {
    local tmp=$1 got want d payload

    # Well-known RFC 4122 vector, an empty name, and a real event key.
    got=$(uuid5 6ba7b810-9dad-11d1-80b4-00c04fd430c8 "python.org")
    want="886313e1-3b8a-5372-9b90-0c9aee199e5d"
    [ "${got}" = "${want}" ] || { echo "selftest: uuid5(python.org) = ${got}, want ${want}" >&2; exit 1; }

    # The same key must key the same event on every run, and differ per date.
    got=$(uuid5 "${UUID_NAMESPACE}" "docker|mezmo/aura|2026-09-01")
    [ "${got}" = "$(uuid5 "${UUID_NAMESPACE}" "docker|mezmo/aura|2026-09-01")" ] \
        || { echo "selftest: uuid5 is not deterministic" >&2; exit 1; }
    [ "${got}" != "$(uuid5 "${UUID_NAMESPACE}" "docker|mezmo/aura|2026-09-02")" ] \
        || { echo "selftest: uuid5 collides across snapshot dates" >&2; exit 1; }

    # The probe UUID must be well formed and never repeat.
    got=$(uuid4)
    case "${got}" in
        [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]-*-4*-[89ab]*-*) ;;
        *) echo "selftest: uuid4 produced '${got}'" >&2; exit 1 ;;
    esac
    [ "${got}" != "$(uuid4)" ] || { echo "selftest: uuid4 repeated" >&2; exit 1; }

    # Calendar validation must reject digit-shaped non-dates and rollovers.
    for d in 2026-09-01 2024-02-29 2026-12-31; do
        valid_date "${d}" || { echo "selftest: rejected valid date ${d}" >&2; exit 1; }
    done
    for d in 2026-99-99 2026-13-01 2026-00-00 2026-02-30 2025-02-29 2026-9-1 "" "2026-09-0'"; do
        if valid_date "${d}"; then echo "selftest: accepted invalid date '${d}'" >&2; exit 1; fi
    done

    # The read-back query must quote its literals exactly once.
    got=$(snapshot_count_query "2026-08-20")
    want="SELECT count(DISTINCT uuid) FROM events WHERE event = 'docker_image_pulls' AND properties.snapshot_date = '2026-08-20'"
    [ "${got}" = "${want}" ] || { echo "selftest: query is"$'\n'"  ${got}"$'\n'"want"$'\n'"  ${want}" >&2; exit 1; }

    # Date arithmetic across month, year, and leap-day boundaries.
    got=$(day_before "2026-03-01"); want="2026-02-28"
    [ "${got}" = "${want}" ] || { echo "selftest: day_before(2026-03-01) = ${got}, want ${want}" >&2; exit 1; }
    got=$(TZ=Pacific/Auckland day_before "2024-03-01"); want="2024-02-29"
    [ "${got}" = "${want}" ] || { echo "selftest: day_before is timezone-sensitive: ${got}" >&2; exit 1; }

    # An image name is a plain path segment, but the counter must stay a number
    # and the person profile must stay suppressed.
    printf '%s\t%s\n' "3538a427-3e7d-5170-bf7d-e9560ebb3468" \
        '{"repository":"mezmo/aura","namespace":"mezmo","image":"aura","pull_count":20098,"star_count":0}' \
        > "${tmp}/chunk"
    payload=$(build_batch "${tmp}/chunk" "2026-09-01" "phc_test")

    got=$(jq -r '.batch[0].properties.pull_count | type' <<<"${payload}")
    [ "${got}" = "number" ] || { echo "selftest: pull_count is ${got}, want number" >&2; exit 1; }
    got=$(jq -r '.batch[0].timestamp' <<<"${payload}")
    [ "${got}" = "2026-09-01T23:59:59Z" ] || { echo "selftest: timestamp = ${got}" >&2; exit 1; }
    got=$(jq -r '.batch[0].distinct_id' <<<"${payload}")
    [ "${got}" = "docker:mezmo/aura" ] || { echo "selftest: distinct_id = ${got}" >&2; exit 1; }
    got=$(jq -r '.batch[0].properties["$process_person_profile"]' <<<"${payload}")
    [ "${got}" = "false" ] || { echo "selftest: person profiles not suppressed" >&2; exit 1; }

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

    local image
    for image in ${DOCKER_IMAGES}; do
        echo "Collecting ${image} pull totals"
        collect_pulls "${image}" >> "${WORK_DIR}/images.ndjson"
    done

    local images
    images=$(wc -l < "${WORK_DIR}/images.ndjson" | tr -d ' ')
    if [ "${images}" -eq 0 ]; then
        echo "error: no images collected from ${DOCKER_IMAGES}" >&2
        exit 1
    fi

    key_images "${WORK_DIR}/images.ndjson" "${date}" "${WORK_DIR}/uuids" > "${WORK_DIR}/keyed"
    split -l "${BATCH_SIZE}" "${WORK_DIR}/keyed" "${WORK_DIR}/chunk."

    local chunk chunks=0
    for chunk in "${WORK_DIR}"/chunk.*; do
        chunks=$((chunks + 1))
        build_batch "${chunk}" "${date}" "${POSTHOG_PROJECT_API_KEY:-phc_dry_run}" \
            > "${WORK_DIR}/payload.${chunks}.json"
    done

    # Every collected image must reach a payload. A mismatch means the keying
    # or chunking dropped rows, which would silently under-report.
    local events
    events=$(jq -s '[.[].batch | length] | add' "${WORK_DIR}"/payload.*.json)
    if [ "${events}" != "${images}" ]; then
        echo "error: built ${events} event(s) from ${images} image(s)" >&2
        exit 1
    fi

    local pulls
    pulls=$(jq -s '[.[].pull_count] | add' "${WORK_DIR}/images.ndjson")
    echo "Snapshot ${date}: ${images} image(s), ${pulls} pull(s), in ${chunks} batch(es)"

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

    echo "Sent ${images} event(s) for ${date} to ${POSTHOG_HOST%/}"

    if [ "${SKIP_VERIFY}" = 1 ]; then
        echo "Skipping read-back verification (SKIP_VERIFY=1)"
        return 0
    fi
    verify_snapshot "${date}" "${images}" "${pulls}" "${probes}"
    # Advisory only: a missing earlier day is worth reporting but is not a
    # failure of this run, and cannot be repaired by retrying it.
    warn_on_gap "${date}" || true
}

main "$@"
