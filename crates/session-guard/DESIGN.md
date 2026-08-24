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
unbounded). A successful claim clears the latch in the same statement.

There is no storage-level fence (probe H5: a thawed zombie's write+fsync
succeeds after a steal). Writes are contained, not intercepted: one
claim = one epoch = one run dir (`{session}/e{k}/`); paths are
write-once (I1); the manifest (JSONB on the row, cumulative) is the only
authority on committed bytes (G2's handshake is the row read itself —
Q7: manifest-in-Postgres, so commit is fsync-then-one-atomic-UPDATE with
no pointer-flip window, I4). Commits are predicated on the fence triple
and an unexpired lease (B1), so Postgres linearizes what can ever become
committed state (G1-prime) even if a failover regresses the epoch (I2).

Turn outcomes: Success/Clarification commit (S3 publishes the cumulative
manifest + op id + park latch); Failure aborts (quarantine + release, no
commit — unrepresentable at the type level: the barrier takes a
`CommitKind`, and `Failure` has no `CommitKind`). Park =
commit-then-release with `parked_turn` set; reify re-claims the same
`TurnId`; the park *algorithm* belongs to HITL-271 — this crate carries
only the latch, the `Parked` admission variant, and the same-TurnId
reify path.

## Type-to-business-rule map (every public item)

