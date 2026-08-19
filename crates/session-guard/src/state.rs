//! The consuming state chain: admission → held claim → fenced run → active
//! turn → committing → committed response. Types consume their predecessor
//! by value, so each stage exists only while the turn owns it. The claim
//! identity is one private [`OwnedClaim`] — lease, capability, and release
//! all derive from it, so they cannot disagree.

use std::future::Future;
use std::path::{Path, PathBuf};

use crate::arbiter::ArbiterGuard;
use crate::claim::{Generation, HolderView, Locality};
use crate::identity::{InstanceId, SessionId, TurnId};
use crate::lease::{HeartbeatLease, LeaseLost, WriteCapability};

/// The one claim identity everything derives from. Never nameable outside
/// the crate; the release action is bound to exactly this claim at
/// construction and takes no arguments.
#[derive(Debug)]
pub(crate) struct OwnedClaim {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) holder: InstanceId,
    pub(crate) generation: Generation,
}

/// Zero-argument release bound to one [`OwnedClaim`] generation at
/// construction. It cannot be invoked for any other generation.
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
    /// Operator-facing, not retryable by the LB.
    #[error("claim root not provisioned: {0}")]
    NotProvisioned(String),
    /// Claim-store I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A request awaiting admission, parsed and ready. Built by the ingress
/// seam after `SessionId` validation, before any visible status.
#[derive(Debug)]
#[must_use]
pub struct IdleRequest {
    /// The session to claim.
    pub session: SessionId,
    /// Who is asking (this instance).
    pub instance: InstanceId,
    /// The turn this request will run, fixed here so claim bodies name
    /// the turn even under client retries.
    pub turn: TurnId,
}

/// A claim is held for the session. Exists only while the turn owns it;
/// the arbiter guard inside keeps same-instance requests out.
#[must_use]
pub struct HeldLock {
    claim: OwnedClaim,
    arbiter: ArbiterGuard,
    lease: HeartbeatLease,
    release: ReleaseAction,
}

impl std::fmt::Debug for HeldLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldLock")
            .field("session", &self.claim.session)
            .field("turn", &self.claim.turn)
            .field("holder", &self.claim.holder)
            .field("generation", &self.claim.generation)
            .finish_non_exhaustive()
    }
}

impl HeldLock {
    /// Assemble a held lock (adapter-side): claim identity, in-process
    /// guard, liveness lease, and the release action bound to that claim.
    pub(crate) fn new(
        claim: OwnedClaim,
        arbiter: ArbiterGuard,
        lease: HeartbeatLease,
        release: ReleaseAction,
    ) -> Self {
        Self {
            claim,
            arbiter,
            lease,
            release,
        }
    }

    /// Who holds the session right now (this instance).
    #[must_use]
    pub fn holder_view(&self) -> HolderView {
        HolderView {
            holder: self.claim.holder.clone(),
            locality: Locality::Here,
            last_heartbeat: None,
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

/// A live turn. Terminal outcomes — success, failure, clarification — all
/// flow through [`complete`](Self::complete); nothing else may follow.
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
    /// End the turn and enter the completion barrier. After this call the
    /// response is not yet authorized — only the barrier's
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

/// Context handed to the injected commit step. Carries everything the
/// commit needs to name the turn — and everything a manifest writer needs
/// to record a clarification as a first-class outcome.
#[derive(Debug, Clone)]
pub struct CommitContext {
    /// The fenced run directory (where manifests go).
    pub run_dir: PathBuf,
    /// The terminal outcome.
    pub outcome: TurnOutcome,
    /// The session.
    pub session: SessionId,
    /// The turn.
    pub turn: TurnId,
    /// The claim generation that authorized the turn.
    pub generation: Generation,
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
    /// [`BarrierError::CommitFailed`] — the commit step failed; the run
    /// is quarantined and the claim released. [`BarrierError::Release`]
    /// — release failed after a durable commit; the authorized response
    /// travels with the error (data is durable) and the session is
    /// flagged wedged.
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
    /// The injected commit step failed; the run is quarantined and the
    /// claim released. `E` is the commit step's own error.
    CommitFailed(E),
    /// The claim release failed after a durable commit. The response is
    /// authorized (carried here, wedged flag set); the session needs
    /// operator attention.
    Release {
        /// The authorized response; delivery is the caller's call.
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
            Self::CommitFailed(e) => Some(e),
            Self::Release { .. } => None,
        }
    }
}

/// Sole authorization to emit a terminal frame. Carries the committed
/// payload bound to session + turn + generation. Not `Clone`; the payload
/// is only reachable through authorization-preserving transforms, so the
/// envelope cannot be discarded short of consuming the whole value.
#[derive(Debug)]
pub struct CommittedResponse<T> {
    payload: T,
    session: SessionId,
    turn: TurnId,
    generation: Generation,
}

impl<T> CommittedResponse<T> {
    /// Build the authorization (barrier-internal).
    pub(crate) fn new(
        payload: T,
        session: SessionId,
        turn: TurnId,
        generation: Generation,
    ) -> Self {
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

    /// Consume the authorization, yielding the payload to the terminal
    /// emitter that consumes this value.
    #[must_use]
    pub fn into_payload(self) -> (T, SessionId, TurnId, Generation) {
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
    /// The claim's generation no longer matches (stolen during
    /// shutdown). Not fixable by force — the new owner's claim stands.
    #[error("claim superseded (generation {0} stolen)")]
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
