# session-guard - design record (Layer 1 skeleton, rev 3)

Claim-based turn admission for multi-instance AURA: at most one service
instance runs a turn for a session at a time, with session memory on a
shared Archil disk. Skeleton = type surface only; every `todo!()` is a
tracked hole (inventory at the bottom). Rev 3 folds the round-2 panel
findings (ledger below) and records the standing decision that the
mutation-linearity question is settled by the Phase-1 litmus, not by
assertion.

## Admission protocol

One well-known claim file per session (`{root}/{session}/CLAIM`) is the
election: fresh admission is a single atomic `O_EXCL` create, so two
instances can never both win a *fresh* admission. The body carries the
holder, turn, claim incarnation, and a heartbeat sequence. Release
renames the claim to an incarnation-unique tombstone
(`{generation}.TOMBSTONE`) and unlinks it. A steal replaces the body
only after `StalenessEvidence` (two validated observations of the same
claim, unchanged heartbeat, staleness-window-separated) is
*revalidated* against a fresh read, yielding a `ValidatedSteal` token.

**Mutation linearity is an open litmus question.** Rename is atomic;
conditional rename is not. Steal, heartbeat renewal, and release are
check-then-rename sequences with bounded races (residual risks below).
The Phase-1 litmus suite decides whether the archil FUSE surface offers
anything stronger than POSIX; if it does not, the documented fallback
is fencing-token semantics: the claim file coordinates admission
optimistically, and `WriteCapability` is the correctness boundary - a
superseded holder's writes fail closed, so a race costs an errored
turn, never a corrupted session. This is the settled design decision;
the type surface (identity-bound capabilities, `ValidatedSteal`) is
built to serve either outcome.

## Type-to-business-rule map (every public item)

