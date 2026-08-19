//! In-process admission: the first serialization layer, consulted before
//! any filesystem claim. Two requests for one session arriving at this
//! instance must not both reach the disk.
//!
//! A session slot has two phases: [`Slot::Admitting`] from the local
//! try-acquire until the filesystem election resolves, [`Slot::Held`]
//! after a successful claim. Membership queries ([`SessionArbiter::holds`])
//! report only `Held`, so a mere admission attempt is never mistaken for
//! an established holder. Any second same-session request — while
//! admitting or held — fails locally.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::identity::SessionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A local admission attempt is in flight (election unresolved).
    Admitting,
    /// This instance holds the session's claim.
    Held,
}

#[derive(Debug, Default)]
struct Held(Mutex<HashMap<SessionId, Slot>>);

/// Process-wide session arbiter. One per server, created by
/// [`crate::build_admission`] and shared by the backend. Cheap to clone;
/// clones share state.
#[derive(Debug, Default, Clone)]
pub struct SessionArbiter {
    held: Arc<Held>,
}

impl SessionArbiter {
    /// An empty arbiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to enter the admission phase for `session` in this process.
    /// `None` means this instance is already admitting or holding a turn
    /// for it — fail fast locally, without touching the claim store.
    #[must_use]
    pub fn try_acquire(&self, session: &SessionId) -> Option<PendingGuard> {
        let mut held = self.held.0.lock().expect("session arbiter mutex poisoned");
        if held.contains_key(session) {
            return None;
        }
        held.insert(session.clone(), Slot::Admitting);
        Some(PendingGuard {
            held: Some(Arc::clone(&self.held)),
            session: Some(session.clone()),
        })
    }

    /// Whether this process currently *holds* `session` (an established
    /// claim, not a mere admission attempt). The read-only membership
    /// query behind `locate_holder`'s local answer.
    #[must_use]
    pub fn holds(&self, session: &SessionId) -> bool {
        self.held
            .0
            .lock()
            .expect("session arbiter mutex poisoned")
            .get(session)
            .is_some_and(|slot| *slot == Slot::Held)
    }
}

/// The admission phase: claimed locally, election unresolved. Dropping
/// it means admission failed — the slot is freed. Confirming it (after
/// the filesystem election succeeds) promotes it to a [`HeldGuard`].
#[derive(Debug)]
pub struct PendingGuard {
    held: Option<Arc<Held>>,
    session: Option<SessionId>,
}

impl PendingGuard {
    /// Promote to held: the filesystem election succeeded and this
    /// instance now holds the claim. Consumes the pending guard, so the
    /// slot can never be both Admitting and Held.
    #[must_use]
    pub fn confirm(mut self) -> HeldGuard {
        let held = self.held.take().expect("pending guard parts present");
        let session = self.session.take().expect("pending guard parts present");
        let mut map = held.0.lock().expect("session arbiter mutex poisoned");
        map.insert(session.clone(), Slot::Held);
        drop(map);
        HeldGuard { held, session }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let (Some(held), Some(session)) = (self.held.take(), self.session.take()) {
            held.0
                .lock()
                .expect("session arbiter mutex poisoned")
                .remove(&session);
        }
    }
}

/// An established local hold. Lives inside [`crate::HeldLock`], so it
/// spans the whole turn; dropping it frees the slot.
#[derive(Debug)]
pub struct HeldGuard {
    held: Arc<Held>,
    session: SessionId,
}

impl Drop for HeldGuard {
    fn drop(&mut self) {
        self.held
            .0
            .lock()
            .expect("session arbiter mutex poisoned")
            .remove(&self.session);
    }
}
