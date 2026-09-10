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
claiming epoch's dir), and every successful write records itself into
the turn's *private* delta — the raw run-directory path never leaves the
crate, so a fabricated or misattributed manifest is unconstructible.
`FencedRun::read_artifact` owns verify-on-first-read with the three-way
miss handling inside (propagation window → `refresh_dir` → `force_cure`
→ fail loud). Scratchpad bytes have their own typed surface
(`scratchpad.rs`): turn-transient, capability-gated, never
manifest-bound, and collected as debris once the epoch passes.

Turn outcomes: the barrier's terminal shape is one `TurnEnd` —
`Commit(CommitKind)` (Success/Clarification) or `Park` (HITL-271's
commit-then-release with the latch set) — and `abort` covers Failure and
cancellation, so no failure value can reach a commit. At the barrier the
injected step produces only the response payload; the barrier consumes
the recorded delta, merges it into the granted base manifest
(`extend_from`), and issues S3 with the latch derived from the `TurnEnd`
(never a free parameter). Commit-unknown reconciles by read-back on the
fence triple plus the op slot: `Applied` proceeds, `NotApplied` rejects
with quarantine (safe — nothing references the run), and
`SupersededUnknown` is *indeterminate*: the run is never quarantined
(its bytes may be manifest-referenced) and the turn is reported lost.

## Type-to-business-rule map (every public item, plus the crate-internal
assembly types)

