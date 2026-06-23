# Telemetry & Privacy

`aura-cli` emits **anonymous** product telemetry to PostHog so maintainers
can answer one question: *are people using the CLI, in which modes, and do
their chat turns succeed?* Telemetry is **CLI-only** (the server sends
nothing), **opt-out**, and **notice-gated**: nothing is sent until you have
seen a one-time notice and sent your first message, and it is never sent at
all if you opt out.

This document is the canonical contract: every state, every event, every
value **not** collected, every control, and how to verify all of it
yourself.

Two principles govern what is here, and any future change (see
[ADR 2026-06-23](adr/2026-06-23-cli-product-telemetry.md)):

1. **No tracking without a concrete improvement hypothesis.** Nothing is
   collected unless we can state how it improves Aura for users.
2. **The why/how is documented in the code**, next to each event.

To see exactly what your install has sent (or *would* send), read this file
and then `cat ~/.aura/telemetry/events.jsonl`. That local log is written for
every captured event in every state, so even a held or disabled install
shows you precisely what it is holding.

---

## The three states

| State | Meaning | Behaviour |
|---|---|---|
| **Unknown** | No preference recorded yet. | **Held.** Events are written to the local inspection log so you can see what *would* be sent, but **nothing is sent**, and held events are **never backfilled** if you later enable. |
| **Enabled** | You sent a message after the notice, or `[telemetry] enabled = true` is recorded. | May send. |
| **Disabled** | A kill switch or explicit opt-out is in effect. | Held; nothing sent. |

### First-run notice and first-message consent

The first time you launch the interactive REPL with no recorded preference,
Aura prints a one-time notice: it states that anonymous telemetry is
collected, links here, and tells you how to opt out. Telemetry stays
**held** (`Unknown`) until you **send your first chat message**:

- sending a message is treated as consent → **Enabled**, and
  `[telemetry] enabled = true` is written to `~/.aura/cli.toml`;
- slash commands never grant consent: `/telemetry disable` → **Disabled**
  (persisted); `/telemetry status`, `/telemetry recent`, `/help`, typos, and
  unknown commands leave the state **Unknown** (inspect first, decide later);
- quitting (`/quit`, Ctrl-D) → stays **Unknown**, so the notice returns next
  launch.

Nothing is sent during the launch in which the notice first appears until
that first message, so you always have a chance to opt out (or inspect with
`/telemetry status` / `/telemetry recent`) before any telemetry leaves your
machine.

### One-shot mode never sends

`aura -q "…"` (one-shot `--query`) is non-interactive: it cannot show the
notice, stays **Unknown**, and only ever writes to the local inspection log.
It never transmits.

---

## What is collected

Each event carries a small, fixed envelope plus the per-event properties
below. Every event has a matching `#[derive(Event)]` struct in
`crates/aura-telemetry/src/events.rs` whose doc comment states why it exists.

**Envelope** (every event): a random anonymous install id (`distinct_id`), a
per-process random session id, the Aura version, OS family
(`linux`/`macos`/`windows`/`other`), deployment method
(`local`/`standalone-cli`/`other`), and `aura_source = "cli"`. PostHog
server-side IP capture and geo-IP are explicitly suppressed (`$ip: ""`,
`$geoip_disable: true`).

| Event | Properties | Why we collect it |
|---|---|---|
| `cli_session_started` | `interactive`, `standalone_mode`, `client_tools_enabled` (all booleans) | Tells us which run modes to prioritise for UX/perf work, and how often the audit-sensitive client-tools path is used. |
| `chat_request_started` | none beyond the envelope | Turn volume, the core adoption signal that justifies continued investment. |
| `chat_request_completed` | `success` (boolean) | Turn success rate surfaces reliability regressions to fix. |

The `chat_request_*` pair fires once per chat turn, the same way regardless
of HTTP or standalone backend (no double-reporting).

## What is **not** collected

No prompts, no responses, no message content of any kind. No model or
provider identifier (deferred until a typed representation is settled). No
token counts, no latency. No file paths, no working directory, no hostname,
IP, MAC, username, CPU arch, kernel, or distro. No free-form strings at all:
the property allow-list (`crates/aura-telemetry/src/properties.rs`) has no
`String` variant, so a free-form field **fails to compile**.

---

## Controls

| Control | Effect |
|---|---|
| Send no message / `/quit` on first run | Stays **Unknown**; nothing sent. |
| `/telemetry disable` | **Disabled**; persists `[telemetry] enabled = false` to `~/.aura/cli.toml`. |
| `[telemetry] enabled = false` in `~/.aura/cli.toml` or a project `.aura/cli.toml` | **Disabled**. `enabled = false` in *either* layer wins. |
| `DO_NOT_TRACK=1` | **Disabled** (honours the [Console Do Not Track](https://consoledonottrack.com/) convention). |
| `AURA_TELEMETRY_DISABLED=1` | **Disabled**. |
| Running under CI (`CI`, `GITHUB_ACTIONS`, …) or `cargo test` | **Disabled** automatically. |
| `AURA_TELEMETRY_LOG_EVENTS=0` (or `false`/`no`/`off`) | Disables the local inspection log (telemetry state is unchanged). |

Self-hosting: point telemetry at your own PostHog with
`AURA_TELEMETRY_ENDPOINT` / `AURA_TELEMETRY_API_KEY` (or the matching
`cli.toml` fields). Env wins over file wins over the built-in defaults
(`https://us.i.posthog.com` and Aura's bundled project key). That bundled
key is a write-only PostHog public key (publish-only, it cannot read any
data back), which is why shipping it in source is safe. If a build ever
clears the key, requests simply 401 and are logged at debug, and Aura is
never affected.

Reset your anonymous install id any time: `rm ~/.aura/install-id`.

---

## Inspect what was (or would be) sent

- `/telemetry status`: current state, endpoint, install-id path,
  inspection-log path, and dropped-event count.
- `/telemetry recent [N]`: the last N inspection-log rows, each marked
  `[sent]` or `[not sent: <reason>]`.
- `cat ~/.aura/telemetry/events.jsonl`: the raw local log (JSON Lines),
  written for every captured event in every state.

## Audit guide

Everything above is verifiable from source:

- `crates/aura-telemetry/src/properties.rs`: the sealed property allow-list
  and the `IntoTelemetryProperty` gate.
- `crates/aura-telemetry/tests/compile_fail/`: proof that a `String`,
  integer, or `PropertyValue` field fails to compile.
- `crates/aura-telemetry/tests/wire_format.rs`: the exact PostHog payload,
  asserted end-to-end against a mock server (run with `--nocapture` to see
  it).
- `crates/aura-telemetry/src/events.rs`: every event, with its why/how.
- `crates/aura-telemetry/src/sink.rs`: the only code that opens an outbound
  connection.
