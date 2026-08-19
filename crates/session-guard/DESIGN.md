# session-guard — design record (Layer 1 skeleton)

Claim-based turn admission for multi-instance AURA: at most one service
instance runs a turn for a session at a time, with session memory on a
shared Archil disk. Skeleton commit = type surface only; every `todo!()`
is a tracked hole (inventory at the bottom).

## Type → business-rule map

| Type | One business rule | Invalid state it forbids |
|---|---|---|
| `SessionId` | A session id is always a safe single path component (charset `[A-Za-z0-9._-]`, ASCII, ≤128 bytes, not `.`/`..`/`latest`) | Path traversal, symlink collision (`latest`), control-byte injection |
| `TurnId` | A turn is identified by a time-ordered UUIDv7 fixed at ingress | Two claims for one retry; non-unique claim file names |
| `InstanceId` | The claim holder is a named service instance (deployment-neutral: hostname, pod, VM, or `AURA_INSTANCE_ID`) | Anonymous/unattributable claims; a k8s-only assumption baked into types |
| `Generation` | Every admission or steal is the next generation; claim files are named with it | In-place claim rewrites; one client deleting another generation's claim |
| `HeartbeatSeq` / `HeartbeatSample` | Liveness is a monotonic counter observed with observer-local time, never wall-clock comparison | Clock-skew faking or masking liveness |
| `StalenessEvidence` | A steal requires adapter-assembled proof that one exact claim (session+fingerprint+holder+gen) had a non-advancing heartbeat across two samples ≥ `stale_after()` apart | Steal from session-id alone; evidence for a different claim incarnation |
| `SessionArbiter` / `ArbiterGuard` | Same-instance same-session requests serialize in-process before any disk access | Two tasks on one instance both reaching the claim store |
| `HeldLock` | A held claim carries its lease, its arbiter slot, and its release action; nothing can hold a claim without all three | Claim without liveness; claim that cannot be released |
| `HeartbeatLease` | The heartbeat actor lives exactly as long as the turn states carry it (by value) | Orphaned heartbeat tasks outliving turns; missing renewal |
| `WriteCapability` | Persistence writes are authorized per generation and fail closed after lease loss | Writes continuing after a steal (fencing by construction) |
| `FencedRun` | A run directory exists only as a state proving the seam created it under claim authority | Run dirs created outside admission |
| `TurnOutcome` | Every terminal path (success/failure/**clarification**) commits | Silent no-manifest exits |
| `CommittingTurn::barrier` | Ordering is commit → lease stop → release → authorize; commit semantics are injected, never owned by the guard | Manifest written after release; warn-and-continue drain |
| `CommittedResponse<T>` | Only this value authorizes the terminal frame; it is bound to session+turn+generation and not `Clone` | Emitting a terminal frame without a committed turn; permit reuse/forgery |
| `AdmissionError` | Only `Busy` is contention (503+Retry-After, LB-retryable); `ContentionLost` distinct; `NotProvisioned` operator-facing | LB retry storms on non-contention failures |
| `AdmissionEnv` / `AdmissionMode` | Admission is env-configured deployment infrastructure (`AURA_SESSION_ADMISSION=off\|lockfile`), defaulting off, mirroring the session store | Per-agent TOML ambiguity; forced-on clusters |
| `TurnAdmission` | The port: admit, evidence-steal, locate-holder — object-safe, no orchestration concepts | Orchestration refactor coupling |

## Seam table

| The module reaches into | Visibility | Notes |
|---|---|---|
| `tokio` (sync, task, time) | pub dep | watch channels, heartbeat actor |
| std fs/path | std | claim file ops in the adapter only |
| Nothing else | — | zero aura-crate, zero orchestration deps; `aura-web-server` consumes via `TurnAdmission` |

Constructor visibility: `StalenessEvidence::new`, `HeartbeatSample::new`,
`LockFingerprint::new`, `HeartbeatLease::new` are `pub(crate)` — only the
adapter can assemble them.

## Residual risks (named)

1. **Drop/abandonment**: consuming typestate orders the happy path but
   cannot compel it (`mem::forget`, aborted tasks). Cancellation is
   handled by `ActiveTurn::abort`; hard abandonment leaks the claim until
   staleness. Linearly beyond the module is convention — enforced at the
   seam, tested by compile-fail + behavioral suites (Layer 2).
2. **Uncached reads**: the archil adapter's reads must bypass client
   cache for evidence and `locate_holder`; the exact mechanism
   (`invalidate-cache`, readdir expiry 0) is a Phase-1 probe, not proven.
3. **Steal/revive race**: evidence revalidation narrows but cannot
   eliminate the window where a paused holder resumes post-steal; the
   `WriteCapability` fail-closed gate is the structural backstop.
4. **Provisioning** (root/session dirs checked in once) lives at the
   deployment seam, outside this crate.

## Hole inventory (baseline at skeleton commit)

`grep -rEn '^\s+todo!\(' src/` → 12 holes:

- `identity.rs`: `SessionId::parse`, `InstanceId::parse`, `InstanceId::from_env`
- `lib.rs`: `AdmissionEnv::from_env`
- `state.rs`: `ActiveTurn::abort`, `CommittingTurn::barrier`
- `adapters.rs`: `LocalAdmission::{admit, locate_holder}`,
  `ClaimFileAdmission::{admit, admit_with_evidence, locate_holder}`

Fill order: identity/config (pure) → LocalAdmission (no fs) →
ClaimFileAdmission admit/locate → barrier/abort → steal path. Each fill
sweeps its `#[expect]` markers and flips its Layer-2 frames red→green.

`clippy.todo` is `warn` through the fill phase; the completion gate flips
it to `deny`.
