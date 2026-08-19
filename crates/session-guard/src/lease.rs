//! Liveness: the heartbeat lease and the write capability it gates.

use crate::claim::Generation;
use crate::identity::SessionId;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Live state of a held claim, published by the heartbeat actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeaseState {
    /// Heartbeats are advancing; the claim is live.
    #[default]
    Live,
    /// The actor could not renew; writes gated on this lease must stop.
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

/// How a lease's actor is shut down: a real heartbeat task, or a static
/// always-live lease for admission modes with nothing to renew.
#[derive(Debug)]
pub(crate) enum LeaseStop {
    /// Cancel the heartbeat task and join it.
    Task(JoinHandle<()>),
    /// Nothing to stop: local admission holds the sender directly.
    Static(watch::Sender<LeaseState>),
}

/// Handle to the heartbeat actor spawned at admission. Carried by value
/// through the turn state chain; consumed by the barrier or abort paths.
#[derive(Debug)]
pub struct HeartbeatLease {
    session: SessionId,
    generation: Generation,
    state: watch::Receiver<LeaseState>,
    stop: LeaseStop,
}

impl HeartbeatLease {
    /// Assemble a lease around an actor (adapter-side).
    pub(crate) fn new(
        session: SessionId,
        generation: Generation,
        state: watch::Receiver<LeaseState>,
        stop: LeaseStop,
    ) -> Self {
        Self {
            session,
            generation,
            state,
            stop,
        }
    }

    /// The generation this lease defends.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Current liveness.
    #[must_use]
    pub fn state(&self) -> LeaseState {
        *self.state.borrow()
    }

    /// A write capability bound to this lease's generation.
    #[must_use]
    pub fn capability(&self) -> WriteCapability {
        WriteCapability {
            generation: self.generation,
            state: self.state.clone(),
        }
    }

    /// Ordered lease shutdown: stop the beats, join the actor. Called only
    /// from the release path, after the completion barrier.
    pub(crate) async fn stop(self) -> Result<(), std::io::Error> {
        todo!("fill: cancel/join task or drop static sender; aura #421 follow-up")
    }
}

/// Generation-bound permission to write on behalf of a turn. Persistence
/// writes check [`assert_live`](Self::assert_live); after lease loss the
/// check fails and the write path must stop.
///
/// Cheap to clone (a watch receiver): hand copies to the persistence layer,
/// but liveness is evaluated against the shared lease state, never cached.
#[derive(Debug, Clone)]
pub struct WriteCapability {
    generation: Generation,
    state: watch::Receiver<LeaseState>,
}

impl WriteCapability {
    /// The generation this capability authorizes.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Fail if the backing lease is no longer live.
    ///
    /// # Errors
    /// [`LeaseLost`] when the claim was stolen or renewal failed.
    pub fn assert_live(&self) -> Result<(), LeaseLost> {
        match *self.state.borrow() {
            LeaseState::Live => Ok(()),
            LeaseState::Lost => Err(LeaseLost {
                generation: self.generation,
            }),
        }
    }
}
