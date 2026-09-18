# P45 CONTRACT skeleton — design record (2026-09-14)

Layer 1 of the park/reify dispatch contract: the compile-clean shared type
surface. Real signatures, real derives, `todo!()` only where behavior lands in
a named fill unit. Existing code compiles unchanged and every existing suite
stays green. Two narrow ACTIVE classification changes ride the surface
(present since the REPAIR-2/3 error taxonomy, disclosed for the owner): an
interim claim-rename failure now renders 503 `reify_unavailable` (was an
undifferentiated 500-class Fault row carrying its diagnostic), and a
blocking-task join failure renders the safe 500 `reify_failed` row. Both are
observable on the wire and await Mike's classification keep/defer ruling at
U(surface); this header previously called them "not new behavior", which was
misleading; corrected 2026-09-15. Every other interim keeps
admission/rename/rollback ordering identical.

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

REPAIR-3 (owner-ruled 2026-09-14) closed the round-2 verification findings
(G1-G8; `.review/p45-skeleton/REPAIR-3.md`, panel in `panel/ROUND-2.md`):
reservation admission became a table-owned seam (no crate-visible lease
constructor), the resumed execution scope became ONE grant-owned `Arc`
(plus the guard's scope carrier and injection constructor), `TerminalRecord`
became an explicitly tagged codec that rejects contradictory payloads, the
addressed-outcome carrier reached recorded-call consumption, the claim
seams' faults classify typed (availability/internal), cleanup gained a
Present outcome and a reservation-owning carrier, and the isolated
`aura-events` build was fixed (chrono `alloc`). All still surface-only.

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
| `TerminalRecord` (REPAIR-2 F6; REPAIR-3 G3) | `aura/src/session_store/record.rs` | The persisted terminal half is EXPLICITLY tagged (`kind`: `decided` / `timed_out`) with unknown fields denied: a record carrying both decided fields and a `deadline` FAILS decode — no untagged first-variant-wins fallthrough, and no compatibility fallback (the park terminal format is unshipped; the external approve/deny wire is a separate surface, unchanged). `TryFrom<TerminalRecord> for AddressedApproval` is the addressed-outcome carrier out of storage into read-or-expire's addressed arm | E1 (write), E2 (read_or_expire), E7 (consult consumption) |
| `FileApprovalStore::open_with_clock` (REPAIR-2 F6) | `aura/src/session_store/file.rs` | The file store's deadline sampling is injectable: one clock, sampled while holding the same lock resolve/remove hold (`StoreClock`), so timeout arbitration is testable and never reads two different nows | E2 |
| `PendingApprovals::read_or_expire` (REPAIR-2 F6) | `aura/src/hitl/registry.rs` | The registry forwards the authority-aware read fail-closed: a store fault is an error, never an outcome — the consult's (E7) per-member read | E3, E7 |
| `SessionStoreError::{UnsupportedOperation, UnsupportedConfiguration}` (REPAIR-2 F7) | `aura/src/session_store/mod.rs` | "The backend does not do this" and "this deployment's store config is rejected" are named, distinct from `BackendUnavailable` — a compiled-but-unsupported park backend never answers a park surface with a connection fault, a panic, or faked success | E-family, E8 (bootstrap admission) |
| `ResumeRefusal::{InvalidEvidence, Unavailable, Internal}` (REPAIR-2 F7) | `aura/src/orchestration/park/resume/evaluate.rs` (+ endpoint arms) | The typed fault rows let the endpoint select 500 `reify_failed` vs 503 `reify_unavailable` by variant, not by diagnostic string; the diagnostics log server-side and stay out of the client payload. `Fault(Diagnostic)` remains the undifferentiated sink until the S2/C fills migrate its sites | S2/C (endpoint mapping), E3 (classification) |
| `RunReservationLease` (REPAIR-2 F1; REPAIR-3 G1) | `aura/src/orchestration/park/lifetime.rs` | The fence wraps one private `Arc<ReservationInner>` (run identity + table): `Clone` shares THAT reservation's occupation, the run releases synchronously on the inner's final drop (the last-reference fence), and NO crate-visible path can assemble a lease — the constructor is module-private, reachable only from [`ReservationTable`]'s admission paths, so admission and construction are one seam and a lease for a never-admitted run is unrepresentable | E4 |
| `ReservationTable` + `AdmissionFault` (REPAIR-3 G1) | `aura/src/orchestration/park/lifetime.rs` | The occupied-run set and its short standard lock are private to the table: `admit` (check-and-insert) and `admit_with` (insert + under-lock step with rollback — the claim-and-rename's atomicity seam) are the ONLY lease-birth paths; `is_live` and `under_standard_lock` (the interim rename-back's mutual exclusion) are the only other readers. `ResumeClaimTable` injects this type, never the raw lock | E4 |
| `RecordedDecisions` (carrier, REPAIR-3 G4) | `aura/src/orchestration/park/recorded_decisions.rs` | The recorded queue, insertion (`push`), consumption (`take`), and preflight (`peek_at`) all carry `AddressedApproval`: `Decided` preserves the recorded decision identity, `TimedOut` rides independently (peeking ready — terminal for its call, never needing identity) and at consumption feeds the EXACT shared `TerminalGateDecision::TimedOut` mapping (`recorded_pre_call`), never a fabricated denial | E7 (consult cutover) |
| `RunExecutionScope` (REPAIR-2 F3) | `aura/src/orchestration/park/lifetime.rs` | Park-owned execution carries reservation+cancellation+tracker as one value; the naked `&TaskTracker` accessor is GONE — spawning goes through `spawn_tracked`/`spawn_blocking_tracked` (register before spawn, lease reference through actual completion), with `cancel` and `drain` exposed separately, so a detached task cannot spawn unregistered or outlive its fence | L1/L2/L3 |
| `ReservationFault` + `ResumeClaimTable::reserve` (hole) | `park/lifetime.rs`, `park/resume/claim.rs` | The claim table generalizes internally (no parallel per-request table); reservation admission is the ordered resume's step 2 — through `ReservationTable::admit`, which is also the only lease-birth path | E4 |
| `ClaimResumeFault::{Unavailable, Internal}` (REPAIR-3 G5) | `park/resume/claim.rs` | The claim seams' faults carry the typed classification: a filesystem rename failure is `Unavailable` (503 `reify_unavailable`) and a blocking-task failure is `Internal` (500 `reify_failed`) — `From<ClaimResumeFault> for ResumeRefusal` maps onto the typed rows, never the interim `Fault` sink; `convert_reserved` and `rename_back_under_reservation` return the same classified vocabulary (Live is impossible under a held reservation) | E4, E3 (classification) |
| `ReservedEvaluation` + `convert_reserved` (holes, REPAIR-2 F2) | `park/resume/evaluate.rs` | The ordered resume's internal carrier (reservation + re-read checkpoint) and the CONSUMING step-6 transition: the same reservation becomes the grant's fence with a lease-holding rename tail and no ownerless gap. Dropping the carrier before conversion releases the reservation with no execution | E4 |
| `ResumeClaimTable::rename_back_under_reservation` (hole, REPAIR-2 F2) | `park/resume/claim.rs` | Step 3's rename-back runs fenced by the held reservation: the blocking tail holds a lease reference through completion (the pre-reservation `rename_back_to_parked` stays only until E4's ordered evaluation replaces it) | E4 |
| `ResumeGrant.reservation` + `.scope` (REPAIR-2 F2; REPAIR-3 G2) | `park/resume/evaluate.rs` | The grant owns the run's RESERVATION lease AND its ONE execution scope, established at conversion (`RunExecutionScope::new` runs once per grant; the interim `authorize` assembly establishes it the same way): single-use, non-cloneable, and `execution_scope()` returns a clone of that same `Arc` — never a fresh token or tracker — so the supervisor, driver, contexts, and guard share one cancellation and drain state | E4, S3/S4 |
| `run_segment_borrowed` (hole, REPAIR-2 F3; signature amended 2026-09-15) | `park/resume/evaluate.rs` | The supervisor's borrowed-grant segment seam: the supervisor retains grant ownership across cancellation and never moves the grant into a cancellable select arm. Since the 2026-09-15 alignment amendment the signature is stream-shaped; beside grant/config/headers it takes the existing event sender (`mpsc::Sender<Result<StreamItem, StreamError>>`); the caller's shared `UsageState`; `outer_budget: Option<Duration>`, projected by the factory from its timeout via the normal `(!timeout.is_zero()).then_some(timeout)` convention and received by the resumed coordinator. It returns `Result<ResumeStreamEnd, SegmentError>`. The driver forwards normal events only, with no duplicate final of its own; configured visibility/buffering is preserved (not every raw worker item must stream). Cancellation is the grant's one execution scope (`grant.execution_scope()`), the sole cancellation path; no second token parameter exists | S3 (factory projection), S4 (driver) |
| `ResumeStreamEnd` (declared 2026-09-15) | `park/resume/evaluate.rs` | The streamed segment's terminal bookkeeping is exactly two-valued: `Completed { final_answer }` hands the final answer to the normal factory finalization (the driver never emits a duplicate final), and `Reparked` reports a fresh park: the publication owner emits `RunParked` once, while the supervisor still applies the normal terminal stream policy. Internal terminal bookkeeping, but nameable through the `orchestration` facade because `run_segment_borrowed` is public (re-exported beside it, same convention) | S4 (driver), S6 (atomic-path retirement) |
| `OrchestratorFactory.reservations` (REPAIR-2 F3) | `aura/src/orchestration/factory.rs` | Shared reservation-table injection into the initial factory's configuration (`with_reservation_table`): the initial park-enabled producer reserves its persistence-bound run id through the shared table immediately after orchestrator construction | L4, E4 |
| `ParkGuardMode` + `new_with_execution_scope` (REPAIR-2 F3; REPAIR-3 G2) | `aura/src/orchestration/park/guard.rs` | The resumed `ParkGuard` is checkpoint-preserving BY CONSTRUCTION and now carries its execution scope: the injection constructor takes MODE AND SCOPE together (a resumed guard cannot be assembled without the scope its deferred sweep and tracked tails spawn through — the same one `Arc` the supervisor holds). The unscoped `new` stays for the interim initial path the L3 fill rewires; `new_resumed` was replaced by the injection constructor | L3 |
| `ToolCallContext::execution_scope` (+`with_execution_scope`) | `aura/src/tool_wrapper.rs` | The scope is optional by type: non-park calls keep unscoped behavior; no call site is injected yet | L2 |
| `RetentionError` + `RetentionExpiresAt::from_publication` (REPAIR-2 F8) | `park/retention.rs` | Stamping is fully checked: `Duration::try_seconds` + `checked_add_signed` behind a typed `Result` (`AgeOutOfRange` / `DeadlineOverflow`) — the `chrono::Duration::seconds` panic above i64::MAX/1000s is unreachable, and an unrepresentable deadline is refused, never wrapped or saturated | E5 |
| `HitlRuntime.park_ttl` + `ParkCommitInputs.park_ttl` (REPAIR-2 F8) | `aura/src/hitl/route.rs`, `park/commit.rs` | The validated retention age is projected from the parsed config into the runtime HITL state and the publication inputs: E5's stamp reads a validated `ParkTtl`, never a raw window. RESOLVED 2026-09-18 (E5, `079ffa53`): the stamp is the publication transaction timestamp plus this age via `from_publication`; the interim bridge and the `decision_window` input are retired | E5 |
| `ParkedRun.retention_expires_at` | `park/document.rs` (REPAIR-1) | The checkpoint document carries the absolute retention deadline as a typed field (serde: RFC 3339 instant); the run-wide decision `expires_at` string is gone from the format, and a stamp that is not a valid instant cannot decode | E5 (publication-transaction stamp and re-park renewal replace the interim commit bridge) |
| `aura_events::RetentionExpiresAt` (REPAIR-2 F9) | `aura-events/src/retention.rs` | The validated absolute-retention wire stamp, in the dependency location every event surface sees: an RFC 3339 instant by construction — `"not-an-instant"` cannot ride an event or cross decode. Wire key and RFC 3339 rendering preserved | SSE fills consume it as-is |
| `RunParked.retention_expires_at` | `orchestration/events.rs`, `stream_events.rs`, `aura-events/orchestration.rs` (REPAIR-2 F9) | All three RunParked surfaces carry the validated stamp type; the terminal park event names the retention deadline, not a decision expiry, and never an arbitrary string | SSE fills consume it as-is |
| `ResumeConflictRow::expired(Vec<BlockingEntry>)` (REPAIR-2 F11) | `park/resume/evaluate.rs` | The expired row never requires an outstanding call: a retention-expired checkpoint with every approval addressed renders the (possibly empty) expired row instead of being unrepresentable. `parked` keeps `NonEmptyBlocking` | E7 (per-call snapshots) |
| `CheckpointPresence` / `CleanupReservation` / `CleanupAdmissionFault` / cleanup seams (REPAIR-2 F11; REPAIR-3 G6; round-3 residue) | `park/cleanup.rs` (new) | Cleanup classification is typed and three-way-complete: PRESENT vs confirmed absent vs inaccessible vs corrupt (a healthy checkpoint is never mislabeled, a missing root or unreadable document is NEVER confirmed absence, and a corrupt file is never absent evidence); the reservation-owning `CleanupReservation` carrier is born ONLY from `acquire(table, path, memory_dir)`, which occupies the run under the shared table (the admission IS the eligibility proof — never a clone of an executing run's lease; a live run answers `CleanupAdmissionFault::Executing`) and derives the checkpoint paths from the SAME validated identity, so fence and paths cannot name different runs; the carrier clones into the blocking reread/deletion tails so the fence survives the awaiting sweep; deletion is encapsulated evidence-first/checkpoint-last with retain-on-failure for retry. Declared only — nothing is wired into the server | E6 (sweep/orphans), E8 (activation) |
| `CompletionInput` | `aura-web-server/src/handlers.rs` | The completion input is single-use and non-cloneable: a grant that could be duplicated would make the once-only rule a runtime check. Since REPAIR-5 `RequestSetup` owns it; since REPAIR-2 F10 the consumed match produces the common (stream, cancel, usage) tuple in BOTH arms | S2/S3 |
| `RigBuilder::prepare_agent_config` (hole) | `aura/src/rig_builder.rs` | The production config projection for a request is fallible and distinct from the debug-only `get_agent_config`; resume never uses the debug path | S1 |
| `OrchestratorFactory::resume_stream_with_timeout` (hole) | `aura/src/orchestration/factory.rs` | Resume enters through the factory, consuming the grant by value and returning the existing stream/cancel/usage tuple — no replayable adapter | S3 |
| `StreamTermination` + `StreamOutcome` (declared 2026-09-15) | `aura-cli/src/api/stream.rs` | Client stream termination is a five-way distinction (`Done`, `EofWithoutDone`, `Malformed { detail }`, `StreamError { detail }`, `Cancelled`) carried beside the received `StreamResult` (today's accumulation, unchanged). The `detail` strings are diagnostic-only: consumers branch on the variant, never the text. DECLARED ONLY: the active parser's behavior is untouched and no old exit is rewritten as `Done`; no parallel parser/helper API exists beside the types | C2: explicitly an integration/signature-cutover unit that changes the existing `process_stream`/`process_sse_events` signatures and their backend/direct/HTTP/REPL/oneshot consumers; NOT a frozen rust-fill task |

## Visibility and seam table

| Surface | Lives at | Visibility | Reached by |
| --- | --- | --- | --- |
| `ParkTtl`, `RouteTimeoutSecs`, `AdmittedParkRoute`, `ParkRouteAdmission`, `ParkAdmissionError`, `validate_park_admission` | `aura_config::park`; `ParkTtl` also the `[hitl.park].park_ttl` field on `ParkConfig` | `pub`, re-exported from `aura_config` | R1 wires into `Config::validate`; runtime admission (R2) reads the ADT |
| `ApprovalAuthority`, `AddressedApproval`, `ApprovalRead` | `aura::hitl::outcome`; `ApprovalAuthority` also a required field on `ParkedApproval` and `ParkedApprovalRecord` | `pub`, re-exported from `aura::hitl` | `ApprovalStore` trait, record codec (E1), registry seam (E3) |
| `read_or_expire`, `retained_rows` | `ApprovalStore` trait | required trait methods | every backend: memory, file, fault double (cfg(test)), redis (feature `session-store-redis`) |
| `TerminalRecord` | `aura::session_store::record` | `pub(crate)`, explicitly tagged (`kind` + denied unknown fields — REPAIR-3 G3) | file store codec (E1/E2); the addressed-outcome carrier (E2/E7) |
| `RetainedApproval` | `aura::session_store` | `pub` | retained scan consumers (E2/E6/E8) |
| `park_authority` | `DecisionRoute` | `pub` method | gate/bridge (R3), poller filtering (R4) |
| `RunExecutionScope`, `RunReservationLease`, `ReservationFault` | `park::lifetime`; REPAIR-2 F12 re-exports ALL THREE through `aura::orchestration` — `ReservationFault` is the error arm of the public `ResumeClaimTable::reserve`, so a caller can name and match `ReservationFault::Live` through the facade | `pub`, re-exported from `aura::orchestration` | `ToolCallContext` field (public type in a pub field), L fills, E4 |
| `ReservationTable` + `AdmissionFault` (REPAIR-3 G1) | `park::lifetime` | `pub(crate)` (never crosses the facade; the occupied-run set and its lock are private to the module) | `ResumeClaimTable` (composition), the E4 fills (admit/admit_with), factory injection via `ResumeClaimTable` |
| `RetentionExpiresAt` (deadline, `from_publication`, `RetentionError`) | `park::retention` | `pub(crate)`; persisted (serde newtype over `DateTime<Utc>`) on every checkpoint document via `ParkedRun.retention_expires_at`. E5-resolved 2026-09-18 (`079ffa53`): the module `#![allow(dead_code)]` retired with the wired stamp; `as_datetime` is production (the RunParked emit site); `from_datetime` is `#[cfg(test)]` fixture-only since the bridge's retirement | the checkpoint commit stamp (E5) |
| `aura_events::RetentionExpiresAt` | `aura-events::retention`, re-exported from `aura_events` | `pub` | every RunParked event surface (aura internal event, aura SSE mirror, aura-events DTO) |
| `reserve`/`rename_back_under_reservation` | `ResumeClaimTable` | `pub` method / `pub(crate)` hole returning the typed `ClaimResumeFault` (REPAIR-3 G5) | E4 resume ordering |
| `ReservedEvaluation`/`convert_reserved` | `park::resume::evaluate` | `pub(crate)` (declared for E4's in-crate ordered evaluation) | E4 |
| `run_segment_borrowed` | `aura::orchestration` (pub through the resume facade list) | `pub` hole (signature amended 2026-09-15; inputs: event sender; shared `UsageState`; `outer_budget: Option<Duration>`; output: `ResumeStreamEnd`) | S3 factory projection, S4 supervisor |
| `ResumeStreamEnd` | `park::resume::evaluate`, re-exported through `park::resume` and `aura::orchestration` beside `run_segment_borrowed` (every type named in a re-exported signature is re-exported with it) | `pub` (declaration, no hole; the S4 todo returns it) | S4 driver, S6 retirement |
| `OrchestratorFactory::with_reservation_table`/`reservation_table` | `orchestration::factory` | `pub` | L4/E4 wiring |
| `CheckpointPresence`, `CleanupReservation`, `RunCleanupOutcome`, cleanup seams | `park::cleanup` | `pub(crate)`, `#![allow(dead_code)]` until E6/E8 | E6/E8 |
| `CompletionInput` | `aura_web_server::handlers` | `pub` enum | owned by `RequestSetup`; both arms wired (F10): Chat = today's producer path, Resume = the factory's S3 hole |
| `prepare_agent_config` | `RigBuilder` | `pub` | S1/S2 resume handler |
| `resume_stream_with_timeout` | `OrchestratorFactory` | `pub` | S2/S3 completion entry |
| `StreamTermination`, `StreamOutcome` | `aura_cli::api::stream` (`pub mod` chain from the crate root, so no dead-code marker is needed) | `pub` declarations, no hole | C2 cutover of `process_stream`/`process_sse_events` and the backend/direct/HTTP/REPL/oneshot consumers |

## Hole inventory

Every `todo!()` in the skeleton after REPAIR-3, with its owning fill-unit
family per the contract's dispatch table:

| # | Site | Fill unit |
| --- | --- | --- |
| ~~1~~ | ~~`crates/aura-config/src/park.rs` `validate_park_admission`~~ FILLED at commit `79fbf06d` (2026-09-15; held patch released by Mike with exact-message approval; eight committed admission tests green) | R1 |
| ~~2~~ | ~~`crates/aura/src/session_store/memory.rs` `read_or_expire`~~ FILLED at commit `0833493b` (2026-09-17; STORE E2 Fill 1, reviewed RED `bee367af`; authority both paths, idempotent TimedOut re-derivation) | E1/E2 |
| ~~3~~ | ~~`crates/aura/src/session_store/file.rs` `read_or_expire`~~ FILLED at commit `0833493b` (2026-09-17; under the resolve/remove lock, injected clock sampled once, durable tagged TimedOut in resolve's write ceremony, decode-closed on torn decision files) | E2 |
| ~~4~~ | ~~`crates/aura/src/session_store/fault_store.rs` `read_or_expire`~~ FILLED at commit `6e222515` (2026-09-17; general-lane cfg(test) delegation) | E1/E2 |
| 5 | `crates/aura-web-server/src/session_store/redis/approval_store.rs` `read_or_expire` (unsupported park backend: returns the typed unsupported-configuration error) | E-family |
| ~~6~~ | ~~`crates/aura/src/orchestration/park/resume/claim.rs` `ResumeClaimTable::reserve`~~ FILLED at commit `9697d80e` (2026-09-17; DISCLOSED owner takeover as L4a's prerequisite — the initial producer's supervisor needs the generalized admit seam. The 4-line delegation to `ReservationTable::admit` with a RED-first golden (admit/Live/release); E4's remaining scope — admit_with ordering, `convert_reserved`, `rename_back_under_reservation`, empty-resume recovery — is untouched and still E4's) | E4 -> L4a |
| ~~7~~ | ~~`crates/aura/src/orchestration/park/lifetime.rs` `RunExecutionScope::spawn_tracked`~~ FILLED at commit `afe73e14` (2026-09-17; LIFETIME L1, reviewed RED `b3cb819e`; tracker-registered wrapper owns the lease clone through the future's actual completion) | L1 |
| ~~8~~ | ~~`crates/aura/src/orchestration/park/lifetime.rs` `RunExecutionScope::spawn_blocking_tracked`~~ FILLED at commit `afe73e14` (2026-09-17; LIFETIME L1; same registration and lease contract on the blocking pool) | L1 |
| ~~9~~ | ~~`crates/aura/src/orchestration/park/lifetime.rs` `RunExecutionScope::drain`~~ FILLED at commit `afe73e14` (2026-09-17; LIFETIME L1; literal `close()` + `wait().await` join barrier — drain is terminal for the scope, supervisor-use discipline lands with L2-L4) | L1 |
| ~~10~~ | ~~`crates/aura/src/orchestration/park/resume/claim.rs` `rename_back_under_reservation`~~ FILLED at commit `2c5713f9` (2026-09-18; RESERVATION E4-F, reviewed RED `105535a6`; lease-fenced blocking tail, the safe ENOENT semantics, Unavailable/Internal classification — wired as the empty-resume recovery by E4-I `22347993`) | E4 |
| ~~11~~ | ~~`crates/aura/src/orchestration/park/resume/evaluate.rs` `convert_reserved`~~ FILLED at commit `2c5713f9` (2026-09-18; RESERVATION E4-F; carrier-consuming conversion, no-ENOENT source, the grant owns the same reservation and its ONE execution scope — wired as the ordered entry's step 6 by E4-I `22347993`, which also retired the interim `claim_and_resume`/`authorize`/`admit`/`check_claim` pipeline and the unfenced `rename_back_to_parked`) | E4 |
| 12 | `crates/aura/src/orchestration/park/resume/evaluate.rs` `run_segment_borrowed` (REPAIR-2 F3; 2026-09-15 alignment amendment recut its SIGNATURE; inputs: event sender; shared `UsageState`; `outer_budget`; output: `ResumeStreamEnd`; the one `todo!()` body is unchanged) | S3/S4 |
| ~~13~~ | ~~`crates/aura/src/session_store/memory.rs` `retained_rows`~~ FILLED at commit `0833493b` (2026-09-17; typed unsupported-operation answer — no park parity) | E1/E2 |
| ~~14~~ | ~~`crates/aura/src/session_store/file.rs` `retained_rows`~~ FILLED at commit `0833493b` (2026-09-17; side-effect-free both-directory scan, decision-file-wins classification) | E2 |
| ~~15~~ | ~~`crates/aura/src/session_store/fault_store.rs` `retained_rows`~~ FILLED at commit `6e222515` (2026-09-17; general-lane cfg(test) delegation) | E1/E2 |
| 16 | `crates/aura-web-server/src/session_store/redis/approval_store.rs` `retained_rows` (REPAIR-2 F11: unsupported-operation answer) | E-family |
| 17 | `crates/aura/src/orchestration/park/cleanup.rs` `inspect_checkpoint_presence` (REPAIR-2 F11; REPAIR-3 G6: renamed from `confirm_checkpoint_absence`, gains `Present`, takes the `CleanupReservation` carrier) | E6 |
| 18 | `crates/aura/src/orchestration/park/cleanup.rs` `delete_expired_run` (REPAIR-2 F11; REPAIR-3 G6: takes the `CleanupReservation` carrier — the blocking tail retains the lease) | E6 |
| 19 | `crates/aura/src/orchestration/park/cleanup.rs` `scan_checkpoint_root` (REPAIR-2 F11) | E6 |
| 20 | `crates/aura/src/rig_builder.rs` `RigBuilder::prepare_agent_config` | S1 |
| 21 | `crates/aura/src/orchestration/factory.rs` `OrchestratorFactory::resume_stream_with_timeout` | S3 |
| 22 | `crates/aura-web-server/src/handlers.rs` `build_completion_config` Resume arm (REPAIR-1/R-5; provider/model, otel query, and message count from the resumed run's factory) | S2/S3 |

Count after the 2026-09-15 alignment amendments: 22 at the committed
alignment baseline; R1's fill (commit `79fbf06d`, same day) removes row 1,
leaving 21 alignment holes plus the separately excluded Redis
`mark_acknowledged` todo. STORE E2 (2026-09-17) fills rows 2/3/4/13/14/15
(`0833493b`, `6e222515`), leaving **15 alignment rows** — of which rows
5/16 are the EXCLUDED Redis holes (unsupported park backend; delete with
the follow-up Redis-removal PR, no fill work owed) and row 6 belongs to
E4. The amendments
declare types (`ResumeStreamEnd`, `StreamTermination`, `StreamOutcome`) and
recut one existing hole's signature (`run_segment_borrowed`, #12) plus one
private method's signature (`WebhookClient::poll_decision`, not a hole); no
new `todo!()` body was added, so no new row is owed. LIFETIME L1 (2026-09-17)
fills rows 7/8/9, leaving **12 alignment rows** — of which rows 5/16 are the
EXCLUDED Redis holes (unsupported park backend; delete with the follow-up
Redis-removal PR, no fill work owed), rows 6/10/11 belong to E4, rows 17-19
to E6, 20 to S1, 21 to S3, 22 to S2/S3, and 12 to S3/S4. RESERVATION E4
(2026-09-18) fills rows 10/11 (`2c5713f9`, wired by the E4-I ordered-entry
integration `22347993`) and row 6 had already filled at `9697d80e` (the
disclosed L4a takeover), leaving **9 alignment rows** — of which rows 5/16
are the EXCLUDED Redis holes, 12 belongs to S3/S4, rows 17-19 to E6, 20 to
S1, 21 to S3, and 22 to S2/S3. E4's open scope is complete: the ordered
entry runs on the shared reservation surface with empty-resume recovery
fenced by it; E7's production consult cutover closed 2026-09-18
(`6f14da4f`) and E5's publication-transaction retention stamp closed the
same day (`079ffa53`) — the 9 rows stand with no E-family owner left open
until CLEANUP (E6/E8, after SSE).

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
`RunExecutionScope::cancel` (token delegation), the REPAIR-3 G1 table seams
(`ReservationTable::{admit, admit_with, is_live, under_standard_lock}` —
check-and-insert, insert-plus-step-with-rollback, membership, and lock-scoped
execution; `RunReservationLease::armed` is module-PRIVATE, reachable only
from the admit paths), `TerminalRecord::into_decision_record` (arm
projection), `ResumeGrant::execution_scope` (clone of the one scope `Arc`),
`ParkGuard::new_with_execution_scope` (mode+scope constructor; nothing
constructs it until L3). The pre-existing redis `mark_acknowledged` todo
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
   `RetentionExpiresAt::from_datetime`; E5 replaced that bridge with the
   publication-transaction stamp plus `park_ttl` (2026-09-18, `079ffa53`).
   Ripples: the golden
   fixture `testdata/park/parked_run_v1.json` (key renamed; value
   normalized to chrono's canonical `"2026-09-02T15:03:11Z"` — the
   round-trip assertion is unchanged), the `orchestrator.rs` frame
   `park_run_with_every_call_decided_stamps_the_decision_window`
   (renamed `..._stamps_publication_plus_park_ttl` by E5, `079ffa53`), the
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

- **F1 EXECUTED** (REPAIR-3 G1 completed the enforcement claim) —
  `RunReservationLease` is an opaque `Clone` wrapper over
  `Arc<ReservationInner>`; private fields; synchronous release on the
  inner's final drop (implemented — the same release the retired
  `ResumeLease` performed, relocated so the final drop of one reservation
  IS the fence). The crate-visible `admitted` constructor is GONE:
  `ReservationTable` owns the occupied-run set, its lock, and the only
  lease-construction path (`admit` / `admit_with` with rollback), so no
  crate code can assemble a lease for a run it never admitted.
  `ResumeLease` is deleted.
- **F2 EXECUTED** — `ReservedEvaluation` carrier + consuming
  `convert_reserved` + fenced `rename_back_under_reservation` declared
  (E4 holes); `ResumeGrant` owns the reservation; `claim_and_resume` is
  re-typed to hand out the reservation lease (behavior-identical interim:
  the ordered reordering of `evaluate_resume` itself is E4's body work —
  the ruled ordering is now expressible without an ownerless gap).
- **F3 EXECUTED** (REPAIR-3 G2 completed the scope composition) — tracked
  spawn helpers + `cancel`/`drain` on the scope (naked tracker accessor
  removed); factory table injection (`with_reservation_table`); grant-to-
  scope (`execution_scope` — now a clone of the grant's ONE scope `Arc`
  established at conversion); borrowed-grant driver (`run_segment_borrowed`);
  `ParkGuard` resumed/preserving construction (`ParkGuardMode`, and since
  REPAIR-3 the `new_with_execution_scope` injection constructor replacing
  `new_resumed` — mode and scope together). L1/L4/S4 seams only.
- **F4 EXECUTED** — `AdmittedParkRoute` (private fields, admission-only
  construction); `PollOrchestration` carries it; the outer route enum
  stays.
- **F5 EXECUTED** — `expected_authority: ApprovalAuthority` on
  `ApprovalStore::resolve`, the registry `resolve`, and all four backend
  surfaces; every caller passes its channel (local ingress/resolver
  `Conversational`, poller `WebhookPoll`). The check itself is E1/E2's
  fill; no caller can any longer bolt a validate-then-resolve race.
- **F6 EXECUTED** (REPAIR-3 G3/G4 closed the codec and carrier residuals) —
  `TerminalRecord` is EXPLICITLY tagged (`kind` + denied unknown fields, so
  contradictory payloads fail decode; the decided arm carries the shipped
  decision fields; unshipped format — no compatibility fallback) with
  fallible conversions to `ResolvedDecision` and `AddressedApproval`;
  file-store clock injection (`open_with_clock`, default wall clock);
  registry `read_or_expire` forward; the addressed-outcome carrier now
  REACHES recorded-call consumption: `RecordedDecisions`'s queue, `push`,
  `take`, and `peek_at` carry `AddressedApproval`, and `recorded_pre_call`
  maps `TimedOut` through the exact shared `TerminalGateDecision::TimedOut`
  feedback.
  **E7 instruction (corrected by REPAIR-3 G4):** timeout ADDRESSES an
  individual call — the consult's per-member read-or-expire returns
  `Addressed` with `TimedOut { deadline }`, the recorded set carries it,
  and its consumption feeds the exact shared
  `TerminalGateDecision::TimedOut` feedback ("tool call denied: approval
  timed out"). It is NOT the checkpoint-retention expired row and never a
  fabricated denial; a run resumes ready once every bundle member is
  addressed, whatever the mix of decisions and timeouts.
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
- **F11 EXECUTED** (REPAIR-3 G6 closed the cleanup residuals) — `expired`
  no longer requires `NonEmptyBlocking`; cleanup seams declared
  (`CheckpointPresence` with a `Present` arm — the type renamed for its
  three-way-complete meaning — `CleanupReservation`, `RunCleanupOutcome`,
  `inspect_checkpoint_presence`, `delete_expired_run`,
  `scan_checkpoint_root`) plus the store-trait retained-row scan
  (`retained_rows`, four backend holes). Nothing is activated.
- **F12 EXECUTED** — `ReservationFault` re-exported through
  `aura::orchestration` alongside the reservation API; this table
  corrected (it lives in `park::lifetime`, not `resume::claim`, and is
  publicly reachable).
- **F14 RECORDED** — residual row below; no validated constructor added
  (see residual risks).
- **F15 EXECUTED** — contract pointer fixed (top of this file).

## Round-2 repairs: owner-ruled EXECUTED (REPAIR-3, 2026-09-14)

Round-2 verification (frontier-reviewer, panel/ROUND-2.md) found 7 BLOCKING
+ 1 MINOR over 2be4f7c7; all eight closed as surface:

- **G1 EXECUTED** — admission and lease construction are one seam:
  `ReservationTable` (lifetime.rs) owns the occupied-run set and lock;
  `admit`/`admit_with` are the only lease-birth paths; the crate-visible
  `RunReservationLease::admitted` is deleted (its replacement, `armed`, is
  module-private). `claim_and_resume` now routes through `admit_with`
  (same one-lock insert-rename-rollback semantics) and the interim
  `rename_back_to_parked` serializes through `under_standard_lock`.
- **G2 EXECUTED** — one shared resumed scope: `ResumeGrant` owns a single
  `Arc<RunExecutionScope>` established at conversion (`authorize`
  establishes it in the interim path); `execution_scope()` clones that
  same Arc; `ParkGuard` carries the scope and gains the
  `new_with_execution_scope` injection constructor (mode + scope
  together). Activation stays with L3.
- **G3 EXECUTED** — `TerminalRecord` is explicitly tagged
  (`kind`/snake_case) with `deny_unknown_fields`: decided fields alongside
  a `deadline` fail decode; no untagged fallthrough, no compatibility
  fallback (unshipped format). External approve/deny wire untouched.
- **G4 EXECUTED** — the addressed-outcome carrier reaches recorded-call
  consumption: `RecordedDecisions`'s queue, `push`, `take`, and `peek_at`
  carry `AddressedApproval` (interim consult records decisions only until
  the E7 cutover); `recorded_pre_call`'s `TimedOut` arm delegates to the
  existing exact shared `TerminalGateDecision::TimedOut` mapping. E7
  instruction corrected above (per-call timeout, never the expired row).
- **G5 EXECUTED** — `ClaimResumeFault::{Unavailable, Internal}` classify
  the claim seams' failures (rename → availability; blocking-task join →
  internal); the `From<ClaimResumeFault> for ResumeRefusal` mapping feeds
  the typed refusal rows; `rename_back_under_reservation` re-typed onto
  the same vocabulary. Interim `Fault` sink sites untouched.
- **G6 EXECUTED** — `CheckpointAbsence` renamed to `CheckpointPresence`
  with a `Present` arm; the `CleanupReservation` carrier (lease + both
  checkpoint paths, `Clone` for the blocking tails) is the input to
  `inspect_checkpoint_presence` (renamed from
  `confirm_checkpoint_absence`) and `delete_expired_run`. Evidence-first/
  checkpoint-last preserved; activation deferred (E6/E8).
- **G7 EXECUTED** — `aura-events` declares chrono features
  `["serde", "alloc"]` (still `default-features = false`, no clock); the
  isolated `cargo check -p aura-events --all-targets
  --no-default-features` passes and joins the verify list.
- **G8 RECORDED** — owner ruling: the two `aura-events` wire-pin tests are
  RETAINED as a narrow authorized exception (below).

## Residual risks

- The commit's retention stamp was the interim bridge (earliest surviving
  ticket expiry, else `now + decision_window`, wrapped via `from_datetime`)
  until E5 landed the publication-transaction stamp plus `park_ttl`
  (2026-09-18, `079ffa53` over the reviewed RED `545c5ce9`): `from_publication`
  is wired, the retention module's `#![allow(dead_code)]` is gone, and
  `decision_window` is retired from `ParkCommitInputs` at all three
  construction sites. RESOLVED.
- The blocking projection and the consult stamped every 409 entry with the
  document's single deadline and consulted one run-wide window; E7's
  per-call snapshot replacement RESOLVED that 2026-09-18 (`6f14da4f`).
  `RefreshedAwaiting.expires_at` (the earliest per-call deadline) survived
  E7 for the retention derivation and was DELETED with it by E5
  2026-09-18 (`079ffa53`) — the retention stamp no longer derives from any
  ticket deadline.
- **F14 residual (owner-ruled):** the public
  `ApprovalRead::Addressed { approval, outcome }` variant permits an
  approval whose row deadline is A paired with `TimedOut { deadline: B }`.
  Consistency is a STORE-construction obligation (the E2 fill stamps the
  row's own deadline when it publishes the terminal record), not a type
  guarantee — a validated constructor was considered and NOT added:
  enum-variant fields are public, so a constructor adds no enforcement
  while implying one. Consumers of the addressed arm must not treat the
  two deadlines as independently trustworthy.
- ~~`evaluate_resume` still enters through the one-shot `claim_and_resume`
  acquisition~~ RESOLVED 2026-09-18 by E4-I (`22347993`): the ordered entry
  runs `reserve` → the one authoritative re-read/recheck → consult →
  `convert_reserved`, the fenced rename-back seam is wired as the
  empty-resume recovery, and the unfenced pre-reservation
  `rename_back_to_parked` is retired with the one-shot acquisition. Residual
  carried forward: the conversion tail inside `convert_reserved` holds its
  lease clone through the same binding shape as the rename-back tail, but
  has no rendezvous golden of its own (a cross-module cfg(test) seam was
  ruled not worth the surface); S3 owns it with the supervisor rework.
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
- **Wire-pin tests RETAINED (G8 owner ruling, 2026-09-14):** the two small
  wire-pin tests in `aura-events/src/retention.rs` are a narrow authorized
  exception — they ground the RFC 3339 wire form of a brand-new public
  type, the same class as the practice's calibration tests, and they
  replace no whole-frame coverage.
- The evaluate-stage intermediate faults (`LocateFault`, `AdmitFault`,
  `FingerprintFault`, `ConsultFault`) still map their diagnostic arms onto
  the interim `Fault` sink; REPAIR-3 G5 classified the CLAIM seams only
  (the reservation transitions round-2 named). The S2/C fills migrate the
  remaining sites, per the F7 disposition.

## Round-3 board-owner residue (2026-09-14)

The round-3 verification seat (three-round cap) confirmed G1-G5 and G7-G8
closed and returned one blocking residue on G6 plus one documentation
minor. Per the board's capped-rounds precedent the mechanical residue was
completed board-owner-executed (logged on the card):

- `CleanupReservation::new` (unchecked lease+paths pairing) is deleted.
  The carrier is born only from `acquire(table, path, memory_dir)`:
  table admission is the eligibility proof (a live run answers
  `CleanupAdmissionFault::Executing`), and the checkpoint paths derive
  from the same validated identity the admission occupied.
- The header's identity claim is scoped: two narrow ACTIVE classification
  changes ride the surface (interim claim-rename failure renders 503
  `reify_unavailable`; blocking-task join failure renders 500
  `reify_failed`). These are observable client-visible classification
  changes awaiting Mike's keep/defer ruling at U(surface); the earlier
  "not new behavior" wording here was misleading and is corrected
  alongside the header (2026-09-15). Recorded for Mike's visibility at
  U(surface).

## Alignment amendments (2026-09-15, surface-only; U(surface) still open)

Mike approved alignment/code surface amendments only; no behavior
activation. Four changes, all declaration/signature; the atomic driver and
its callers are entirely unchanged until S4/S6:

1. **`ResumeStreamEnd` + `run_segment_borrowed` recut** (owning units
   S3/S4, with S6 retiring the atomic path). The enum is exactly
   `Completed { final_answer: String } | Reparked`: `Completed` hands the
   final answer to the normal factory finalization (the driver emits no
   duplicate final) and `Reparked` reports a fresh park: the publication
   owner emits `RunParked` once, and the supervisor still applies the
   normal terminal stream policy. The hole's signature now takes, beside
   grant/config/headers: the existing event sender
   (`mpsc::Sender<Result<StreamItem, StreamError>>`); the caller's shared
   `UsageState`; `outer_budget: Option<Duration>`. It returns
   `Result<ResumeStreamEnd, SegmentError>`. The factory projects
   `outer_budget` from its timeout by the normal
   `(!timeout.is_zero()).then_some(timeout)` convention and the resumed
   coordinator receives it; cancellation is `grant.execution_scope()`,
   the sole cancellation path, and there is deliberately no second token
   parameter. Normal configured visibility/buffering is preserved: not
   every raw worker item must stream. Re-exported through `park::resume`
   and `aura::orchestration` beside the driver (facade convention: every
   type named in a re-exported signature travels with it). The `todo!()`
   stands.
2. **`StreamTermination` + `StreamOutcome` declared** in
   `aura-cli/src/api/stream.rs` (owning unit C2). C2 is explicitly an
   integration/signature-cutover unit: it changes the existing
   `process_stream`/`process_sse_events` and their
   backend/direct/HTTP/REPL/oneshot consumers, NOT a frozen rust-fill
   task; declaring the types early only fixes the vocabulary. The active
   parser is untouched, no old exit is rewritten as `Done`, `detail`
   text is diagnostic-only, no `non_exhaustive` (no real need), and no
   parallel parser/helper API was introduced. The types sit behind the
   crate's `pub mod` chain, so no dead-code marker was needed.
3. **`WebhookClient::poll_decision` gains `_row_headers:
   Option<&HeaderMap>`** (owning unit H1 for the overlay; H2 for the
   poller's row wiring). Intentionally ignored pending H1: H1/H2 later
   use the existing header machinery (`notify`'s per-name overlay) and
   the row's actual headers. Every caller passes `None` today: the
   poller's tick read plus 13 in-file route.rs test call sites; those
   test edits are mechanical signature conformance only, with assertions
   unchanged. No overlay behavior was added ahead of U(surface).
4. **Classification wording corrected** in this file's header and the
   round-3 residue: the two ACTIVE classification changes are described
   as observable and awaiting Mike's ruling at U(surface), replacing the
   misleading "not new behavior" claim. No historical ledger content
   unrelated to that claim was rewritten.

Pending activation: every amendment above is inert until its owning fill
unit lands behind U(surface). The atomic `run_segment` path, the
orchestrator's atomic driver, the CLI parser loop, and the poller's
behavior are byte-for-byte their pre-amendment selves apart from the
declared names and the `None` arguments.
