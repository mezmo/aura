# P45 CONTRACT skeleton — design record (2026-09-14)

Layer 1 of the park/reify dispatch contract: the compile-clean shared type
surface. Real signatures, real derives, `todo!()` only where behavior lands in
a named fill unit. Existing code compiles unchanged and every existing suite
stays green; nothing behavioral moved.

Contract (REPAIR-2 corrected the pointer):
`/Users/mshearer/workspace/aura-session-docs/boards/aura-orchestration-mode/docs/board/plans/2026-09-14-park-reify-dispatch.md`
(the wiki checkout's `boards/aura-orchestration-mode/docs/board/plans/`
tree; there is no worktree-relative copy).

REPAIR-1 (owner-ruled 2026-09-14) executed the five contract-mandated
replacements below (retention field, event field, authority field, park_ttl
config field, completion input ownership) with their authorized test ripples.

REPAIR-2 (owner-ruled 2026-09-14) executed the two-seat design panel's
ACCEPT findings (F1-F12, F14 residual row, F15 pointer) — see the per-finding
notes at the bottom. F13 (fixture channel combos) is DEFERRED to owner-authored
Layer 2 work and no fixture was touched for it.

## What landed, mapped to the business rule each type enforces

| Type | Where | Rule enforced / illegal state forbidden | Fill unit that consumes it |
| --- | --- | --- | --- |
| `ParkTtl` (+`ParkTtlZero`) | `aura-config/src/park.rs` | Retention age is disk evidence lifetime, nonzero by construction; a zero age cannot exist downstream of `try_new` — and since REPAIR-1 the age is the `[hitl.park].park_ttl` config field itself (serde `try_from = "u64"`), so zero is refused at config parse; default 3600s by contract | R1 (relation), E5 (stamp) |
| `RouteTimeoutSecs` | `aura-config/src/park.rs` | The route's decision window travels labeled, never a bare second count next to the retention age | R1 |
| `AdmittedParkRoute` (REPAIR-2 F4) | `aura-config/src/park.rs` | The admitted parking payload's fields are PRIVATE: the `park_ttl >= route.timeout_secs` relation cannot be forged, because only `validate_park_admission`'s body constructs the pair — a `PollOrchestration{ttl:1, timeout:3600}` forgery is now unrepresentable | R1 |
| `ParkRouteAdmission`/`NonParkingRoute` | `aura-config/src/park.rs` | Route exclusivity is a two-variant vocabulary: webhook-poll+orchestration is the ONLY parking path (carrying the opaque admitted payload); conversational and webhook-sync are `NonParking` — an inadmissible combination cannot travel as data, only as `ParkAdmissionError` | R1, R2 |
| `ParkAdmissionError` | `aura-config/src/park.rs` | Each exclusivity rule (park+conversational, park+sync, poll without orchestration, poll without park, ttl below route timeout) is a named variant carrying the validated inputs it ruled on | R1 |
| `validate_park_admission` (hole) | `aura-config/src/park.rs` | The one admission authority; runtime park admission re-derives from its output instead of re-checking raw config. Since REPAIR-1 the retention age is read from `hitl.park.park_ttl`, so the signature takes the config table and the orchestration mode flag only | R1 |
| `ApprovalAuthority` | `aura/src/hitl/outcome.rs` | Persisted row ownership: a row parked by one channel cannot be consumed through another; wrong authority is unknown, not forbidden knowledge. Since REPAIR-3 the authority is a required field on every parked row — `ParkedApproval` and `ParkedApprovalRecord` — with no serde default and no old-row derivation; local ingress and the standalone resolver construct `Conversational`, the 207 bridge `WebhookPoll` | R4 (record), E2/E3 (checks) |
| `AddressedApproval` | `aura/src/hitl/outcome.rs` | Expiry is a durable per-call `TimedOut { deadline }`, never a fabricated approval or denial; a decision is a decision | E1 (codec), E7 (consult) |
| `ApprovalRead` | `aura/src/hitl/outcome.rs` | One read answers exactly: missing / pending / addressed-with-outcome; missing and corrupt evidence can never surface as an outcome | E2/E3 |
| `ApprovalStore::read_or_expire` | `aura/src/session_store/mod.rs` (+4 impls) | Authority, deadline, and terminal-winner checks happen under one store serialization boundary; errors are errors | E2 (file), E1/E2 (memory), E3 (registry seam) |
| `ApprovalStore::resolve(expected_authority)` (REPAIR-2 F5) | `aura/src/session_store/mod.rs` (+4 impls, registry, every caller) | The resolver presents the channel it resolves under; authority, deadline, and terminal-winner arbitration stay inside the ONE store serialization boundary — a validate-then-resolve race ahead of it is unrepresentable at the signature level. Wrong authority = `NotFound`, no mutation (E1/E2 enforce; R4 wires the callers) | R4, E1/E2 |
| `RetainedApproval` + `ApprovalStore::retained_rows` (REPAIR-2 F11) | `aura/src/session_store/mod.rs` (+4 impl holes) | The cleanup scan's row: pending AND already-addressed rows — retention, not decidability, is the question, so `list_pending` cannot stand in for it | E2 (file), E6/E8 |
| `TerminalRecord` (REPAIR-2 F6) | `aura/src/session_store/record.rs` | The persisted terminal half is tagged: a stored `TimedOut { deadline }` round-trips without fabricating a denial, and the shipped decided wire shape decodes unchanged (untagged over disjoint required fields — decided needs `approved`/`decided_at`, timed-out needs `deadline`). `TryFrom<TerminalRecord> for AddressedApproval` is the addressed-outcome carrier out of storage into read-or-expire's addressed arm | E1 (write), E2 (read_or_expire), E7 (consult consumption) |
| `FileApprovalStore::open_with_clock` (REPAIR-2 F6) | `aura/src/session_store/file.rs` | The file store's deadline sampling is injectable: one clock, sampled while holding the same lock resolve/remove hold (`StoreClock`), so timeout arbitration is testable and never reads two different nows | E2 |
| `PendingApprovals::read_or_expire` (REPAIR-2 F6) | `aura/src/hitl/registry.rs` | The registry forwards the authority-aware read fail-closed: a store fault is an error, never an outcome — the consult's (E7) per-member read | E3, E7 |
| `SessionStoreError::{UnsupportedOperation, UnsupportedConfiguration}` (REPAIR-2 F7) | `aura/src/session_store/mod.rs` | "The backend does not do this" and "this deployment's store config is rejected" are named, distinct from `BackendUnavailable` — a compiled-but-unsupported park backend never answers a park surface with a connection fault, a panic, or faked success | E-family, E8 (bootstrap admission) |
| `ResumeRefusal::{InvalidEvidence, Unavailable, Internal}` (REPAIR-2 F7) | `aura/src/orchestration/park/resume/evaluate.rs` (+ endpoint arms) | The typed fault rows let the endpoint select 500 `reify_failed` vs 503 `reify_unavailable` by variant, not by diagnostic string; the diagnostics log server-side and stay out of the client payload. `Fault(Diagnostic)` remains the undifferentiated sink until the S2/C fills migrate its sites | S2/C (endpoint mapping), E3 (classification) |
| `RunReservationLease` (REPAIR-2 F1) | `aura/src/orchestration/park/lifetime.rs` | The fence wraps one private `Arc<ReservationInner>` (run identity + table): `Clone` shares THAT reservation's occupation, the run releases synchronously on the inner's final drop (the last-reference fence), and no crate-visible field can assemble a lease that never reserved — construction is `admitted(...)`, called only after successful table admission | E4 |
| `RunExecutionScope` (REPAIR-2 F3) | `aura/src/orchestration/park/lifetime.rs` | Park-owned execution carries reservation+cancellation+tracker as one value; the naked `&TaskTracker` accessor is GONE — spawning goes through `spawn_tracked`/`spawn_blocking_tracked` (register before spawn, lease reference through actual completion), with `cancel` and `drain` exposed separately, so a detached task cannot spawn unregistered or outlive its fence | L1/L2/L3 |
| `ReservationFault` + `ResumeClaimTable::reserve` (hole) | `park/lifetime.rs`, `park/resume/claim.rs` | The claim table generalizes internally (no parallel per-request table); reservation admission is the ordered resume's step 2 | E4 |
| `ReservedEvaluation` + `convert_reserved` (holes, REPAIR-2 F2) | `park/resume/evaluate.rs` | The ordered resume's internal carrier (reservation + re-read checkpoint) and the CONSUMING step-6 transition: the same reservation becomes the grant's fence with a lease-holding rename tail and no ownerless gap. Dropping the carrier before conversion releases the reservation with no execution | E4 |
| `ResumeClaimTable::rename_back_under_reservation` (hole, REPAIR-2 F2) | `park/resume/claim.rs` | Step 3's rename-back runs fenced by the held reservation: the blocking tail holds a lease reference through completion (the pre-reservation `rename_back_to_parked` stays only until E4's ordered evaluation replaces it) | E4 |
| `ResumeGrant.reservation` (REPAIR-2 F2) | `park/resume/evaluate.rs` | The grant owns the run's RESERVATION lease (single-use, non-cloneable): whatever hands the grant its fence, the run releases only with the grant and every lease reference it handed out. `execution_scope()` is the supervisor's grant-to-scope seam (S3/S4) | E4, S3/S4 |
| `run_segment_borrowed` (hole, REPAIR-2 F3) | `park/resume/evaluate.rs` | The supervisor's borrowed-grant segment seam: the supervisor retains grant ownership across cancellation and never moves the grant into a cancellable select arm | S4 |
| `OrchestratorFactory.reservations` (REPAIR-2 F3) | `aura/src/orchestration/factory.rs` | Shared reservation-table injection into the initial factory's configuration (`with_reservation_table`): the initial park-enabled producer reserves its persistence-bound run id through the shared table immediately after orchestrator construction | L4, E4 |
| `ParkGuardMode::Resumed` + `new_resumed` (REPAIR-2 F3) | `aura/src/orchestration/park/guard.rs` | The resumed `ParkGuard` is checkpoint-preserving BY CONSTRUCTION: its drop never sweeps retained rows merely because the segment did not re-park — retention cleanup (E6/E8), not the guard, reclaims expired evidence | L3 |
| `ToolCallContext::execution_scope` (+`with_execution_scope`) | `aura/src/tool_wrapper.rs` | The scope is optional by type: non-park calls keep unscoped behavior; no call site is injected yet | L2 |
| `RetentionError` + `RetentionExpiresAt::from_publication` (REPAIR-2 F8) | `park/retention.rs` | Stamping is fully checked: `Duration::try_seconds` + `checked_add_signed` behind a typed `Result` (`AgeOutOfRange` / `DeadlineOverflow`) — the `chrono::Duration::seconds` panic above i64::MAX/1000s is unreachable, and an unrepresentable deadline is refused, never wrapped or saturated | E5 |
| `HitlRuntime.park_ttl` + `ParkCommitInputs.park_ttl` (REPAIR-2 F8) | `aura/src/hitl/route.rs`, `park/commit.rs` | The validated retention age is projected from the parsed config into the runtime HITL state and the publication inputs: E5's stamp reads a validated `ParkTtl`, never a raw window. The interim bridge (earliest ticket expiry, else `now + decision_window`) still stamps; `decision_window` stays its input until E5 replaces it | E5 |
| `ParkedRun.retention_expires_at` | `park/document.rs` (REPAIR-1) | The checkpoint document carries the absolute retention deadline as a typed field (serde: RFC 3339 instant); the run-wide decision `expires_at` string is gone from the format, and a stamp that is not a valid instant cannot decode | E5 (publication-transaction stamp and re-park renewal replace the interim commit bridge) |
| `aura_events::RetentionExpiresAt` (REPAIR-2 F9) | `aura-events/src/retention.rs` | The validated absolute-retention wire stamp, in the dependency location every event surface sees: an RFC 3339 instant by construction — `"not-an-instant"` cannot ride an event or cross decode. Wire key and RFC 3339 rendering preserved | SSE fills consume it as-is |
| `RunParked.retention_expires_at` | `orchestration/events.rs`, `stream_events.rs`, `aura-events/orchestration.rs` (REPAIR-2 F9) | All three RunParked surfaces carry the validated stamp type; the terminal park event names the retention deadline, not a decision expiry, and never an arbitrary string | SSE fills consume it as-is |
| `ResumeConflictRow::expired(Vec<BlockingEntry>)` (REPAIR-2 F11) | `park/resume/evaluate.rs` | The expired row never requires an outstanding call: a retention-expired checkpoint with every approval addressed renders the (possibly empty) expired row instead of being unrepresentable. `parked` keeps `NonEmptyBlocking` | E7 (per-call snapshots) |
| `CheckpointAbsence` / `RunCleanupOutcome` / cleanup seams (REPAIR-2 F11) | `park/cleanup.rs` (new) | Cleanup classification is typed: confirmed absence vs inaccessible vs corrupt are distinct (a missing root or unreadable document is NEVER confirmed absence); deletion is encapsulated evidence-first/checkpoint-last with retain-on-failure for retry. Declared only — nothing is wired into the server | E6 (sweep/orphans), E8 (activation) |
| `CompletionInput` | `aura-web-server/src/handlers.rs` | The completion input is single-use and non-cloneable: a grant that could be duplicated would make the once-only rule a runtime check. Since REPAIR-5 `RequestSetup` owns it; since REPAIR-2 F10 the consumed match produces the common (stream, cancel, usage) tuple in BOTH arms | S2/S3 |
| `RigBuilder::prepare_agent_config` (hole) | `aura/src/rig_builder.rs` | The production config projection for a request is fallible and distinct from the debug-only `get_agent_config`; resume never uses the debug path | S1 |
| `OrchestratorFactory::resume_stream_with_timeout` (hole) | `aura/src/orchestration/factory.rs` | Resume enters through the factory, consuming the grant by value and returning the existing stream/cancel/usage tuple — no replayable adapter | S3 |