| Item | One business rule | Invalid state it forbids |
|---|---|---|
| `SessionId` | Exactly 1..=128 bytes of ASCII `[A-Za-z0-9._-]`, not `.`/`..`/`latest` | Empty id resolving to the session root; traversal; symlink collision |
| `TurnId` (+`parse`) | A turn id is a unique UUID fixed at ingress (locally v7; wire accepts any UUID); reify reuses the parked turn's id | Claim rows that cannot name the claiming turn; reify under a fresh id |
| `PodId` (+`parse`, `from_env`) | The holder's pod is a stable RFC-1123 name (1..=253 bytes): `AURA_POD_ID` (fail loud when invalid) → `POD_NAME` → `HOSTNAME` | Anonymous claim rows; a controller unable to map pod-Deleted → row |
| `HolderId` (+`parse`) | Each acquire *attempt* mints a fresh UUIDv7 (I2) — the second fence every mutation predicates on | A stale holder linearizing a commit after a failover epoch regression |
| `OpId` (+`parse`) | One logical commit = one id, minted by the barrier, reused by its retries (B3) | Commit-unknown resolved by blind re-UPDATE |
| `Epoch` (+`initial`/`next`/`from_raw` crate-internal) | Per-session monotonic fencing token; dir name `e{k}`; construction only inside the crate | Caller-minted epochs; wrapped counters (u64 exhaustion is a typed dead end) |
| `session_dir` / `epoch_dir` | The only path derivations: `{root}/{session}` and `{session}/e{k}` | Hand-assembled layout strings at the seam |
| `LeaseDeadline` | A server-clock timestamp (`clock_timestamp()` product); never compared against pod clocks | Cross-clock lease comparisons on the safety path |
| `LeaseTtl` / `SelfFenceMargin` / `BeatInterval` | Non-zero durations by construction; config additionally requires margin < ttl | Degenerate zero leases; a margin that swallows the ttl |
| `SelfFenceDeadline` (internal) | Conservative local expiry re-anchored at each beat *transmission* (`transmit + ttl − margin`, codex M9); unfenced in local mode | A self-fence that waits on a response; Live-after-wedge capabilities |
| `Liveness` / `Revocation` / `ActorExitGuard` (internal) | Unchanged from rev 11: capabilities observe (clone/drop-safe); only the lease and the exit guard hold revocation authority | Live-after-death capabilities; a dropped capability killing a live lease |
| `HeartbeatLease` / `ClaimLeaseSource` | Unchanged machinery, adapted identity: the lease owns the S2 heartbeat loop (anchor-at-transmission + wake-and-join shutdown); the first failed renewal revokes (Lost and PG-down both fail closed, I5) | Orphaned heartbeats; a renewal landing after release begins |
| `WriteCapability` | Writes authorized for one `(session, turn, epoch, holder)` quad, failing closed on revocation *or* self-fence expiry | Cross-claim capability misuse; writes after a steal |
| `Manifest` | The cumulative committed artifact set; grows only through `declare`, which rejects duplicate paths (I1 made structural) | A manifest that can overwrite an entry |
| `ArtifactPath` / `Digest` (+`Invalid*`) | Epoch-qualified relative path; 32-byte sha256 (hex wire); both parse-don't-validate, and serde deserialization validates too | Unvalidated strings in the committed set |
| `ManifestEntry` | digest + turn + epoch + byte length, all validated types (`bytes` is a plain metric — no rule branches on it) | Provenance separated from content |
| `DeclareError` | The write-once violation carries the already-declared path | Silent double-declare |
| `ReadMiss` | The two read-failure classes stay distinct: `NotFound` (propagation — retry/escalate) vs `Corrupt` (fail loud, never retry) | Corruption retried as propagation |
| `SessionArbiter` / guards (internal) | Unchanged: same-instance serialization before any store access; Admitting vs Held two-phase | Two local tasks both reaching the store |
| `AcquiredClaim` (internal) | Admission output is one sealed bundle; `new_local`/`new_pg` fix the lease plan at acquisition and demand the backend's private proof token; `into_held` derives the lease from the same identity | Claim/lease/release identity disagreement; a pg claim taking a static lease |
| `HeldLock` | Held claim = identity + arbiter slot + lease + bound release + optional repair lane; assembled only via `into_held`; `create_run` derives the epoch dir from the claim's own epoch (the seam no longer chooses layout) | Hand-assembled holds; run dirs outside the epoch partition |
| `CommitKind` | The only two outcomes that may commit; `Failure` has no `CommitKind` and routes to `abort` | A failed turn reaching the barrier |
| `CommitContext` | Barrier-assembled commit inputs incl. the minted `OpId`; private fields, not constructible outside the crate | Forgeable commit context; out-of-order commit |
| `CommittingTurn::barrier` | Ordering: commit(ctx) → lease stop → release → authorize; commit is lazy and context-fed | Eager commit; manifest-after-release |
| `CleanupOutcome` / `BarrierError` | Unchanged: commit failure carries cleanup results; release failure after durable commit carries the authorized response | Silently lost cleanup failures |
| `CommittedResponse<T>` | Terminal-frame authorization bound to session+turn+epoch+holder; constructor private to the barrier module | Detached, reused, or forged authorization |
| `AdmissionError` | `Busy` (retryable, fixed hint) vs `Parked` (no hint, honest HITL signal) vs `StoreUnavailable` (fail-stop, I5) vs `NotProvisioned` (operator) vs `Io`; `ContentionLost` deleted (a lost S1 race *is* Busy) | Retry storms on non-contention failures; HITL parks mistaken for contention |
| `ReleaseError` | `Superseded` is final (the new owner stands), never force-fixed; `StoreUnavailable` distinct from `Io` | Force-unwedging by type accident |
| `FenceCause` | Write-path failures are lease-loss or I/O, nothing else | Misclassified EROFS |
| `LeaseState` / `LeaseLost` | Liveness is Live or Lost, and loss names session + epoch + holder | Anonymous loss |
| `HolderView` / `Locality` | Opaque: `here(pod)` or `remote(pod, deadline)`; the remote pair arrives together at one construction site (crate-internal contract, Layer-2 pinned) | Forged locality/deadline pairings |
| `StoreUnavailable` | PG-down is one typed failure, diagnostic payload only | Silent degradation of the claim path |
| `AdmissionEnv` / mode proofs | Config validated once; `pg()` narrows only when mode=Pg *and* a URL is present, so `PgAdmissionEnv` is unforgeable for a half-configured pg mode | Local backend from a pg config; url-less pg backend |
| `PgUrl` | Connection URL validated at config (scheme-checked) | Bare connection strings across the boundary |
| `RepairLaneKind` | `auto`/`cli`/`s3api`, default auto | Unvalidated lane names |
| `AdmissionMode` / `build_admission` | One factory: mode proof + shared arbiter + pod identity; build-once-per-server documented | Per-backend arbiters; caller-supplied identities |
| `TurnAdmission` | The port: admit + locate_holder; steals are internal to S1 | Orchestration coupling; hand-rolled steals |
| `LocalAdmission` / `PgAdmission` | The factory's two products; both require their mode proof at construction and store the pod identity plus a backend-private lease proof token | Caller-named holders; pg claims on static leases |

## Seam table