| Item | One business rule | Invalid state it forbids |
|---|---|---|
| `SessionId` | Exactly 1..=128 bytes of ASCII `[A-Za-z0-9._-]`, not `.`/`..`/`latest` | Empty id resolving to the claim root; traversal; symlink collision |
| `TurnId` (+`parse`) | A turn is a time-ordered UUIDv7 fixed at ingress; wire form parses back | Claim bodies that cannot name the claiming turn |
| `InstanceId` (+`parse`, `from_env`) | The claim holder is a named service instance (1..=64 bytes, same charset); explicit `AURA_INSTANCE_ID` fails loud when invalid, fallback only when absent | Anonymous claims; k8s-only assumptions; silent bad config |
| `Generation` | Each admission/steal mints a fresh UUIDv7 incarnation | Reused or wrapped ids; tombstone name collisions across restarts |
| `HeartbeatSeq` | Liveness is a monotonic counter, never wall clock; exhaustion is a typed dead end | Clock-skew fakery; silent wraparound |
| `claim_path` / `tombstone_path` | The election is one fixed name; releases target incarnation-unique tombstones | Two fresh admissions both winning; deleting another incarnation's claim |
| `ObservedClaim` | Every claim judgment comes from one complete validated read (private wire, fallible `from_wire`) | Pairing raw samples with remembered metadata; unvalidated wire fields |
| `StalenessEvidence` (internal) + `ValidatedSteal` | A steal needs two same-claim observations, unchanged heartbeat, window-separated, then revalidation against a fresh read | Steal on session-id alone; stale evidence about a moved claim |
| `EvidenceError` / `WireError` / `HeartbeatExhausted` / `InvalidTurnId` / `InvalidSessionId` / `InvalidInstanceId` | Diagnostics only; nothing branches on their payloads | Domain logic on raw text |
| `SessionArbiter` / `ArbiterGuard` | Same-instance same-session requests serialize before any disk access; guard lives inside `HeldLock` so it spans the turn; `holds()` is the read-only membership query | Two local tasks both reaching the claim store; unanswerable locate_holder |
| `AcquiredClaim` (internal) | Admission output is one sealed bundle: identity plus release bound at construction; the lease derives from the same `lease_source` | Claim/lease/release identity disagreement |
| `HeldLock` | A held claim carries identity, arbiter slot, lease, and bound release, assembled only via `HeldLock::new` from an `AcquiredClaim` | Hand-assembled holds; missing liveness |
| `HeartbeatLease` / `ClaimLeaseSource` | Liveness ends structurally: stop, drop, actor exit (exit guard), and sender drop all revoke first; actor is aborted, never detached | Orphaned heartbeats; Live-after-death capabilities |
| `WriteCapability` | Writes are authorized for one session+turn+incarnation triple and fail closed after revocation (identity-complete, checkable against the write target) | Cross-session capability misuse; writes after a steal |
| `Revocation` (internal) | The lost flag is an atomic; every end path sets it before anything else | Live reads from a dead lease; per-check subscription cost |
| `BeatInterval` | The beat is non-zero by construction | `stale_after` degenerating to zero |
| `AdmissionEnv` | Config is validated once; fields private; zero intervals are errors | Unvalidated public config |
| `AdmissionMode` / `build_admission` | The single factory picks the backend, injects one shared arbiter and one instance identity; backend constructors are crate-internal | `LocalAdmission` for a `lockfile` config; per-backend arbiters; caller-supplied identities |
| `TurnAdmission` | The port: admit + locate_holder; steals are internal | Orchestration coupling; hand-rolled steals |
| `LocalAdmission` / `ClaimFileAdmission` | Backends are the factory's two products; they differ only in cross-instance mechanics; both store the backend-owned instance identity | Caller-named holders; identity disagreement between backends |
| `IdleRequest` | Admission input is a validated session plus the turn fixed at ingress; the acting instance is never caller input | Forged holder identities |
| `FencedRun` | A run directory exists as a state only after the seam created it under claim authority | Run dirs outside admission |
| `TurnOutcome` | Every terminal path (success/failure/clarification) commits | Silent no-manifest exits |
| `CommitContext` | The commit step learns outcome, run dir, and claim identity from the barrier; not constructible outside the crate | Forgeable commit context; out-of-order commit |
| `CommittingTurn::barrier` | Ordering is commit(ctx), then lease stop, then release, then authorize; commit is lazy and context-fed | Eager commit; manifest-after-release; warn-and-continue |
| `CleanupOutcome` / `BarrierError` | Commit failure carries the commit error plus the cleanup outcome; release failure after durable commit carries the authorized response; both variants display | Silently lost cleanup failures; lost payloads on wedges |
| `CommittedResponse<T>` | Terminal-frame authorization bound to session+turn+incarnation; payload reachable only through envelope-preserving `map` or consuming `into_parts` | Detached, reused, or forged authorization |
| `AdmissionError` | Only `Busy` is LB-retryable contention; `ContentionLost` distinct; `NotProvisioned` operator-facing; `Io` covers invalid-wire (InvalidData, source preserved) | Retry storms on non-contention failures |
| `ReleaseError` | `Superseded` is final (the new owner stands), never force-fixed | Force-unwedging by type accident |
| `FenceCause` | Write-path failures are lease-loss or I/O, nothing else | Misclassified EROFS |
| `LeaseState` / `LeaseLost` | Liveness is Live or Lost, and loss names the session and incarnation | Anonymous loss |
| `STALE_FACTOR` / `stale_after` | A claim is stealable after 3 missed beats | Sub-revocation steals |
| `HolderView` | `Here{holder}` (local hold, no disk read) or `Remote{holder, last_heartbeat}` (always an observation); each variant carries exactly what it can prove | Forged locality/heartbeat pairings |

## Seam table

| Reach | Visibility | Notes |
|---|---|---|
| `tokio` (sync watch, task) | pub dependency | revocation channel, actor join |
| std fs/path, atomics | std | claim ops live in `ClaimFileAdmission` only |
| no aura crates, no orchestration types | - | consumed via `TurnAdmission` |

