# Shared machinery for the PostHog download-snapshot scripts.
#
# Each snapshot script collects download counters from one source and files
# them against a date. Everything here is the part that does not depend on
# which source: identifiers, batching, sending, and the read-back that proves
# the send landed. A caller supplies the collector and its own main.
#
# Set before sourcing:
#   EVENT_NAME      - PostHog event name for one snapshot record
#   UUID_NAMESPACE  - namespace for the version-5 event UUIDs, permanent
#   SUBJECT_PREFIX  - distinct_id prefix naming the source, e.g. "github"
#   COUNT_PROPERTY  - event property holding the counter (default download_count)
#
# The configuration variables below are defaulted here and read by callers.
# A caller owns WORK_DIR and must point it at a directory it created.

POSTHOG_HOST="${POSTHOG_HOST:-https://us.i.posthog.com}"
POSTHOG_API_HOST="${POSTHOG_API_HOST:-https://us.posthog.com}"
POSTHOG_PROJECT_ID="${POSTHOG_PROJECT_ID:-443794}"
BATCH_SIZE="${BATCH_SIZE:-1000}"
VERIFY_TIMEOUT="${VERIFY_TIMEOUT:-600}"
SKIP_VERIFY="${SKIP_VERIFY:-0}"
DRY_RUN="${DRY_RUN:-0}"
SNAPSHOT_DATE="${SNAPSHOT_DATE:-}"
COUNT_PROPERTY="${COUNT_PROPERTY:-download_count}"
WORK_DIR=""

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
# A fresh UUID, so nothing an earlier run wrote can be mistaken for it.
uuid4() {
    local h b6 b8
    h=$(openssl rand -hex 16)
    printf -v b6 '%02x' $(( 0x${h:12:2} & 0x0f | 0x40 ))
    printf -v b8 '%02x' $(( 0x${h:16:2} & 0x3f | 0x80 ))
    printf '%s-%s-%s%s-%s%s-%s\n' \
        "${h:0:8}" "${h:8:4}" "${b6}" "${h:14:2}" "${b8}" "${h:18:2}" "${h:20:12}"
}

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

# Run a HogQL query and print the response body.
#
# Plain --retry covers the transient cases (timeouts, 429, 5xx) and leaves
# 4xx alone: an auth or project error repeated four times is noise, and the
# status is worth naming because every likely cause is a misconfiguration
# rather than an outage.
hogql_request() {
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
    printf '%s\n' "${body}"
}

hogql_scalar() {
    hogql_request "$1" | jq -r '.results[0][0]'
}

