//! Liveness: the heartbeat lease and the write capability it gates.
//! Revocation is structural: the watch sender is shared with a
//! [`Revocation`] the stop/drop paths mark Lost *before* anything else,
//! and a closed channel reads as Lost, never Live.

use crate::claim::Generation;
use crate::identity::SessionId;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Live state of a held claim, published by the heartbeat actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeaseState {
    /// Heartbeats are advancing; the claim is live.
    #[default]
    Live,
    /// The lease ended (stopped, dropped, or lost); writes gated on it
    /// must stop.
    Lost,
}

/// The claim lease was lost (steal superseded us or renewal failed
/// fail-closed). The turn must quarantine and stop writing.
#[derive(Debug, thiserror::Error)]
#[error("session claim lease lost (generation {generation})")]
pub struct LeaseLost {
    /// The generation that was lost.
    pub generation: Generation,
}

/// Shared revocation flag: one watch sender, marked Lost by every stop or
/// drop path. A dropped sender reads as Lost through the receiver.
#[derive(Debug, Clone)]
pub(crate) struct Revocation(watch::Sender<LeaseState>);

impl Revocation {
    /// A live revocation (sender side only, adapter-held).
    pub(crate) fn new() -> (Self, watch::Receiver<LeaseState>) {
        let (tx, rx) = watch::channel(LeaseState::Live);
        (Self(tx), rx)
    }

    /// Mark lost. Idempotent; subsequent sends are no-ops.
    pub(crate) fn revoke(&self) {
        self.0.send_if_modified(|state| {
            let changed = *state == LeaseState::Live;
            *state = LeaseState::Lost;
            changed
        });
    }

    /// Whether the flag is Lost *or the channel is closed*.
    pub(crate) fn is_lost(&self) -> bool {
        match *self.0.subscribe().borrow() {
            LeaseState::Live => false,
            LeaseState::Lost => true,
        }
    }
}

/// Handle to the heartbeat lease, carried by value through the turn state
/// chain. Stopping or dropping it revokes every capability derived from
/// it; the heartbeat actor (if any) is aborted, never orphaned.
#[derive(Debug)]
pub struct HeartbeatLease {
    session: SessionId,
    generation: Generation,
    revocation: Revocation,
    actor: Option<JoinHandle<()>>,
}

impl HeartbeatLease {
    /// Assemble a lease with no background actor (local admission): the
    /// revocation flag alone decides liveness.
    pub(crate) fn static_lease(session: SessionId, generation: Generation) -> Self {
        let (revocation, _) = Revocation::new();
        Self {
            session,
            generation,
            revocation,
            actor: None,
        }
    }

    /// Assemble a lease around a running heartbeat actor (adapter-side).
    /// The actor must exit when the revocation flag turns Lost.
    pub(crate) fn with_actor(
        session: SessionId,
        generation: Generation,
        actor: JoinHandle<()>,
    ) -> (Self, watch::Receiver<LeaseState>) {
        let (revocation, rx) = Revocation::new();
        (
            Self {
                session,
                generation,
                revocation,
                actor: Some(actor),
            },
            rx,
        )
    }

    /// The generation this lease defends.
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

    /// A write capability bound to this lease's generation. Cheap to
    /// clone; liveness is always read from the shared revocation, never
    /// cached.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        WriteCapability {
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
        // Abandonment path: revoke synchronously; abort without joining.
        self.revocation.revoke();
        if let Some(actor) = self.actor.take() {
            actor.abort();
        }
    }
}

/// Generation-bound permission to write on behalf of a turn. Persistence
/// writes check [`assert_live`](Self::assert_live) at the write seam; a
/// lost or closed lease fails the check.
#[derive(Debug, Clone)]
pub struct WriteCapability {
    generation: Generation,
    revocation: Revocation,
}

impl WriteCapability {
    /// The generation this capability authorizes.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Fail if the backing lease is no longer live (stopped, dropped, or
    /// stolen).
    ///
    /// # Errors
    /// [`LeaseLost`] when the lease is gone.
    pub fn assert_live(&self) -> Result<(), LeaseLost> {
        if self.revocation.is_lost() {
            Err(LeaseLost {
                generation: self.generation,
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
    /// [`std::num::NonZeroU64`-style rejection] when zero: returns the
    /// raw value for the caller to report.
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