Visibility honesty: `pub(crate)` means any module *inside this crate*
can call it. The internal assembly points are `AcquiredClaim::new`,
`HeldLock::new`, `Generation::mint`, `ObservedClaim::from_wire`,
`StalenessEvidence::{from_observations, revalidate}`, `HeartbeatLease::
{static_from, with_actor}`, `Revocation::new`, backend constructors,
and `release_for`. None are reachable outside the crate; Layer-2
compile-fail tests pin that.

## Residual risks (named)

1. **Mutation races (open litmus)**: steal, heartbeat renewal, and
   release are check-then-rename; two concurrent steals can both
   succeed locally. Boundary until the litmus rules: (a) `ValidatedSteal`
   revalidation narrows the window to one rename; (b) identity-bound
   `WriteCapability` fails the loser's writes closed; (c) incarnation
   minting makes tombstones collision-free. A race costs an errored
   turn, not a corrupted session.
2. **Release-closure binding is unprovable by types**: the release
   closure's captured path is the adapter's contract (one construction
   site: `release_for`); Layer-2 tests pin it.
3. **Check-then-write race on `assert_live`**: advisory at the call
   site; the structural backstop is heartbeat-renewal failure after
   revocation.
4. **Uncached reads**: the archil adapter must bypass client cache
   (invalidate-cache or readdir expiry 0); Phase-1 litmus, not proven.
5. **Ordinary cancellation leaves a claim live-looking until
   staleness** (drop revokes the lease but never runs the async
   release): bounded availability cost, one staleness window;
   documented rather than fixed (a detached cleanup task would trade a
   wedge risk for it).
6. **Abandonment** (`mem::forget`, task kill): claim leaks until
   staleness; same bounded window.

## Panel ledger

Round 1 (codex gpt-5.6-sol, 14 findings; frontier seat returned empty,
rerouted per failed-delegation rule):

| # | Finding | Disposition | Repair |
|---|---|---|---|
| C1 | Unique claim names cannot arbitrate | Accepted | Fixed-name CLAIM, O_EXCL election (r1); steal/revalidate hardened (r2/r3) |
| C2 | Claim/lease/release generations independently assemblable | Accepted | OwnedClaim (r1), then AcquiredClaim + ClaimLeaseSource (r3) |
| C3 | Evidence neither validated nor producible | Accepted | ObservedClaim + from_observations (r1); revalidate -> ValidatedSteal (r3) |
| C4 | Lease end does not revoke capabilities | Accepted | Revocation (r2); exit guard + atomic flag + Drop-revoke (r3) |
| C5 | Error paths discard owned state | Accepted | HeldLock::abort + ActiveTurn::abort (r2) |
| C6 | Barrier signature cannot uphold ordering | Accepted | lazy FnOnce(CommitContext) (r2); CleanupOutcome (r3) |
| C7 | Terminal authorization detachable | Accepted | map/into_parts only (r2) |
| C8 | Public wire type bypasses identities | Accepted | private ClaimWire + ObservedClaim (r2) |
| C9 | Invalid configs constructible | Accepted | private fields + factory (r2) |
| C10 | Empty SessionId allowed | Accepted | 1..=128 (r2) |
| C11 | Arbiter per backend | Accepted | shared via factory (r2) |
| C12 | DESIGN.md accountability incomplete | Accepted | rewritten (r2); per-finding ledger + full map (this rev) |
| C13 | Immutability doc contradicts renewal | Accepted | protocol paragraph (r2) |
| C14 | Prose lint (tricolon) | Accepted | fixed (r2) |

