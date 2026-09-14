# P45 CONTRACT skeleton — design record (2026-09-14)

Layer 1 of the park/reify dispatch contract: the compile-clean shared type
surface. Real signatures, real derives, `todo!()` only where behavior lands in
a named fill unit. Existing code compiles unchanged and every existing suite
stays green; nothing behavioral moved.

Contract: `docs/board/plans/2026-09-14-park-reify-dispatch.md` (workspace
`~/workspace/aura-p56-sync-webhook`).

REPAIR-1 (owner-ruled 2026-09-14) executed the five contract-mandated
replacements below (retention field, event field, authority field, park_ttl
config field, completion input ownership) with their authorized test ripples.

## What landed, mapped to the business rule each type enforces

| Type | Where | Rule enforced / illegal state forbidden | Fill unit that consumes it |
| --- | --- | --- | --- |
| `ParkTtl` (+`ParkTtlZero`) | `aura-config/src/park.rs` | Retention age is disk evidence lifetime, nonzero by construction; a zero age cannot exist downstream of `try_new` — and since REPAIR-1 the age is the `[hitl.park].park_ttl` config field itself (serde `try_from = "u64"`), so zero is refused at config parse; default 3600s by contract | R1 (relation), E5 (stamp) |
| `RouteTimeoutSecs` | `aura-config/src/park.rs` | The route's decision window travels labeled, never a bare second count next to the retention age | R1 |
| `ParkRouteAdmission`/`NonParkingRoute` | `aura-config/src/park.rs` | Route exclusivity is a two-variant vocabulary: webhook-poll+orchestration is the ONLY parking path; conversational and webhook-sync are `NonParking` — an inadmissible combination cannot travel as data, only as `ParkAdmissionError` | R1, R2 |
| `ParkAdmissionError` | `aura-config/src/park.rs` | Each exclusivity rule (park+conversational, park+sync, poll without orchestration, poll without park, ttl below route timeout) is a named variant carrying the validated inputs it ruled on | R1 |
| `validate_park_admission` (hole) | `aura-config/src/park.rs` | The one admission authority; runtime park admission re-derives from its output instead of re-checking raw config. Since REPAIR-1 the retention age is read from `hitl.park.park_ttl`, so the signature takes the config table and the orchestration mode flag only | R1 |
| `ApprovalAuthority` | `aura/src/hitl/outcome.rs` | Persisted row ownership: a row parked by one channel cannot be consumed through another; wrong authority is unknown, not forbidden knowledge. Since REPAIR-3 the authority is a required field on every parked row — `ParkedApproval` and `ParkedApprovalRecord` — with no serde default and no old-row derivation; local ingress and the standalone resolver construct `Conversational`, the 207 bridge `WebhookPoll` | R4 (record), E2/E3 (checks) |
| `AddressedApproval` | `aura/src/hitl/outcome.rs` | Expiry is a durable per-call `TimedOut { deadline }`, never a fabricated approval or denial; a decision is a decision | E1 (codec), E7 (consult) |
| `ApprovalRead` | `aura/src/hitl/outcome.rs` | One read answers exactly: missing / pending / addressed-with-outcome; missing and corrupt evidence can never surface as an outcome | E2/E3 |
| `ApprovalStore::read_or_expire` | `aura/src/session_store/mod.rs` (+4 impls) | Authority, deadline, and terminal-winner checks happen under one store serialization boundary; errors are errors | E2 (file), E1/E2 (memory), E3 (registry seam) |
| `DecisionRoute::park_authority` | `aura/src/hitl/route.rs` | The route→authority mapping is total and singular — inline registration answers `Conversational`, the 207 bridge `WebhookPoll`, a hold route never parks | R3/R4 |
| `RunReservationLease` | `aura/src/orchestration/park/lifetime.rs` | The fence is shared (`Clone` = same occupation): blocking work keeps it after an awaiting request drops; release happens once, when the last reference ends | E4 |
| `RunExecutionScope` | `aura/src/orchestration/park/lifetime.rs` | Park-owned execution carries reservation+cancellation+tracker as one value; a detached task cannot spawn unregistered | L1/L2/L3 |
| `ReservationFault` + `ResumeClaimTable::reserve` (hole) | `park/resume/claim.rs` | The claim table generalizes internally (no parallel per-request table); reservation admission is the ordered resume's step 2 | E4 |
| `ToolCallContext::execution_scope` (+`with_execution_scope`) | `aura/src/tool_wrapper.rs` | The scope is optional by type: non-park calls keep unscoped behavior; no call site is injected yet | L2 |
| `RetentionExpiresAt` | `park/retention.rs` | The retention deadline is computed by checked conversion+addition only (never wrap/saturate), and is a distinct type from any per-call approval deadline | E5 |
| `ParkedRun.retention_expires_at` | `park/document.rs` (REPAIR-1) | The checkpoint document carries the absolute retention deadline as a typed field (serde: RFC 3339 instant); the run-wide decision `expires_at` string is gone from the format, and a stamp that is not a valid instant cannot decode | E5 (publication-transaction stamp and re-park renewal replace the interim commit bridge) |
| `RunParked.retention_expires_at` | `orchestration/events.rs`, `stream_events.rs`, `aura-events/orchestration.rs` (REPAIR-2) | The terminal park event names the retention deadline, not a decision expiry; the SSE wire key is `retention_expires_at` | SSE fills consume it as-is |
| `CompletionInput` | `aura-web-server/src/handlers.rs` | The completion input is single-use and non-cloneable: a grant that could be duplicated would make the once-only rule a runtime check. Since REPAIR-5 `RequestSetup` owns it; the Chat arm is the mechanical mapping of the former `query`/`chat_history`/`streaming_agent` fields, and the Resume arm is reachable only through the S2/S3 holes | S2/S3 |
| `RigBuilder::prepare_agent_config` (hole) | `aura/src/rig_builder.rs` | The production config projection for a request is fallible and distinct from the debug-only `get_agent_config`; resume never uses the debug path | S1 |
| `OrchestratorFactory::resume_stream_with_timeout` (hole) | `aura/src/orchestration/factory.rs` | Resume enters through the factory, consuming the grant by value and returning the existing stream/cancel/usage tuple — no replayable adapter | S3 |

