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
use tokio::time::Instant;

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
        let id = request.decision_id;
        let now = chrono::Utc::now();
        let (tx, rx) = oneshot::channel();
        let entry = PendingEntry {
            parked: ParkedApproval {
                request,
                registered_at: now,
                expires_at: now
                    + chrono::Duration::from_std(timeout).expect("approval timeout fits in chrono"),
            },
            wake: tx,
        };
        self.0
            .entries
            .lock()
            .expect("registry lock poisoned")
            .insert(id, entry);
        AwaitingDecision::new(id, rx, Instant::now() + timeout)
    }

    /// Resolve a parked approval, waking its await. Removes the entry, so a
    /// `DecisionId` resolves at most once in-process.
    pub fn resolve(&self, id: &DecisionId, decision: ApprovalDecision) -> Result<(), ResolveError> {
        let entry = self
            .0
            .entries
            .lock()
            .expect("registry lock poisoned")
            .remove(id);
        match entry {
            Some(entry) => {
                let _ = entry.wake.send(decision);
                Ok(())
            }
            None => Err(ResolveError::NotFound),
        }
    }

    /// Expire a parked approval after timeout/cancellation so later ingress
    /// returns [`ResolveError::NotFound`].
    pub fn remove(&self, id: &DecisionId) {
        self.0
            .entries
            .lock()
            .expect("registry lock poisoned")
            .remove(id);
    }

    /// Cancel every approval parked under a request id (stream drop / shutdown);
    /// their awaits resolve to `Cancelled`.
    pub fn cancel_request(&self, request_id: &str) {
        self.0
            .entries
            .lock()
            .expect("registry lock poisoned")
            .retain(|_, entry| entry.parked.request.request_id != request_id);
    }
}

impl Default for PendingApprovals {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::request_cancellation::RequestCancelToken;
    use serde_json::json;

    use super::*;
    use crate::hitl::decision::{
        AgentScope, ApprovalOrigin, ApprovalOutcome, CancelReason, DecisionId,
    };
    use crate::hitl::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};

    fn test_request(request_id: &str) -> ApprovalRequest {
        ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: request_id.to_string(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "test_*".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "test_tool".to_string(),
                arguments: json!({}),
            }],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn register_and_resolve_approved() {
        let registry = PendingApprovals::new();
        let req = test_request("req-1");
        let id = req.decision_id;
        let handle = registry.register(req, Duration::from_secs(60));
        let cancel = RequestCancelToken::unbound();

        registry
            .resolve(&id, ApprovalDecision::Approved)
            .expect("resolve succeeds");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn register_and_resolve_denied() {
        let registry = PendingApprovals::new();
        let req = test_request("req-2");
        let id = req.decision_id;
        let handle = registry.register(req, Duration::from_secs(60));
        let cancel = RequestCancelToken::unbound();

        registry
            .resolve(
                &id,
                ApprovalDecision::Denied {
                    reason: Some("not safe".into()),
                },
            )
            .expect("resolve succeeds");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: Some("not safe".into())
            })
        );
    }

    #[test]
    fn resolve_unknown_id_returns_not_found() {
        let registry = PendingApprovals::new();
        let unknown = DecisionId::generate();
        assert_eq!(
            registry.resolve(&unknown, ApprovalDecision::Approved),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn resolve_twice_returns_not_found_on_second() {
        let registry = PendingApprovals::new();
        let req = test_request("req-3");
        let id = req.decision_id;
        let _handle = registry.register(req, Duration::from_secs(60));

        registry
            .resolve(&id, ApprovalDecision::Approved)
            .expect("first resolve succeeds");
        assert_eq!(
            registry.resolve(&id, ApprovalDecision::Approved),
            Err(ResolveError::NotFound)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_request_drops_matching_entries() {
        let registry = PendingApprovals::new();
        let req_a = test_request("req-cancel");
        let req_b = test_request("req-keep");
        let id_b = req_b.decision_id;
        let handle_a = registry.register(req_a, Duration::from_secs(60));
        let handle_b = registry.register(req_b, Duration::from_secs(60));
        let cancel = RequestCancelToken::unbound();

        registry.cancel_request("req-cancel");

        assert_eq!(
            handle_a.outcome(&cancel).await,
            ApprovalOutcome::Cancelled(CancelReason::SenderDropped)
        );

        registry
            .resolve(&id_b, ApprovalDecision::Approved)
            .expect("unrelated entry survives");
        assert_eq!(
            handle_b.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
    }

    #[test]
    fn register_sets_expires_at_from_timeout() {
        let registry = PendingApprovals::new();
        let req = test_request("req-ts");
        let id = req.decision_id;
        let timeout = Duration::from_secs(300);
        let before = chrono::Utc::now();
        let _handle = registry.register(req, timeout);
        let after = chrono::Utc::now();

        let entries = registry.0.entries.lock().unwrap();
        let entry = entries.get(&id).expect("entry exists");
        let delta = entry.parked.expires_at - entry.parked.registered_at;
        assert_eq!(delta, chrono::Duration::from_std(timeout).unwrap());
        assert!(entry.parked.registered_at >= before);
        assert!(entry.parked.registered_at <= after);
    }
}
