//! # session-guard
//!
//! Claim-based turn admission for multi-instance AURA: at most one
//! service instance runs a turn for a given session at a time, with
//! cross-instance memory on a shared Archil disk.
//!
//! Two layers keep that promise:
//!
//! 1. **In-process** — a session arbiter (internal) serializes
//!    same-session requests arriving at one instance, before any claim
//!    store access.
//! 2. **Cross-instance** — a Postgres claims table is the sole claim
//!    authority (ruling 2026-08-20/21: the claim-file protocol is
//!    retired). One single-statement `INSERT .. ON CONFLICT DO UPDATE
//!    .. WHERE lease_expired` (S1) is the election *and* the steal:
//!    atomic under READ COMMITTED (I7). The row is never deleted; its
//!    `epoch` is the session's monotonic fencing token, and every
//!    mutation predicates on `(session_id, epoch, holder_id)` plus the
//!    server-side lease clock (I2, I6).
//!
//! There is no storage-level fence (probe H5: a zombie's write+fsync
//! succeeds after a steal), so writes are *contained*, not intercepted:
//! one claim = one epoch = one run dir (`{session}/e{k}/`), paths are
//! write-once (I1), and the cumulative manifest in the claims row is the
//! only authority on committed bytes. The artifact I/O surface is part
//! of the claim: [`ActiveTurn::write_artifact`] is the only way
//! manifest-bound bytes are written (temp + fsync + rename, one write
//! per path — I1/I4 as structure), and [`FencedRun::read_artifact`] owns
//! verify-on-first-read with the three-way miss handling. GC is
//! debris-only and epoch-scoped (I3); a thawed zombie's writes land
//! where nothing reads them and the next claim's sweep reclaims them.
//!
//! The consuming state machine (one instance's view of one request):
//!
//! ```text
//! Idle ──admit──────────► HeldLock ──create_run──► FencedRun ──activate──► ActiveTurn
//!  │                        │         (asserts capability, derives e{k}/    │
//!  ├─busy──► 503           │          from the claim's epoch under the      │
//!  └─parked──► parked      │          bound session root, creates,          ├─complete(CommitKind)─┐
//!      (no retry hint)     │          rechecks; error returns the lock)     ├─park()───────────────┼─► CommittingTurn
//!                          └─create_run Err: abort returned lock            └─Failure──► abort     │      barrier(payload(ctx))
//!                              abandonment (drop revokes)                   (no commit)           │         │
//!                                                                                               ▼
//!                                                                                        CommittedResponse
//! ```
//!
//! Only a [`CommittedResponse`] authorizes emitting the terminal frame of
//! a turn (SSE `[DONE]`, final body, A2A artifact). Mid-turn streaming
//! events are not gated.
//!
//! ## Named invariants
//!
//! - **I1** Write-once per path: a manifest-referenced file is never
//!   opened for write; [`Manifest::declare`] rejects duplicates.
//! - **I2** [`HolderId`] freshness is the second fence: minted per
//!   acquire attempt, so commit/heartbeat predicates keep Postgres
//!   linearizing commits even if a failover regresses the epoch.
//! - **I3** Epoch-scope GC deletion: a sweep deletes only under epochs
//!   strictly below its keep-set's claims-row read.
//! - **I4** Commit = fsync data files (concurrent) → one atomic S3; the
//!   manifest lives in the row, so publication has no pointer-flip
//!   window.
//! - **I5** Fail-stop on PG-down: no degraded mode.
//! - **I6** All lease math uses `clock_timestamp()`, never `now()`
//!   (verified against PG 18 docs 2026-08-23).
//! - **I7** READ COMMITTED pinned (the default); never DELETE claim rows.
//!
//! ## Configuration
//!
//! Env-configured deployment infrastructure, one instance per server,
//! exactly like the session store (`crates/aura-config/src/session_store.rs`):
//!
//! | Env var | Meaning |
//! | ------- | ------- |
//! | `AURA_SESSION_ADMISSION` | `off` (default) or `pg` |
//! | `AURA_SESSION_ADMISSION_PG_URL` | connection URL (required in `pg` mode) |
//! | `AURA_SESSION_ADMISSION_BEAT_INTERVAL_MS` | heartbeat interval (default 5000) |
//! | `AURA_SESSION_ADMISSION_LEASE_TTL_MS` | server-side lease ttl (default 15000) |
//! | `AURA_SESSION_ADMISSION_FENCE_MARGIN_MS` | self-fence margin (default 250) |
//! | `AURA_SESSION_ADMISSION_RETRY_AFTER_MS` | `Busy` retry hint (default 1000) |
//! | `AURA_SESSION_ADMISSION_PROPAGATION_WINDOW_MS` | read-miss retry window (default 30000) |
//!
//! Build the backend with [`build_admission`] — `off` yields a
//! [`LocalAdmission`] (arbiter-only), `pg` a [`PgAdmission`] against the
//! claims table at the configured URL. The factory shares one internal
//! arbiter and one pod identity across the process. Build it once per
//! server and share the product: a second factory call creates a second,
//! independent arbiter, which would void same-process exclusion in `off`
//! mode (the row-lock election still backstops `pg` mode).

