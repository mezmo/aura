//! Request-to-session ownership transfer for parked approvals (ADR
//! 2026-07-21, decision 10).
//!
//! A request's teardown deletes its parked approvals in two halves: a
//! synchronous local cancel at guard drop, plus a spawned async store/bus
//! cleanup that may run long after — or never poll at shutdown. A park
//! commit transfers approval ownership from request scope to session scope
//! before the response closes, and both halves must then leave the store
//! entries in place: the approvals are exactly what the park exists to
//! preserve.
//!
//! Markers live in a process-wide registry keyed by request id (the
//! `RequestCancellation` pattern), because the teardown guard and the park
//! commit sit on opposite sides of the agent stack: the guard registers at
//! request start, and the park commit looks the marker up by the request id
//! it already carries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Who owns a request's parked approvals during teardown.
enum OwnershipPhase {
    /// Default: teardown deletes the request's approvals.
    RequestOwned,
    /// A park commit took ownership; teardown must leave approvals in place.
    #[expect(
        dead_code,
        reason = "staged for #271: constructed by the ownership transfer"
    )]
    SessionOwned,
    /// A teardown half already began deleting; a transfer can no longer
    /// preserve the approvals.
    TearingDown,
}

/// Shared ownership marker consulted by both teardown halves and flipped by
/// the park commit. Cheap to clone; all clones observe one state.
#[derive(Clone)]
pub struct ApprovalOwnership(Arc<Mutex<OwnershipPhase>>);

/// A park tried to take ownership after teardown had begun deleting: the
/// approvals may already be gone, so the park must fail closed instead of
/// committing a checkpoint whose approvals do not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("approval ownership cannot transfer to the session: request teardown already began")]
pub struct TeardownUnderway;

static REGISTRY: OnceLock<Mutex<HashMap<String, ApprovalOwnership>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, ApprovalOwnership>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

impl ApprovalOwnership {
    /// Register a fresh request-scoped marker under `request_id` and return
    /// it. The request's teardown guard owns the registration; the park
    /// commit reaches the same marker through [`Self::for_request`].
    #[must_use]
    pub fn register(request_id: &str) -> Self {
        let ownership = Self(Arc::new(Mutex::new(OwnershipPhase::RequestOwned)));
        registry()
            .lock()
            .expect("approval ownership registry poisoned")
            .insert(request_id.to_string(), ownership.clone());
        ownership
    }

    /// The marker registered for `request_id`, if its request is still live:
    /// the park commit's lookup seam.
    #[must_use]
    pub fn for_request(request_id: &str) -> Option<Self> {
        registry()
            .lock()
            .expect("approval ownership registry poisoned")
            .get(request_id)
            .cloned()
    }

    /// Drop the registry entry for `request_id`. Existing clones (the guard,
    /// its spawned cleanup task) keep working; only the lookup ends.
    pub fn unregister(request_id: &str) {
        registry()
            .lock()
            .expect("approval ownership registry poisoned")
            .remove(request_id);
    }

    /// Transfer the request's approvals to session scope, atomically with
    /// respect to both teardown halves: after `Ok`, neither half deletes.
    /// Must complete before the park-induced stream closure.
    pub fn transfer_to_session(&self) -> Result<(), TeardownUnderway> {
        todo!("staged for #271: park-commit ownership transfer")
    }

    /// A teardown half asks whether it may delete the request's approvals.
    /// Each half calls this at the moment it would delete — the async half
    /// from inside its spawned task, so a transfer that lands between guard
    /// drop and that task's poll still wins.
    pub fn begin_teardown(&self) -> bool {
        let mut phase = self.0.lock().expect("approval ownership lock poisoned");
        match *phase {
            OwnershipPhase::RequestOwned | OwnershipPhase::TearingDown => {
                *phase = OwnershipPhase::TearingDown;
                true
            }
            OwnershipPhase::SessionOwned => {
                todo!("staged for #271: session-owned teardown leaves approvals in place")
            }
        }
    }
}
