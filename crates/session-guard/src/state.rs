//! The consuming state chain: admission → held claim → fenced run →
//! active turn → committing → committed response. Types consume their
//! predecessor by value, so each stage exists only while the turn owns
//! it. Admission output is one sealed [`AcquiredClaim`] — the claim
//! identity and its bound release arrive together, and the lease derives
//! from the same identity inside `into_held`, so
//! identity, lease, and release cannot disagree.

use std::future::Future;
use std::path::{Path, PathBuf};

use crate::arbiter::HeldGuard;
use crate::claim::Generation;
use crate::identity::{InstanceId, SessionId, TurnId};
use crate::lease::{HeartbeatLease, LeaseLost, WriteCapability};

/// The complete output of a successful admission (adapter-side,
/// crate-internal): the claim identity plus the release action bound to
/// that exact claim at construction. The lease is derived from the same
/// identity inside `into_held`. (The closure's captured path
/// cannot be proven by types — closures are opaque; that binding is the
/// adapter's contract, tested in Layer 2 and recorded as residual risk.)
pub(crate) struct AcquiredClaim {
    session: SessionId,
    turn: TurnId,
    holder: InstanceId,
    generation: Generation,
    release: ReleaseAction,
    lease_plan: LeasePlan,
}

/// How the lease for this claim renews. Chosen by the adapter at
/// acquisition (local = static, claim-file = heartbeat), NOT by the
/// caller at assembly — so the claim-file backend cannot silently take a
/// never-revoking static lease.
pub(crate) enum LeasePlan {
    /// No renewal (local admission): revocation is the only end.
    Static,
    /// Lease-owned heartbeat loop over an adapter renewal write.
    Heartbeat {
        beat: crate::lease::BeatInterval,
        write: Box<RenewalWrite>,
    },
}

/// One adapter renewal write (the claim-body rewrite).
pub(crate) type RenewalWriteFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<(), std::io::Error>> + Send>>;
pub(crate) type RenewalWrite = dyn FnMut() -> RenewalWriteFuture + Send;

impl AcquiredClaim {
    /// Assemble a local (static-lease) claim (adapter-side). The release
    /// action must close over this claim's file; that binding is the
    /// adapter's contract, tested in Layer 2.
    pub(crate) fn new_local(
        session: SessionId,
        turn: TurnId,
        holder: InstanceId,
        generation: Generation,
        release: ReleaseAction,
    ) -> Self {
        Self {
            session,
            turn,
            holder,
            generation,
            release,
            lease_plan: LeasePlan::Static,
        }
    }

    /// Assemble a claim-file (heartbeat-lease) claim (adapter-side).
    /// `write` performs one renewal and must return only after the
    /// claim-body write completes (the same class of crate-internal
    /// contract as the release closure; residual risk, Layer-2 tested).
    pub(crate) fn new_with_heartbeat(
        session: SessionId,
        turn: TurnId,
        holder: InstanceId,
        generation: Generation,
        release: ReleaseAction,
        beat: crate::lease::BeatInterval,
        write: Box<RenewalWrite>,
    ) -> Self {
        Self {
            session,
            turn,
            holder,
            generation,
            release,
            lease_plan: LeasePlan::Heartbeat { beat, write },
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
            holder,
            generation,
            release,
            lease_plan,
        } = self;
        let source = crate::lease::ClaimLeaseSource {
            session: session.clone(),
            turn,
            generation,
        };
        let lease = match lease_plan {
            LeasePlan::Static => HeartbeatLease::static_from(source),
            LeasePlan::Heartbeat { beat, write } => {
                HeartbeatLease::with_heartbeat(source, beat, write)
            }
        };
        HeldLock::from_parts(session, turn, holder, generation, release, arbiter, lease)
    }
}

/// Zero-argument release bound to one claim at construction.
pub(crate) type ReleaseAction = Box<dyn FnOnce() -> ReleaseFuture + Send>;

pub(crate) type ReleaseFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<(), ReleaseError>> + Send>>;

/// Why admission failed. Only [`AdmissionError::Busy`] is contention: it
/// maps to HTTP 503 + `Retry-After` and is the sole LB-retryable variant.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// A live claim exists for the session.
    #[error("session busy: held by {holder}")]
    Busy {
        /// Who holds the session, for routing and honest hints.
        holder: InstanceId,
        /// How long the caller should wait before retrying.
        retry_after: std::time::Duration,
    },
    /// A steal raced a revival or another steal. Retryable, but distinct
    /// from `Busy`.
    #[error("contention lost during steal")]
    ContentionLost,
    /// The claim root is absent or not provisioned (cold-deploy wedge).
    /// Operator-facing, not retryable by the LB. Diagnostic-only payload.
    #[error("claim root not provisioned: {0}")]
    NotProvisioned(String),
    /// Claim-store I/O failure (including invalid claim content, mapped
    /// deliberately with its source preserved).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A request awaiting admission, parsed and ready. Built by the ingress
/// seam after `SessionId` validation, before any visible status. The
/// acting instance is the backend's own configured identity, not caller
/// input.
#[derive(Debug)]
#[must_use]
pub struct IdleRequest {
    /// The session to claim.
    pub session: SessionId,
    /// The turn this request will run, fixed here so claim bodies name
    /// the turn even under client retries.
    pub turn: TurnId,
}

/// A claim is held for the session. Exists only while the turn owns it;
/// the arbiter guard inside keeps same-instance requests out.
#[must_use]
pub struct HeldLock {
    session: SessionId,
    turn: TurnId,
    holder: InstanceId,
    generation: Generation,
    // Drop order matters: the lease (first) revokes before the arbiter
    // slot (second) frees, so no window exists where another local
    // request acquires the slot while a capability still reads Live.
    lease: HeartbeatLease,
    arbiter: HeldGuard,
    release: ReleaseAction,
}

