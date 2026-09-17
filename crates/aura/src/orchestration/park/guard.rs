//! The run-scoped park guard (park mode).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::hitl::PendingApprovals;

use super::commit::cancel_run_approvals;
use super::lifetime::RunExecutionScope;

/// The guard's sweep disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkGuardMode {
    /// The initial producer: an unpublished drop sweeps the run's parked
    /// tickets by owner id.
    Initial,
    /// A resumed segment: checkpoint-preserving — cancellation never sweeps
    /// retained rows merely because the segment did not re-park. Retention
    /// cleanup (E6/E8), not the guard, reclaims expired evidence.
    Resumed,
}

/// Arms the unpublished-end sweep once the run has parked a call.
pub(crate) struct ParkGuard {
    registry: PendingApprovals,
    run_id: String,
    request_id: String,
    mode: ParkGuardMode,
    /// The execution scope the guard's deferred work spawns through: a
    /// clone of the run's ONE scope (the grant's for a resumed segment, the
    /// initial producer's for a first run), so the guard's tails register
    /// with the same tracker the supervisor drains before the fence
    /// releases. Set-once arming: the resume path sets it at construction
    /// (`new_with_execution_scope`); the initial producer arms it after the
    /// factory reserves the run (`arm_execution_scope`) — the guard is
    /// built before the persistence-bound run id exists to reserve.
    execution_scope: std::sync::OnceLock<Arc<RunExecutionScope>>,
    published: AtomicBool,
    armed: AtomicBool,
}

impl ParkGuard {
    /// Create the guard for an initial producer's run; inert until the
    /// first [`Self::record`].
    pub(crate) fn new(registry: PendingApprovals, run_id: String, request_id: String) -> Arc<Self> {
        Arc::new(Self {
            registry,
            run_id,
            request_id,
            mode: ParkGuardMode::Initial,
            execution_scope: std::sync::OnceLock::new(),
            published: AtomicBool::new(false),
            armed: AtomicBool::new(false),
        })
    }

    /// Create the guard with its sweep disposition and its execution scope
    /// TOGETHER — the L3 fill's injection seam. A resumed guard is always
    /// checkpoint-preserving AND scoped: its deferred sweep and tracked
    /// work spawn through the same [`RunExecutionScope`] the supervisor,
    /// driver, and tool contexts hold (cloned from the grant's one scope),
    /// so a guard tail can neither spawn unregistered nor outlive the
    /// run's drain.
    pub(crate) fn new_with_execution_scope(
        registry: PendingApprovals,
        run_id: String,
        request_id: String,
        mode: ParkGuardMode,
        execution_scope: Arc<RunExecutionScope>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            run_id,
            request_id,
            mode,
            execution_scope: std::sync::OnceLock::from(execution_scope),
            published: AtomicBool::new(false),
            armed: AtomicBool::new(false),
        })
    }

    /// Arm the deferred sweep's execution scope (set-once): the initial
    /// producer's factory calls this once it has reserved the
    /// persistence-bound run; an already-scoped (resumed) guard ignores it.
    pub(crate) fn arm_execution_scope(&self, scope: Arc<RunExecutionScope>) {
        let _ = self.execution_scope.set(scope);
    }

    /// Record parked calls; the first record arms the guard.
    pub(crate) fn record(&self, pending: &[crate::orchestration::PendingCall]) {
        if !pending.is_empty() {
            self.armed.store(true, Ordering::Release);
        }
    }

    /// Mark the run's checkpoint published; the drop becomes a no-op.
    pub(crate) fn mark_published(&self) {
        self.published.store(true, Ordering::Release);
    }

    /// The guard's sweep disposition — read by the L3b goldens to prove a
    /// resume segment builds the checkpoint-preserving guard.
    #[cfg(test)]
    pub(crate) fn mode(&self) -> ParkGuardMode {
        self.mode
    }

    /// A clone of the guard's execution scope, if any — the L3b goldens
    /// compare it by `Arc::ptr_eq` against the grant's ONE scope.
    #[cfg(test)]
    pub(crate) fn execution_scope(&self) -> Option<Arc<RunExecutionScope>> {
        self.execution_scope.get().cloned()
    }
}

