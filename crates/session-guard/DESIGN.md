# session-guard - design record (Layer 1 skeleton, rev 12)

Claim-based turn admission for multi-instance AURA: at most one service
instance runs a turn for a session at a time, with session memory on a
shared Archil disk. Skeleton = type surface only; every `todo!()` is a
tracked hole (inventory at the bottom).

**Rev 12 is the Postgres pivot.** Mike ruled 2026-08-20/21: the
claim-file protocol (rev 11 and earlier) is retired — Postgres is the
sole claim authority, no claim-file adapter ships. The design survived a
grill, two replicated rig probe runs, and a codex adversarial gate (6
BLOCKING + 4 MINOR, all ten folded). Rulings from the 2026-08-23 review
session are folded here too (manifest-in-PG, debris-only epoch-scoped
GC, three-way miss handling, two-tier repair lane). The flow-chart
companion is `DESIGN.html` (rendered review packet, untracked pending a
commit decision). Revisions ≤ 11 and their panel ledger live in git
history; the retired design's recorded weaknesses (mutation linearity
across check-then-rename, the staleness-window steal, unfenced
session-shared mutations) are exactly what the Postgres authority
deletes.

## Admission protocol

One row per session in `session_claims`, never deleted. The election is
one statement (S1): `INSERT .. ON CONFLICT (session_id) DO UPDATE ..
WHERE lease_expires_at < clock_timestamp() AND (parked_turn IS NULL OR
parked_turn = $turn)`. Under READ COMMITTED (pinned, I7) a concurrent
claimant blocks on the row lock, then re-evaluates the WHERE against the
winner's updated row and matches zero rows — the first racer after lease
expiry steals atomically, documented Postgres behavior. The lease
deadline is computed inside the locked update via `clock_timestamp()`
(codex B2; `now()` is transaction-start and would reintroduce the bug —
verified against PG 18 docs 2026-08-23, invariant I6).

Zero S1 rows is classified by one follow-up read (S1_CLASSIFY) into
`Busy` (live lease — configured fixed retry hint; codex M7) or `Parked`
(park latch set on a different turn — no retry hint; the HITL wait is
unbounded). The classify read can race a release landing between S1 and
it (the row then reads as claimable again); the claim implementation
retries S1 once in that case rather than inventing a third refusal
shape. A successful claim clears the latch in the same statement.

There is no storage-level fence (probe H5: a thawed zombie's write+fsync
succeeds after a steal). Writes are contained, not intercepted: one
claim = one epoch = one run dir (`{session}/e{k}/`); paths are
write-once (I1); the manifest (JSONB on the row, cumulative) is the only
authority on committed bytes. The manifest rides the granted row, so the
G2 handshake is the claim read itself (Q7: manifest-in-Postgres). Commit
is one atomic UPDATE after the data fsyncs (I4), predicated on the fence
triple and an unexpired lease (B1) — Postgres linearizes what can ever
become committed state (G1-prime), even if a failover regresses the
epoch (I2).

The artifact I/O surface is part of the claim, so I1 and I4 are
structural rather than conventional: `ActiveTurn::write_artifact` is the
only way manifest-bound bytes are written (temp + fsync + vendor-atomic
rename + parent-dir fsync, one write per path per turn, only into the
claiming epoch's dir), and `FencedRun::read_artifact` owns
verify-on-first-read with the three-way miss handling inside (propagation
window → `refresh_dir` → `force_cure` → fail loud). Scratchpad bytes are
exempt: turn-transient, never manifest-bound, and collected as debris
once the epoch passes.

Turn outcomes: Success/Clarification commit (the barrier merges the
turn's delta into the granted base manifest via `extend_from`, then S3
publishes manifest + op id + park latch); Failure aborts (quarantine +
release, no commit — unrepresentable at the type level: the barrier
takes a `CommitKind`, and `Failure` has no `CommitKind`). Park =
commit-then-release with the latch written; reify re-claims the same
`TurnId`; the park *algorithm* belongs to HITL-271 — this crate carries
only the latch, the `Parked` admission variant, and the same-TurnId
reify path. The latch value is derived from the committing turn
(`ParkLatch`), never passed as a free parameter, so a contradictory
latch/turn combination is unrepresentable.

## Type-to-business-rule map (every public item, plus the crate-internal
assembly types)