impl std::fmt::Debug for HeldLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldLock")
            .field("session", &self.session)
            .field("turn", &self.turn)
            .field("holder", &self.holder)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl HeldLock {
    /// Assemble from parts (private: only `into_held` calls this,
    /// which is what binds lease to claim by construction).
    fn from_parts(
        session: SessionId,
        turn: TurnId,
        holder: InstanceId,
        generation: Generation,
        release: ReleaseAction,
        arbiter: HeldGuard,
        lease: HeartbeatLease,
    ) -> Self {
        Self {
            session,
            turn,
            holder,
            generation,
            lease,
            arbiter,
            release,
        }
    }

    /// Who holds the session right now (this process, no disk read).
    #[must_use]
    pub fn holder_view(&self) -> crate::claim::HolderView {
        crate::claim::HolderView::here(self.holder.clone())
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
    /// capability is asserted, the directory created *by this lock*, and
    /// liveness re-checked after creation — so the fenced run can never
    /// carry a directory authorized by a different claim or an
    /// already-lost lease. Non-recursive (the session directory must
    /// already exist); `AlreadyExists` is a hard error, not retried.
    ///
    /// # Errors
    /// [`CreateRunError`] carries the [`FenceCause`] and *returns the
    /// lock* so the caller can [`abort`](Self::abort) cleanly instead of
    /// dropping into the abandonment window.
    pub async fn create_run(self, path: PathBuf) -> Result<FencedRun, CreateRunError> {
        let capability = self.lease.capability();
        match capability.assert_live() {
            Ok(()) => {}
            Err(cause) => {
                return Err(CreateRunError {
                    cause: cause.into(),
                    lock: self,
                });
            }
        }
        if let Err(cause) = tokio::fs::create_dir(&path).await {
            return Err(CreateRunError {
                cause: FenceCause::Io(cause),
                lock: self,
            });
        }
        if let Err(cause) = capability.assert_live() {
            return Err(CreateRunError {
                cause: cause.into(),
                lock: self,
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

/// `create_run` failed; the lock is returned for clean abort.
#[derive(Debug)]
pub struct CreateRunError {
    /// Why creation failed.
    pub cause: FenceCause,
    /// The still-held lock; call [`HeldLock::abort`] (a bare drop
    /// revokes but abandons the claim to the staleness window).
    pub lock: HeldLock,
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
    /// The fenced run directory.
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

/// A live turn. Terminal outcomes — success, failure, clarification —
/// all flow through [`complete`](Self::complete); nothing else may
/// follow.
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

/// The terminal outcome of a turn. Every path commits, including
/// clarification (which today writes no manifest — this fixes that).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The turn produced an answer.
    Success,
    /// The turn failed.
    Failure,
    /// The turn ended in a clarification or direct response.
    Clarification,
}

impl ActiveTurn {
    /// End the turn and enter the completion barrier. After this call
    /// the response is not yet authorized — only the barrier's
    /// [`CommittedResponse`] is.
    pub fn complete(self, outcome: TurnOutcome) -> CommittingTurn {
        CommittingTurn {
            run: self.run,
            outcome,
        }
    }

    /// Abort mid-turn (client disconnect, cancellation): quarantine the
    /// run, stop the lease, release the claim. Distinct from abandonment:
    /// the process is alive to clean up.
    pub async fn abort(self) -> Result<(), ReleaseError> {
        todo!("fill: quarantine + lease stop + release; aura #421 follow-up")
    }
}

/// Context handed to the injected commit step. Assembled by the barrier
/// from the turn's own state; fields are private with read accessors, so
/// a context is not constructible or forgeable outside the crate.
#[derive(Debug, Clone)]
pub struct CommitContext {
    run_dir: PathBuf,
    outcome: TurnOutcome,
    session: SessionId,
    turn: TurnId,
    generation: Generation,
}

impl CommitContext {
    /// The fenced run directory (where manifests go).
    #[must_use]
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The terminal outcome.
    #[must_use]
    pub const fn outcome(&self) -> TurnOutcome {
        self.outcome
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

    /// The claim incarnation that authorized the turn.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
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
    outcome: TurnOutcome,
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
        todo!("fill: commit(ctx) → lease stop → release → response; aura #421 follow-up")
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
/// payload bound to session + turn + incarnation. Not `Clone`; the
/// payload is only reachable through authorization-preserving
/// transforms, so the envelope cannot be discarded short of consuming
/// the whole value.
#[derive(Debug)]
pub struct CommittedResponse<T> {
    payload: T,
    session: SessionId,
    turn: TurnId,
    generation: Generation,
}

impl<T> CommittedResponse<T> {
    /// Build the authorization (private to this module: only the barrier
    /// body constructs it).
    fn new(payload: T, session: SessionId, turn: TurnId, generation: Generation) -> Self {
        Self {
            payload,
            session,
            turn,
            generation,
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
            generation: self.generation,
        }
    }

    /// Consume the authorization, yielding the payload and its identity
    /// to the terminal emitter that consumes this value.
    #[must_use]
    pub fn into_parts(self) -> (T, SessionId, TurnId, Generation) {
        (self.payload, self.session, self.turn, self.generation)
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
    /// The claim's incarnation no longer matches (stolen during
    /// shutdown). Not fixable by force — the new owner's claim stands.
    #[error("claim superseded (incarnation {0} retired)")]
    Superseded(Generation),
    /// Filesystem failure while releasing.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Why a fence or write path failed.
#[derive(Debug, thiserror::Error)]
pub enum FenceCause {
    /// The backing claim lease was lost; writes were stopped.
    #[error(transparent)]
    LeaseLost(#[from] LeaseLost),
    /// Filesystem rejection (e.g. EROFS on the shared mount).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
