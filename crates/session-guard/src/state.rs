//! The consuming state chain: admission → held claim → fenced run →
//! active turn → committing → committed response. Types consume their
//! predecessor by value, so each stage exists only while the turn owns
//! it. Admission output is one sealed [`AcquiredClaim`] — the claim
//! identity and its bound release arrive together, and the lease derives
//! from the same identity inside [`HeldLock::new`], so identity, lease,
//! and release cannot disagree.

use std::future::Future;
use std::path::{Path, PathBuf};

use crate::arbiter::HeldGuard;
use crate::claim::Generation;
use crate::identity::{InstanceId, SessionId, TurnId};
use crate::lease::ActorExitGuard;
use crate::lease::{HeartbeatLease, LeaseLost, WriteCapability};
use tokio::task::JoinHandle;

/// The complete output of a successful admission (adapter-side,
/// crate-internal): the claim identity plus the release action bound to
/// that exact claim at construction. The lease is derived from the same
/// identity inside [`HeldLock::new`]. (The closure's captured path
/// cannot be proven by types — closures are opaque; that binding is the
/// adapter's contract, tested in Layer 2 and recorded as residual risk.)
pub(crate) struct AcquiredClaim {
    session: SessionId,
    turn: TurnId,
    holder: InstanceId,
    generation: Generation,
    release: ReleaseAction,
}

impl AcquiredClaim {
    /// Assemble (adapter-side). The release action must close over this
    /// claim's file; that binding is the adapter's contract, tested in
    /// Layer 2.
    pub(crate) fn new(
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
        }
    }

    /// Consume the claim into a held lock with a static (actor-free)
    /// lease — local admission. The lease is built from this claim's own
    /// identity inside this call, so identity, lease, and release cannot
    /// disagree.
    pub(crate) fn into_held_local(self, arbiter: HeldGuard) -> HeldLock {
        let lease = HeartbeatLease::static_from(crate::lease::ClaimLeaseSource {
            session: self.session.clone(),
            turn: self.turn,
            generation: self.generation,
        });
        HeldLock::from_parts(self, arbiter, lease)
    }

    /// Consume the claim into a held lock with an actor-backed lease —
    /// claim-file admission. `spawn` receives the matching
    /// [`ActorExitGuard`] and must move it into the task it spawns (a
    /// guard dropped outside the task revokes immediately: fail-safe).
    /// The lease is built from this claim's own identity inside this
    /// call.
    pub(crate) fn into_held_with_actor(
        self,
        arbiter: HeldGuard,
        spawn: impl FnOnce(ActorExitGuard) -> JoinHandle<()>,
    ) -> HeldLock {
        let lease = HeartbeatLease::with_actor(
            crate::lease::ClaimLeaseSource {
                session: self.session.clone(),
                turn: self.turn,
                generation: self.generation,
            },
            spawn,
        );
        HeldLock::from_parts(self, arbiter, lease)
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
    /// Assemble from parts (private: only the `into_held_*` constructors
    /// on [`AcquiredClaim`] call this, which is what binds lease to
    /// claim by construction).
    fn from_parts(acquired: AcquiredClaim, arbiter: HeldGuard, lease: HeartbeatLease) -> Self {
        Self {
            session: acquired.session,
            turn: acquired.turn,
            holder: acquired.holder,
            generation: acquired.generation,
            lease,
            arbiter,
            release: acquired.release,
        }
    }

    /// Who holds the session right now (this process, no disk read).
    #[must_use]
    pub fn holder_view(&self) -> crate::claim::HolderView {
        crate::claim::HolderView::Here {
            holder: self.holder.clone(),
        }
    }

    /// The write capability bound to this claim.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        self.lease.capability()
    }

    /// Bind the run directory the seam just created under claim
    /// authority. The seam performs the `create_dir` with the capability
    /// checked; on failure it must call [`abort`](Self::abort) — a bare
    /// drop also revokes, but abort releases cleanly.
    pub fn open_run(self, run_dir: PathBuf) -> FencedRun {
        FencedRun {
            lock: self,
            run_dir,
        }
    }

    /// Early cleanup: the seam failed between admission and the turn
    /// (persistence init, run-dir creation). Revokes the lease and
    /// releases the claim.
    pub async fn abort(self) -> Result<(), ReleaseError> {
        todo!("fill: lease stop + release; aura #421 follow-up")
    }
}

/// A run directory whose creation was authorized by a live
/// [`WriteCapability`]. The sole constructor performs the `create_dir`
/// under the capability check, so the value itself is the proof; the
/// path inside is not forgeable.
#[derive(Debug)]
#[must_use]
pub struct RunDir(PathBuf);

impl RunDir {
    /// Create the run directory under claim authority: the capability
    /// must be live at creation time. Non-recursive (`create_dir`
    /// semantics: the parent — the session directory — must already
    /// exist; an `AlreadyExists` collision is a hard error, not
    /// retried).
    ///
    /// # Errors
    /// [`FenceCause::LeaseLost`] when the capability is no longer live;
    /// [`FenceCause::Io`] for the underlying filesystem error.
    pub fn create_under(capability: &WriteCapability, path: PathBuf) -> Result<Self, FenceCause> {
        capability.assert_live()?;
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

/// The run directory exists and was created under this claim's authority.
/// Isolation only — exclusion is the claim, not the directory.
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
