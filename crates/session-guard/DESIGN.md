# session-guard - design record (Layer 1 skeleton, rev 2)

Claim-based turn admission for multi-instance AURA: at most one service
instance runs a turn for a session at a time, with session memory on a
shared Archil disk. Skeleton commit = type surface only; every `todo!()`
is a tracked hole (inventory at the bottom). Rev 2 folds the two-seat
panel findings (panel ledger below).

## Admission protocol in one paragraph

One well-known claim file per session (`{root}/{session}/CLAIM`) is the
election: fresh admission is a single atomic `O_EXCL` create, so two
instances can never both win admission. The claim body carries the
holder, turn, generation, and a heartbeat sequence. A release renames the
claim to a unique tombstone (`{gen}.TOMBSTONE`) and unlinks the
tombstone. A steal replaces the body (tmp + rename) only after
`StalenessEvidence` built from two validated observations of the same
claim, at least the staleness window apart, revalidated against a fresh
read at steal time.

## Type-to-business-rule map (every public type)

| Type | One business rule | Invalid state it forbids |
|---|---|---|
| `SessionId` | Exactly 1..=128 bytes of ASCII `[A-Za-z0-9._-]`, not `.`/`..`/`latest` | Empty id resolving to the claim root; traversal; symlink collision |
| `TurnId` | A turn is a time-ordered UUIDv7 fixed at ingress | Claim bodies that cannot name the claiming turn |
| `InstanceId` | The claim holder is a named service instance (1..=64 bytes, same charset); deployment-neutral (`AURA_INSTANCE_ID`, hostname, pod, VM) | Anonymous claims; a k8s-only assumption in the types |
| `Generation` | Each admission/steal is the next generation | Rewriting history; ambiguous release targets |
| `HeartbeatSeq` | Liveness is a monotonic counter, never wall clock | Clock-skew faking or masking liveness |
| `claim_path` / `tombstone_path` | The election is one fixed name; releases target generation-unique tombstones | Two instances both winning admission; deleting another generation's live claim |
| `ObservedClaim` | Every judgment about a claim comes from one complete validated read (wire is private, parsed through `from_wire`) | Pairing raw samples with remembered metadata; unvalidated wire fields |
| `StalenessEvidence` (crate-internal) | A steal needs two same-claim observations, non-advancing heartbeat, window-separated; no public constructor exists | Steal on session-id alone; evidence about a different claim incarnation |
| `EvidenceError` / `WireError` | Diagnostics only; nothing branches on their payloads | Domain logic on raw text |
| `SessionArbiter` / `ArbiterGuard` | Same-instance same-session requests serialize in-process before any disk access; the guard lives inside `HeldLock` so it spans the turn | Two tasks on one instance both reaching the claim store |
| `HeldLock` | A held claim is one `OwnedClaim` identity plus its arbiter slot, lease, and bound release; assembled only by the adapter (`HeldLock::new` is crate-internal) | Lease/release/claim generation disagreement; unattributable holds |
| `HeartbeatLease` | Liveness ends structurally: stop/drop revokes first, the actor is aborted (never detached), a closed channel reads Lost | Orphaned heartbeat tasks; Live-after-death capabilities |
| `WriteCapability` | Writes are generation-bound and fail closed once the revocation trips | Writes continuing after a steal or stop |
| `BeatInterval` | The beat is non-zero by construction | `stale_after` degenerating to zero |
| `AdmissionEnv` | Config is validated once; fields private; zero intervals are errors, not clamps | Unvalidated public config |
| `AdmissionMode` / `build_admission` | The single factory picks the backend and shares one arbiter; backend constructors are crate-internal | `LocalAdmission` built for a `lockfile` config; per-backend arbiters |
| `TurnAdmission` | The port: admit + locate_holder; steals are internal (evidence is not externally producible) | Orchestration coupling; callers hand-rolling steals |
| `LocalAdmission` / `ClaimFileAdmission` | Backends differ only in cross-instance mechanics | (documented behavior, no invariant of their own) |
| `IdleRequest` | Admission input is pre-validated identities, turn fixed at ingress | Raw strings at the seam |
| `FencedRun` | A run directory exists as a state only after the seam created it under claim authority | Run dirs outside admission |
| `TurnOutcome` | Every terminal path (success/failure/clarification) commits | Silent no-manifest exits |
| `CommitContext` | The commit step learns outcome, run dir, and claim identity from the barrier | Commit work before/outside the barrier's ordering |
| `CommittingTurn::barrier` | Ordering is commit(ctx) then lease stop then release then authorize; commit semantics are injected | Manifest-after-release; eager commit; warn-and-continue |
| `BarrierError` | Commit failure quarantines; release failure after durable commit carries the authorized response | Losing the payload on a wedge; conflating the two |
| `CommittedResponse<T>` | Terminal-frame authorization bound to session+turn+generation; payload reachable only through envelope-preserving `map` or consuming `into_payload` | Detached, reused, or forged authorization |
| `AdmissionError` | Only `Busy` is LB-retryable contention; `ContentionLost` distinct; `NotProvisioned` operator-facing | Retry storms on non-contention failures |
| `ReleaseError` | `Superseded` is final (the new owner stands), never force-fixed | Force-unwedging by type accident |
| `FenceCause` | Write-path failures are lease-loss or I/O, nothing else | Misclassified EROFS |