| Item | One business rule | Invalid state it forbids |
|---|---|---|
| `SessionId` | Exactly 1..=128 bytes of ASCII `[A-Za-z0-9._-]`, not `.`/`..`/`latest` | Empty id resolving to the session root; traversal; symlink collision |
| `TurnId` (+`parse`) | A turn id is a unique UUID fixed at ingress, with two documented wire forms (serde compact for the manifest JSONB; string parse for HTTP/PG text); reify reuses the parked turn's id | Claim rows that cannot name the claiming turn; reify under a fresh id |
| `PodId` (+`parse`, `from_env`) | The holder's pod is a stable RFC-1123 name (1..=253 bytes): `AURA_POD_ID` (fail loud when invalid) → `POD_NAME` → `HOSTNAME` | Anonymous claim rows; a controller unable to map pod-Deleted → row |
| `HolderId` (+`parse`) | Each acquire *attempt* mints a fresh UUIDv7 (I2) — the second fence every mutation predicates on | A stale holder linearizing a commit after a failover epoch regression |
| `OpId` (+`parse`) | One logical commit = one id, minted by the barrier, reused by its retries (B3) | Commit-unknown resolved by blind re-UPDATE |
| `Epoch` | Per-session monotonic fencing token starting at 1; construction crate-internal (`initial`/`next`); both ingress paths validate — `from_raw` returns `None` for 0 (store edge), `Deserialize` rejects 0 (manifest) — so epoch 0 is unreachable; SQL `bigint` domain noted (`i64::MAX` ceiling, unreachable in practice) | Caller-minted epochs; epoch 0 smuggled in through a corrupted or foreign-written row |
| `session_dir` / `epoch_dir` | The only path derivations: `{root}/{session}` and `{session}/e{k}` | Hand-assembled layout strings at the seam |
| `LeaseDeadline` | A server-clock timestamp (`clock_timestamp()` product); never compared against pod clocks | Cross-clock lease comparisons on the safety path |
| `LeaseTtl` / `SelfFenceMargin` / `BeatInterval` | Non-zero durations by construction; config requires margin < ttl | Degenerate zero leases; a margin that swallows the ttl |
| `SelfFenceDeadline` (internal) | Conservative local expiry: `Unfenced` and `Fenced` are distinct states and `Fenced` always holds a concrete anchor (no Option — "fenced but never anchored" is unrepresentable); anchored at the S1 transmission instant (`granted_at`, single-sourced from the granted claim record) and re-anchored at each beat transmission (`transmit + ttl − margin`, codex M9) | A self-fence that never fires; an unfenced pre-first-beat window; a self-fence that waits on a response |
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
| `AcquiredClaim` (internal) | Admission output is one sealed bundle: identity + bound session root + base manifest + paired backend services + release; `new_local`/`new_pg` fix the lease plan at acquisition and demand the backend's private proof token; `into_held` derives the lease from the same identity and the claim record's `granted_at` | Claim/lease/release identity disagreement; a pg claim taking a static lease; a claim unbound from its session root |
| `Backend` (internal) | Backend services are paired by construction: `Local` (neither) or `Pg { store, repair }` (both) | A store-without-repair claim (a commit path with no cure, or a cure with no store) |
| `GrantedClaim` (internal) | The granted row as one bundle, including `granted_at` (the S1 transmission instant) — the lease's initial self-fence anchor comes from this record and nowhere else | A lease built without its M9 anchor; a self-fence anchored at assembly time |
| `HeldLock` | Held claim = identity + bound session root + base manifest + backend + lease + arbiter slot + release; `create_run` is argument-free — it derives the epoch dir under the claim's own bound root (no caller-selected layout, no session-A-dir-under-session-B); `abort` reports a full `CleanupOutcome` | Hand-assembled holds; run dirs outside the epoch partition; cross-session root confusion; a silently lost abort cleanup failure |
| `FencedRun` / `read_artifact` | The G2 base view (`manifest()`) and the verified read path ride the run; the read path owns the three-way miss handling internally; the raw run-dir path never leaves the crate; the turn's paths are *reserved* in a synchronous `issued` set before any await or filesystem mutation, so concurrent writes of one path cannot interleave | The seam hand-rolling digest checks or repair escalation; an unguarded write path around the typed surfaces; a rename race between two concurrent writers of one path |
| `ActiveTurn` / `write_artifact` | The only manifest-bound write path: reservation taken synchronously first, capability asserted, own-epoch only, temp+fsync+rename+dir-fsync, digest computed, and each write *recorded into the turn's private delta* — the barrier commits exactly the recorded set. A failed write consumes the reservation (single-artifact retry is unsupported; the turn aborts) | A second write to one path; a write outside the claiming epoch; an unfsynced file entering the manifest; a fabricated or foreign-epoch delta |
| `ScratchpadName` / `ScratchpadError` | Turn-transient I/O with its own typed surface: single-component names, capability-gated, same-turn reads fail loud on stale pointers (`NotFound` names turn + entry) | Raw path math at the seam; silent empty reads on stale scratchpad pointers |
| `CommitKind` | The only two *committing* outcomes; `Failure` has no `CommitKind` and routes to `abort` | A failed turn reaching the barrier |
| `TurnEnd` | The barrier's terminal shape in one type: `Commit(CommitKind)` or `Park`; the S3 latch is derived from it (`latch()`), so park intent has a representable route and the latch value stays the committing turn's id | A latch supplied without park intent; a park that cannot reach the barrier |
| `CommitContext` | Barrier-assembled inputs incl. the minted `OpId` and the `TurnEnd`; private fields, not constructible outside the crate; carries no run-dir path (the payload step needs identities, not layout) | Forgeable commit context; out-of-order commit; a raw path bypass |
| `CommittingTurn::barrier` | The crate owns the full ordering: injected payload step (`Result<T, E>` only) → consume the recorded private delta → `extend_from` merge → S3 via the bound store (commit-unknown → reconcile) → lease stop → release → authorize | Eager commit; a commit step that returns Ok without publishing; a callback-fabricated manifest |
| `CommitRejection` | The *definite* crate-side commit failures as distinct causes: delta merge (`Declare`), fence loss, proven non-landing (`NotLanded` — quarantine-safe), pre-S3 store error. Indeterminacy is not here, and `Store` is documented pre-S3-only | Quarantining bytes a committed manifest may reference; a post-S3 store error routed to a quarantining variant |
| `IndeterminateCause` | The two unknowable-outcome causes as types: `SupersededUnknown` and `ReconcileUnavailable`; every indeterminate outcome routes to `CommitIndeterminate`, which has no quarantine field | A silently quarantined commit-in-doubt |
| `CleanupOutcome` / `BarrierError` | Both cleanup results carried as real `Result`s; aborts return `CleanupOutcome` too; `CleanupOutcome` and `CommittedResponse` are `#[must_use]` like the rest of the chain; `CommitIndeterminate` carries a typed `IndeterminateCause` and *no* quarantine field | Silently lost cleanup failures; a wedged-session report on a steal; deletion of maybe-committed bytes; a silently discarded abort or authorization |
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
| `KeepSet` / `SweepScope` / `DebrisSweep` (internal) | Debris-only collection at claim time; `SweepScope::permits` is implemented code (I3): only epochs strictly below the keep-set's claims-row read | A thawed zombie-GC touching a new holder's files |
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
   same epoch (one pre-, one post-failover). `holder_id` freshness (I2)
   keeps every *Postgres* mutation predicate sound, so the store still
   linearizes commits. But panel round 4 showed the filesystem side is
   weaker than this paragraph originally claimed: with epoch-only run
   dirs, the two same-epoch holders share a directory, so a stale holder
   can overwrite the winner's artifact bytes at a path the winner's
   manifest references (digest-detected on read — never silent), and a
   stale holder's quarantine-after-LostFence can delete committed files
   in the shared dir. **v1 is gated by deployment posture:**
   single-primary fail-stop Postgres makes the scenario unreachable, and
   that posture is the v1 rule. The recorded fix — `(epoch, holder)`
   -namespaced run dirs (`e{k}/h-{holder}/`, enforced through
   `ArtifactPath`, `write_artifact`, and GC) — is **deferred by Mike's
   ruling (2026-08-23) and must land before the claims table ever sits on
   an async-failover Postgres.** Greenfield means landing it later costs
   no migration; deploying onto async-failover PG without it is a known
   corruption window.
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

