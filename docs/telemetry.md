# Telemetry & Privacy

Aura emits **anonymous-tier** product telemetry to PostHog so
maintainers can answer the production-adoption question and prioritise
work against real usage signal. Telemetry is **notice-gated**: it is
held until you have been shown a one-time notice (or have explicitly
enabled it), and it is never sent before then. This document is the
canonical user-facing contract: every state, every event collected,
every value **not** collected, every control, every inspection path.

To see what your install has sent — or *would* send — read this file
and then check `~/.aura/telemetry/events.jsonl` (CLI) or
`{memory_dir}/telemetry/events.jsonl` (server). Both are written
locally for every captured event regardless of state, so even a held
(`Unknown`) or disabled install shows you exactly what it is holding.

This contract is also a build artefact, not just prose: the
**type-level allow-list** in `crates/aura-telemetry/src/properties.rs`
will not compile if an event grows a free-form field, and the
[audit guide](#audit-guide) below names every source and test file you
can read to verify the wire payload yourself.

---

## The three states

Telemetry is always in exactly one state:

| State | Meaning | Behaviour |
|---|---|---|
| **Unknown** | No preference recorded yet. | **Held.** Events are written to the local inspection log (so you can inspect what *would* be sent), but **nothing is sent** and held events are **never backfilled** if you later enable. |
| **Enabled** | A notice was shown and not opted out of, or you/an operator explicitly enabled. | May send. |
| **Disabled** | A kill switch or explicit opt-out is in effect. | Held; nothing sent. |

### First-run notice (interactive CLI)

The first time you launch the **interactive REPL** with no recorded
preference, Aura prints a one-time notice — it states that telemetry is
collected, links to this document, and tells you how to opt out.
Telemetry stays **held** until you **send your first chat message**:

- sending a message to the agent is treated as consent → telemetry
  becomes **Enabled** and `[telemetry] enabled = true` is written to
  `~/.aura/cli.toml`;
- slash commands never grant consent: `/telemetry disable` →
  **Disabled** (persisted); `/telemetry status`, `/help`, typos, and
  unknown commands leave the state **Unknown** (you can inspect first,
  decide later);
- quitting immediately (`/quit`) → stays **Unknown**, so you see the
  notice again next launch.

Nothing is sent during the launch in which the notice first appears
until that first message — so you always have a chance to opt out (or
inspect with `/telemetry status` / `/telemetry recent`) before any
telemetry leaves your machine.

### Non-interactive surfaces

Surfaces that cannot show a notice never enter `Enabled` on their own:

- **One-shot `aura --query …`** never participates: no notice, nothing
  sent.
- **The web server** is non-interactive. Its own events stay **Unknown**
  (holding, inspectable, unsent) unless an operator explicitly sets
  `[telemetry] enabled = true` / `AURA_TELEMETRY_ENABLED=true`. A
  consenting CLI can additionally authorise telemetry about *its own*
  requests without changing the server's state — see
  [Consent propagation](#consent-propagation-cli--server).
- **CI / test harnesses** are non-interactive and additionally
  hard-disabled (below), so they never send.

---

## Controls

| Mechanism | Effect | Notes |
|-----------|--------|-------|
| `DO_NOT_TRACK=1` | **Disabled** | Industry-standard cross-tool opt-out; beats everything, even an explicit enable. |
| `AURA_TELEMETRY_DISABLED=1` | **Disabled** | Aura-specific opt-out; same precedence tier. |
| `AURA_TELEMETRY_ENABLED=true` / `=false` | **Enabled** / **Disabled** | Explicit operator/power-user opt-in or opt-out. |
| `[telemetry] enabled = true` / `false` in `cli.toml` or main config | **Enabled** / **Disabled** | The recorded preference. Absent ⇒ `Unknown`. |
| `/telemetry enable` / `/telemetry disable` (REPL) | **Enabled** / **Disabled** | Persists the preference to `~/.aura/cli.toml`. |
| `AURA_TELEMETRY_LOG_EVENTS=0` | Disables the **local inspection log** only | Does not affect sending; just stops `events.jsonl` from being written. |

The following is a **server-only operator switch**. It does not change
the telemetry state and defaults to **off**:

| Mechanism | Effect | Notes |
|-----------|--------|-------|
| `--telemetry-inspect-exposed` / `AURA_TELEMETRY_INSPECT_EXPOSED=1` | Allow `GET /telemetry/recent` from non-loopback peers | Off by default (loopback-only). Needed in docker/proxy topologies; see [From the server](#from-the-server). |

**Resolution precedence (highest first):** hard disables
(`DO_NOT_TRACK`, `AURA_TELEMETRY_DISABLED`, CI, cargo-test) →
`AURA_TELEMETRY_ENABLED` → `[telemetry] enabled` preference → otherwise
`Unknown`. A hard disable beats an explicit enable, so a misconfigured
`AURA_TELEMETRY_ENABLED=true` in CI still cannot send. The state (and,
when held, the reason) is recorded in the local inspection log so you
can audit any launch.

Boolean values are parsed leniently: `false`, `no`, `off`, `0`, and
empty are false; everything else is true. So `DO_NOT_TRACK=false` does
**not** disable (it means "do track").

## Hard-disabled environments

Regardless of any explicit enable, telemetry is forced to **Disabled**
when any of these are set:

`CI`, `GITHUB_ACTIONS`, `BUILDKITE`, `JENKINS_URL`, `CIRCLECI`,
`GITLAB_CI`, `TF_BUILD`, `TEAMCITY_VERSION`, `TRAVIS`,
`CARGO_TARGET_TMPDIR`, `RUST_TEST_THREADS`.

This keeps CI, contributor, and test environments out of the
production-install count even if telemetry is mistakenly enabled there.

## Consent propagation (CLI → server)

When you use the CLI in HTTP mode against an Aura server, the CLI — once
**Enabled** — attaches an `X-Aura-Telemetry-Consent: enabled` header to
its requests. That consent is **request-scoped**: it authorises
telemetry about *that request*, and nothing else.

Propagation is deliberately **not** a global switch. An inbound consent
header never changes the server's telemetry state and never enables
sending for other users or for the server's own lifecycle events. The
header is unauthenticated and trivially spoofable, so binding it to a
process-wide flip would let any client (curl, a scanner, a proxy
injecting headers) turn a shared server's telemetry on for everyone —
the exact failure this design avoids. Because consent rides with the
request, a forged header can at most cause anonymous telemetry about
the forger's *own* request to be sent, which is harmless.

Concretely, when a server handles a request carrying a valid consent
header, the per-request events generated for it are captured through
[`TelemetryHandle::capture_consented`], which sends without touching the
server's global `Unknown`/`Enabled` state. An operator opt-out always
wins: a **Disabled** server holds even consented events and sends
nothing. The server's own events (e.g. `server_started`) are governed by
the operator's own decision (`[telemetry] enabled` / env), never by a
client header. The CLI and server keep separate anonymous install IDs;
the `aura_source` property distinguishes them.

> **Status:** Phase 1 defines no per-request *server* events, so there
> is nothing for the server to propagate yet — the CLI already sends the
> header, and the request-scoped capture path is in place for the
> per-request server events that Phase 2 introduces.

[`TelemetryHandle::capture_consented`]: ../crates/aura-telemetry/src/handle.rs

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
| `cli_session_started` | Once per interactive `aura-cli` REPL session, after telemetry becomes **Enabled** (a recorded opt-in, or the first-run notice + first non-opt-out input). Never emitted in one-shot `--query` mode. | `interactive: bool`, `standalone_mode: bool`, `client_tools_enabled: bool`. |
| `telemetry_opt_out` | Once at init **when telemetry is Disabled**. Inspection-log only; never sent on the wire. | `reason: string` — names the kill switch that took effect (e.g. `DoNotTrack`, `Ci(GITHUB_ACTIONS)`). |

When telemetry is **Unknown** (held), every captured event is written
to the inspection log with `sent: false` and `not_sent_reason:
"Unknown"` — these are the "would send" payloads — and `cli_session_started`
is captured only once the session becomes `Enabled` (so it is never
emitted while held, and held events are never backfilled).

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
aura> /telemetry enable
aura> /telemetry disable
```

`status` prints the current state (`unknown` / `active` /
`disabled (reason)`), the endpoint, and the install-id and
inspection-log paths. `recent N` prints the last `N` records from the
inspection log, oldest-first. `enable` / `disable` record your
preference in `~/.aura/cli.toml` and take effect immediately for the
running session — including undoing each other (`/telemetry disable`
then `/telemetry enable` re-enables this session). The one exception:
a startup kill switch (`DO_NOT_TRACK`, `AURA_TELEMETRY_DISABLED`, or
config `enabled = false`) holds telemetry off for the whole process, so
`/telemetry enable` then only saves the preference for the next launch
(and only once that kill switch is cleared) — it says so rather than
claiming the session is active.

### From the server

```bash
curl http://127.0.0.1:8080/telemetry/recent?limit=50
```

Returns `{ "events": [...] }` with the same record shape as the local
JSONL file. **The endpoint enforces a loopback peer-address check by
default**: requests originating from a non-loopback address receive a
`403`. If the inspection log has been disabled via
`AURA_TELEMETRY_LOG_EVENTS=0` the endpoint returns `503`.

In **Docker / Kubernetes** the in-container peer address of a
host-published request is the bridge gateway, not `127.0.0.1`, so the
loopback check would `403` even the host operator. Two options:

- Inspect from inside the container, where the peer really is loopback:
  `docker exec aura curl -s localhost:8080/telemetry/recent`.
- Or opt into remote access with `--telemetry-inspect-exposed` /
  `AURA_TELEMETRY_INSPECT_EXPOSED=1`, which lifts the loopback gate.

> ⚠️ The loopback gate is a convenience, not an auth boundary. Behind a
> same-host reverse proxy every client already appears as `127.0.0.1`,
> so do not rely on it to keep the event timeline private on an exposed
> server — restrict the route at your proxy/ingress instead.

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

- `Unknown` when the event was **held** because no telemetry preference
  has been recorded yet. These are the "would send" rows: the full
  payload is recorded locally but nothing left your machine.
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

- CLI: `~/.aura/install-id` (falls back to `{memory_dir}/install-id`,
  then `.aura/install-id` in the working directory, for accounts without
  `$HOME`).
- Server: `{memory_dir}/install-id` when `memory_dir` is configured — the
  durable, mounted location that survives container recreation — otherwise
  `$HOME/.aura/install-id`, then `.aura/install-id` in the working
  directory.

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

Note that the **server is non-interactive**, so its own events stay in
the `Unknown` state and send nothing until an operator opts in
explicitly (`[telemetry] enabled = true` / `AURA_TELEMETRY_ENABLED=true`).
A consenting CLI authorises telemetry only about its own requests and
does not enable the server's own events (see
[Consent propagation](#consent-propagation-cli--server)). The
persistence below is what makes the install count *stable once enabled*;
it does not by itself cause the server to send.

- **Docker Compose** (the quickstart): the bundled `docker-compose.yml`
  sets `memory_dir` and mounts a named `aura-state` volume there, so
  *when you enable telemetry* the install id is stable across restarts
  and image pulls. The server does not send until enabled.
- **Kubernetes / Helm**: mount a small `PersistentVolumeClaim` at your
  configured `memory_dir`. (Chart support for this is tracked
  upstream.)
- **Bare metal / laptop**: the CLI uses `$HOME` automatically and its id
  is stable — no action needed. A server run this way keys durability off
  `memory_dir`, not `$HOME`, so if you enable it without setting
  `memory_dir` it still emits the one-time nudge below; point `memory_dir`
  at a persistent path to silence it.

If Aura is **Enabled** but the install-id has no persistent location, it
logs a single **warning** at startup
(`telemetry install-id is not on persistent storage …`). That is the
nudge: set `memory_dir` to a mounted volume. The warning never appears
while `Unknown` or `Disabled` (neither sends, so neither is counted), and
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
| `crates/aura-telemetry/src/disable.rs` | `TelemetryState` and `decide_state` — the tri-state resolution (hard disable / explicit enable / preference / Unknown). Tests cover every branch and precedence pair. |
| `crates/aura-telemetry/src/handle.rs` | `TelemetryHandle::enable` / `set_disabled` and the three-way `capture_payload`. `consent_state.rs` proves Unknown holds, `enable()` sends without backfilling, and a kill switch can't be revived. |
| `crates/aura-cli/src/repl/telemetry_notice.rs` | The first-run notice text and the first-message consent decision, both unit-tested. |
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