| Item | One business rule | Invalid state it forbids |
|---|---|---|
| `SessionId` | Exactly 1..=128 bytes of ASCII `[A-Za-z0-9._-]`, not `.`/`..`/`latest` | Empty id resolving to the session root; traversal; symlink collision |
| `TurnId` (+`parse`) | A turn id is a unique UUID fixed at ingress, with two documented wire forms (serde compact for the manifest JSONB; string parse for HTTP/PG text); reify reuses the parked turn's id | Claim rows that cannot name the claiming turn; reify under a fresh id |
| `PodId` (+`parse`, `from_env`) | The holder's pod is a stable RFC-1123 name (1..=253 bytes): `AURA_POD_ID` (fail loud when invalid) → `POD_NAME` → `HOSTNAME` | Anonymous claim rows; a controller unable to map pod-Deleted → row |
| `HolderId` (+`parse`) | Each acquire *attempt* mints a fresh UUIDv7 (I2) — the second fence every mutation predicates on | A stale holder linearizing a commit after a failover epoch regression |
| `OpId` (+`parse`) | One logical commit = one id, minted by the barrier, reused by its retries (B3) | Commit-unknown resolved by blind re-UPDATE |
| `Epoch` | Per-session monotonic fencing token starting at 1; construction crate-internal (`initial`/`next`/`from_raw`); deserialization validates (0 rejected); SQL `bigint` domain noted (`i64::MAX` ceiling, unreachable in practice) | Caller-minted epochs; epoch 0 smuggled in through a corrupted or foreign-written row |
| `session_dir` / `epoch_dir` | The only path derivations: `{root}/{session}` and `{session}/e{k}` | Hand-assembled layout strings at the seam |
| `LeaseDeadline` | A server-clock timestamp (`clock_timestamp()` product); never compared against pod clocks | Cross-clock lease comparisons on the safety path |
| `LeaseTtl` / `SelfFenceMargin` / `BeatInterval` | Non-zero durations by construction; config requires margin < ttl | Degenerate zero leases; a margin that swallows the ttl |
| `SelfFenceDeadline` (internal) | Conservative local expiry: `Unfenced` and `Fenced` are distinct states (a fence that cannot anchor is worse than none); anchored at the S1 transmission instant (`granted_at`) and re-anchored at each beat transmission (`transmit + ttl − margin`, codex M9) | A self-fence that never fires; an unfenced pre-first-beat window; a self-fence that waits on a response |
| `Liveness` / `Revocation` / `ActorExitGuard` (internal) | Unchanged from rev 11: capabilities observe (clone/drop-safe); only the lease and the exit guard hold revocation authority | Live-after-death capabilities; a dropped capability killing a live lease |
| `HeartbeatLease` / `ClaimLeaseSource` | Unchanged machinery, adapted identity: the lease owns the S2 heartbeat loop (anchor-at-transmission + wake-and-join shutdown); the first failed renewal revokes (Lost and PG-down both fail closed, I5) | Orphaned heartbeats; a renewal landing after release begins |
| `WriteCapability` | Writes authorized for one `(session, turn, epoch, holder)` quad, failing closed on revocation *or* self-fence expiry | Cross-claim capability misuse; writes after a steal |
| `Manifest` | The cumulative committed artifact set; transparent serde as the entries map (a fresh row's `'{}'` jsonb *is* the empty manifest); grows through `declare`, and `extend_from` merges a turn's delta into the granted base so commit can never blindly replace history | A manifest that can overwrite an entry; a fresh-claim deserialize failure |
| `ArtifactPath` | Epoch-qualified relative path whose parsed prefix epoch is carried as a field, so provenance is checkable (`declare` enforces the tie); serde deserialization validates | Unvalidated strings in the committed set; path/entry provenance disagreement |
| `Digest` (+`InvalidDigest`, `InvalidArtifactPath`) | 32-byte sha256, hex wire; parse-don't-validate on every ingress path | Unvalidated strings in the committed set |
| `ManifestEntry` | digest + turn + epoch + byte length, all validated types (`bytes` is a plain metric — no rule branches on it) | Provenance separated from content |
| `DeclareError` | The two declaration rules as two variants: `AlreadyDeclared` (I1) and `EpochMismatch` (path/entry tie) | Silent double-declare; misattributed provenance |
| `ReadMiss` | The two read-failure classes stay distinct: `NotFound` (propagation — retry/escalate) vs `Corrupt` (fail loud, never retry) | Corruption retried as propagation |
| `VerifiedRead` / `ReadError` | Verified bytes plus the entry they verified against; constructed only by the read path after the digest check; the error escapes only after window + repair are exhausted | Unverified bytes reaching the seam; retry decisions leaking to callers |
| `ArtifactWriteError` | The write path's four rejections as four variants: `Duplicate`, `WrongEpoch`, `LeaseLost`, `Io` | A duplicate write and a wrong-epoch write collapsing into one undiagnosable error |
| `SessionArbiter` / guards (internal) | Unchanged: same-instance serialization before any store access; Admitting vs Held two-phase | Two local tasks both reaching the store |
| `AcquiredClaim` (internal) | Admission output is one sealed bundle: identity + bound session root + base manifest + store handle + release; `new_local`/`new_pg` fix the lease plan at acquisition and demand the backend's private proof token; `into_held` derives the lease from the same identity | Claim/lease/release identity disagreement; a pg claim taking a static lease; a claim unbound from its session root |
| `GrantedClaim` (internal) | The granted row as one bundle, including `granted_at` (the S1 transmission instant) so the self-fence's initial anchor travels with the claim | A lease built without its M9 anchor |
| `HeldLock` | Held claim = identity + bound session root + base manifest + store + lease + arbiter slot + release + repair lane; `create_run` is argument-free — it derives the epoch dir under the claim's own bound root (no caller-selected layout, no session-A-dir-under-session-B) | Hand-assembled holds; run dirs outside the epoch partition; cross-session root confusion |
| `FencedRun` / `read_artifact` | The G2 base view (`manifest()`) and the verified read path ride the run; the read path owns the three-way miss handling internally | The seam hand-rolling digest checks or repair escalation |
| `ActiveTurn` / `write_artifact` | The only manifest-bound write path: capability asserted, one write per path per turn, own-epoch only, temp+fsync+rename+dir-fsync, digest computed for the returned entry (I1/I4 as structure) | A second write to one path; a write outside the claiming epoch; an unfsynced file entering the manifest |
| `CommitKind` | The only two outcomes that may commit; `Failure` has no `CommitKind` and routes to `abort` | A failed turn reaching the barrier |
| `CommitContext` | Barrier-assembled commit inputs incl. the minted `OpId`; private fields, not constructible outside the crate | Forgeable commit context; out-of-order commit |
| `CommittingTurn::barrier` | The crate owns the full ordering: injected write-and-declare step (returns payload + delta) → `extend_from` merge → S3 via the bound store (commit-unknown → reconcile) → lease stop → release → authorize | Eager commit; a commit step that returns Ok without publishing; manifest-after-release |
| `CommitRejection` | The crate-side commit failures as distinct causes: delta merge (`Declare`), fence loss, store error, unknowable-after-supersession | An indeterminate commit reported as retryable |
| `CleanupOutcome` / `BarrierError` | Both cleanup results carried as real `Result`s; the injected step's failure (`CommitFailed`) and the authority's rejection (`CommitRejected`) are distinct variants | Silently lost cleanup failures; a wedged-session report on a steal |
| `CommittedResponse<T>` | Terminal-frame authorization bound to session+turn+epoch+holder; constructor private to the barrier module | Detached, reused, or forged authorization |
| `AdmissionError` | `Busy` (retryable, fixed hint) vs `Parked` (no hint, honest HITL signal) vs `StoreUnavailable` (fail-stop, I5) vs `NotProvisioned` (operator) vs `Io`; `ContentionLost` deleted (a lost S1 race *is* Busy) | Retry storms on non-contention failures; HITL parks mistaken for contention |
| `ReleaseError` | Only genuine failures: `StoreUnavailable`, `Io`. Supersession is a clean idempotent store outcome (`ReleaseOutcome::Superseded`), not an error | A steal misreported as a wedge |
| `FenceCause` | Write-path failures are lease-loss or I/O, nothing else | Misclassified EROFS |
| `LeaseState` / `LeaseLost` | Liveness is Live or Lost, and loss names session + epoch + holder | Anonymous loss |
| `HolderView` / `Locality` | Opaque: `here(pod)` or `remote(pod, deadline)`; the remote pair arrives together at one construction site (crate-internal contract, Layer-2 pinned) | Forged locality/deadline pairings |
| `StoreUnavailable` | PG-down is one typed failure, diagnostic payload only | Silent degradation of the claim path |
| `ClaimStore` / `ClaimRef` / `ClaimRequest` (internal) | The SQL port: one method per statement; every mutation predicates on the fence triple; `ClaimRef` carries `turn` so the park latch is derived, never free | A mutation without its fence; a hand-picked latch value |
| `ParkLatch` (internal) | The latch is `NotParked` or `Parked`; `Parked` writes the *committing* turn's id | A latch naming a turn that is not the committer |
| `HeartbeatOutcome` / `CommitOutcome` / `ReleaseOutcome` / `CommitDisposition` (internal) | Each statement's outcomes enumerate exactly the shapes its row count can produce; reconcile distinguishes `Applied` / `NotApplied` / `SupersededUnknown` by reading the fence triple alongside the op slot | A superseded commit falsely reported NotApplied |
| `KeepSet` / `GcScope` / `DebrisSweep` (internal) | Debris-only collection at claim time; `GcScope::permits` is implemented code (I3): only epochs strictly below the keep-set's claims-row read | A thawed zombie-GC touching a new holder's files |
| `RepairLane` (internal) | Two cures as one seam: `refresh_dir` (invalidate-cache; unmeasured, benign on failure) and `force_cure` (checkout -f + checkin, 518 ms measured); CLI/S3-API choice is config | Repair policy hard-coded per call site |
| `AdmissionEnv` / mode proofs | Config validated once; `pg()` narrows only when mode=Pg *and* a URL is present; the pg proof carries beat/ttl/margin/window/lane accessors | Local backend from a pg config; a heartbeat lease that cannot be assembled |
| `PgUrl` | Connection URL validated at config (scheme-checked) | Bare connection strings across the boundary |
| `RepairLaneKind` / `AdmissionMode` | `auto`/`cli`/`s3api`, `off`/`pg`; defaults safe | Unvalidated mode names |
| `build_admission` | One factory: mode proof + shared arbiter + pod identity + mandatory session root; build-once-per-server documented | Per-backend arbiters; caller-supplied identities; rootless claims |
| `TurnAdmission` | The port: admit + locate_holder; steals are internal to S1 | Orchestration coupling; hand-rolled steals |
| `LocalAdmission` / `PgAdmission` | The factory's two products; both require their mode proof at construction and store root + pod identity plus a backend-private lease proof token | Caller-named holders; pg claims on static leases |

## Seam table

| Reach | Visibility | Notes |
|---|---|---|
| `tokio` (task, time, sync Notify, fs) | pub dependency | heartbeat loop spawn/join; revocation is a shared atomic + Notify |
| `tokio-postgres` | pub dependency | the `PgStore` fill; `with-serde_json-1` for the manifest jsonb. Unconditional in rev 12 so the workspace gate (`--workspace --all-targets`) compiles the pg module; feature-gating is an integration-time decision |
| serde / serde_json | pub dependency | manifest jsonb wire (transparent map shape); `ArtifactPath`/`Digest`/`Epoch` deserialize with validation |
| uuid (v7, serde) | pub dependency | HolderId/OpId/TurnId minting and wire |
| std fs/path, atomics, Mutex, BTreeSet | std | GC sweep; `SelfFenceDeadline`'s Mutex and `FencedRun`'s issued-set never cross an await |
| no aura crates, no orchestration types | - | consumed via `TurnAdmission` |

Visibility honesty: `pub(crate)` means any module *inside this crate*
can call it. The internal assembly points are
`AcquiredClaim::{new_local, new_pg}`, `HeldLock::from_parts` (private,
via `HeldParts`), `Epoch::{initial, next, from_raw}`, `HolderId::mint`,
`OpId::mint`, `HeartbeatLease::{static_from, with_heartbeat}`,
`Revocation::new`, `SelfFenceDeadline::{fenced, anchor_at}`,
`HolderView::{here, remote}`, `KeepSet::from_manifest`,
`VerifiedRead::new` (private), and the SQL constants in `store.rs`.
Backend proof-token constructors are stronger than `pub(crate)`:
`LocalLeaseProof(())` is private to `adapters::local`,
`PgLeaseProof(())` to `adapters::pg`. Backend constructors are
crate-internal and require the narrowed config proofs
(`LocalAdmissionEnv` / `PgAdmissionEnv`) whose fields are private to the
sibling `config` module. None are reachable outside the crate; Layer-2
compile-fail tests must pin that when the test layer lands.

## Residual risks (named)

1. **PG failover epoch regression.** An async-replica failover can lose
   the latest epoch increment; two holders can then both be assigned the
   same epoch (one pre-, one post-failover). *Survivable, lossy*:
   `holder_id` freshness (I2) keeps every mutation predicate sound, so
   Postgres still linearizes commits; the cost is double-written epoch
   dirs (debris for GC), not corrupted committed state. Deployment rule:
   single-primary fail-stop is the v1 posture; do not inherit an
   async-failover managed default without revisiting this paragraph.
2. **Release/renewal closure binding is unprovable by types.** The
   release closure's captured fence triple and the renewal closure's S2
   are adapter contracts (one construction site each); Layer-2 pins
   them.
3. **Check-then-write race on `assert_live`.** Advisory at the call
   site; the structural backstop is server-side: S3's fence+lease
   predicate rejects a lost claim's commit no matter what local
   liveness said.
4. **Minutes-scale post-crash propagation is unmeasured** (two rig runs,
   healthy network). The propagation-window default (30 s) is a guess
   until the vendor's cache-TTL/`invalidate-cache` latency answers land
   or a soak test measures the tail. The protocol stalls, never lies.
5. **`invalidate-cache` is unmeasured** (vendor info arrived after the
   probe runs). It is the first repair tier precisely because its
   failure is benign (falls through to `force_cure`). Do not make it
   load-bearing before a rig measurement.
6. **archil CLI presence inside CSI-mounted pods is unknown**; it picks
   the default `RepairLane` impl (`Auto` discovery is the fill-phase
   behavior).
7. **Ordinary cancellation leaves the row live until lease expiry**
   (bounded availability cost, one TTL); pod death is covered by the
   controller's S4 pod-variant (M10).
8. **Abandonment** (`mem::forget`, task kill): the row leaks until
   lease expiry; the drop path aborts the heartbeat loop, and an
   in-flight S2 (unabortable in flight) may land after revocation —
   bounded by the lease predicate (B1): a late renewal on an expired
   claim matches zero rows.
9. **Unbounded join on a wedged store**: `stop()` joins the heartbeat
   loop; a hung S2 (network wedge) hangs `stop()` and the barrier with
   it. Liveness-only, operator-visible, never corruption.
10. **Lazy PG connect**: `build_admission` stays sync; the first claim
    pays connect latency, and connect failure surfaces as
    `StoreUnavailable` (fail-stop applies there too).
11. **Manifest growth**: cumulative per session in one jsonb column —
    fine at hundreds of entries; compaction/summarization is a future
    product card, not a rev-12 concern. The manifest's transparent map
    shape has no version marker; if the shape changes, add `v` at the
    first change, not after two shapes exist.
12. **Pod-name reuse in `release_pod`** (panel round 1): a delayed
    pod-Deleted event can expire rows held by a *same-named* replacement
    pod. Bounded harm: the replacement's next heartbeat matches zero
    rows, so it self-fences — an unnecessary session bounce, never
    corruption, and never worse than waiting out the TTL. A pod-UID
    column is the fix if the bounce rate ever justifies it.
13. **The classify-then-retry loop in `claim`** bounds its internal S1
    retry at one attempt; a pathological release/reclaim churn could in
    principle re-race. Bounded retry plus `Busy` is the honest fallback
    — the caller's configured hint still applies.

## Panel ledger

Model identities (author/reviewer invariant): the rev-12 skeleton author
is OpenCode session model `kimi-for-coding/k3`. Panel routing per
`REVIEW-TOOLING.md` (OpenCode board-owner row): seat 1 (adversarial
invalid-states) = `rust-reviewer` subagent (baseten/zai-org/GLM-5.2);
seat 2 (logic + seams) = codex CLI (gpt-5.6-sol). Three distinct
families; the invariant holds. Both seats passed the nonce read-path
pre-vet. The codex design-packet gate re-run (the handoff's continuation
task) is a separate, later gate on the filled packet.

Round 1 (rev-12 skeleton commit c20a4414; seat 1 GLM-5.2: FAIL, 1
BLOCKING + 4 MINOR; seat 2 codex gpt-5.6-sol: FAIL, 12 BLOCKING + 3
MINOR; costs: codex lane 103,127 tokens):

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|
| 1 | both | Self-fence never anchors: `fenced()`/`unfenced()` both store `None`, `anchor_at` no-ops, and no initial anchor exists before the first beat | Accepted | `SelfFenceState::{Unfenced, Fenced{deadline}}` enum; initial anchor at the S1 transmission instant (`granted_at` carried by `GrantedClaim` into lease construction) (r12.1) |
| 2 | GLM | `Epoch` derives `Deserialize` admitting 0 | Accepted | validating `Deserialize` (rejects below `initial()`) (r12.1) |
| 3 | GLM | `ManifestEntry.epoch` untied to the path's epoch prefix | Accepted | `ArtifactPath` carries the parsed epoch; `declare` rejects mismatch with `DeclareError::EpochMismatch` (r12.1) |
| 4 | GLM | `TurnId` doc conflates the two wire forms | Accepted | doc rewritten: serde compact (manifest JSONB) vs string parse (HTTP/PG) (r12.1) |
| 5 | GLM | S3 skips `$4` with no in-SQL comment | Accepted | S3 renumbered contiguous `$1..$6` (r12.1) |
| 6 | codex | `barrier` cannot execute S3: no store, no base manifest, arbitrary callback can return Ok without publishing | Accepted | barrier owns the full commit: injected step returns `(payload, delta)`; barrier merges via `extend_from` and issues S3 through the store bound in `HeldLock`; `CommitRejection` models the crate-side failures (r12.1) |
| 7 | codex | Granted manifest discarded before `HeldLock`; repair lane unreachable by the read seam | Accepted | base manifest bound into `HeldLock`; `manifest()` on `FencedRun`/`ActiveTurn`; `read_artifact` owns window + repair escalation internally (r12.1) |
| 8 | codex | I1 not structural: raw run dir + cloneable capability allow double-writing a path | Accepted | `ActiveTurn::write_artifact` is the only manifest-bound write path (capability asserted, dup/epoch checks, temp+fsync+rename+dir-fsync, digest computed) (r12.1) |
| 9 | codex | Manifest path/epoch mismatch + S3 blindly replaces the cumulative manifest | Accepted | as #3 plus the barrier's `extend_from` merge — commit publishes base+delta, never a bare replacement (r12.1) |
| 10 | codex | `PgAdmission` stores no root; `create_run` accepts any caller-selected session root | Accepted | session root bound at admission (both modes; factory takes a mandatory `root`); `create_run` is argument-free over the bound root (r12.1) |
| 11 | codex | `PgAdmission` lacks `BeatInterval`; `PgAdmissionEnv` exposes no beat accessor | Accepted | `PgAdmissionEnv::beat()` added and stored on the adapter (r12.1) |
| 12 | codex | Fresh-row `'{}'` jsonb cannot deserialize into `Manifest` (expects an `entries` field) | Accepted | `Manifest` is `#[serde(transparent)]` over the entries map (r12.1) |
| 13 | codex | `kind`/`parked` can form contradictory combinations | Accepted | `ParkLatch { NotParked, Parked }` derives the latch from the committing turn (`ClaimRef.turn`); `CommitKind` dropped from the store signature, lives at the barrier (r12.1) |
| 14 | codex | Reconcile can falsely return `NotApplied` after a supersession overwrites the op slot | Accepted | S5 reads the fence triple too; `CommitDisposition::SupersededUnknown` + `CommitRejection::ReconcileUnknown` report the indeterminacy honestly (r12.1) |
| 15 | codex | S1 classification races a release landing between S1 and the classify read | Accepted | `claim` contract: one internal S1 retry when classification observes a claimable row (r12.1) |
| 16 | codex | `S4_RELEASE_POD` has no port; pod-name reuse can expire a same-named replacement | Accepted in part | `ClaimStore::release_pod` added; name-reuse hazard recorded as residual risk 12 (bounded to a bounce, never corruption) rather than schema growth |
| 17 | codex | Supersession double-modeled as success (`ReleaseOutcome`) and error (`ReleaseError::Superseded`) | Accepted | `ReleaseError::Superseded` deleted; the store outcome is the single model (r12.1) |
| 18 | codex | `CleanupOutcome.quarantined: bool` discards the quarantine failure | Accepted | `quarantine: Result<(), std::io::Error>` (r12.1) |
| 19 | codex | `Epoch` u64 vs PG signed `bigint` | Accepted | `i64` domain documented on the type; store-edge conversion at the driver boundary (r12.1) |

## Hole inventory (rev 12.1 baseline, post-round-1 repairs)

`grep -rEn '^\s+todo!\(' src/` returns 33 holes:

- `identity.rs` (6): `SessionId::parse`, `TurnId::parse`,
  `PodId::parse`, `PodId::from_env`, `HolderId::parse`, `OpId::parse`
- `config.rs` (2): `PgUrl::parse`, `AdmissionEnv::from_env`
- `manifest.rs` (2): `ArtifactPath::parse`, `Digest::from_hex`
- `gc.rs` (1): `DebrisSweep::sweep`
- `repair.rs` (5): `CliRepairLane::{refresh_dir, force_cure}`,
  `S3ApiRepairLane::{refresh_dir, force_cure}`, `build_repair_lane`
- `state.rs` (5): `HeldLock::abort`, `ActiveTurn::abort`,
  `CommittingTurn::barrier`, `FencedRun::read_artifact`,
  `ActiveTurn::write_artifact`
- `lib.rs` (1): `build_admission`
- `adapters/local.rs` (2): `LocalAdmission::{admit, locate_holder}`
- `adapters/pg.rs` (9): `PgAdmission::{admit, locate_holder}`,
  `PgStore::{claim, heartbeat, commit, release, release_pod,
  reconcile_commit, locate}`

Fill units, each with its exit criterion (markers swept, inventory
accounted, its Layer-2 frames green):

1. identities + config/factory (parse rules, env chains, validation,
   mode dispatch + shared arbiter + repair-lane build)
2. manifest wire (`ArtifactPath::parse`, `Digest::from_hex`) + serde
   round-trip goldens (including the `'{}'` fresh-row case and the
   epoch-0 rejection)
3. `PgStore` connection + statements S1–S6 (scripted `ClaimStore`
   double for unit goldens; a live-PG integration test separately)
4. `PgAdmission::admit` + `locate_holder` (claim flow, classify retry,
   GC hook, lease/lock assembly, outcome mapping)
5. `LocalAdmission` (arbiter + static lease + no-op release)
6. artifact I/O (`write_artifact` publication sequence + dup/epoch
   enforcement; `read_artifact` verify + window + two-tier escalation)
7. `HeldLock::abort` + `ActiveTurn::abort` + `barrier` (ordering,
   merge, reconcile, cleanup algebra, OpId mint)
8. `DebrisSweep::sweep` + `RepairLane` impls (CLI first; S3-API may
   wait on the vendor answer)
9. `create_run` EROFS cure-retry (the H4 escalation — behavior change to
   an already-real body; its golden pins the retry-once shape)

`clippy.todo` stays `warn` through the fill phase; the completion gate
flips it to `deny`. Fill-phase gate invocation (an auditor running the
bare `-D warnings` form sees the tracked holes as errors by design):

```
cargo clippy --workspace --all-targets -- -D warnings -A clippy::todo
```
