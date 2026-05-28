# Telemetry & Privacy

Aura emits **opt-out, anonymous-tier** product telemetry to PostHog so
maintainers can answer the production-adoption question and prioritise
work against real usage signal. This document is the canonical
user-facing contract: every event that is collected, every value that
is **not** collected, every kill switch, every inspection path. If you
want to know what your install has been sending, read this file and
then check `~/.aura/telemetry/events.jsonl` (CLI) or
`{memory_dir}/telemetry/events.jsonl` (server). Both are written
locally on every event whether or not telemetry is enabled.

This contract is also a build artefact, not just prose: the
**type-level allow-list** in `crates/aura-telemetry/src/properties.rs`
will not compile if an event grows a free-form field, and the
[audit guide](#audit-guide) below names every source and test file you
can read to verify the wire payload yourself.

---

## TL;DR — kill switches

| Mechanism | What it disables | When to use |
|-----------|------------------|-------------|
| `DO_NOT_TRACK=1` | All outbound telemetry | Industry-standard cross-tool opt-out; honored first. |
| `AURA_TELEMETRY_DISABLED=1` | All outbound telemetry | Aura-specific opt-out. |
| `AURA_TELEMETRY_ENABLED=false` | All outbound telemetry | Same effect; intended for shell profiles. |
| `[telemetry] enabled = false` in `cli.toml` or main config | All outbound telemetry | Persistent project-level disable. |
| `/telemetry disable` (REPL) | All outbound telemetry (per-user) | Convenience; writes `enabled = false` into `~/.aura/cli.toml`. |
| `AURA_TELEMETRY_LOG_EVENTS=0` | The **local inspection log** only | Stops `events.jsonl` from being written. The wire-side kill switches above do not affect it. |

Precedence is `DO_NOT_TRACK` → `AURA_TELEMETRY_DISABLED` → CI auto-disable
→ cargo-test auto-disable → `AURA_TELEMETRY_ENABLED=false` →
`[telemetry] enabled = false`. The **first match wins**; the reason
that took effect is recorded in the local inspection log so a user can
audit what disabled telemetry on any given launch.

## Auto-disabled environments

Telemetry is disabled automatically — no env var required — when any of
the following are set:

`CI`, `GITHUB_ACTIONS`, `BUILDKITE`, `JENKINS_URL`, `CIRCLECI`,
`GITLAB_CI`, `TF_BUILD`, `TEAMCITY_VERSION`, `TRAVIS`,
`CARGO_TARGET_TMPDIR`, `RUST_TEST_THREADS`.

The first two cover the bulk of OSS CI; the bottom two cover cargo's
own integration-test and unit-test harnesses. If you run Aura under a
CI provider not on this list and want auto-disable, set `CI=true` in
your job environment (the universal convention).

---

## What we collect

Every event carries the same **envelope** plus event-specific
properties. The envelope is built once at telemetry init and added to
every PostHog payload:

| Envelope field | Type | Source |
|---|---|---|
| `event` | string | The event name (a `'static` literal). |
| `distinct_id` | UUID v4 | The install UUID at `~/.aura/install-id` (CLI) or `{memory_dir}/install-id` (server). Never tied to a user identity. |
| `timestamp` | ISO-8601 UTC | The moment the background task built the JSON. |
| `properties.aura_version` | string | `CARGO_PKG_VERSION` resolved at compile time. |
| `properties.aura_source` | enum: `web-server`, `cli` | Which Aura process emitted the event. |
| `properties.os_family` | enum: `linux`, `macos`, `windows`, `other` | `cfg!(target_os)`. Coarse on purpose — no arch, kernel, distro. |
| `properties.deployment_method` | enum: `local`, `docker`, `k8s`, `standalone-cli`, `other` | Settable via `AURA_DEPLOYMENT_METHOD`. |
| `properties.session_id` | UUID v4 | Per-process; correlates events from one run **without** revealing identity. Regenerated on every launch. |
| `properties.$ip` | empty string (`""`) | Suppresses PostHog server-side IP enrichment. |
| `properties.$geoip_disable` | `true` | Suppresses PostHog server-side geoip enrichment. |

### Events (Phase 1)

The Phase-1 event surface is intentionally small. Each phase of the
rollout adds a few more events; this table grows along with them.

| Event | Trigger | Event-specific properties |
|---|---|---|
| `server_started` | Once per `aura-web-server` boot, after logging init. | `default_agent_set: bool` — whether the operator pinned a default agent in config. |
| `cli_session_started` | Once per `aura-cli` invocation, after `AppConfig::load`. | `interactive: bool`, `standalone_mode: bool`, `client_tools_enabled: bool`. |
| `telemetry_opt_out` | Once at init **when telemetry is disabled**. Inspection-log only; never sent on the wire. | `reason: string` — names the kill switch that took effect (e.g. `DoNotTrack`, `Ci(GITHUB_ACTIONS)`). |

---

## What we DO NOT collect

The following values **cannot** appear in any event under the current
type system. Adding a field that would carry one of them fails to
compile; see [How to add an event](#how-to-add-a-new-event).

- Chat messages, prompts, responses, system prompts.
- Tool arguments, tool results, MCP request/response bodies.
- File paths, file contents, command-line args, env-var values.
- IP addresses, hostnames, MAC addresses, kernel version, CPU arch,
  Linux distro, container ID, Kubernetes namespace.
- API keys, auth headers, model API URLs, OAuth tokens, raw URLs.
- Agent name / alias plaintext, usernames, email addresses,
  git remotes.
- Anything the LLM has generated or the user has typed.

If you see any of these in `events.jsonl`, that is a bug and we want
to hear about it.

## Anonymity guarantees

- The install UUID is the only persistent identifier. It is **never**
  passed to PostHog's `identify` API; there is no method on
  `TelemetryHandle` that takes a user identifier.
- `$ip: ""` and `$geoip_disable: true` are on every event to prevent
  PostHog from filling in IP or geo data from the TCP socket.
- Events live in PostHog's anonymous tier; `distinct_id` is never
  upgraded to or merged with any account.

---

## Inspecting what was sent

### From the CLI

```text
aura> /telemetry status
aura> /telemetry recent
aura> /telemetry recent 100
```

`status` prints whether telemetry is active, which kill switch (if
any) silenced it, the endpoint, and the install-id and inspection-log
paths. `recent N` prints the last `N` records from the inspection
log, oldest-first. (Phase 1 wires these commands; see task list.)

### From the server

```bash
curl http://127.0.0.1:8080/telemetry/recent?limit=50
```

Returns `{ "events": [...] }` with the same record shape as the local
JSONL file. **The endpoint enforces a loopback peer-address check**:
requests originating from a non-loopback address receive a `403`
with an explanatory body, regardless of how the server is bound. If
the inspection log has been disabled via `AURA_TELEMETRY_LOG_EVENTS=0`
the endpoint returns `503` and an `"error": "inspection log
disabled …"` body.

### Reading the file directly

```bash
tail -F ~/.aura/telemetry/events.jsonl                 # CLI
tail -F $MEMORY_DIR/telemetry/events.jsonl             # server
```

Each line is one event. The fields are:

```json
{
  "ts": "2026-05-28T12:00:00Z",
  "event": "server_started",
  "properties": { … the same property bag PostHog received … },
  "sent": true,
  "not_sent_reason": null
}
```

`sent` is `true` only after the wire-side POST to PostHog completed
successfully. `not_sent_reason` is `null` in that case; otherwise it
names *why* the event was not delivered — one of:

- A kill-switch label (`DoNotTrack`, `AuraDisabled`,
  `Ci(GITHUB_ACTIONS)`, `CargoTest`, `ConfigDisabled`) when a kill
  switch took effect.
- `ChannelFull` when the background task could not keep up and the
  event was dropped at the channel boundary.
- `PostFailed(<category>)` when the POST returned an error.
  Categories: `timeout`, `network`, `http_4xx`, `http_5xx`,
  `http_other`, `other`. The per-request timeout is
  `TelemetryConfig::post_timeout` (default **1.5 s**) and is bounded
  intentionally below the **2 s** shutdown drain budget the CLI and
  server use, so a slow endpoint cannot let an in-flight POST outlive
  shutdown and skip its inspection-log row. Tune `post_timeout`
  higher only if you also raise the shutdown budget; the relationship
  `post_timeout < shutdown_budget` is the contract that makes the
  "row for every captured event" guarantee hold.

The line is written for every captured event — including dropped and
failed-to-send ones — so you can verify both that the kill switch
took effect and that the network sink actually delivered. JSONL
written by an earlier build (which used `disable_reason`) is read
back transparently via a serde alias; new writes use
`not_sent_reason`.

The file rotates at 1000 lines; the previous file is kept at
`events.jsonl.1` (single backup, overwrites on next rotation). To
suppress the inspection log entirely, set
`AURA_TELEMETRY_LOG_EVENTS=0`.

---

## Self-hosted sink

If you do not want events going to Mezmo's PostHog project, point
Aura at your own PostHog (self-hosted or your own PostHog Cloud
project):

```toml
# In your aura config.toml or cli.toml
[telemetry]
endpoint = "https://posthog.example.internal"
api_key = "phc_your_write_only_public_key"
```

or via environment:

```bash
export AURA_TELEMETRY_ENDPOINT=https://posthog.example.internal
export AURA_TELEMETRY_API_KEY=phc_your_write_only_public_key
```

The default endpoint is `https://us.i.posthog.com`. The default API
key is the public **write-only** key for the project Mezmo uses to
operate the production-install count; it is intentionally embedded in
the source because public PostHog project keys cannot read anything.

---

## Install ID

A single UUID v4, persisted in a single file:

- CLI: `~/.aura/install-id`
- Server: `~/.aura/install-id` if `$HOME` is available, otherwise
  `{memory_dir}/install-id`.

The file is mode `0600` on Unix, written via a `hard_link` race-safe
publication pattern (see `crates/aura-telemetry/src/install_id.rs` for
the algorithm and the convergence proof in the tests).

**If the file is corrupt** — anything other than a valid UUID v4 —
Aura **does not auto-recover**. Each run silently falls back to a
fresh per-run UUID (logged at `tracing::debug!`) without modifying the
file. Auto-recovery without a cross-process lock cannot guarantee
convergence, and a botched recovery would split a real install's
reported identity across multiple PostHog `distinct_id`s. The trade we
make instead is under-counting that install until you reset:

```bash
rm ~/.aura/install-id
```

The next Aura launch generates a fresh persistent UUID. You will be
counted as a new install.

### Containerized / read-only deployments

The install-id only stabilises the install count if it lives on
**persistent storage**. A stateless container has none by default, so
without a little care every container recreation or image pull looks
like a brand-new install.

The mechanism is the one production deployments already use:
**`memory_dir` on a mounted volume.** The install-id (and the local
inspection log) are rooted there, so persisting `memory_dir` persists
your install identity — no telemetry-specific knob required. This also
aligns with what a "production install" *is*: a deployment doing
sustained real work already persists `memory_dir` for scratchpad and
orchestration artifacts, so it gets a stable install-id for free.
A throwaway eval clone that persists nothing gets a fresh id each run
and naturally does not accumulate as a stable install — which is the
correct outcome.

- **Docker Compose** (the quickstart): the bundled `docker-compose.yml`
  already sets `memory_dir` and mounts a named `aura-state` volume
  there. Nothing to do — the install count is stable out of the box.
- **Kubernetes / Helm**: mount a small `PersistentVolumeClaim` at your
  configured `memory_dir`. (Chart support for this is tracked
  upstream.)
- **Bare metal / laptop**: `$HOME` is used automatically; no action
  needed.

If Aura starts with telemetry active but no persistent location for the
install-id, it logs a single **warning** at startup
(`telemetry install-id is not on persistent storage …`). That is the
nudge: set `memory_dir` to a mounted volume. The warning never appears
when telemetry is disabled (a disabled install is never counted), and
never on a normal laptop run.

If your container is **fully read-only with no writable volume at all**,
telemetry still functions — events are sent and the install simply
reports a fresh id per run. To opt out entirely in that environment,
use the `DO_NOT_TRACK=1` / `AURA_TELEMETRY_DISABLED=1` env switches,
which need no filesystem access.

---

## How to add a new event

The audit checklist for any PR that adds telemetry:

1. Add the event struct in `crates/aura-telemetry/src/events.rs`
   with `#[derive(Event)] #[aura_event(name = "…")]`.
2. Every field must have a type that implements
   `aura_telemetry::IntoTelemetryProperty`. If you need a new type,
   add a `PropertyValue` variant in `crates/aura-telemetry/src/properties.rs`
   and implement the trait for the source type.
3. Compile must succeed — if it doesn't, the compiler will name the
   trait and (in the help text) enumerate the existing allow-list.
   That's the structural anti-PII gate; do not bypass it with
   `String` or `serde_json::Value` fields.
4. Add a row to the [Events](#events-phase-1) table in this file.
   PRs that add an event without updating this table will be sent
   back.
5. Add an integration test against `wiremock` that asserts the body
   shape, following the pattern in
   `crates/aura-telemetry/tests/wire_format.rs`.
6. Document the **negative**: if the event represents a path where
   PII could plausibly appear (e.g. a tool call), restate explicitly
   in the table what is *not* in the property bag.

---

## Audit guide

The entire telemetry surface lives in `crates/aura-telemetry`. To
verify the contract above without running the code:

| File | What to look for |
|---|---|
| `crates/aura-telemetry/src/properties.rs` | The sealed `PropertyValue` enum and the `IntoTelemetryProperty` trait. No `String`, no `serde_json::Value`, no blanket impl on `PropertyValue` itself — that last one was a deliberate close-up after review feedback so `PropertyValue` cannot be smuggled into an event's property map. |
| `crates/aura-telemetry/src/events.rs` | One typed struct per event. Adding fields here requires a `PropertyValue` variant first. |
| `crates/aura-telemetry/src/disable.rs` | The kill-switch decision tree. Tests cover every branch and every precedence pair. |
| `crates/aura-telemetry/src/install_id.rs` | The race-safe install-UUID persistence. Concurrent first-launch tests prove all callers converge on one UUID; a separate test proves that corrupt files are read-only-fallback (never auto-recovered) so a multi-process race cannot split identity. |
| `crates/aura-telemetry/src/inspection_log.rs` | The local JSONL writer and rotation. |
| `crates/aura-telemetry/src/sink.rs` | The **only** code that builds the JSON sent to PostHog and the only outbound HTTP call. The `$ip: ""` and `$geoip_disable: true` lines live here. |
| `crates/aura-telemetry/src/handle.rs` | `TelemetryHandle::capture`, the fire-and-forget entry point, and the background batching task. |
| `crates/aura-telemetry/tests/wire_format.rs` | Boots a wiremock, captures one event, asserts the **literal** bytes that would have gone to PostHog. Run with `cargo test -p aura-telemetry --test wire_format -- --nocapture` to print them. |
| `crates/aura-telemetry/tests/compile_fail/` | Snapshots proving that `String`, `u64`, and `PropertyValue`-typed fields are rejected at compile time. |
| `crates/aura-telemetry/tests/derive_smoke.rs` | Confirms a valid event struct round-trips its properties into a JSON-renderable payload. |

---

## Proposed "production install" rule (subject to product ratification)

An install is treated as a **production install** in a given 30-day
window if it emits `chat_request_completed` with `success = true` on
at least 5 distinct calendar days within the window **and** emits at
least one `tool_invoked` event during the window. CI installs are
excluded structurally (they never report). This rule is computable
from raw events via a single PostHog HogQL query and requires no
additional instrumentation. The rule will be ratified by product
before Phase 3 (chat lifecycle) ships, and will be linked from this
section once it lands.
