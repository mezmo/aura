//! The consuming state chain: admission → held claim → fenced run →
//! active turn → committing → committed response. Types consume their
//! predecessor by value, so each stage exists only while the turn owns
//! it. Admission output is one sealed [`AcquiredClaim`] — the claim
//! identity and its bound release arrive together, and the lease derives
//! from the same identity inside `into_held`, so
//! identity, lease, and release cannot disagree.
//!
//! The claim identity is the fence triple `(session, epoch, holder)` —
//! every Postgres mutation predicates on it (invariant I2), and every
//! local capability names it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::arbiter::HeldGuard;
use crate::epoch::Epoch;
use crate::identity::{HolderId, OpId, PodId, SessionId, TurnId};
use crate::lease::{
    BeatInterval, HeartbeatLease, LeaseLost, LeaseTtl, SelfFenceMargin, WriteCapability,
};
use crate::repair::RepairLane;
use crate::store::StoreUnavailable;

/// The complete output of a successful admission (adapter-side,
/// crate-internal): the claim identity plus the release action bound to
/// that exact claim at construction. The lease is derived from the same
/// identity inside `into_held`. (The closure's captured fence triple
/// cannot be proven by types — closures are opaque; that binding is the
/// adapter's contract, tested in Layer 2 and recorded as residual risk.)
pub(crate) struct AcquiredClaim {
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
    pod: PodId,
    release: ReleaseAction,
    lease_plan: LeasePlan,
    repair: Option<Arc<dyn RepairLane>>,
}

/// How the lease for this claim renews. Chosen by the adapter at
/// acquisition (local = static, pg = heartbeat), NOT by the caller at
/// assembly — so the Postgres backend cannot silently take a
/// never-revoking static lease.
pub(crate) enum LeasePlan {
    /// No renewal (local admission): revocation is the only end.
    Static,
    /// Lease-owned heartbeat loop over an adapter S2 write.
    Heartbeat {
        /// Beat cadence (config).
        beat: BeatInterval,
        /// Server-side lease ttl (config).
        ttl: LeaseTtl,
        /// Self-fence margin (config).
        margin: SelfFenceMargin,
        /// One S2 renewal.
        write: Box<RenewalWrite>,
    },
}

/// Renewal inputs for a Postgres heartbeat lease.
pub(crate) struct HeartbeatRenewal {
    pub(crate) beat: BeatInterval,
    pub(crate) ttl: LeaseTtl,
    pub(crate) margin: SelfFenceMargin,
    pub(crate) write: Box<RenewalWrite>,
}

/// One adapter renewal write (one S2). Renewal failure of any shape —
/// Lost (zero rows) or store-unavailable — maps to an error here, so the
/// lease fails closed either way (I5).
pub(crate) type RenewalWriteFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<(), std::io::Error>> + Send>>;
pub(crate) type RenewalWrite = dyn FnMut() -> RenewalWriteFuture + Send;

impl AcquiredClaim {
    /// Assemble a local (static-lease) claim (adapter-side). Epoch and
    /// holder are nominal in local mode (no authority to fence against)
    /// but are still minted, so every downstream identity is real. The
    /// `proof` token's constructor is private to `adapters::local`, so
    /// no other backend can build a static-lease claim. The release
    /// action must close over this claim's identity; that binding is the
    /// adapter's contract, tested in Layer 2.
    pub(crate) fn new_local(
        _proof: crate::adapters::local::LocalLeaseProof,
        session: SessionId,
        turn: TurnId,
        pod: PodId,
        release: ReleaseAction,
    ) -> Self {
        Self {
            session,
            turn,
            epoch: Epoch::initial(),
            holder: HolderId::mint(),
            pod,
            release,
            lease_plan: LeasePlan::Static,
            repair: None,
        }
    }

    /// Assemble a Postgres-fenced (heartbeat-lease) claim (adapter-side).
    /// The `proof` token's constructor is private to `adapters::pg`, so
    /// no other backend can build a heartbeat-lease claim. `write`
    /// performs one S2 renewal and must return only after the statement
    /// completes (the same class of crate-internal contract as the
    /// release closure; residual risk, Layer-2 tested).
    #[expect(clippy::too_many_arguments, reason = "one claim identity bundle")]
    pub(crate) fn new_pg(
        _proof: crate::adapters::pg::PgLeaseProof,
        session: SessionId,
        turn: TurnId,
        epoch: Epoch,
        holder: HolderId,
        pod: PodId,
        release: ReleaseAction,
        renewal: HeartbeatRenewal,
        repair: Arc<dyn RepairLane>,
    ) -> Self {
        let HeartbeatRenewal {
            beat,
            ttl,
            margin,
            write,
        } = renewal;
        Self {
            session,
            turn,
            epoch,
            holder,
            pod,
            release,
            lease_plan: LeasePlan::Heartbeat {
                beat,
                ttl,
                margin,
                write,
            },
            repair: Some(repair),
        }
    }