Round 2 (rev-12.1 commit c02251ba; seat 1 GLM-5.2: PASS with 5 MINOR;
seat 2 codex gpt-5.6-sol: FAIL, 8 BLOCKING + 1 MINOR; costs: codex lane
132,719 tokens):

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|
| 20 | codex+GLM | `granted_at` duplicated: `GrantedClaim.granted_at` discarded while `HeartbeatRenewal.granted_at` was used — two sources for the M9 anchor | Accepted | `HeartbeatRenewal` drops the field; the lease's plan takes the claim record's `granted_at` — one source of truth (r12.2) |
| 21 | codex | The barrier callback returned the manifest delta, so a fabricated manifest (unwritten or foreign-epoch entries) was constructible | Accepted | the callback returns only the payload; every `write_artifact` records itself into the turn's *private* delta, and the barrier commits exactly that set (`take_delta`) (r12.2) |
| 22 | codex | `run_dir()` accessors on `FencedRun`/`CommitContext` exposed a raw write path around the typed surface | Accepted | public path accessors removed; scratchpad I/O gained its own typed surface (`scratchpad.rs`); the only raw path crossing the boundary is the `LostAfterCreate` cleanup payload (r12.2) |
| 23 | codex | Park intent had no representable route to the barrier (store took `ParkLatch` but no turn-outcome mapped to it) | Accepted | `TurnEnd::{Commit(CommitKind), Park}`; `ActiveTurn::{complete, park}`; the latch derives from the end via `latch()` (r12.2) |
| 24 | codex | An indeterminate commit (reconcile = superseded-unknown) was routed through a variant whose cleanup quarantines the run — potentially deleting manifest-referenced bytes | Accepted | `BarrierError::CommitIndeterminate` carries no quarantine field: stop, release, report lost, leave the epoch for manifest-aware GC; `CommitRejection::NotLanded` covers the proven-never-landed case (r12.2) |
| 25 | codex | `ActiveTurn::abort`/`HeldLock::abort` returned a single `ReleaseError`, discarding the quarantine result | Accepted | both aborts return `CleanupOutcome` (both results reported) (r12.2) |
| 26 | codex | The classify-then-retry decision needed a server-derived claimable fact (pod clocks cannot evaluate `lease_expires_at`) | Accepted | `S1_CLASSIFY` computes `(lease_expires_at < clock_timestamp()) AS expired` server-side (r12.2) |
| 27 | codex | `reconcile_commit` took `session` independently of the `ClaimRef`, so one session could be read against another claim's fence | Accepted | session derives from the claim ref; the separate parameter is gone (r12.2) |
| 28 | GLM | `Fenced { deadline: Option<Instant> }` still permitted fenced-never-anchored | Accepted | `Fenced { deadline: Instant }` — the Option is gone (r12.2) |
| 29 | GLM | `Epoch::from_raw` accepted 0, bypassing the `Deserialize` validation | Accepted | `from_raw` returns `Option<Epoch>` (`None` at 0); the store edge fails loud on it (r12.2) |
| 30 | GLM | A hand-built delta with self-consistent but foreign-epoch entries passed `declare` | Accepted | closed structurally by #21: the delta is recorded only by `write_artifact` (own-epoch enforced); `declare`'s tie check remains the second line (r12.2) |
| 31 | GLM | `HeldParts` carried store and repair as independent `Option`s | Accepted | paired into the `Backend::{Local, Pg{store, repair}}` enum (r12.2) |