## Seam table

| Reach | Visibility | Notes |
|---|---|---|
| `tokio` (sync watch, task, time) | pub dependency | revocation, heartbeat actor |
| std fs/path | std | claim ops live in `ClaimFileAdmission` only |
| no aura crates, no orchestration types | - | consumed via `TurnAdmission` |

Visibility honesty: `pub(crate)` means any module *inside this crate*
can call it, not "only the adapter". The assembly points are
`HeldLock::new`, `ObservedClaim::from_wire`, `StalenessEvidence::from_observations`,
`HeartbeatLease::{static_lease, with_actor}`, and the backend
constructors - all `pub(crate)`. Outside the crate, none are reachable;
Layer-2 compile-fail tests pin that.

## Residual risks (named)

1. **Release/steal race on the claim name**: a release renames CLAIM to
   its tombstone after a generation re-read; a steal landing between the
   re-read and the rename would move the stealer's claim into the old
   holder's tombstone. The stealer's next heartbeat revalidation fails
   (its claim is gone) and its capability revokes; the window is one
   rename wide. Phase-1 litmus characterizes it; it is ordering loss,
   not data loss.
2. **Check-then-write race on `assert_live`**: the capability check is
   advisory at the call site; the structural guarantee is that
   heartbeats fail after revocation, so a stolen claim stops being
   renewed and the thief's evidence revalidation bounds the overlap.
3. **Uncached reads**: the archil adapter's reads must bypass client
   cache (invalidate-cache or readdir expiry 0); Phase-1 probe, not
   proven.
4. **Abandonment**: `mem::forget` or task kill leaks the claim until
   staleness; the Drop path revokes and aborts what it can reach.
5. **Provisioning** of root/session dirs lives at the deployment seam.

## Panel ledger (Layer 1)

Round 1: frontier seat returned empty (failed delegation, not a pass);
re-dispatched to a different family per routing rules. Codex seat
(gpt-5.6-sol): FAIL, 12 BLOCKING + 3 MINOR. All 15 folded in rev 2:
fixed-name election (C1), OwnedClaim identity + zero-arg release (C2),
observation-bound evidence + steal moved inside admit (C3), structural
revocation + abort-not-detach + closed-channel-is-Lost (C4), abort
transitions on HeldLock/ActiveTurn (C5), lazy context-fed commit +
release-failure carries the response (C6), envelope-preserving
CommittedResponse (C7), private wire + ObservedClaim (C8), validated
config + single factory (C9), min session length (C10), shared arbiter
(C11), this record (C12), immutability docs (C13), prose fix (C14).

## Hole inventory (rev 2 baseline)

`grep -rEn '^\s+todo!\(' src/` returns 16 holes:

- `identity.rs` (3): `SessionId::parse`, `InstanceId::parse`,
  `InstanceId::from_env`
- `claim.rs` (3): `ObservedClaim::from_wire`, `ObservedClaim::to_wire`,
  `StalenessEvidence::from_observations`
- `lib.rs` (2): `AdmissionEnv::from_env`, `build_admission`
- `state.rs` (3): `HeldLock::abort`, `ActiveTurn::abort`,
  `CommittingTurn::barrier`
- `adapters.rs` (5): `LocalAdmission::{admit, locate_holder}`,
  `ClaimFileAdmission::{observe, admit, locate_holder}`

Fill order: identity/config (pure) then LocalAdmission (no fs) then
ClaimFileAdmission observe/admit then barrier/aborts then the steal path
inside admit. Each fill sweeps its `#[expect]` markers and flips its
Layer-2 frames red to green. `clippy.todo` stays `warn` through the fill
phase; the completion gate flips it to `deny`.