## Visibility and seam table

| Surface | Lives at | Visibility | Reached by |
| --- | --- | --- | --- |
| `ParkTtl`, `RouteTimeoutSecs`, `ParkRouteAdmission`, `ParkAdmissionError`, `validate_park_admission` | `aura_config::park`; `ParkTtl` also the `[hitl.park].park_ttl` field on `ParkConfig` | `pub`, re-exported from `aura_config` | R1 wires into `Config::validate`; runtime admission (R2) reads the ADT |
| `ApprovalAuthority`, `AddressedApproval`, `ApprovalRead` | `aura::hitl::outcome`; `ApprovalAuthority` also a required field on `ParkedApproval` and `ParkedApprovalRecord` | `pub`, re-exported from `aura::hitl` | `ApprovalStore` trait, record codec (E1), registry seam (E3) |
| `read_or_expire` | `ApprovalStore` trait | required trait method | every backend: memory, file, fault double (cfg(test)), redis (feature `session-store-redis`) |
| `park_authority` | `DecisionRoute` | `pub` method | gate/bridge (R3), poller filtering (R4) |
| `RunExecutionScope`, `RunReservationLease` | `park::lifetime` | `pub`, re-exported from `aura::orchestration` | `ToolCallContext` field (public type in a pub field), L fills |
| `RetentionExpiresAt` | `park::retention` | `pub(crate)`, module `#![allow(dead_code)]` until E5 fills `from_publication`; persisted (serde newtype over `DateTime<Utc>`) on every checkpoint document via `ParkedRun.retention_expires_at` | the checkpoint commit stamp (E5); `as_datetime`/`from_datetime` are the interim commit bridge |
| `reserve`/`ReservationFault` | `ResumeClaimTable` | `pub` method / `pub` enum in `park::resume::claim` | E4 resume ordering |
| `CompletionInput` | `aura_web_server::handlers` | `pub` enum | owned by `RequestSetup` (REPAIR-5); Chat arm wired, Resume arms are the S2/S3 holes below |
| `prepare_agent_config` | `RigBuilder` | `pub` | S1/S2 resume handler |
| `resume_stream_with_timeout` | `OrchestratorFactory` | `pub` | S2/S3 completion entry |