    /// Consume the claim into a held lock. The lease is built from this
    /// claim's own identity and its adapter-chosen lease plan inside this
    /// call, so identity, lease, and release cannot disagree, and the
    /// lease kind is never caller-selected.
    pub(crate) fn into_held(self, arbiter: HeldGuard) -> HeldLock {
        let Self {
            session,
            turn,
            epoch,
            holder,
            pod,
            release,
            lease_plan,
            repair,
        } = self;
        let source = crate::lease::ClaimLeaseSource {
            session: session.clone(),
            turn,
            epoch,
            holder,
        };
        let lease = match lease_plan {
            LeasePlan::Static => HeartbeatLease::static_from(source),
            LeasePlan::Heartbeat {
                beat,
                ttl,
                margin,
                write,
            } => HeartbeatLease::with_heartbeat(source, beat, ttl, margin, write),
        };
        HeldLock::from_parts(
            session, turn, epoch, holder, pod, release, arbiter, lease, repair,
        )
    }
}

/// Zero-argument release bound to one claim at construction (one S4).
pub(crate) type ReleaseAction = Box<dyn FnOnce() -> ReleaseFuture + Send>;

pub(crate) type ReleaseFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<(), ReleaseError>> + Send>>;

/// Why admission failed. Only [`AdmissionError::Busy`] is contention: it
/// maps to HTTP 503 + `Retry-After` and is the sole LB-retryable variant.
/// [`AdmissionError::Parked`] is deliberately not retryable on a hint —
/// the HITL wait is unbounded and routing must surface it honestly.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// A live claim exists for the session.
    #[error("session busy: held by pod {holder}")]
    Busy {
        /// Which pod holds the session, for routing and honest hints.
        holder: PodId,
        /// How long the caller should wait before retrying (configured
        /// fixed hint — a zero-row S1 carries no deadline; codex M7).
        retry_after: std::time::Duration,
    },
    /// The session is parked on a different turn, awaiting an external
    /// driver (HITL). Not contention; no retry hint.
    #[error("session parked on turn {turn} (awaiting external resolution)")]
    Parked {
        /// The turn the session is parked on.
        turn: TurnId,
    },
    /// The claim authority is unreachable. Fail-stop (I5): the claim
    /// path fails loudly rather than degrading.
    #[error(transparent)]
    StoreUnavailable(#[from] StoreUnavailable),
    /// The session root is absent or not provisioned (cold-deploy wedge).
    /// Operator-facing, not retryable by the LB. Diagnostic-only payload.
    #[error("session root not provisioned: {0}")]
    NotProvisioned(String),
    /// Claim-path I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A request awaiting admission, parsed and ready. Built by the ingress
/// seam after `SessionId` validation, before any visible status. The
/// acting pod is the backend's own configured identity, not caller
/// input.
#[derive(Debug)]
#[must_use]
pub struct IdleRequest {
    /// The session to claim.
    pub session: SessionId,
    /// The turn this request will run, fixed here so the claim names the
    /// turn even under client retries (and so reify presents the parked
    /// turn id).
    pub turn: TurnId,
}

/// A claim is held for the session. Exists only while the turn owns it;
/// the arbiter guard inside keeps same-instance requests out.
#[must_use]
pub struct HeldLock {
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
    pod: PodId,
    // Drop order matters: the lease (first) revokes before the arbiter
    // slot (second) frees, so no window exists where another local
    // request acquires the slot while a capability still reads Live.
    lease: HeartbeatLease,
    arbiter: HeldGuard,
    release: ReleaseAction,
    /// The writability-wedge cure (H4) available to `create_run`'s fill:
    /// `Some` under the Postgres backend, `None` in local mode (no
    /// shared mount, no delegation to cure).
    repair: Option<Arc<dyn RepairLane>>,
}

impl std::fmt::Debug for HeldLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldLock")
            .field("session", &self.session)
            .field("turn", &self.turn)
            .field("epoch", &self.epoch)
            .field("holder", &self.holder)
            .field("pod", &self.pod)
            .finish_non_exhaustive()
    }
}

