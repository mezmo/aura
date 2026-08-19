//! The consuming state chain: admission → held claim → fenced run → active
//! turn → committing → committed response. Types consume their predecessor
//! by value, so each stage exists only while the turn owns it.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::arbiter::ArbiterGuard;
use crate::claim::{Generation, HolderView, Locality};
use crate::identity::{InstanceId, SessionId, TurnId};
use crate::lease::{HeartbeatLease, LeaseLost, WriteCapability};

/// Why admission failed. Only [`AdmissionError::Busy`] is contention: it
/// maps to HTTP 503 + `Retry-After` and is the sole LB-retryable variant.
/// Everything else is a 500-class failure that must not trigger the
/// contention retry path.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// A live claim exists for the session.
    #[error("session busy: held by {holder_instance}")]
    Busy {
        /// Who holds the session, for routing and honest hints.
        holder_instance: InstanceId,
        /// How long the caller should wait before retrying.
        retry_after: Duration,
    },
    /// The steal raced a revival or another steal; the evidence no longer
    /// matches the claim. Retryable, but distinct from `Busy`.
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
    /// The turn this request will run, fixed here so claim names are
    /// unique even under retries.
    pub turn: TurnId,
}

/// A claim is held for the session. Exists only while the turn owns it;
/// the arbiter guard inside keeps same-instance requests out.
#[must_use]
pub struct HeldLock {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) instance: InstanceId,
    pub(crate) generation: Generation,
    pub(crate) arbiter: ArbiterGuard,
    pub(crate) lease: HeartbeatLease,
    pub(crate) release: ReleaseAction,
}

impl std::fmt::Debug for HeldLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldLock")
            .field("session", &self.session)
            .field("turn", &self.turn)
            .field("instance", &self.instance)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// How a state releases its claim: an adapter-owned boxed release bound to
/// one claim generation. Never nameable by callers.
pub(crate) type ReleaseFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<(), ReleaseError>> + Send>>;
pub(crate) type ReleaseAction = Box<dyn FnOnce(Generation) -> ReleaseFuture + Send>;

impl HeldLock {
    /// Who holds the session right now (this instance).
    #[must_use]
    pub fn holder_view(&self) -> HolderView {
        HolderView {
            instance: self.instance.clone(),
            locality: Locality::Here,
            last_heartbeat: None,
        }
    }

    /// The write capability bound to this claim.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        self.lease.capability()
    }

    /// Bind the run directory the seam just created and fenced. The seam
    /// performs the `create_dir` under [`WriteCapability`] authority; this
    /// step records it, making the fence part of the turn state.
    pub fn open_run(self, run_dir: PathBuf) -> FencedRun {
        FencedRun {
            lock: self,
            run_dir,
        }
    }
}

/// Why opening a run failed, returning the lock for cleanup.
#[derive(Debug, thiserror::Error)]
#[error("open run failed: {cause}")]
pub struct OpenRunError {
    /// Underlying cause.
    pub cause: FenceCause,
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

    /// Arm the turn: persistence init (with the capability) happens at the
    /// seam between this call and the first write.
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

/// The completion barrier: commit → release → authorize. The commit step
/// is injected (orchestration owns manifest semantics; the guard owns
/// ordering). No step may warn-and-continue.
#[derive(Debug)]
#[must_use]
pub struct CommittingTurn {
    run: FencedRun,
    outcome: TurnOutcome,
}

impl CommittingTurn {
    /// Run the barrier: await `commit`, stop the heartbeat lease, release
    /// the claim (generation-checked), then authorize the response.
    ///
    /// # Errors
    /// [`BarrierError::CommitFailed`] — the commit future failed; the run
    /// is quarantined and the claim released. [`BarrierError::Release`]
    /// — release failed after commit; the response is authorized (data is
    /// durable) but the session is flagged wedged.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by aura #421 follow-up"
    )]
    pub async fn barrier<T, E, F>(self, commit: F) -> Result<CommittedResponse<T>, BarrierError<E>>
    where
        F: Future<Output = Result<T, E>>,
    {
        todo!("fill: commit → lease stop → release → response; aura #421 follow-up")
    }
}

/// Why the barrier failed.
#[derive(Debug, thiserror::Error)]
pub enum BarrierError<E> {
    /// The injected commit step failed; the run is quarantined and the
    /// claim released. The error is the commit future's own.
    #[error("commit failed; run quarantined")]
    CommitFailed(E),
    /// The claim release failed after a durable commit. The response may
    /// be delivered, but the session needs operator attention.
    #[error("release failed after commit; session wedged")]
    Release(#[source] ReleaseError),
}

/// Sole authorization to emit a terminal frame. Carries the committed
/// payload and everything needed to name the turn in logs. Not `Clone`;
/// the terminal emitters (SSE `[DONE]`, final body, A2A artifact) consume
/// it by value.
#[derive(Debug)]
pub struct CommittedResponse<T> {
    payload: T,
    session: SessionId,
    turn: TurnId,
    generation: Generation,
}

impl<T> CommittedResponse<T> {
    /// The committed payload by reference.
    #[must_use]
    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Consume the authorization together with the payload.
    #[must_use]
    pub fn into_payload(self) -> T {
        self.payload
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
    /// The claim file's generation no longer matches (stolen during
    /// shutdown). Not an error to fix by force — the new owner's claim
    /// must stand.
    #[error("claim superseded (generation {0} stolen)")]
    Superseded(Generation),
    /// Filesystem failure while releasing.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