## Hole inventory

Every `todo!()` introduced by this skeleton, with its owning fill-unit family
per the contract's dispatch table:

| # | Site | Fill family |
| --- | --- | --- |
| 1 | `crates/aura-config/src/park.rs:181` `validate_park_admission` (REPAIR-1 tightened the signature: the retention age is read from `hitl.park.park_ttl`) | R1 |
| 2 | `crates/aura/src/session_store/memory.rs:157` `read_or_expire` | E1/E2 |
| 3 | `crates/aura/src/session_store/file.rs:541` `read_or_expire` | E2 |
| 4 | `crates/aura/src/session_store/fault_store.rs:103` `read_or_expire` (cfg(test) double; compile-conformance ripple of the trait method, disclosed to the owner) | E1/E2 |
| 5 | `crates/aura-web-server/src/session_store/redis/approval_store.rs:393` `read_or_expire` (unsupported park backend: returns the typed unsupported-configuration error) | E-family |
| 6 | `crates/aura/src/orchestration/park/resume/claim.rs:212` `ResumeClaimTable::reserve` | E4 |
| 7 | `crates/aura/src/orchestration/park/lifetime.rs:45` `RunReservationLease::drop` | E4 |
| 8 | `crates/aura/src/rig_builder.rs:70` `RigBuilder::prepare_agent_config` | S1 |
| 9 | `crates/aura/src/orchestration/factory.rs:59` `OrchestratorFactory::resume_stream_with_timeout` | S3 |
| 10 | `crates/aura-web-server/src/handlers.rs:510` `build_completion_config` Resume arm (REPAIR-1/R-5: provider/model, otel query, and message count from the resumed run's factory) | S2/S3 |
| 11 | `crates/aura-web-server/src/handlers.rs:605` `execute_completion` Resume arm (REPAIR-1/R-5: the factory's `resume_stream_with_timeout` consumes the grant and returns the resumed stream) | S2/S3 |

Not holes (implemented, trivially): `ParkTtl::try_new`, `RetentionExpiresAt::from_publication`
(checked arithmetic), `DecisionRoute::park_authority` (total projection), all
field accessors. The pre-existing redis `mark_acknowledged` todo (approval_store.rs:176)
is the contract's separately-reported fast follow, untouched here.

## Contract-mandated replacements: owner-ruled EXECUTED (REPAIR-1, 2026-09-14)

The five replacements the skeleton withheld under brief rule 10 were ruled
EXECUTE with authorized test ripples. All five executed; no site stopped.

1. **R-1 EXECUTED — `ParkedRun.expires_at` → `retention_expires_at`
   (`RetentionExpiresAt`).** The field is a serde newtype over
   `DateTime<Utc>` (RFC 3339 instant on the wire); `build_document`,
   `ParkCommitOutcome`, and every reader take/return the typed deadline.
   The interim commit bridge stamps the same value the old string body did
   (earliest surviving ticket expiry, else `now + decision_window`) via
   `RetentionExpiresAt::from_datetime`; E5 replaces that bridge with the
   publication-transaction stamp plus `park_ttl`. Ripples: the golden
   fixture `testdata/park/parked_run_v1.json` (key renamed; value
   normalized to chrono's canonical `"2026-09-02T15:03:11Z"` — the
   round-trip assertion is unchanged), the `orchestrator.rs` frame
   `park_run_with_every_call_decided_stamps_the_decision_window`, the
   `commit.rs` fixture literals, and the `goldens.rs` `parked_document`
   helper (its `<fresh expiry>` pins needed no edit: the blocking entries'
   stamps are unchanged per-call wire values). The bad-stamp parse error
   paths in the consult and projection are gone — an invalid instant is
   now unrepresentable at decode.
2. **R-2 EXECUTED — `RunParked` carries `retention_expires_at`.** Renamed
   across `orchestration/events.rs`, `stream_events.rs` (enum + `run_parked`
   constructor + SSE literal pin), `aura-events/orchestration.rs` (enum +
   constructor), the `orchestrator.rs` event literal and test binding, and
   the web render arm in `streaming/handlers.rs`. The event stamp is the
   document deadline re-rendered via `to_rfc3339()`.
3. **R-3 EXECUTED — required `ApprovalAuthority` on `ParkedApproval` and
   `ParkedApprovalRecord`.** No serde default, no old-row derivation. The
   gate's shared `park_register` takes the authority: the direct park path
   (local ingress) constructs `Conversational`, the 207 bridge constructs
   `WebhookPoll`; `PendingApprovals::register` (inline registration)
   constructs `Conversational`. Fixture ripples derive per site: poller
   rows (rows the reconciler polls, including the born-notified 207-bridge
   shape) are `WebhookPoll`; every local-path fixture (registry, memory
   store, guard, continuation consult, commit, orchestrator park frames,
   resume goldens, web-server store conformance) is `Conversational`. The
   record.rs legacy-row JSON fixture gained `"authority":
   "conversational"`; its egress-absence assertion is unchanged.
4. **R-4 EXECUTED — `[hitl.park].park_ttl` on `ParkConfig`.** Typed
   `ParkTtl`, serde `try_from = "u64"` (zero refused at parse), default
   3600s. All six `ParkConfig { .. }` literals take
   `ParkTtl::default()` — none of the six frames asserts retention
   behavior (route admission, capability split, egress mapping, instant
   decide, park-commit publication, resume identity; the two park-commit
   frames assert publication and event ids, not retention values).
   `validate_park_admission` tightened: the now-derivable `park_ttl`
   parameter is gone (read from `hitl.park.park_ttl`); the orchestration
   mode stays a bool (`bind_identity` precedent).
5. **R-5 EXECUTED — `RequestSetup` owns `CompletionInput`.** The
   `query`/`chat_history`/`streaming_agent` fields are replaced by the
   owned input; `prepare_request` builds `Chat { agent, query, history }`
   from exactly what it passed before; both read sites match, the Chat arm
   returns exactly today's values, and the Resume arms are the two new
   S2/S3 holes. The `sse_approval_subscription_exists_before_stream_startup`
   literal ripple is the authorized one.

## Residual risks

- The five contract-mandated replacements are executed (above), so the
  document, events, parked rows, config, and completion input carry the
  contract shape. The commit's retention stamp is still the interim bridge
  (earliest surviving ticket expiry, else `now + decision_window`, wrapped
  via `from_datetime`) until E5 lands the publication-transaction stamp plus
  `park_ttl`; `from_publication` remains unwired and the retention module's
  `#![allow(dead_code)]` stays for it.
- The blocking projection and the consult still stamp every 409 entry with
  the document's single deadline and still consult one run-wide window;
  E7 owns the per-call snapshot replacement. `RefreshedAwaiting.expires_at`
  (the earliest per-call deadline) stays for the same reason.
- `validate_park_admission` keeps `orchestration_enabled: bool` (a mode
  flag, `bind_identity` precedent); the park_ttl parameter is gone — the
  relation reads `hitl.park.park_ttl`.
- `RouteTimeoutSecs` is a labeling newtype, not a validated one — any
  zero-timeout policy belongs to the R1 relation, not the wrapper.
- The memory backend's `read_or_expire` hole must enforce authority for
  inline requests WITHOUT park parity (contract section "Outcome and expiry
  seam"); the hole message states this so the fill cannot forget it.
- `fault_store.rs` (cfg(test) double) carries a signature-conformance todo —
  disclosed, not a behavioral test edit; flagged for the owner.
- `handlers.rs`'s two new S2/S3 holes are match arms, not functions, so they
  carry no `#[expect]` markers; a Resume construction reaching them panics
  with the owning unit named.