impl HeldLock {
    /// Assemble from parts (private: only `into_held` calls this,
    /// which is what binds lease to claim by construction).
    #[expect(clippy::too_many_arguments, reason = "one claim identity bundle")]
    fn from_parts(
        session: SessionId,
        turn: TurnId,
        epoch: Epoch,
        holder: HolderId,
        pod: PodId,
        release: ReleaseAction,
        arbiter: HeldGuard,
        lease: HeartbeatLease,
        repair: Option<Arc<dyn RepairLane>>,
    ) -> Self {
        Self {
            session,
            turn,
            epoch,
            holder,
            pod,
            lease,
            arbiter,
            release,
            repair,
        }
    }

    /// Who holds the session right now (this process, no store read).
    #[must_use]
    pub fn holder_view(&self) -> crate::claim::HolderView {
        crate::claim::HolderView::here(self.pod.clone())
    }

    /// The write capability bound to this claim.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        self.lease.capability()
    }

    /// Current lease liveness (diagnostics/metrics).
    #[must_use]
    pub fn lease_state(&self) -> crate::lease::LeaseState {
        self.lease.state()
    }

    /// Create and bind this claim's run directory in one step: the
    /// capability is asserted, the epoch dir `e{k}/` derived *from this
    /// claim's epoch* (the seam no longer chooses the layout — the
    /// epoch-partition ruling makes it structural), created by this
    /// lock, and liveness re-checked after creation — so the fenced run
    /// can never carry a directory authorized by a different claim or an
    /// already-lost lease. Non-recursive (the session directory must
    /// already exist); `AlreadyExists` is a hard error, not retried.
    ///
    /// Fill note (H4): an `EROFS` from a wedged post-crash delegation
    /// escalates through `self.repair` (`force_cure` on the session
    /// dir, 518 ms measured) and retries once before failing.
    ///
    /// # Errors
    /// [`CreateRunError`] *returns the lock* in every variant so the
    /// caller can [`abort`](Self::abort) cleanly instead of dropping
    /// into the abandonment window. [`CreateRunError::LostAfterCreate`]
    /// additionally carries the created directory's path so the caller
    /// can remove or quarantine it before aborting — otherwise a fixed
    /// turn id could retry into `AlreadyExists` forever.
    pub async fn create_run(self, session_root: &Path) -> Result<FencedRun, CreateRunError> {
        let capability = self.lease.capability();
        if let Err(cause) = capability.assert_live() {
            return Err(CreateRunError::NotLive {
                cause: cause.into(),
                lock: self,
            });
        }
        let path = crate::epoch::epoch_dir(session_root, self.epoch);
        if let Err(cause) = tokio::fs::create_dir(&path).await {
            return Err(CreateRunError::Create { cause, lock: self });
        }
        if capability.assert_live().is_err() {
            return Err(CreateRunError::LostAfterCreate {
                lock: self,
                run_dir: path,
            });
        }
        Ok(FencedRun {
            lock: self,
            run_dir: path,
        })
    }

    /// Early cleanup: the seam failed between admission and the turn
    /// (persistence init, run-dir creation). Revokes the lease and
    /// releases the claim.
    pub async fn abort(self) -> Result<(), ReleaseError> {
        todo!("fill: lease stop + release; aura #421 follow-up")
    }
}

/// `create_run` failed; every variant returns the lock for clean abort.
#[derive(Debug)]
pub enum CreateRunError {
    /// The capability was already lost before anything was created.
    NotLive {
        /// Why liveness failed.
        cause: FenceCause,
        /// The still-held lock.
        lock: HeldLock,
    },
    /// Directory creation failed; nothing was created.
    Create {
        /// The filesystem error.
        cause: std::io::Error,
        /// The still-held lock.
        lock: HeldLock,
    },
    /// The directory was created, then the lease was found lost. The
    /// path is carried so the caller can remove or quarantine the
    /// orphaned directory before aborting — a bare `abort` would leave
    /// it, and a fixed turn id retrying would hit `AlreadyExists`.
    LostAfterCreate {
        /// The still-held lock.
        lock: HeldLock,
        /// The directory that was created.
        run_dir: PathBuf,
    },
}

/// The run directory exists and was created under this claim's authority
/// by [`HeldLock::create_run`]. Isolation only — exclusion is the claim,
/// not the directory.
#[derive(Debug)]
#[must_use]
pub struct FencedRun {
    lock: HeldLock,
    run_dir: PathBuf,
}

