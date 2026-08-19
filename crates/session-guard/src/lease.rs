//! Liveness: the heartbeat lease and the write capability it gates.
//! Revocation is structural and total: the shared flag is revoked on
//! every end path — explicit stop, lease drop, actor exit (via the exit
//! guard the actor task owns), and `Revocation`'s own Drop — so a closed
//! channel always reads Lost, and the capability checks a held receiver,
//! not a fresh subscription.

use crate::claim::Generation;
use crate::identity::{SessionId, TurnId};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Live state of a held claim, published on the revocation channel.
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

/// Shared revocation flag. Clones share state; dropping any clone
/// revokes, so the channel can never read Live after every holder is
/// gone. Liveness reads hit an atomic, not a fresh watch subscription.
#[derive(Debug, Clone)]
pub(crate) struct Revocation {
    sender: std::sync::Arc<watch::Sender<LeaseState>>,
    lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Revocation {
    /// A live revocation (sender side).
    pub(crate) fn new() -> Self {
        let (tx, _rx) = watch::channel(LeaseState::Live);
        Self {
            sender: std::sync::Arc::new(tx),
            lost: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Mark lost. Idempotent.
    pub(crate) fn revoke(&self) {
        self.lost.store(true, std::sync::atomic::Ordering::Release);
        self.sender.send_if_modified(|state| {
            let changed = *state == LeaseState::Live;
            *state = LeaseState::Lost;
            changed
        });
    }

    /// Whether the flag is Lost. Every end path (stop, drop, actor exit,
    /// sender drop) revokes first, so this is authoritative.
    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for Revocation {
    fn drop(&mut self) {
        self.revoke();
    }
}
/// Owned by the heartbeat actor task: revokes when the task exits for
/// any reason (return, panic, abort), so actor death can never leave a
/// lease Live.
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
/// Built by [`crate::state::AcquiredClaim::lease_source`], so lease,
/// capability, and release all share one claim identity.
#[derive(Debug, Clone)]
pub(crate) struct ClaimLeaseSource {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) generation: Generation,
}

/// The heartbeat lease, carried by value through the turn state chain.
/// Derived from one [`ClaimLeaseSource`] identity, so its session and
/// incarnation can never disagree with the claim it defends.
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
    /// revocation flag alone decides liveness. Bindings come from
    /// `source`.
    pub(crate) fn static_from(source: ClaimLeaseSource) -> Self {
        Self {
            session: source.session,
            turn: source.turn,
            generation: source.generation,
            revocation: Revocation::new(),
            actor: None,
        }
    }

    /// A lease around a running heartbeat actor. The actor MUST hold an
    /// [`ActorExitGuard`] built from a clone of the same revocation, and
    /// must exit when the flag turns Lost. Binding comes from `source`.
    pub(crate) fn with_actor(
        source: ClaimLeaseSource,
        revocation: Revocation,
        actor: JoinHandle<()>,
    ) -> Self {
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
        if self.revocation.is_lost() {
            LeaseState::Lost
        } else {
            LeaseState::Live
        }
    }

    /// A write capability bound to this lease's full identity.
    /// Cheap to clone; liveness is read from the shared flag.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        WriteCapability {
            session: self.session.clone(),
            turn: self.turn,
            generation: self.generation,
            revocation: self.revocation.clone(),
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
        // Revocation's own Drop, but explicit is clearer); abort without
        // joining.
        self.revocation.revoke();
        if let Some(actor) = self.actor.take() {
            actor.abort();
        }
    }
}

/// Permission to write on behalf of one turn of one session, under one
/// claim incarnation. Identity-complete: a capability can be checked
/// against the exact write target, not just a bare generation. Fails
/// closed once the backing lease revokes.
#[derive(Debug, Clone)]
pub struct WriteCapability {
    session: SessionId,
    turn: TurnId,
    generation: Generation,
    revocation: Revocation,
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
        if self.revocation.is_lost() {
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