#![allow(dead_code)]
// session-guard skeleton rev 12 (Postgres-fenced claims fold): remove
// slices as bodies fill (aura #421 follow-up).

mod adapters;
mod arbiter;
mod claim;
mod config;
mod epoch;
mod gc;
mod identity;
mod lease;
mod manifest;
mod repair;
mod scratchpad;
mod state;
mod store;

pub use adapters::{LocalAdmission, PgAdmission};
pub use claim::{HolderView, Locality};
pub use config::{AdmissionConfigError, AdmissionEnv, AdmissionMode, PgUrl};
pub use epoch::{Epoch, epoch_dir, session_dir};
pub use identity::{
    HolderId, InvalidHolderId, InvalidOpId, InvalidPodId, InvalidSessionId, InvalidTurnId, OpId,
    PodId, SessionId, TurnId,
};
pub use lease::{
    BeatInterval, LeaseDeadline, LeaseLost, LeaseState, LeaseTtl, SelfFenceMargin, WriteCapability,
};
pub use manifest::{
    ArtifactPath, DeclareError, Digest, InvalidArtifactPath, InvalidDigest, Manifest,
    ManifestEntry, ReadMiss,
};
pub use scratchpad::{InvalidScratchpadName, ScratchpadError, ScratchpadName};
pub use state::{
    ActiveTurn, AdmissionError, ArtifactWriteError, BarrierError, CleanupOutcome, CommitContext,
    CommitKind, CommitRejection, CommittedResponse, CommittingTurn, CreateRunError, FenceCause,
    FencedRun, HeldLock, IdleRequest, IndeterminateCause, ReadError, ReleaseError, TurnEnd,
    VerifiedRead,
};
pub use store::StoreUnavailable;

use std::path::PathBuf;
use std::sync::Arc;

use crate::identity::PodId as FactoryPodId;

/// Build the deployment's admission backend from validated config: one
/// shared arbiter, one backend, selected by mode. The single factory —
/// backend constructors require a mode-proof narrowed from `env`, so a
/// [`LocalAdmission`] can never be built from a `pg` config. `root` is
/// the session-store root: every claim binds its session dir under it at
/// admission, so a claim can never create or sweep beneath another
/// session's root. `pod` is this pod's identity (the `holder_pod`
/// column).
///
/// # Errors
/// [`AdmissionConfigError`] when the narrowed mode proof is unavailable
/// for the configured mode (for `pg`: URL missing — though
/// [`AdmissionEnv::from_env`] rejects that earlier).
pub fn build_admission(
    env: &AdmissionEnv,
    root: PathBuf,
    pod: FactoryPodId,
) -> Result<Arc<dyn TurnAdmission>, AdmissionConfigError> {
    let arbiter = crate::arbiter::SessionArbiter::new();
    match env.mode() {
        AdmissionMode::Off => {
            let local = env.local().ok_or_else(|| {
                AdmissionConfigError("build_admission: local config unavailable".to_string())
            })?;
            Ok(Arc::new(LocalAdmission::new(pod, root, arbiter, local)))
        }
        AdmissionMode::Pg => {
            let pg = env.pg().ok_or_else(|| {
                AdmissionConfigError("build_admission: pg config unavailable".to_string())
            })?;
            let store: Arc<dyn crate::store::ClaimStore> =
                Arc::new(crate::adapters::pg::PgStore::new(pg.pg_url()));
            let repair = crate::repair::build_repair_lane();
            Ok(Arc::new(PgAdmission::new(
                store, pod, root, arbiter, pg, repair,
            )))
        }
    }
}

/// The port: everything the web server and HITL routing consume. Steals
/// are adapter-internal (the S1 statement subsumes them); callers see
/// ordinary admission outcomes, including [`AdmissionError::Parked`]
/// when a session waits on an external driver.
#[async_trait::async_trait]
pub trait TurnAdmission: Send + Sync {
    /// Fresh admission: atomically claim the session (S1), or fail
    /// [`AdmissionError::Busy`] / [`AdmissionError::Parked`] if a live
    /// claim or a park latch exists (a lease-expired claim is stolen
    /// atomically inside the same statement).
    async fn admit(&self, req: IdleRequest) -> Result<HeldLock, AdmissionError>;

    /// Fresh read-only holder lookup for HITL routing, callable from any
    /// instance. Only a live lease reports a holder.
    async fn locate_holder(&self, session: &SessionId) -> std::io::Result<Option<HolderView>>;
}