## Visibility and seam table

| Surface | Lives at | Visibility | Reached by |
| --- | --- | --- | --- |
| `ParkTtl`, `RouteTimeoutSecs`, `AdmittedParkRoute`, `ParkRouteAdmission`, `ParkAdmissionError`, `validate_park_admission` | `aura_config::park`; `ParkTtl` also the `[hitl.park].park_ttl` field on `ParkConfig` | `pub`, re-exported from `aura_config` | R1 wires into `Config::validate`; runtime admission (R2) reads the ADT |
| `ApprovalAuthority`, `AddressedApproval`, `ApprovalRead` | `aura::hitl::outcome`; `ApprovalAuthority` also a required field on `ParkedApproval` and `ParkedApprovalRecord` | `pub`, re-exported from `aura::hitl` | `ApprovalStore` trait, record codec (E1), registry seam (E3) |
| `read_or_expire`, `retained_rows` | `ApprovalStore` trait | required trait methods | every backend: memory, file, fault double (cfg(test)), redis (feature `session-store-redis`) |
| `TerminalRecord` | `aura::session_store::record` | `pub(crate)` | file store codec (E1/E2); the addressed-outcome carrier (E2/E7) |
| `RetainedApproval` | `aura::session_store` | `pub` | retained scan consumers (E2/E6/E8) |
| `park_authority` | `DecisionRoute` | `pub` method | gate/bridge (R3), poller filtering (R4) |
| `RunExecutionScope`, `RunReservationLease`, `ReservationFault` | `park::lifetime`; REPAIR-2 F12 re-exports ALL THREE through `aura::orchestration` — `ReservationFault` is the error arm of the public `ResumeClaimTable::reserve`, so a caller can name and match `ReservationFault::Live` through the facade | `pub`, re-exported from `aura::orchestration` | `ToolCallContext` field (public type in a pub field), L fills, E4 |
| `RetentionExpiresAt` (deadline, `from_publication`, `RetentionError`) | `park::retention` | `pub(crate)`, module `#![allow(dead_code)]` until E5 fills the stamp; persisted (serde newtype over `DateTime<Utc>`) on every checkpoint document via `ParkedRun.retention_expires_at` | the checkpoint commit stamp (E5); `as_datetime`/`from_datetime` are the interim commit bridge |
| `aura_events::RetentionExpiresAt` | `aura-events::retention`, re-exported from `aura_events` | `pub` | every RunParked event surface (aura internal event, aura SSE mirror, aura-events DTO) |
| `reserve`/`rename_back_under_reservation` | `ResumeClaimTable` | `pub` method / `pub(crate)` hole | E4 resume ordering |
| `ReservedEvaluation`/`convert_reserved` | `park::resume::evaluate` | `pub(crate)` (declared for E4's in-crate ordered evaluation) | E4 |
| `run_segment_borrowed` | `aura::orchestration` (pub through the resume facade list) | `pub` hole | S4 supervisor |
| `OrchestratorFactory::with_reservation_table`/`reservation_table` | `orchestration::factory` | `pub` | L4/E4 wiring |
| `CheckpointAbsence`, `RunCleanupOutcome`, cleanup seams | `park::cleanup` | `pub(crate)`, `#![allow(dead_code)]` until E6/E8 | E6/E8 |
| `CompletionInput` | `aura_web_server::handlers` | `pub` enum | owned by `RequestSetup`; both arms wired (F10): Chat = today's producer path, Resume = the factory's S3 hole |
| `prepare_agent_config` | `RigBuilder` | `pub` | S1/S2 resume handler |
| `resume_stream_with_timeout` | `OrchestratorFactory` | `pub` | S2/S3 completion entry |

## Hole inventory

Every `todo!()` in the skeleton after REPAIR-2, with its owning fill-unit
family per the contract's dispatch table:

| # | Site | Fill family |
| --- | --- | --- |
| 1 | `crates/aura-config/src/park.rs` `validate_park_admission` (REPAIR-1 tightened the signature; REPAIR-2 F4 made `PollOrchestration` carry the opaque `AdmittedParkRoute` its body constructs) | R1 |
| 2 | `crates/aura/src/session_store/memory.rs` `read_or_expire` | E1/E2 |
| 3 | `crates/aura/src/session_store/file.rs` `read_or_expire` | E2 |
| 4 | `crates/aura/src/session_store/fault_store.rs` `read_or_expire` (cfg(test) double; compile-conformance ripple of the trait method, disclosed to the owner) | E1/E2 |
| 5 | `crates/aura-web-server/src/session_store/redis/approval_store.rs` `read_or_expire` (unsupported park backend: returns the typed unsupported-configuration error) | E-family |
| 6 | `crates/aura/src/orchestration/park/resume/claim.rs` `ResumeClaimTable::reserve` | E4 |
| 7 | `crates/aura/src/orchestration/park/lifetime.rs` `RunExecutionScope::spawn_tracked` (REPAIR-2 F3) | L1 |
| 8 | `crates/aura/src/orchestration/park/lifetime.rs` `RunExecutionScope::spawn_blocking_tracked` (REPAIR-2 F3) | L1 |
| 9 | `crates/aura/src/orchestration/park/lifetime.rs` `RunExecutionScope::drain` (REPAIR-2 F3) | L1 |
| 10 | `crates/aura/src/orchestration/park/resume/claim.rs` `rename_back_under_reservation` (REPAIR-2 F2) | E4 |
| 11 | `crates/aura/src/orchestration/park/resume/evaluate.rs` `convert_reserved` (REPAIR-2 F2) | E4 |
| 12 | `crates/aura/src/orchestration/park/resume/evaluate.rs` `run_segment_borrowed` (REPAIR-2 F3) | S4 |
| 13 | `crates/aura/src/session_store/memory.rs` `retained_rows` (REPAIR-2 F11: unsupported-operation answer — no park parity) | E1/E2 |
| 14 | `crates/aura/src/session_store/file.rs` `retained_rows` (REPAIR-2 F11) | E2 |
| 15 | `crates/aura/src/session_store/fault_store.rs` `retained_rows` (REPAIR-2 F11) | E1/E2 |
| 16 | `crates/aura-web-server/src/session_store/redis/approval_store.rs` `retained_rows` (REPAIR-2 F11: unsupported-operation answer) | E-family |
| 17 | `crates/aura/src/orchestration/park/cleanup.rs` `confirm_checkpoint_absence` (REPAIR-2 F11) | E6 |
| 18 | `crates/aura/src/orchestration/park/cleanup.rs` `delete_expired_run` (REPAIR-2 F11) | E6 |
| 19 | `crates/aura/src/orchestration/park/cleanup.rs` `scan_checkpoint_root` (REPAIR-2 F11) | E6 |
| 20 | `crates/aura/src/rig_builder.rs` `RigBuilder::prepare_agent_config` | S1 |
| 21 | `crates/aura/src/orchestration/factory.rs` `OrchestratorFactory::resume_stream_with_timeout` | S3 |
| 22 | `crates/aura-web-server/src/handlers.rs` `build_completion_config` Resume arm (REPAIR-1/R-5; provider/model, otel query, and message count from the resumed run's factory) | S2/S3 |

Holes REMOVED by REPAIR-2:
- `RunReservationLease::drop` (old #7): the per-reservation `ReservationInner`
  drop now performs the release — the identical release the retired
  `ResumeLease` performed, moved onto the inner object whose final drop IS the
  last-reference fence.
- `execute_completion` Resume arm (old #11): REPAIR-2 F10 restructured the
  entry — subscriptions register first, the owned input matches exactly once,
  and the Resume arm is now the mechanical call into the factory's S3 hole
  (#21). A Resume construction panics with the S3 hole's message.

Not holes (implemented, trivially): `ParkTtl::try_new`,
`RetentionExpiresAt::from_publication` (checked arithmetic, REPAIR-2 F8),
`DecisionRoute::park_authority` (total projection), `PendingApprovals::read_or_expire`
(one-line fail-closed forward, REPAIR-2 F6), all field accessors,
`RunExecutionScope::cancel` (token delegation), `RunReservationLease::admitted`
(construction-after-admission), `ParkGuard::new_resumed` (mode constructor;
the mode check in `Drop` keeps the declared seam honest — nothing constructs
it until L3). The pre-existing redis `mark_acknowledged` todo
(approval_store.rs) is the contract's separately-reported fast follow,
untouched here. The two small wire-pin tests in `aura-events/src/retention.rs`
and the `retained_rows`/`resolve` conformance sites are the REPAIR-2 additions
disclosed in its report.

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
   helper.
2. **R-2 EXECUTED — `RunParked` carries `retention_expires_at`.** Renamed
   across all three event surfaces; since REPAIR-2 F9 the field is the
   validated `aura_events::RetentionExpiresAt` everywhere. The event stamp
   is the document deadline carried as the validated instant (one
   conversion at the orchestrator's emit site).
3. **R-3 EXECUTED — required `ApprovalAuthority` on `ParkedApproval` and
   `ParkedApprovalRecord`.** No serde default, no old-row derivation. The
   gate's shared `park_register` takes the authority: the direct park path
   (local ingress) constructs `Conversational`, the 207 bridge constructs
   `WebhookPoll`; `PendingApprovals::register` (inline registration)
   constructs `Conversational`. Fixture ripples: poller rows
   `WebhookPoll`; every local-path fixture `Conversational`.
4. **R-4 EXECUTED — `[hitl.park].park_ttl` on `ParkConfig`.** Typed
   `ParkTtl`, serde `try_from = "u64"` (zero refused at parse), default
   3600s. `validate_park_admission` tightened: the now-derivable
   `park_ttl` parameter is gone; the orchestration mode stays a bool.
5. **R-5 EXECUTED — `RequestSetup` owns `CompletionInput`.** The
   `query`/`chat_history`/`streaming_agent` fields are replaced by the
   owned input; `prepare_request` builds `Chat { agent, query, history }`
   from exactly what it passed before. The `sse_approval_subscription_
   exists_before_stream_startup` literal ripple is the authorized one.

## Panel findings: owner-ruled EXECUTED (REPAIR-2, 2026-09-14)

Every ACCEPT finding from the two-seat panel
(`.review/p45-skeleton/panel/LEDGER.md`) landed as surface; the ledger's
dispositions govern. F13 is deferred; no fixture was touched for it.

- **F1 EXECUTED** — `RunReservationLease` is an opaque `Clone` wrapper over
  `Arc<ReservationInner>`; private fields; construction only via
  `admitted` after successful table admission; synchronous release on the
  inner's final drop (implemented — the same release the retired
  `ResumeLease` performed, relocated so the final drop of one reservation
  IS the fence). `ResumeLease` is deleted.
- **F2 EXECUTED** — `ReservedEvaluation` carrier + consuming
  `convert_reserved` + fenced `rename_back_under_reservation` declared
  (E4 holes); `ResumeGrant` owns the reservation; `claim_and_resume` is
  re-typed to hand out the reservation lease (behavior-identical interim:
  the ordered reordering of `evaluate_resume` itself is E4's body work —
  the ruled ordering is now expressible without an ownerless gap).
- **F3 EXECUTED** — tracked spawn helpers + `cancel`/`drain` on the scope
  (naked tracker accessor removed); factory table injection
  (`with_reservation_table`); grant-to-scope (`execution_scope`);
  borrowed-grant driver (`run_segment_borrowed`); `ParkGuard`
  resumed/preserving construction (`ParkGuardMode`, `new_resumed`). L1/L4/S4
  seams only.
- **F4 EXECUTED** — `AdmittedParkRoute` (private fields, admission-only
  construction); `PollOrchestration` carries it; the outer route enum
  stays.
- **F5 EXECUTED** — `expected_authority: ApprovalAuthority` on
  `ApprovalStore::resolve`, the registry `resolve`, and all four backend
  surfaces; every caller passes its channel (local ingress/resolver
  `Conversational`, poller `WebhookPoll`). The check itself is E1/E2's
  fill; no caller can any longer bolt a validate-then-resolve race.
- **F6 EXECUTED** — `TerminalRecord` (tagged, shipped decided shape
  unchanged) with fallible conversions to `ResolvedDecision` and
  `AddressedApproval`; file-store clock injection (`open_with_clock`,
  default wall clock); registry `read_or_expire` forward; the
  addressed-outcome carrier reaches recorded-call consumption through
  `TryFrom<TerminalRecord> for AddressedApproval` (E7's cutover maps
  `Decided` into the recorded set and `TimedOut` into the expired row).
  External approve/deny wire unchanged.
- **F7 EXECUTED** — `SessionStoreError` gains `UnsupportedOperation` and
  `UnsupportedConfiguration`; `ResumeRefusal` gains typed
  `InvalidEvidence`/`Unavailable`/`Internal` rows with fixed client codes
  (500 `reify_failed` / 503 `reify_unavailable`) and server-side-only
  diagnostics; `Fault` stays as the interim sink.
- **F8 EXECUTED** — `from_publication` returns
  `Result<RetentionExpiresAt, RetentionError>` with checked duration
  construction and checked addition; `ParkTtl` is projected into
  `HitlRuntime.park_ttl` and `ParkCommitInputs.park_ttl` (one publication
  timestamp source for the persisted deadline and the event — the commit's
  single resolved value already feeds both; E5 swaps the bridge for the
  publication-transaction stamp).
- **F9 EXECUTED** — `aura_events::RetentionExpiresAt` (validated RFC 3339
  wire stamp; chrono added to the serde-only events crate with no clock
  feature); all three `RunParked` surfaces carry it; wire key and RFC 3339
  rendering preserved; mechanical test-literal ripples only (the one
  `+00:00` SSE pin normalizes to the canonical `Z` form, matching the
  REPAIR-1 golden normalization).
- **F10 EXECUTED** — `execute_completion` restructured: guards and
  side-channel subscriptions register FIRST, the owned input matches
  exactly ONCE, both arms produce the common (stream, cancel sender,
  usage state) tuple; callback/telemetry agent factors separately (Chat:
  the request's agent; Resume: the reusable factory — never a replayable
  adapter). The Chat arm is today's producer path, behavior-identical; the
  Resume arm is the S3 hole call.
- **F11 EXECUTED** — `expired` no longer requires `NonEmptyBlocking`;
  cleanup seams declared (`CheckpointAbsence`, `RunCleanupOutcome`,
  `confirm_checkpoint_absence`, `delete_expired_run`,
  `scan_checkpoint_root`) plus the store-trait retained-row scan
  (`retained_rows`, four backend holes). Nothing is activated.
- **F12 EXECUTED** — `ReservationFault` re-exported through
  `aura::orchestration` alongside the reservation API; this table
  corrected (it lives in `park::lifetime`, not `resume::claim`, and is
  publicly reachable).
- **F14 RECORDED** — residual row below; no validated constructor added
  (see residual risks).
- **F15 EXECUTED** — contract pointer fixed (top of this file).

## Residual risks

- The commit's retention stamp is still the interim bridge (earliest
  surviving ticket expiry, else `now + decision_window`, wrapped via
  `from_datetime`) until E5 lands the publication-transaction stamp plus
  `park_ttl`; `from_publication` remains unwired and the retention
  module's `#![allow(dead_code)]` stays for it. `decision_window` remains
  `ParkCommitInputs`'s interim input alongside the projected `park_ttl`.
- The blocking projection and the consult still stamp every 409 entry with
  the document's single deadline and still consult one run-wide window;
  E7 owns the per-call snapshot replacement. `RefreshedAwaiting.expires_at`
  (the earliest per-call deadline) stays for the same reason.
- **F14 residual (owner-ruled):** the public
  `ApprovalRead::Addressed { approval, outcome }` variant permits an
  approval whose row deadline is A paired with `TimedOut { deadline: B }`.
  Consistency is a STORE-construction obligation (the E2 fill stamps the
  row's own deadline when it publishes the terminal record), not a type
  guarantee — a validated constructor was considered and NOT added:
  enum-variant fields are public, so a constructor adds no enforcement
  while implying one. Consumers of the addressed arm must not treat the
  two deadlines as independently trustworthy.
- `evaluate_resume` still enters through the one-shot
  `claim_and_resume` acquisition (behavior-identical interim); the E4 fill
  reorders it onto `reserve` → re-read/recheck → consult →
  `convert_reserved`. Until then the fenced rename-back seam
  (`rename_back_under_reservation`) and the consuming transition are
  declared but unwired, and the unfenced pre-reservation
  `rename_back_to_parked` remains in production.
- The memory backend's `read_or_expire` hole must enforce authority for
  inline requests WITHOUT park parity; the hole message states this so the
  fill cannot forget it.
- `resolve` accepts `expected_authority` but no backend enforces it yet
  (E1/E2 fill); until then the signature prevents the
  validate-then-resolve race but not the cross-channel resolve itself.
- `validate_park_admission` keeps `orchestration_enabled: bool` (a mode
  flag, `bind_identity` precedent); the park_ttl parameter is gone — the
  relation reads `hitl.park.park_ttl`.
- `RouteTimeoutSecs` is a labeling newtype, not a validated one — any
  zero-timeout policy belongs to the R1 relation, not the wrapper.
- `fault_store.rs` (cfg(test) double) carries signature-conformance todos —
  disclosed, not behavioral test edits; flagged for the owner.
- `handlers.rs`'s remaining S2/S3 hole is the `build_completion_config`
  Resume arm (a match arm, no `#[expect]` marker); a Resume construction
  reaching it panics with the owning unit named.
- The two wire-pin tests in `aura-events/src/retention.rs` are REPAIR-2
  additions (disclosed in the report; the owner may strike them).
