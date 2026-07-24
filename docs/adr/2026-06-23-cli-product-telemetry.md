<!-- markdownlint-disable MD033 -->
# CLI-only anonymous product telemetry with purpose-gated collection

- Status: **accepted**
- Deciders: Charles Johnson
- Date: 2026-06-23

Technical Story: ports a CLI-scoped subset of the
`charlesjohnson/posthog-spec-plan` prototype.

## Context and Problem Statement

Maintainers cannot answer the production-adoption question for `aura-cli`
(*are people actually running it, in which modes, and do their chat turns
succeed?*) without a usage signal. We want that signal, but Aura is an
open-source tool a security-conscious audience will read before trusting,
so telemetry has to be demonstrably narrow and demonstrably honest. A
prior prototype built telemetry across the whole stack (CLI, web server,
config, Docker). That is more surface than we want to own or defend.

We also want a standing rule that prevents telemetry from sprawling later:
a future contributor should not be able to add "just one more field"
without justifying it, and a reader should be able to see, at the
collection site, exactly why each datum exists.

## Decision Drivers <!-- optional -->

- The collection surface **MUST** be auditable in isolation, so a reader
  can verify what is and isn't sent without tracing through the whole
  codebase.
- Telemetry **MUST NOT** carry personal or free-form data: no prompts, no
  responses, no file paths, no host identifiers, no model identifiers
  (deferred), no token counts.
- Collection **MUST** be opt-out and **MUST NOT** send anything before the
  user has seen a one-time notice or explicitly enabled it.
- Every event and every property **MUST** have a concrete improvement
  hypothesis (a stated way it makes Aura better for users) recorded
  before it is collected. If we cannot name the benefit, we do not collect
  the datum.
- Every collection site **MUST** document, in code, *why* it is tracked and
  *how* the signal will be used.
- Scope **SHOULD** stay CLI-only; the server, Docker, and agent-config
  paths are explicitly out of scope.
- A telemetry failure **MUST NOT** alter or block any user-facing behaviour.

## Considered Options

- CLI-only anonymous telemetry to PostHog, opt-out, with a compile-time
  property allow-list (this decision)
- Full-stack telemetry (the prototype: CLI + web server + config + Docker)
- A hand-written typed allow-list with no proc-macro
- No telemetry

## Decision Outcome

Chosen: **CLI-only anonymous telemetry to PostHog, opt-out, with a
compile-time property allow-list**, governed by two binding principles.

**Principle 1: no tracking without a concrete improvement hypothesis.**
We do not add an event or property until we can state how it will improve
Aura for users (performance, UX, reliability, or what to prioritise). This
is a reviewer checklist item: a PR that adds a field without a documented
benefit is not approved.

**Principle 2: document the why/how at the tracking site.** Every event
struct and every property field carries a doc comment stating why it is
tracked and how the signal is used. The justification lives next to the
code, not only in this ADR.

**Privacy as a build artifact.** Telemetry lives in two dedicated crates
(`aura-telemetry`, `aura-telemetry-derive`) so it can be read in one place.
Event structs are built with `#[derive(Event)]`; every field type must
implement a sealed `IntoTelemetryProperty` trait, which is **not**
implemented for `String`, `&str`, integers, or `PropertyValue` itself.
Adding a free-form field therefore fails to compile, and `compile_fail`
tests pin that guarantee. The install identifier is the PostHog
`distinct_id` only and can never appear as an event property, by
construction.

**Consent.** Three states: `Unknown` / `Enabled` / `Disabled`. The first
interactive REPL launch with no recorded preference prints a one-time
notice and stays held (`Unknown`): events are written to a local
inspection log so the user can see what *would* be sent, but nothing is
transmitted. Sending the first chat message is treated as consent →
`Enabled`. Slash commands never grant consent; `/telemetry disable` →
`Disabled`; quitting leaves `Unknown` so the notice returns next launch.
Kill switches (`DO_NOT_TRACK`, `AURA_TELEMETRY_DISABLED`, CI markers,
`[telemetry] enabled = false`) always win. One-shot `--query` is
non-interactive, cannot show the notice, stays `Unknown`, and never sends.

**Events (v1).** `cli_session_started` (`interactive`, `standalone_mode`,
`client_tools_enabled`) and a `chat_request_started` / `chat_request_completed`
pair (`success: bool` only), fired once per turn regardless of backend.

**Deferred.** Model-identifier signal is deferred until a typed,
non-free-string representation is settled (a bounded `ModelFamily` enum or
a reviewed `ModelId` newtype), so it cannot land as a free-form string.

### Positive Consequences <!-- optional -->

- The privacy contract is enforced by the compiler, not by review vigilance.
- The two principles bound future growth: collection cannot expand without
  a stated user benefit documented in code.
- The whole surface is two crates plus the CLI wiring, small to audit.
- Held/disabled installs still write the local inspection log, so any user
  can verify the kill switch with `cat ~/.aura/telemetry/events.jsonl`.

### Negative Consequences <!-- optional -->

- The compile-time allow-list adds a proc-macro crate to maintain.
- Adding a legitimately new property is deliberately more work (typed
  variant + doc row + justification), trading convenience for safety.
- First-message-as-consent is implicit; we mitigate with the notice and the
  no-backfill, never-send-while-held guarantees.

## Pros and Cons of the Options <!-- optional -->

### CLI-only anonymous telemetry with a compile-time allow-list

- Good: smallest defensible surface; privacy enforced structurally.
- Good: answers the adoption question for the surface users actually run.
- Bad: proc-macro crate and the deferred model-id representation.

### Full-stack telemetry (the prototype)

- Good: one mechanism everywhere; server-side signal too.
- Bad: far larger surface to audit and defend; server/Docker/config
  coupling we do not want to own for the adoption question.

### Hand-written typed allow-list, no proc-macro

- Good: no second crate.
- Bad: the "no free-form fields" rule becomes a convention a reviewer must
  catch, not a compile error, which is weaker for an audience that reads
  the code.

### No telemetry

- Good: zero privacy surface.
- Bad: leaves the adoption question unanswerable; we cannot prioritise work
  against real usage.

## Links <!-- optional -->

- [docs.mezmo.com/aura/telemetry](https://docs.mezmo.com/aura/telemetry): the user-facing privacy contract
  (states, events, kill switches, audit guide).
- Allow-list and gate: `crates/aura-telemetry/src/properties.rs` and the
  `compile_fail` tests in `crates/aura-telemetry/tests/`.
- RFC 2119 keywords: <https://www.rfc-editor.org/rfc/rfc2119>