| Reach | Visibility | Notes |
|---|---|---|
| `tokio` (task, time, sync Notify, fs) | pub dependency | heartbeat loop spawn/join; revocation is a shared atomic + Notify |
| `tokio-postgres` | pub dependency | the `PgStore` fill; `with-serde_json-1` for the manifest jsonb. Unconditional in rev 12 so the workspace gate (`--workspace --all-targets`) compiles the pg module; feature-gating is an integration-time decision |
| serde / serde_json | pub dependency | manifest jsonb wire; `ArtifactPath`/`Digest` deserialize with validation |
| uuid (v7, serde) | pub dependency | HolderId/OpId/TurnId minting and wire |
| std fs/path, atomics, Mutex | std | GC sweep; `SelfFenceDeadline`'s Mutex never crosses an await |
| no aura crates, no orchestration types | - | consumed via `TurnAdmission` |

Visibility honesty: `pub(crate)` means any module *inside this crate*
can call it. The internal assembly points are
`AcquiredClaim::{new_local, new_pg}`, `HeldLock::from_parts` (private),
`Epoch::{initial, next, from_raw}`, `HolderId::mint`, `OpId::mint`,
`HeartbeatLease::{static_from, with_heartbeat}`, `Revocation::new`,
`SelfFenceDeadline::{fenced, anchor_at}`, `HolderView::{here, remote}`,
`KeepSet::from_manifest`, and the SQL constants in `store.rs`. Backend
proof-token constructors are stronger than `pub(crate)`:
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
    product card, not a rev-12 concern. The manifest's JSON envelope has
    no version field yet; if the shape changes, add `v` at the first
    change, not after two shapes exist.

## Panel ledger (rev 12)

Model identities (author/reviewer invariant): the rev-12 skeleton author
is OpenCode session model `kimi-for-coding/k3`. Panel routing per
`REVIEW-TOOLING.md` (OpenCode board-owner row): seat 1 (adversarial
invalid-states) = `rust-reviewer` subagent (baseten/zai-org/GLM-5.2);
seat 2 (logic + seams) = codex CLI (gpt-5.6-sol). Three distinct
families; the invariant holds. The codex design-packet gate re-run (the
handoff's continuation task) is a separate, later gate on the filled
packet.

Rounds: pending (the panel reviews the rev-12 skeleton commit).

| # | Seat | Finding | Disposition | Repair |
|---|---|---|---|---|

## Hole inventory (rev 12 baseline)

`grep -rEn '^\s+todo!\(' src/` returns 30 holes:

- `identity.rs` (6): `SessionId::parse`, `TurnId::parse`,
  `PodId::parse`, `PodId::from_env`, `HolderId::parse`, `OpId::parse`
- `config.rs` (2): `PgUrl::parse`, `AdmissionEnv::from_env`
- `manifest.rs` (2): `ArtifactPath::parse`, `Digest::from_hex`
- `gc.rs` (1): `DebrisSweep::sweep`
- `repair.rs` (5): `CliRepairLane::{refresh_dir, force_cure}`,
  `S3ApiRepairLane::{refresh_dir, force_cure}`, `build_repair_lane`
- `state.rs` (3): `HeldLock::abort`, `ActiveTurn::abort`,
  `CommittingTurn::barrier`
- `lib.rs` (1): `build_admission`
- `adapters/local.rs` (2): `LocalAdmission::{admit, locate_holder}`
- `adapters/pg.rs` (8): `PgAdmission::{admit, locate_holder}`,
  `PgStore::{claim, heartbeat, commit, release, reconcile_commit,
  locate}`

Fill units, each with its exit criterion (markers swept, inventory
accounted, its Layer-2 frames green):

1. identities + config/factory (parse rules, env chains, validation,
   mode dispatch + shared arbiter + repair-lane build)
2. manifest wire (`ArtifactPath::parse`, `Digest::from_hex`) + serde
   round-trip goldens
3. `PgStore` connection + statements S1–S6 (against a scripted
   `ClaimStore` double for unit goldens; a live-PG integration test
   separately)
4. `PgAdmission::admit` + `locate_holder` (claim flow, GC hook,
   lease/lock assembly, outcome mapping)
5. `LocalAdmission` (arbiter + static lease + no-op release)
6. `HeldLock::abort` + `ActiveTurn::abort` + `barrier` (ordering,
   cleanup algebra, OpId mint)
7. `DebrisSweep::sweep` + `RepairLane` impls (CLI first; S3-API may
   wait on the vendor answer)
8. `create_run` EROFS cure-retry (the H4 escalation — behavior change to
   an already-real body; its golden pins the retry-once shape)

`clippy.todo` stays `warn` through the fill phase; the completion gate
flips it to `deny`. Fill-phase gate invocation (an auditor running the
bare `-D warnings` form sees the tracked holes as errors by design):

```
cargo clippy --workspace --all-targets -- -D warnings -A clippy::todo
```
