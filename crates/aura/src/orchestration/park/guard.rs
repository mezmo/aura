//! The run-scoped park guard (park mode).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::hitl::PendingApprovals;

use super::commit::cancel_run_approvals;

/// Arms the unpublished-end sweep once the run has parked a call.
pub(crate) struct ParkGuard {
    registry: PendingApprovals,
    run_id: String,
    request_id: String,
    published: AtomicBool,
    parked_calls: Mutex<usize>,
}

impl ParkGuard {
    /// Create the guard for a run; inert until the first [`Self::record`].
    pub(crate) fn new(registry: PendingApprovals, run_id: String, request_id: String) -> Arc<Self> {
        Arc::new(Self {
            registry,
            run_id,
            request_id,
            published: AtomicBool::new(false),
            parked_calls: Mutex::new(0),
        })
    }

    /// Record parked calls; the first record arms the guard.
    pub(crate) fn record(&self, pending: &[crate::orchestration::PendingCall]) {
        if pending.is_empty() {
            return;
        }
        *self.parked_calls.lock().expect("park guard lock poisoned") += pending.len();
    }

    /// Mark the run's checkpoint published; the drop becomes a no-op.
    pub(crate) fn mark_published(&self) {
        self.published.store(true, Ordering::Release);
    }
}

impl Drop for ParkGuard {
    fn drop(&mut self) {
        // An unpublished drop sweeps the run's parked tickets by owner id; a
        // process crash inside the commit is the one accepted window.
        if self.published.load(Ordering::Acquire) {
            return;
        }
        if *self.parked_calls.lock().expect("park guard lock poisoned") == 0 {
            return;
        }
        let registry = self.registry.clone();
        let run_id = self.run_id.clone();
        let request_id = self.request_id.clone();
        // Drop cannot await; the sweep runs as its own task on the runtime
        // that dropped the guard. Off-runtime drops (a test teardown) log
        // and skip.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    cancel_run_approvals(&registry, &run_id, &request_id).await;
                });
            }
            Err(_) => {
                tracing::warn!(
                    run_id = %run_id,
                    "park guard dropped off-runtime; parked approvals left for expiry",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::hitl::{
        AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId, PROTOCOL_VERSION,
        ParkedApproval, PendingApprovals,
    };
    use crate::orchestration::{RunId, TaskIdentity};
    use crate::session_store::{ApprovalStore, InMemoryApprovalStore, InMemoryEventBus};

    fn registry_with_store() -> (PendingApprovals, Arc<InMemoryApprovalStore>) {
        let store = Arc::new(InMemoryApprovalStore::new());
        let registry = PendingApprovals::with_backend(
            store.clone() as Arc<dyn ApprovalStore>,
            Arc::new(InMemoryEventBus::new()),
        );
        (registry, store)
    }

    fn worker_scope(run_id: RunId) -> AgentScope {
        AgentScope::Worker {
            run_id,
            task: TaskIdentity::new(0, Some("operations".to_string())),
            session_id: None,
        }
    }

    fn durable_approval(
        decision_id: DecisionId,
        owner: &str,
        scope: &AgentScope,
    ) -> ParkedApproval {
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "test-instance".to_string(),
                decision_id,
                request_id: owner.to_string(),
                scope: scope.clone(),
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "kubectl_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "kubectl_apply".to_string(),
                    arguments: serde_json::json!({ "namespace": "prod" }),
                    tool_call_intent: None,
                }],
            },
            registered_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        }
    }

    fn parked_call(decision_id: DecisionId) -> crate::orchestration::PendingCall {
        crate::orchestration::PendingCall {
            decision_id,
            tool_name: "kubectl_apply".to_string(),
            arguments: serde_json::json!({ "namespace": "prod" }),
            call_id: "call_1".to_string(),
        }
    }

    #[tokio::test]
    async fn unpublished_guard_drop_cancels_run_approvals() {
        let (registry, store) = registry_with_store();
        let run_id: RunId = "0191e8c0-2222-7000-8000-000000000042".parse().unwrap();
        let request_id = format!("req_guard_{}", uuid::Uuid::new_v4().simple());
        let mut events = crate::approval_event_broker::subscribe(&request_id).await;

        let scope = worker_scope(run_id);
        let decision_id = DecisionId::generate();
        registry
            .register_durable(durable_approval(
                decision_id,
                &format!("run:{run_id}"),
                &scope,
            ))
            .await
            .unwrap();

        let guard = ParkGuard::new(registry.clone(), run_id.to_string(), request_id.clone());
        guard.record(std::slice::from_ref(&parked_call(decision_id)));
        drop(guard);

        // The sweep runs as its own task; poll the store until it empties.
        for _ in 0..200 {
            if store.get(&decision_id).await.unwrap().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            store.get(&decision_id).await.unwrap().is_none(),
            "no decidable approval outlives the unpublished run"
        );

        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Some(crate::approval_event_broker::ApprovalLifecycleEvent::Completed(
                completed,
            ))) => {
                assert_eq!(completed.decision_id, decision_id.to_string());
                let outcome = serde_json::to_value(&completed.outcome).unwrap();
                assert_eq!(outcome["kind"], "cancelled");
            }
            other => panic!("expected a completed(cancelled) event, got {other:?}"),
        }

        crate::approval_event_broker::unsubscribe(&request_id).await;
    }

    #[tokio::test]
    async fn published_guard_drop_leaves_approvals_parked() {
        let (registry, store) = registry_with_store();
        let run_id: RunId = "0191e8c0-3333-7000-8000-000000000042".parse().unwrap();
        let scope = worker_scope(run_id);
        let decision_id = DecisionId::generate();
        registry
            .register_durable(durable_approval(
                decision_id,
                &format!("run:{run_id}"),
                &scope,
            ))
            .await
            .unwrap();

        let guard = ParkGuard::new(registry.clone(), run_id.to_string(), "req_x".to_string());
        guard.record(std::slice::from_ref(&parked_call(decision_id)));
        guard.mark_published();
        drop(guard);

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            store.get(&decision_id).await.unwrap().is_some(),
            "a published run's approvals survive its end"
        );
    }

    #[tokio::test]
    async fn unrecorded_guard_drop_is_inert() {
        let (registry, store) = registry_with_store();
        let run_id: RunId = "0191e8c0-4444-7000-8000-000000000042".parse().unwrap();
        let other = DecisionId::generate();
        registry
            .register_durable(durable_approval(
                other,
                "run:someone-else",
                &worker_scope(run_id),
            ))
            .await
            .unwrap();

        let guard = ParkGuard::new(registry.clone(), run_id.to_string(), "req_y".to_string());
        drop(guard);

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            store.get(&other).await.unwrap().is_some(),
            "an inert guard cancels nothing"
        );
    }
}