impl FencedRun {
    /// The fenced run directory (this claim's epoch dir).
    #[must_use]
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The write capability bound to this claim (persistence checks it
    /// before every write).
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        self.lock.capability()
    }

    /// Arm the turn: persistence init (with the capability) happens at
    /// the seam between this call and the first write; a failure there
    /// calls [`abort`](ActiveTurn::abort) on the result.
    pub fn activate(self) -> ActiveTurn {
        ActiveTurn { run: self }
    }
}

/// A live turn. Terminal paths are exactly two:
/// [`complete`](Self::complete) with a [`CommitKind`] (Success or
/// Clarification — both commit), or [`abort`](Self::abort) (Failure and
/// mid-turn cancellation — quarantine, no commit). "Success/Clarification
/// commit, Failure aborts" is unrepresentable-to-violate: no failure
/// value can reach the barrier.
#[derive(Debug)]
#[must_use]
pub struct ActiveTurn {
    run: FencedRun,
}

impl ActiveTurn {
    /// The write capability bound to this claim (persistence checks it
    /// before every write).
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        self.run.capability()
    }
}

/// The committable terminal outcome of a turn (codex B5: the trait
/// speaks in commit kinds, and Failure is not one — it aborts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitKind {
    /// The turn produced an answer.
    Success,
    /// The turn ended in a clarification or direct response (commits
    /// like success: the artifact set includes the clarification
    /// record).
    Clarification,
}

impl std::fmt::Display for CommitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitKind::Success => f.write_str("success"),
            CommitKind::Clarification => f.write_str("clarification"),
        }
    }
}

impl ActiveTurn {
    /// End the turn and enter the completion barrier. After this call
    /// the response is not yet authorized — only the barrier's
    /// [`CommittedResponse`] is.
    pub fn complete(self, kind: CommitKind) -> CommittingTurn {
        CommittingTurn {
            run: self.run,
            kind,
        }
    }

    /// Abort mid-turn (failure, client disconnect, cancellation):
    /// quarantine the run, stop the lease, release the claim. Distinct
    /// from abandonment: the process is alive to clean up. Nothing
    /// commits — the turn's files stay unreferenced debris for GC.
    pub async fn abort(self) -> Result<(), ReleaseError> {
        todo!("fill: quarantine + lease stop + release; aura #421 follow-up")
    }
}

/// Context handed to the injected commit step. Assembled by the barrier
/// from the turn's own state; fields are private with read accessors, so
/// a context is not constructible or forgeable outside the crate. The
/// barrier mints the [`OpId`] at assembly: one barrier run is one
/// logical commit, and every retry of its S3 reuses this id (B3).
#[derive(Debug, Clone)]
pub struct CommitContext {
    run_dir: PathBuf,
    kind: CommitKind,
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
    op: OpId,
}

impl CommitContext {
    /// The fenced run directory (where the turn's artifacts live).
    #[must_use]
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The committable outcome.
    #[must_use]
    pub const fn kind(&self) -> CommitKind {
        self.kind
    }

    /// The session.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The turn.
    #[must_use]
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// The claim epoch that authorized the turn.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The acquire attempt that authorized the turn.
    #[must_use]
    pub const fn holder(&self) -> HolderId {
        self.holder
    }

    /// The logical commit id (commit-unknown reconciliation key, B3).
    #[must_use]
    pub const fn op(&self) -> OpId {
        self.op
    }
}

/// How the barrier's cleanup fared when the commit step failed.
#[derive(Debug)]
pub struct CleanupOutcome {
    /// Whether the run was quarantined.
    pub quarantined: bool,
    /// The claim release result.
    pub release: Result<(), ReleaseError>,
}

/// The completion barrier: commit → release → authorize. The commit step
/// is injected and lazy — it receives the context and runs only inside
/// the barrier, so no commit work can precede admission-controlled
/// ordering.
#[derive(Debug)]
#[must_use]
pub struct CommittingTurn {
    run: FencedRun,
    kind: CommitKind,
}

impl CommittingTurn {
    /// Run the barrier: invoke `commit` (lazy, context-fed), stop the
    /// heartbeat lease, release the claim, then authorize the response.
    ///
    /// # Errors
    /// [`BarrierError::CommitFailed`] — the commit step failed; carries
    /// the commit error plus the [`CleanupOutcome`] (quarantine and
    /// release results) so no cleanup failure is silently lost.
    /// [`BarrierError::Release`] — release failed after a durable
    /// commit; the authorized response travels with the error (data is
    /// durable) and the session is flagged wedged.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub async fn barrier<T, E, F, Fut>(
        self,
        commit: F,
    ) -> Result<CommittedResponse<T>, BarrierError<T, E>>
    where
        F: FnOnce(CommitContext) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        todo!("fill: mint OpId, commit(ctx) → lease stop → release → response; aura #421 follow-up")
    }
}

