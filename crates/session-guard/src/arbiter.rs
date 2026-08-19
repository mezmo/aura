//! In-process admission: the first serialization layer, consulted before
//! any filesystem claim. Two requests for one session arriving at this
//! instance must not both reach the disk.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::identity::SessionId;

#[derive(Debug, Default)]
struct Held(Mutex<HashSet<SessionId>>);

/// Process-wide session arbiter. One per server, shared by the admission
/// backend. Held sessions are a set; dropping the guard removes them.
/// Cheap to clone — clones share state (the guard owns one clone).
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

    /// Whether this process currently holds `session` (read-only
    /// membership query for `locate_holder`).
    #[must_use]
    pub fn holds(&self, session: &SessionId) -> bool {
        self.held
            .0
            .lock()
            .expect("session arbiter mutex poisoned")
            .contains(session)
    }

    /// Try to hold `session` in this process. `None` means this instance
    /// is already running a turn for it — fail fast locally, without
    /// touching the claim store.
    #[must_use]
    pub fn try_acquire(&self, session: &SessionId) -> Option<ArbiterGuard> {
        let mut held = self.held.0.lock().expect("session arbiter mutex poisoned");
        held.insert(session.clone()).then(|| ArbiterGuard {
            held: Arc::clone(&self.held),
            session: session.clone(),
        })
    }
}

/// Releases the arbiter slot on drop. Held inside [`crate::HeldLock`] so
/// in-process admission lasts exactly as long as the claim.
#[derive(Debug)]
pub struct ArbiterGuard {
    held: Arc<Held>,
    session: SessionId,
}

impl Drop for ArbiterGuard {
    fn drop(&mut self) {
        self.held
            .0
            .lock()
            .expect("session arbiter mutex poisoned")
            .remove(&self.session);
    }
}