Round 2 (rust-reviewer seat, 11 findings; codex seat, 14 findings):

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|
| R1 | rust | HeldLock::new accepts disagreeing parts | Accepted | AcquiredClaim + lease_source derivation (r3) |
| R2 | rust | WriteCapability not bound to session/turn | Accepted | identity-complete capability (r3) |
| R3 | rust | IdleRequest.instance can contradict backend | Accepted | field removed; backend-owned identity (r3) |
| R4 | rust | open_run/activate/CommitContext forgeable | Accepted partially | CommitContext constructed by barrier only (r3); run-dir binding stays at the seam by design (abort is the cleanup path; recorded) |
| R5 | rust | Superseded generation can produce CommittedResponse | Accepted | lease stop before authorize + identity-bound capability make post-commit steal fail the release; response still travels (durable data) - documented as residual |
| R6 | rust | Generation/heartbeat overflow; gen zero | Accepted | UUIDv7 incarnations + try_next Option (r3) |
| R7 | rust | TurnId has no parser (from_wire unfillable) | Accepted | TurnId::parse (r3) |
| R8 | rust | HolderView contradictory states | Accepted | enum variants (r3) |
| R9 | rust | Backends exported with no rule, root unreachable | Rejected | backends are the factory's named products (documentation value); rule added to map (r3) |
| R10 | rust | NotProvisioned(String) unstructured | Rejected | diagnostic-only payload, same convention as WireError; rule documents it |
| R11 | rust | Cancellation leaves bounded stale claim | Accepted | documented residual risk 5 |
| K1 | codex | Mutation ops not linearizable | Accepted as open | litmus-first decision recorded above; fencing fallback designed in |
| K2 | codex | Actor death leaves Live; per-check subscribe cost | Accepted | ActorExitGuard + atomic flag (r3) |
| K3 | codex | C2 still assemblable | Accepted | as R1 (r3) |
| K4 | codex | Revalidation not representable | Accepted | ValidatedSteal token (r3) |
| K5 | codex | ClaimWire::initial / TurnId::parse seams missing | Accepted | crate-visible initial + parse (r3) |
| K6 | codex | Generation persistence / tombstone reuse | Accepted | UUIDv7 incarnations (r3) |
| K7 | codex | Commit-failure loses cleanup failure | Accepted | CleanupOutcome (r3) |
| K8 | codex | BarrierError has no Display | Accepted | variant displays (r3) |
| K9 | codex | Holder identity/locality dishonest | Accepted | as R3/R8 + arbiter holds() (r3) |
| K10 | codex | InstanceId::from_env fail-soft | Accepted | Result + fallback-only-when-absent (r3) |
| K11 | codex | observe erases invalid-wire | Accepted | ObserveError internal, mapped contract documented (r3) |
| K12 | codex | Ledger not per-finding | Accepted | this table (r3) |
| K13 | codex | Prose lint tricolon | Accepted | rewritten (r3) |
| K14 | codex | Dependency features broader than needed | Deferred | workspace-level feature slimming touches all crates; queued behind the fill phase |

## Hole inventory (rev 3 baseline)

`grep -rEn '^\s+todo!\(' src/` returns 18 holes:

- `identity.rs` (4): `SessionId::parse`, `TurnId::parse`,
  `InstanceId::parse`, `InstanceId::from_env`
- `claim.rs` (4): `Generation::parse`, `ObservedClaim::from_wire`,
  `StalenessEvidence::from_observations`,
  `StalenessEvidence::revalidate`
- `lib.rs` (2): `AdmissionEnv::from_env`, `build_admission`
- `state.rs` (3): `HeldLock::abort`, `ActiveTurn::abort`,
  `CommittingTurn::barrier`
- `adapters.rs` (5): `LocalAdmission::{admit, locate_holder}`,
  `ClaimFileAdmission::{observe, release_for, admit, locate_holder}`

Fill units, each with its exit criterion (markers swept, inventory
accounted, its Layer-2 frames green):

1. identities + config (pure; unit tests on parse rules)
2. wire + observation (`from_wire`/`to_wire`/`Generation::parse`)
3. evidence pair + revalidate (window/binding rules)
4. `LocalAdmission` (arbiter + static lease + no-op release)
5. `ClaimFileAdmission::observe` + `locate_holder`
6. `release_for` + `HeldLock::abort` (tombstone protocol)
7. `ClaimFileAdmission::admit` (election + steal path + actor)
8. `ActiveTurn::abort` + `barrier` (ordering + cleanup algebra)

`clippy.todo` stays `warn` through the fill phase; the completion gate
flips it to `deny`.
