//! The per-process registry that parks conversational approvals (Route B) and
//! the decision a later request resolves them with.
//!
//! This is the first mutable chat-path state that crosses request boundaries:
//! an approval is registered during one request's stream and resolved by a
//! `POST /v1/approvals/{id}` arriving as a different request. It follows the A2A
//! `SharedTaskStore` idiom — a `Clone` newtype over an `Arc`, constructed once
//! in `main`, not a global static.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use super::decision::{ApprovalDecision, AwaitingDecision, DecisionId, Timestamp};
use super::protocol::ApprovalRequest;

/// The cross-request registry of parked conversational approvals.
///
/// Clone newtype over an `Arc`. Cloned onto `AppState` and into each per-request
/// build context; a decision must land on the process that parked the call.
#[derive(Clone)]
pub struct PendingApprovals(Arc<PendingApprovalsInner>);

struct PendingApprovalsInner {
    // `std::sync::Mutex`: every operation is a synchronous map op (insert /
    // remove / oneshot send); nothing awaits while holding the lock. The
    // `BTreeMap` is keyed on `DecisionId` (UUID v7, time-ordered), so iteration
    // is chronological registration order — oldest pending approval first.
    entries: Mutex<BTreeMap<DecisionId, PendingEntry>>,
}

/// One parked approval: a serialization-ready core plus the runtime-only wake
/// handle. The split is what lets durable parking (#209) persist
/// [`ParkedApproval`] as-is later, without serializing the oneshot.
struct PendingEntry {
    parked: ParkedApproval,
    wake: oneshot::Sender<ApprovalDecision>,
}

/// The serializable record of a parked approval. Carries everything needed to
/// re-render and re-validate the approval after a restart.
pub struct ParkedApproval {
    pub request: ApprovalRequest,
    pub registered_at: Timestamp,
    pub expires_at: Timestamp,
}

/// Why a [`PendingApprovals::resolve`] could not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveError {
    /// No live entry for that id: unknown, already resolved, or expired all
    /// collapse to a missing map entry.
    NotFound,
}

impl PendingApprovals {
    /// Create an empty registry. Constructed once in `main`.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(PendingApprovalsInner {
            entries: Mutex::new(BTreeMap::new()),
        }))
    }

    /// Register a parked approval, returning the await handle. Holds the paired
    /// `oneshot::Sender` in the entry.
    #[must_use]
    pub fn register(&self, request: ApprovalRequest, timeout: Duration) -> AwaitingDecision {
        let _ = (&self.0, request, timeout);
        todo!(
            "build oneshot + ParkedApproval, insert keyed by decision id, return AwaitingDecision"
        )
    }

    /// Resolve a parked approval, waking its await. Removes the entry, so a
    /// `DecisionId` resolves at most once in-process.
    pub fn resolve(&self, id: &DecisionId, decision: ApprovalDecision) -> Result<(), ResolveError> {
        let _ = (&self.0, id, decision);
        todo!("remove entry; send decision on its oneshot; NotFound if absent")
    }

    /// Cancel every approval parked under a request id (stream drop / shutdown);
    /// their awaits resolve to `Cancelled`.
    pub fn cancel_request(&self, request_id: &str) {
        let _ = (&self.0, request_id);
        todo!(
            "drop entries whose request.request_id matches; dropping the sender cancels the await"
        )
    }
}

impl Default for PendingApprovals {
    fn default() -> Self {
        Self::new()
    }
}
