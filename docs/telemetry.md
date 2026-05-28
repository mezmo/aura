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

Localhost-bound by default. Same JSONL records as the CLI command,
returned as a JSON array.

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
  "disable_reason": null
}
```

When telemetry is disabled, `sent` is `false` and `disable_reason`
names the kill switch. The line is still written so you can verify
the kill switch took effect.

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
the algorithm and the convergence proof in the tests). To reset your
install identity:

```bash
rm ~/.aura/install-id
```

The next Aura launch generates a fresh UUID. You will be counted as a
new install.

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
| `crates/aura-telemetry/src/install_id.rs` | The race-safe install-UUID persistence. Concurrent-launch tests prove all callers converge on one UUID. |
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