# The whole first row, tab separated.
hogql_row() {
    hogql_request "$1" | jq -r '.results[0] | @tsv'
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

# Add a probe event to a payload, carrying a UUID no other run can produce.
#
# The snapshot events are identical across runs of the same date by design, so
# finding them proves the snapshot is right but not that this run wrote
# anything: PostHog answers 200 OK to a batch sent with a dead token, and on a
# re-run of an already-populated date the stale rows satisfy every check. The
# probe rides the same request, so it is absent exactly when that request was
# discarded.
add_probe() {
    local payload=$1 probe=$2 ts
    ts=$(jq -rn 'now | strftime("%Y-%m-%dT%H:%M:%SZ")')
    jq --arg uuid "${probe}" --arg ts "${ts}" --arg event "${EVENT_NAME}_probe" --arg prefix "${SUBJECT_PREFIX}" '
        .batch += [{uuid: $uuid, event: $event, distinct_id: ($prefix + ":probe"),
                    timestamp: $ts,
                    properties: {"$process_person_profile": false}}]' \
        "${payload}" > "${payload}.probed"
    mv "${payload}.probed" "${payload}"
}

probes_landed() {
    local list
    list=$(sed "s/^/'/; s/$/'/" "${WORK_DIR}/probes" | paste -sd, -)
    hogql_scalar "SELECT count(DISTINCT uuid) FROM events \
        WHERE event = '${EVENT_NAME}_probe' AND uuid IN (${list})"
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

build_batch() {
    local chunk=$1 date=$2 api_key=$3
    jq -R -n --arg key "${api_key}" --arg date "${date}" --arg event "${EVENT_NAME}" --arg prefix "${SUBJECT_PREFIX}" '
        {api_key: $key,
         batch: [inputs
           | index("\t") as $i
           | (.[:$i]) as $uuid
           | (.[$i+1:] | fromjson) as $r
           | {uuid: $uuid,
              event: $event,
              distinct_id: ($prefix + ":" + $r.repository),
              timestamp: ($date + "T23:59:59Z"),
              properties: ($r + {snapshot_date: $date, "$process_person_profile": false})}]}
    ' "${chunk}"
}

# Report how many of this run's own event UUIDs are queryable at this run's
# timestamp, and what download total those events carry.
#
# The count alone is not enough. A date-wide count would let UUIDs from an
# earlier run cover for a missing event, and scoping to this run's UUIDs still
# cannot see a discarded retry whose asset set is unchanged, because the
# earlier run already ingested those exact UUIDs. The total closes that:
# counters only rise, so a stale row carries a smaller number than the payload
# just sent. Pinning the timestamp keeps a snapshot filed against the wrong
# instant from reading as verified.
count_ingested() {
    local date=$1 chunk list row found downloads events=0 total=0
    for chunk in "${WORK_DIR}"/uchunk.*; do
        list=$(sed "s/^/'/; s/$/'/" "${chunk}" | paste -sd, -)
        row=$(hogql_row "SELECT count(), sum(dl) FROM ( \
            SELECT uuid, max(toIntOrZero(toString(properties.${COUNT_PROPERTY}))) AS dl \
            FROM events \
            WHERE event = '${EVENT_NAME}' \
              AND timestamp = toDateTime('${date} 23:59:59') \
              AND uuid IN (${list}) \
            GROUP BY uuid)")
        found=${row%%$'\t'*}
        downloads=${row##*$'\t'}
        # A chunk none of whose events are queryable yet answers with count 0
        # and a null total, which @tsv renders as an empty field. That is
        # "nothing has landed yet", which the poll is here to wait out, not a
        # malformed answer to abort on.
        case "${found}" in '' | null) found=0 ;; esac
        case "${downloads}" in '' | null) downloads=0 ;; esac
        for value in "${found}" "${downloads}"; do
            case "${value}" in
                '' | *[!0-9]*)
                    echo "error: PostHog answered the read-back with '${row}', which is not a count and a total" >&2
                    return 1 ;;
            esac
        done
        events=$(( events + found ))
        total=$(( total + downloads ))
    done
    printf '%s\t%s\n' "${events}" "${total}"
}

# Poll until the snapshot is queryable, since ingestion lags the send.
verify_snapshot() {
    local date=$1 expected=$2 expected_downloads=$3 probes=$4
    local deadline row got downloads seen
    split -l 500 "${WORK_DIR}/uuids" "${WORK_DIR}/uchunk."
    deadline=$(( $(date +%s) + VERIFY_TIMEOUT ))
    while :; do
        row=$(count_ingested "${date}")
        got=${row%%$'\t'*}
        downloads=${row##*$'\t'}
        seen=$(probes_landed)
        case "${seen}" in '' | null) seen=0 ;; esac
        if [ "${got}" -ge "${expected}" ] && [ "${downloads}" -ge "${expected_downloads}" ] \
           && [ "${seen}" -ge "${probes}" ]; then
            echo "Verified ${got} of ${expected} event(s), ${downloads} counted, and ${seen} probe(s) in PostHog for ${date}"
            return 0
        fi
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            echo "error: PostHog holds ${got} of ${expected} event(s), ${downloads} of ${expected_downloads} counted, and ${seen} of ${probes} probe(s) for ${date} after ${VERIFY_TIMEOUT}s" >&2
            return 1
        fi
        sleep 5
    done
}
