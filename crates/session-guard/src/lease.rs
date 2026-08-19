//! Liveness: the heartbeat lease and the write capability it gates.
//!
//! Two roles are split by type so they cannot be confused:
//!
//! - [`Liveness`] is a clone-safe, drop-safe *observation* of the lease
//!   state (an atomic flag). Capabilities hold clones; dropping or
//!   cloning one never changes the state.
//! - [`Revocation`] is the *authority* to end the lease. Exactly the
//!   lease itself and the actor's [`ActorExitGuard`] hold one; dropping
//!   either revokes. Misuse fails safe: an exit guard dropped outside
//!   the actor task revokes immediately (lease reads Lost), never
//!   Live-after-death.

use crate::claim::Generation;
use crate::identity::{SessionId, TurnId};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;

/// Live state of a held claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeaseState {
    /// Heartbeats are advancing; the claim is live.
    #[default]
    Live,
    /// The lease ended (stopped, dropped, actor exited, or lost); writes
    /// gated on it must stop.
    Lost,
}

/// The claim lease was lost (steal superseded us, renewal failed, or the
/// actor died). The turn must quarantine and stop writing.
#[derive(Debug, thiserror::Error)]
#[error("session claim lease lost (incarnation {generation})")]
pub struct LeaseLost {
    /// The incarnation that was lost.
    pub generation: Generation,
    /// The session it belonged to.
    pub session: SessionId,
}

/// Clone-safe, drop-safe liveness observation: a shared atomic flag.
/// Holding or dropping a `Liveness` never alters state; only a
/// [`Revocation`] can.
#[derive(Debug, Clone)]
pub(crate) struct Liveness {
    lost: Arc<AtomicBool>,
}

impl Liveness {
    /// Whether the lease is Lost. Authoritative: every end path sets the
    /// flag before anything else.
    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }
}

/// The authority to end a lease. Held only by [`HeartbeatLease`] and the
/// actor's [`ActorExitGuard`]; dropping either revokes.
#[derive(Debug, Clone)]
pub(crate) struct Revocation {
    liveness: Liveness,
}

impl Revocation {
    /// A live revocation bound to a fresh liveness flag.
    pub(crate) fn new() -> Self {
        Self {
            liveness: Liveness {
                lost: Arc::new(AtomicBool::new(false)),
            },
        }
    }

    /// Mark lost. Idempotent.
    pub(crate) fn revoke(&self) {
        self.liveness.lost.store(true, Ordering::Release);
    }

    /// A clone-safe observation of this revocation's flag.
    pub(crate) fn liveness(&self) -> Liveness {
        self.liveness.clone()
    }
}

impl Drop for Revocation {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Owned by the heartbeat actor task: revokes when the task exits for
/// any reason (return, panic, abort), so actor death can never leave a
/// lease Live. Dropping it *outside* the task also revokes — misuse
/// fails toward Lost, never toward a false Live.
#[derive(Debug)]
pub(crate) struct ActorExitGuard {
    revocation: Revocation,
}

impl ActorExitGuard {
    /// Arm the exit guard for a starting actor.
    pub(crate) fn new(revocation: Revocation) -> Self {
        Self { revocation }
    }
}

impl Drop for ActorExitGuard {
    fn drop(&mut self) {
        self.revocation.revoke();
    }
}

/// The identity a lease (and everything derived from it) is bound to.
/// Constructed only at [`crate::state::AcquiredClaim`]'s lease-building
/// site, so lease, capability, and release all share one claim identity.
#[derive(Debug, Clone)]
pub(crate) struct ClaimLeaseSource {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) generation: Generation,
}

/// The heartbeat lease, carried by value through the turn state chain.
/// Owns the [`Revocation`], so the lease's end (stop, drop) is the
/// lease's revocation.
#[derive(Debug)]
pub struct HeartbeatLease {
    session: SessionId,
    turn: TurnId,
    generation: Generation,
    revocation: Revocation,
    actor: Option<JoinHandle<()>>,
}