Round 3 (rev-12.2 commit 70961c68; seat 1 GLM-5.2: PASS with 3 MINOR;
seat 2 codex gpt-5.6-sol: FAIL, 2 BLOCKING; costs: codex lane 139,680
tokens):

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|
| 32 | codex+GLM | Reconcile *failure* after commit-unknown (S5 itself unavailable) had no documented home and could be misrouted into the quarantine-carrying `CommitRejection::Store` | Accepted | `IndeterminateCause::{SupersededUnknown, ReconcileUnavailable}`; every post-S3-dispatch indeterminacy routes to `CommitIndeterminate` with a typed cause; `CommitRejection::Store` documented pre-S3-only (r12.3) |
| 33 | codex | The delta swap dropped the synchronous reservation set: two concurrent `write_artifact` calls for one path could both pass a delta-based dup check and interleave renames | Accepted | `FencedRun.issued: Mutex<BTreeSet<ArtifactPath>>` restored as a *reservation* taken synchronously before any await or filesystem mutation; a failed write consumes its reservation (r12.3) |
| 34 | GLM | `CleanupOutcome` not `#[must_use]` — an ignored abort outcome would silently drop both results | Accepted | `#[must_use]` added (r12.3) |
| 35 | GLM | `CommittedResponse` not `#[must_use]` — a dropped authorization would not warn | Accepted | `#[must_use]` added (r12.3) |

Round 4 (rev-12.3 commits 1b4c7751 + de3d0ad9; seat 1 GLM-5.2: PASS
with 1 MINOR; seat 2 codex gpt-5.6-sol: FAIL, 1 BLOCKING; costs: codex
lane 121,357 tokens):

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|
| 36 | GLM | `#[from]` on `CommitRejection::Store` makes the wrong routing the path of least resistance (a post-S3 store error could be `.into()`-ed into the quarantining variant) | Accepted | `#[from]` dropped; every construction site must name the variant (r12.4) |
| 37 | codex | Epoch-only run namespacing breaks under PG failover regression: two same-epoch holders share a dir, so a stale holder can overwrite the winner's manifest-referenced bytes or quarantine the shared dir | **Deferred — Mike's ruling 2026-08-23.** Gated by the v1 single-primary fail-stop posture, under which the scenario is unreachable; greenfield means no migration cost to land it later | Recorded fix: `(epoch, holder)`-namespaced run dirs through `ArtifactPath`, `write_artifact`, manifest, and GC. **Trigger: must land before the claims table ever sits on an async-failover Postgres.** Residual risk 1 rewritten to state the exposure honestly |

## Hole inventory (rev 12.4 baseline, post-round-4; 36 holes, unchanged in count)

`grep -rEn '^\s+todo!\(' src/` returns 36 holes:

- `identity.rs` (6): `SessionId::parse`, `TurnId::parse`,
  `PodId::parse`, `PodId::from_env`, `HolderId::parse`, `OpId::parse`
- `config.rs` (2): `PgUrl::parse`, `AdmissionEnv::from_env`
- `manifest.rs` (2): `ArtifactPath::parse`, `Digest::from_hex`
- `gc.rs` (1): `DebrisSweep::sweep`
- `repair.rs` (5): `CliRepairLane::{refresh_dir, force_cure}`,
  `S3ApiRepairLane::{refresh_dir, force_cure}`, `build_repair_lane`
- `scratchpad.rs` (3): `ScratchpadName::parse`,
  `ActiveTurn::{write_scratchpad, read_scratchpad}`
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
   epoch-0 rejection, both directions)
3. `PgStore` connection + statements S1–S6 (scripted `ClaimStore`
   double for unit goldens; a live-PG integration test separately)
4. `PgAdmission::admit` + `locate_holder` (claim flow, classify retry on
   the server-computed `expired`, GC hook, lease/lock assembly, outcome
   mapping)
5. `LocalAdmission` (arbiter + static lease + no-op release)
6. artifact I/O (`write_artifact` publication sequence + dup/epoch
   enforcement + private-delta recording; `read_artifact` verify +
   window + two-tier escalation; scratchpad write/read)
7. `HeldLock::abort` + `ActiveTurn::abort` + `barrier` (ordering, delta
   merge, reconcile algebra incl. indeterminate-never-quarantine, cleanup
   reporting, OpId mint)
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