/// Why the barrier failed.
#[derive(Debug, thiserror::Error)]
pub enum BarrierError<T, E> {
    /// The injected commit step failed. The run was quarantined (if
    /// possible) and the claim released (if possible); both outcomes are
    /// in `cleanup`, so a failed cleanup is never silently dropped.
    #[error("commit failed; quarantine={}, released={}", cleanup.quarantined, cleanup.release.is_ok())]
    CommitFailed {
        /// The commit step's own error.
        #[source]
        error: E,
        /// Quarantine and release results.
        cleanup: CleanupOutcome,
    },
    /// The claim release failed after a durable commit. The response is
    /// authorized (carried here; delivery is the caller's call); the
    /// session needs operator attention.
    #[error("release failed after commit; session wedged")]
    Release {
        /// The authorized response.
        response: CommittedResponse<T>,
        /// The release failure.
        #[source]
        error: ReleaseError,
    },
}

impl<T, E> BarrierError<T, E> {
    /// The commit error, if this was a commit failure.
    #[must_use]
    pub fn commit_error(&self) -> Option<&E> {
        match self {
            Self::CommitFailed { error, .. } => Some(error),
            Self::Release { .. } => None,
        }
    }

    /// Take the authorized response out of a release failure, if this
    /// was one. Delivery after a wedge is the caller's decision.
    #[must_use]
    pub fn into_release_response(self) -> Option<CommittedResponse<T>> {
        match self {
            Self::Release { response, .. } => Some(response),
            Self::CommitFailed { .. } => None,
        }
    }
}

/// Sole authorization to emit a terminal frame. Carries the committed
/// payload bound to session + turn + epoch + holder. Not `Clone`; the
/// payload is only reachable through authorization-preserving
/// transforms, so the envelope cannot be discarded short of consuming
/// the whole value.
#[derive(Debug)]
pub struct CommittedResponse<T> {
    payload: T,
    session: SessionId,
    turn: TurnId,
    epoch: Epoch,
    holder: HolderId,
}

impl<T> CommittedResponse<T> {
    /// Build the authorization (private to this module: only the barrier
    /// body constructs it).
    fn new(payload: T, session: SessionId, turn: TurnId, epoch: Epoch, holder: HolderId) -> Self {
        Self {
            payload,
            session,
            turn,
            epoch,
            holder,
        }
    }

    /// Transform the payload while preserving the authorization envelope.
    /// The only public way to reach the payload's value.
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> CommittedResponse<U> {
        CommittedResponse {
            payload: f(self.payload),
            session: self.session,
            turn: self.turn,
            epoch: self.epoch,
            holder: self.holder,
        }
    }

    /// Consume the authorization, yielding the payload and its identity
    /// to the terminal emitter that consumes this value.
    #[must_use]
    pub fn into_parts(self) -> (T, SessionId, TurnId, Epoch, HolderId) {
        (
            self.payload,
            self.session,
            self.turn,
            self.epoch,
            self.holder,
        )
    }

    /// Turn identity for logging and metrics.
    #[must_use]
    pub fn turn(&self) -> TurnId {
        self.turn
    }

    /// Session identity for logging and metrics.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

/// Why a claim release failed.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The claim's fence triple no longer matches (stolen during
    /// shutdown). Not fixable by force — the new owner's claim stands.
    #[error("claim superseded (epoch {epoch}, holder {holder} retired)")]
    Superseded {
        /// The epoch that was retired.
        epoch: Epoch,
        /// The acquire attempt that was retired.
        holder: HolderId,
    },
    /// The claim authority is unreachable (fail-stop, I5).
    #[error(transparent)]
    StoreUnavailable(#[from] StoreUnavailable),
    /// Filesystem failure while releasing (quarantine path).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Why a fence or write path failed.
#[derive(Debug, thiserror::Error)]
pub enum FenceCause {
    /// The backing claim lease was lost; writes were stopped.
    #[error(transparent)]
    LeaseLost(#[from] LeaseLost),
    /// Filesystem rejection (e.g. EROFS on the shared mount — the H4
    /// wedge class).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
