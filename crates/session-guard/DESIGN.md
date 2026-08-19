# session-guard - design record (Layer 1 skeleton, rev 6)

Claim-based turn admission for multi-instance AURA: at most one service
instance runs a turn for a session at a time, with session memory on a
shared Archil disk. Skeleton = type surface only; every `todo!()` is a
tracked hole (inventory at the bottom). Rev 6 folds the round-5 panel findings
(ledger below); the mutation-linearity question is settled by the
Phase-1 litmus, not by assertion.

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
anything stronger than POSIX; if it does not, the fallback is
fencing-token semantics, stated here with its holes named: (a) a steal
requires 3 observed missed beats, but a *paused* holder (never
scheduled) has experienced no failed renewal, so its local capability
still reads Live until its actor next runs - the litmus must include a
paused-holder-resumes-after-steal probe; (b) once the old holder's
renewal does run and fails, the lease revokes within one beat; (c) turn
*data* is turn-scoped (each turn writes its own run directory, so
overlap damages at most the loser's own in-flight run), but two
session-shared mutations are NOT fenced and must be handled at the aura
seam: the best-effort `latest` symlink, and `prune_session_runs`
(which also deletes sibling run dirs and is already slated to move out
of request init into a GC protocol). Residual window: writes issued
between a steal landing and the loser's next `assert_live` check. This
chain is the *designed* boundary and is unverified until the litmus
runs; the litmus gate treats it as a candidate, not a proof.

## Type-to-business-rule map (every public item)

| Item | One business rule | Invalid state it forbids |
|---|---|---|
| `SessionId` | Exactly 1..=128 bytes of ASCII `[A-Za-z0-9._-]`, not `.`/`..`/`latest` | Empty id resolving to the claim root; traversal; symlink collision |
| `TurnId` (+`parse`) | A turn id is a unique UUID fixed at ingress (locally minted v7; wire accepts any UUID) | Claim bodies that cannot name the claiming turn |
| `InstanceId` (+`parse`, `from_env`) | The claim holder is a named service instance (1..=64 bytes, same charset); explicit `AURA_INSTANCE_ID` fails loud when invalid, fallback only when absent | Anonymous claims; k8s-only assumptions; silent bad config |
| `Generation` | Each admission/steal mints a fresh unique incarnation (locally UUIDv7; the wire accepts any UUID, so the invariant is uniqueness) | Reused or wrapped ids; tombstone name collisions across restarts |
| `HeartbeatSeq` | Liveness is a monotonic counter, never wall clock; construction is crate-internal (`initial`/`try_next`/`new`); exhaustion is a typed dead end | Clock-skew fakery; silent wraparound; forged heartbeats |
| `claim_path` / `tombstone_path` | The election is one fixed name; releases target incarnation-unique tombstones | Two fresh admissions both winning; deleting another incarnation's claim |
| `ObservedClaim` | Every claim judgment comes from one complete validated read (private wire, fallible `from_wire`) | Pairing raw samples with remembered metadata; unvalidated wire fields |
| `StalenessEvidence` (internal) + `ValidatedSteal` | A steal needs two same-claim observations, unchanged heartbeat, window-separated, then revalidation against a strictly later observation (id-ordered, non-wrapping); the token (private fields, accessor-read) carries the session and the superseded incarnation | Steal on session-id alone; stale evidence about a moved claim; a cloned sample masquerading as fresh; a forged token |
| `EvidenceError` / `WireError` / `HeartbeatExhausted` / `InvalidTurnId` / `InvalidSessionId` / `InvalidInstanceId` / `AdmissionConfigError` | Diagnostics only; nothing branches on their payloads | Domain logic on raw text |
| `SessionArbiter` / `PendingGuard` / `HeldGuard` (crate-internal) | Same-instance same-session requests serialize before any disk access; a slot is Admitting until the election resolves, Held after confirm (confirm is crate-internal, so no external caller can manufacture a Held slot without an adapter-driven election); `holds()` reports Held only; the held guard lives inside `HeldLock` so it spans the turn | Two local tasks both reaching the claim store; an admission attempt mistaken for a holder; forged local holds |
| `AcquiredClaim` (internal) | Admission output is one sealed bundle consumed by `into_held_local`/`into_held_with_actor`, which build the lease from the same identity internally | Claim/lease/release identity disagreement |
| `HeldLock` | A held claim carries identity, arbiter slot, lease, and bound release, assembled only via private `from_parts` from an `AcquiredClaim` (whose `into_held_*` constructors build the lease) | Hand-assembled holds; missing liveness |
| `HeartbeatLease` / `ClaimLeaseSource` / `Liveness` / `Revocation` | Liveness ends structurally (atomic + Notify; only the lease and the actor exit guard hold revocation authority); the lease OWNS the heartbeat loop (wake-and-join cooperative shutdown, so an in-flight renewal completes before release - abort would not guarantee this, as tokio fs writes ride unabortable `spawn_blocking`); the first failed renewal revokes; capabilities merely observe (clone/drop-safe) | Orphaned heartbeats; Live-after-death capabilities; a dropped capability killing a live lease; a renewal landing after release begins |
| `WriteCapability` | Writes are authorized for one session+turn+incarnation triple and fail closed after revocation (identity-complete, checkable against the write target) | Cross-session capability misuse; writes after a steal |
| `Revocation` (internal) | The lost flag is an atomic; every end path sets it before anything else | Live reads from a dead lease; per-check subscription cost |
| `BeatInterval` | The beat is non-zero by construction | `stale_after` degenerating to zero |
| `AdmissionEnv` | Config is validated once; fields private; zero intervals are errors | Unvalidated public config |
| `AdmissionMode` / `build_admission` | The single factory picks the backend, injects one shared arbiter and one instance identity; backend constructors are crate-internal; the docs state the build-once-per-server requirement (a second call creates an independent arbiter, voiding `off`-mode same-process exclusion) | `LocalAdmission` for a `lockfile` config; per-backend arbiters; caller-supplied identities |
| `TurnAdmission` | The port: admit + locate_holder; steals are internal | Orchestration coupling; hand-rolled steals |
| `LocalAdmission` / `ClaimFileAdmission` | Backends are the factory's two products; they differ only in cross-instance mechanics; both store the backend-owned instance identity | Caller-named holders; identity disagreement between backends |
| `IdleRequest` | Admission input is a validated session plus the turn fixed at ingress; the acting instance is never caller input | Forged holder identities |
| `FencedRun` | A run directory enters the state only via `HeldLock::create_run`, which asserts THIS lock's capability and creates the directory in one step (non-recursive; `AlreadyExists` is a hard error) - so the directory can never be authorized by a different claim | Run dirs outside admission; cross-claim directory swaps |
| `TurnOutcome` | Every terminal path (success/failure/clarification) commits | Silent no-manifest exits |
| `CommitContext` | The commit step learns outcome, run dir, and claim identity from the barrier; private fields, not constructible outside the crate | Forgeable commit context; out-of-order commit |
| `CommittingTurn::barrier` | Ordering is commit(ctx), then lease stop, then release, then authorize; commit is lazy and context-fed | Eager commit; manifest-after-release; warn-and-continue |
| `CleanupOutcome` / `BarrierError` | Commit failure carries the commit error plus the cleanup outcome; release failure after durable commit carries the authorized response; both variants display | Silently lost cleanup failures; lost payloads on wedges |
| `CommittedResponse<T>` | Terminal-frame authorization bound to session+turn+incarnation; constructor private to the barrier module; payload reachable only through envelope-preserving `map` or consuming `into_parts` | Detached, reused, or forged authorization |
| `AdmissionError` | Only `Busy` is LB-retryable contention; `ContentionLost` distinct; `NotProvisioned` operator-facing; `Io` covers invalid-wire (InvalidData, source preserved) | Retry storms on non-contention failures |
| `ReleaseError` | `Superseded` is final (the new owner stands), never force-fixed | Force-unwedging by type accident |
| `FenceCause` | Write-path failures are lease-loss or I/O, nothing else | Misclassified EROFS |
| `LeaseState` / `LeaseLost` | Liveness is Live or Lost, and loss names the session and incarnation | Anonymous loss |
| `STALE_FACTOR` / `stale_after` | A claim is stealable after 3 missed beats | Sub-revocation steals |
| `HolderView` | Opaque: `here` (local hold, no disk read) or `remote(&ObservedClaim)` (both fields from ONE observation); accessors read, never construct | Forged locality/heartbeat pairings; pairings from different reads |

## Seam table

| Reach | Visibility | Notes |
|---|---|---|
| `tokio` (task, time, sync Notify, fs) | pub dependency | heartbeat loop spawn/join; revocation is a shared atomic + Notify |
| std fs/path, atomics | std | claim ops live in `ClaimFileAdmission` only |
| no aura crates, no orchestration types | - | consumed via `TurnAdmission` |

Visibility honesty: `pub(crate)` means any module *inside this crate*
can call it. The internal assembly points are `AcquiredClaim::new`,
`HeldLock::from_parts` (private), `Generation::mint`, `ObservedClaim::from_wire`,
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
   minting makes tombstones collision-free. Turn-scoped data limits a
   race's blast radius to the loser's own run, but session-shared
   mutations (the `latest` symlink, pruning) remain UNFENCED until the
   aura seam moves them out of request handling - until then a resumed
   loser can repoint `latest` or delete sibling runs, so
   *session-visible corruption remains possible*; the no-corruption
   claim is withdrawn pending both the litmus and the seam work.
2. **Release-closure binding is unprovable by types**: the release
   closure's captured path is the adapter's contract (one construction
   site: `release_for`, which derives paths from the observation);
   Layer-2 tests pin it.
2a. **Run-dir creation is seam-authorized but seam-executed** (R4,
   accepted in part): `RunDir::create_under` checks the capability at
   creation, but the directory the seam *chooses* (naming, parent) is
   the seam's contract; the guard proves authorization, not layout.
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

Model identities (for the author/reviewer invariant): the skeleton
author is OpenCode session model opencode-go/glm-5.3. Seat-2 reviewer
throughout: codex gpt-5.6-sol. Rounds 2 and 3 seat 1 was dispatched to
the `rust-reviewer` pin, which reads `openai/gpt-5.6-sol` - the same
family as the codex seat, a routing collision recorded here (the two seats still ran as independent contexts and converged
on overlapping findings, but the different-family invariant was NOT
satisfied in rounds 2-3). Round 4 seat 1 reroutes to the
`frontier-reviewer` pin (kimi-for-coding/k3) to restore the invariant.

Round 1 (codex gpt-5.6-sol, 14 findings; frontier seat kimi k3 returned
empty, rerouted per failed-delegation rule):

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

Round 3 (rust-reviewer pin = gpt-5.6-sol, 10 findings; codex
gpt-5.6-sol, 11 findings - same family on both seats, see the routing
note above):

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|
| T1 | rust+codex | Dropping any capability clone revokes the lease (Revocation::Drop) | Accepted | Liveness/Revocation split: capabilities observe, only lease + exit guard hold authority (r4) |
| T2 | rust+codex | Lease not type-linked to claim at HeldLock::new | Accepted | into_held_local / into_held_with_actor consuming constructors (r4) |
| T3 | codex | with_actor exit-guard contract unenforceable | Accepted | with_actor spawns via closure receiving the guard; dropped-outside-task fails Lost (r4) |
| T4 | rust | CommittedResponse::new forgeable in-crate | Accepted | private to state module (r4) |
| T5 | rust | CommitContext publicly forgeable | Accepted | private fields + accessors (r4) |
| T6 | rust | FencedRun accepts any PathBuf | Accepted | RunDir::create_under token (r4) |
| T7 | rust | HolderView variant construction forgeable | Accepted | #[non_exhaustive] (r4) |
| T8 | codex | release_for lacks session (cannot derive tombstone path) | Accepted | release_for(&ObservedClaim) (r4) |
| T9 | codex | ValidatedSteal: clone can masquerade as fresh; token lacks session | Accepted | ObservationId ordering + EvidenceError::NotFresh + token carries session (r4) |
| T10 | codex | holds() reports admission attempts as holders | Accepted | PendingGuard::confirm two-phase arbiter (r4) |
| T11 | rust | HeldLock field drop order frees arbiter before lease revokes | Accepted | lease field ordered before arbiter (r4) |
| T12 | codex | "errored turn, never corrupted session" unsupported | Accepted | fencing chain documented as candidate with residual window; litmus gate (r4) |
| T13 | rust+codex | Hole inventory count wrong (19, adapters 6) | Accepted | corrected (r4) |
| T14 | rust+codex | Generation/TurnId UUIDv7 doc vs permissive parse | Accepted | invariant restated as uniqueness; locally minted v7, wire accepts any UUID (r4) |
| T15 | codex | Ledger lacks model identity columns | Accepted | model-identity paragraph added (r4) |
| T16 | codex | root() unreachable dead surface | Accepted | pub(crate) (r4) |
| T17 | rust | AdmissionConfigError missing from map | Accepted | added to diagnostics row (r4) |
| T18 | codex | R4 seam contract absent from residual risks | Accepted | risk 2a added (r4) |

Round 4 (seat 1 frontier-reviewer pin = kimi k3, 7 findings; seat 2
codex gpt-5.6-sol, 9 findings; different-family invariant restored):

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|
| F1 | both | Recorded r4 repairs for open_run/release_for/ValidatedSteal had not landed in code (ledger said they had) | Accepted | landed with asserted patches in r5; process note below |
| F2 | both | open_run still accepts raw PathBuf (RunDir bypassed) | Accepted | open_run(RunDir); FencedRun stores the token (r5) |
| F3 | kimi | enum-level #[non_exhaustive] does not block variant construction | Accepted | HolderView is an opaque struct with crate-internal here/remote constructors (r5) |
| F4 | both | release_for still lacks session | Accepted | release_for(&ObservedClaim) (r5) |
| F5 | both | ValidatedSteal still lacks session | Accepted | token carries {session, superseded} (r5) |
| F6 | codex | with_actor guard/handle association unenforced (dummy handle) | Accepted | lease spawns the wrapper task owning the guard; body receives a Liveness observation (r5) |
| F7 | codex | HeldGuard constructible outside (public confirm) | Accepted | arbiter API crate-internal; exports removed (r5) |
| F8 | codex | fencing chain: paused holder; session-shared mutations | Accepted | chain restated with holes named (latest symlink, prune); paused-holder litmus added (r5) |
| F9 | both | watch channel dead surface | Accepted | channel deleted; atomic flag only (r5) |
| F10 | codex | RunDir::create_under sync I/O on async path | Accepted | async tokio::fs::create_dir (r5) |
| F11 | both | DESIGN.md stale (rev 3 title, HeldLock::new refs) | Accepted | r5 rewrite |
| F12 | kimi | HeartbeatSeq::new public (forged heartbeats) | Accepted | pub(crate) (r5) |

Process note (F1): the r4 fold script applied replacements without
asserting they matched, so three repairs silently missed while the
ledger recorded them as done - caught only by the panel reading code.
Rule going forward: every fold patch asserts its replacements, and the
ledger records repairs only after `grep` verification of the landed
source.

Round 5 (seat 1 frontier-reviewer pin = kimi k3: PASS with 7 minors;
seat 2 codex gpt-5.6-sol: FAIL, 5 BLOCKING + 2 MINOR; different-family
invariant held):

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|
| V1 | codex | risk 1 still claims "errored turn, not corrupted session" while latest/prune are unfenced | Accepted | no-corruption claim withdrawn; corruption possibility stated until seam work lands (r6) |
| V2 | codex+kimi | RunDir not bound to the authorizing lock (cross-claim swap) | Accepted | open_run+RunDir replaced by HeldLock::create_run (asserts this lock's capability, creates, binds in one step) (r6) |
| V3 | codex | ValidatedSteal pub(crate) fields forgeable in-crate | Accepted | private fields + crate-internal accessors (r6) |
| V4 | codex | HolderView::remote pairs holder+heartbeat from possibly different reads | Accepted | remote(&ObservedClaim) derives both from one observation (r6) |
| V5 | codex | stop() aborts the wrapper; spawn_blocking fs writes are unabortable, so a renewal can land after release begins | Accepted | lease-owned heartbeat loop with wake-and-join cooperative shutdown (in-flight renewal completes before stop returns) (r6) |
| V6 | codex+kimi | stale docs (seam table watch row, HeldLock::new refs, SessionArbiter link) | Accepted | r6 rewrite |
| V7 | codex | ObservationId fetch_add wraps | Accepted | checked mint; typed exhaustion (r6) |
| V8 | kimi | dead public exports (ObservedClaim/EvidenceError/HeartbeatExhausted without public producers) | Accepted | dropped from exports (r6) |
| V9 | kimi | HeartbeatSeq::initial/try_next public (forged sequences) | Accepted | pub(crate) (r6) |
| V10 | kimi | LeaseState public without public producer | Accepted | HeldLock::lease_state() added (r6) |
| V11 | kimi | one-arbiter invariant is caller discipline | Accepted | build-once requirement documented on the factory and in the map row (r6) |
| V12 | kimi | fill-phase gate invocation undocumented | Accepted | recorded (r6) |
| V13 | kimi | ValidatedSteal map row overstated ("full superseded identity") | Accepted | reworded to session + superseded incarnation (r6) |

## Hole inventory (rev 6 baseline)

`grep -rEn '^\s+todo!\(' src/` returns 19 holes:

- `identity.rs` (4): `SessionId::parse`, `TurnId::parse`,
  `InstanceId::parse`, `InstanceId::from_env`
- `claim.rs` (4): `Generation::parse`, `ObservedClaim::from_wire`,
  `StalenessEvidence::from_observations`,
  `StalenessEvidence::revalidate`
- `lib.rs` (2): `AdmissionEnv::from_env`, `build_admission`
- `state.rs` (3): `HeldLock::abort`, `ActiveTurn::abort`,
  `CommittingTurn::barrier`
- `adapters.rs` (6): `LocalAdmission::{admit, locate_holder}`,
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
flips it to `deny`. Fill-phase gate invocation (an auditor running the
bare `-D warnings` form sees the tracked holes as errors by design):

```
cargo clippy --workspace --all-targets -- -D warnings -A clippy::todo
```