impl Drop for ParkGuard {
    fn drop(&mut self) {
        // The resumed guard is checkpoint-preserving: whatever ends the
        // segment, retained evidence survives for the next resume or the
        // retention cleanup — only the initial producer sweeps.
        if self.mode == ParkGuardMode::Resumed {
            return;
        }
        // An unpublished drop sweeps the run's parked tickets by owner id; a
        // process crash inside the commit is the one accepted window.
        if self.published.load(Ordering::Acquire) {
            return;
        }
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let registry = self.registry.clone();
        let run_id = self.run_id.clone();
        let request_id = self.request_id.clone();
        // Drop cannot await; the sweep spawns its own task on the runtime
        // that dropped the guard. Off-runtime drops (a test teardown) log
        // and skip.
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                cancel_run_approvals(&registry, &run_id, &request_id, self.execution_scope.get());
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
    use std::pin::pin;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::hitl::{
        AgentScope, ApprovalAuthority, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId,
        PROTOCOL_VERSION, ParkedApproval, PendingApprovals,
    };
    use crate::orchestration::{ReservationTable, RunId, TaskIdentity, run_owner_id};
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
            authority: ApprovalAuthority::Conversational,
            egress_headers: None,
            acknowledgment: crate::hitl::AcknowledgmentState::RequiresNotification,
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

    /// L3b golden (RED today): a scoped guard's deferred sweep registers with
    /// the run's tracker before it spawns, so `drain` waits it out — an
    /// untracked `tokio::spawn` lets drain return while the sweep is still in
    /// flight.
    #[tokio::test]
    async fn scoped_guard_sweep_registers_with_tracker() {
        let (registry, store) = registry_with_store();
        let run_id: RunId = "0191e8c0-6666-7000-8000-000000000042".parse().unwrap();
        let request_id = format!("req_scoped_{}", uuid::Uuid::new_v4().simple());
        let table = ReservationTable::new();
        let lease = table.admit(run_id).expect("the run admits");
        let scope = RunExecutionScope::new(lease);

        let decision_id = DecisionId::generate();
        registry
            .register_durable(durable_approval(
                decision_id,
                &format!("run:{run_id}"),
                &worker_scope(run_id),
            ))
            .await
            .unwrap();

        let guard = ParkGuard::new_with_execution_scope(
            registry.clone(),
            run_id.to_string(),
            request_id,
            ParkGuardMode::Initial,
            scope.clone(),
        );
        guard.record(std::slice::from_ref(&parked_call(decision_id)));
        drop(guard);

        // The sweep registers with the tracker BEFORE spawning, so a single
        // synchronous poll of `drain` — with no runtime scheduling between —
        // sees a still-pending barrier. Untracked (the pre-L3b state), the
        // tracker is empty and the poll completes at once.
        {
            let mut drain = pin!(scope.drain());
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            assert!(
                std::future::Future::poll(drain.as_mut(), &mut cx).is_pending(),
                "a scoped guard's sweep must register with the tracker, so drain waits it out"
            );
        }
        tokio::time::timeout(Duration::from_secs(1), scope.drain())
            .await
            .expect("the registered sweep drains once its tail ends");
        assert!(
            store.get(&decision_id).await.unwrap().is_none(),
            "the scoped guard's sweep clears the run's parked ticket"
        );
    }

    /// L3b characterization (GREEN today, must stay green): a resumed guard
    /// is checkpoint-preserving — its drop never sweeps retained approvals,
    /// scoped or not.

    #[tokio::test]
    async fn armed_initial_guard_sweep_registers_with_tracker() {
        use crate::orchestration::ReservationTable;
        use crate::orchestration::park::lifetime::RunExecutionScope;

        let (registry, store) = registry_with_store();
        let run_id: RunId = "0191e8c0-5155-7000-8000-000000000042".parse().unwrap();
        let request_id = format!("req_guard_arm_{}", uuid::Uuid::new_v4().simple());
        let table = ReservationTable::new();
        let lease = table.admit(run_id).expect("admission");
        let scope = RunExecutionScope::new(lease);

        let scope_for_worker = worker_scope(run_id);
        let decision_id = DecisionId::generate();
        registry
            .register_durable(durable_approval(
                decision_id,
                &format!("run:{run_id}"),
                &scope_for_worker,
            ))
            .await
            .unwrap();

        // The initial path's shape: an unscoped guard armed with the run's
        // scope once the factory reserved the run.
        let guard = ParkGuard::new(registry.clone(), run_id.to_string(), request_id.clone());
        guard.arm_execution_scope(Arc::clone(&scope));
        guard.record(std::slice::from_ref(&parked_call(decision_id)));
        drop(guard);

        // The sweep registered with the tracker: drain cannot complete
        // while the sweep is still publishing (deterministic first-poll
        // probe, as in the scoped-construction golden).
        let mut drain = pin!(scope.drain());
        assert!(
            drain
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending(),
            "an armed initial guard's sweep must register with the tracker"
        );

        tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .expect("drain completes once the sweep finishes");
        assert!(
            store.get(&decision_id).await.unwrap().is_none(),
            "the sweep cleared the parked ticket"
        );
    }

    #[tokio::test]
    async fn resumed_guard_drop_preserves_parked_approvals() {
        let (registry, store) = registry_with_store();
        let run_id: RunId = "0191e8c0-7777-7000-8000-000000000042".parse().unwrap();
        let request_id = format!("req_resumed_{}", uuid::Uuid::new_v4().simple());
        let table = ReservationTable::new();
        let lease = table.admit(run_id).expect("the run admits");
        let scope = RunExecutionScope::new(lease);

        let decision_id = DecisionId::generate();
        registry
            .register_durable(durable_approval(
                decision_id,
                &format!("run:{run_id}"),
                &worker_scope(run_id),
            ))
            .await
            .unwrap();

        let guard = ParkGuard::new_with_execution_scope(
            registry.clone(),
            run_id.to_string(),
            request_id,
            ParkGuardMode::Resumed,
            scope,
        );
        guard.record(std::slice::from_ref(&parked_call(decision_id)));
        drop(guard);

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            store.get(&decision_id).await.unwrap().is_some(),
            "a resumed segment's drop preserves retained approvals"
        );
    }

    #[tokio::test]
    async fn unrecorded_guard_drop_is_inert() {
        let (registry, store) = registry_with_store();
        let run_id: RunId = "0191e8c0-4444-7000-8000-000000000042".parse().unwrap();
        let other = DecisionId::generate();
        // The ticket belongs to this run's owner id, so the arming condition
        // is load-bearing: an armed guard's drop would sweep and clear it.
        registry
            .register_durable(durable_approval(
                other,
                &run_owner_id(&run_id.to_string()),
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