impl HeartbeatLease {
    /// A lease with no background actor (local admission): the
    /// revocation flag alone decides liveness.
    pub(crate) fn static_from(source: ClaimLeaseSource) -> Self {
        Self {
            session: source.session,
            turn: source.turn,
            generation: source.generation,
            revocation: Revocation::new(),
            actor: None,
        }
    }

    /// A lease around a heartbeat actor. The lease itself spawns the
    /// wrapper task that owns the [`ActorExitGuard`], so the guard, the
    /// returned handle, and the actor body are one task by construction:
    /// any exit of that task (return, panic, abort) revokes, and the
    /// lease's stop/drop aborts exactly that task. The body receives a
    /// clone-safe [`Liveness`] observation for its exit checks.
    pub(crate) fn with_actor<F, Fut>(source: ClaimLeaseSource, body: F) -> Self
    where
        F: FnOnce(Liveness) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let revocation = Revocation::new();
        let guard = ActorExitGuard::new(revocation.clone());
        let fut = body(revocation.liveness());
        let actor = tokio::spawn(async move {
            let _guard = guard;
            fut.await;
        });
        Self {
            session: source.session,
            turn: source.turn,
            generation: source.generation,
            revocation,
            actor: Some(actor),
        }
    }

    /// The incarnation this lease defends.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Current liveness.
    #[must_use]
    pub fn state(&self) -> LeaseState {
        if self.revocation.liveness.is_lost() {
            LeaseState::Lost
        } else {
            LeaseState::Live
        }
    }

    /// A write capability bound to this lease's full identity. Cheap to
    /// clone and drop: capabilities observe liveness, they never revoke.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        WriteCapability {
            session: self.session.clone(),
            turn: self.turn,
            generation: self.generation,
            liveness: self.revocation.liveness.clone(),
        }
    }

    /// Ordered lease shutdown: revoke first (every capability fails
    /// closed from this instant), then abort-and-join the actor.
    pub(crate) async fn stop(mut self) {
        self.revocation.revoke();
        if let Some(actor) = self.actor.take() {
            actor.abort();
            let _ = actor.await;
        }
    }
}

impl Drop for HeartbeatLease {
    fn drop(&mut self) {
        // Abandonment path: revoke synchronously (also covered by
        // Revocation's own Drop); abort without joining.
        self.revocation.revoke();
        if let Some(actor) = self.actor.take() {
            actor.abort();
        }
    }
}

/// Permission to write on behalf of one turn of one session, under one
/// claim incarnation. Identity-complete: a capability can be checked
/// against the exact write target, not just a bare generation. Fails
/// closed once the backing lease revokes; cloning and dropping a
/// capability never changes lease state.
#[derive(Debug, Clone)]
pub struct WriteCapability {
    session: SessionId,
    turn: TurnId,
    generation: Generation,
    liveness: Liveness,
}

impl WriteCapability {
    /// The session this capability authorizes writes for.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The turn this capability belongs to.
    #[must_use]
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// The claim incarnation.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Fail if the backing lease is no longer live.
    ///
    /// # Errors
    /// [`LeaseLost`] naming this capability's identity when the lease is
    /// gone.
    pub fn assert_live(&self) -> Result<(), LeaseLost> {
        if self.liveness.is_lost() {
            Err(LeaseLost {
                generation: self.generation,
                session: self.session.clone(),
            })
        } else {
            Ok(())
        }
    }
}

/// How often the heartbeat actor beats, derived from config. Non-zero by
/// construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeatInterval(Duration);

impl BeatInterval {
    /// Wrap a non-zero interval.
    ///
    /// # Errors
    /// The raw zero duration when `interval` is zero, for the caller to
    /// report as a config error.
    pub fn new(interval: Duration) -> Result<Self, Duration> {
        if interval.is_zero() {
            Err(interval)
        } else {
            Ok(Self(interval))
        }
    }

    /// The interval duration.
    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }
}
