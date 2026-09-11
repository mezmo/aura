//! The config-gate surface: a [`ToolWrapper`] that gates tool calls whose name
//! matches a configured glob behind the deployment's [`DecisionRoute`].
//!
//! Composed first in the wrapper chain. `request_approval` (the agent-callable
//! surface) is excluded from glob matching so the gate never gates the approval
//! tool itself.

use std::sync::Arc;

use async_trait::async_trait;
use aura_config::GlobPattern;
use rig::tool::ToolError;
use serde_json::Value;

use super::decision::{AgentScope, ApprovalOrigin, DecisionId};
use super::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
use super::registry::{ParkedApproval, PendingApprovals};
use super::route::{ApprovalError, DecisionRoute, GateDecision};
use crate::orchestration::{
    BlockedCell, CallKey, ParkGuard, PendingCall, RecordedDecisions, run_owner_id,
};
use crate::tool_wrapper::{PreCallOutcome, ToolCallContext, ToolWrapper};

/// The placeholder tool result a parked call returns.
const PARK_SENTINEL: &str =
    "This tool call is parked pending human approval. It has not run. Do not retry.";

/// Park-arm state.
struct ParkContext {
    /// Registry over the approval store.
    registry: PendingApprovals,
    /// The worker's blocked cell.
    cell: Arc<BlockedCell>,
    /// The run's park guard.
    guard: Arc<ParkGuard>,
}

/// Gates matching tool calls behind an approval decision.
pub struct HitlApprovalWrapper {
    /// Compiled globs whose match raises a [`ApprovalOrigin::ConfigGate`].
    ///
    /// [`ApprovalOrigin::ConfigGate`]: super::decision::ApprovalOrigin::ConfigGate
    patterns: Arc<[GlobPattern]>,
    /// Shared across single-agent and orchestration; held by `Arc` because the
    /// gate and the agent tool both reference one route.
    route: Arc<DecisionRoute>,
    /// Who this wrapper speaks for, stamped onto every request it raises.
    scope: AgentScope,
    /// Global request id, for SSE event routing.
    request_id: String,
    /// `[agent].name` of the config that built this agent.
    agent_name: String,
    /// Instance ID of the AURA process that built this wrapper.
    instance_id: String,
    /// Park arm state.
    park: Option<ParkContext>,
    /// The run's recorded decisions for its parked calls; `None` on the live
    /// path so behavior is unchanged. When present, a recorded decision is
    /// consumed before the park arm; a miss while the task is strict is a
    /// resume fault, and a miss otherwise re-parks.
    recorded_decisions: Option<Arc<RecordedDecisions>>,
}

impl HitlApprovalWrapper {
    #[must_use]
    pub fn new(
        patterns: Arc<[GlobPattern]>,
        route: Arc<DecisionRoute>,
        scope: AgentScope,
        request_id: String,
        agent_name: String,
        instance_id: String,
    ) -> Self {
        Self {
            patterns,
            route,
            scope,
            request_id,
            agent_name,
            instance_id,
            park: None,
            recorded_decisions: None,
        }
    }

    /// Arm the park arm: glob-matched calls park as durable approvals.
    #[must_use]
    pub(crate) fn with_park(
        mut self,
        registry: PendingApprovals,
        cell: Arc<BlockedCell>,
        guard: Arc<ParkGuard>,
    ) -> Self {
        self.park = Some(ParkContext {
            registry,
            cell,
            guard,
        });
        self
    }

    /// Arm the recorded-decisions consult: a glob-matched call checks the
    /// run's recorded decisions before the park arm. `None` (the default)
    /// leaves the live path byte-identical. Wired by the orchestrator
    /// continuation (P44 commit 3).
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn with_recorded_decisions(mut self, recorded: Arc<RecordedDecisions>) -> Self {
        self.recorded_decisions = Some(recorded);
        self
    }

    /// First configured glob that matches `tool_name`, never gating the
    /// approval tool itself ("request_approval" == RequestApprovalTool::NAME).
    /// Any match gates the call; the returned pattern is only the reported
    /// `origin.matched_pattern`, so pattern order has no effect on gating.
    fn matched_pattern(&self, tool_name: &str) -> Option<&str> {
        if tool_name == "request_approval" {
            return None;
        }
        self.patterns
            .iter()
            .find(|p| p.matches(tool_name))
            .map(|p| p.as_str())
    }

    /// The park arm: register durably, publish, append to the blocked cell,
    /// and short-circuit with the inert sentinel. Ordering is load-bearing —
    /// a register error must fail the call closed before anything is
    /// published or recorded, so no checkpoint can reference a decision id
    /// the store does not hold.
    async fn park_pre_call(
        &self,
        park: &ParkContext,
        matched: &str,
        args: &Value,
        ctx: &ToolCallContext,
    ) -> Result<PreCallOutcome, ToolError> {
        // Park is worker-only: the scope carries the run and task identity.
        let AgentScope::Worker { run_id, .. } = &self.scope else {
            return Err(ToolError::ToolCallError(
                "tool call blocked: park mode requires an orchestration worker scope"
                    .to_string()
                    .into(),
            ));
        };
        // The route timeout bounds the decision window until a park TTL exists.
        let Some((_, timeout)) = self.route.park_registry() else {
            return Err(ToolError::ToolCallError(
                "tool call blocked: park mode requires a park-capable route"
                    .to_string()
                    .into(),
            ));
        };

        // Egress capture, resolved where the route was built per request: a
        // mapped destination with no usable value closes the registration
        // before anything persists — notify is egress auth with no later
        // reify checkpoint (deliberately stricter than identity docking).
        let egress_headers = match self.route.park_egress() {
            Ok(headers) => (!headers.is_empty()).then(|| headers.into_owned()),
            Err(err) => {
                tracing::warn!(
                    tool_name = %ctx.tool_name,
                    error = %err,
                    "park-mode webhook egress capture failed; failing the gated call closed",
                );
                return Err(ToolError::ToolCallError(
                    format!("tool call blocked: {err}").into(),
                ));
            }
        };

        let now = chrono::Utc::now();
        let expires_at =
            now + chrono::Duration::from_std(timeout).expect("approval timeout fits in chrono");
        let decision_id = DecisionId::generate();
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            instance_id: self.instance_id.clone(),
            decision_id,
            // Request teardown sweeps `cancel_request` by the live request
            // id; the run-scoped owner id keeps a parked ticket out of that
            // sweep. The run's own sweep passes the same `run_owner_id`.
            request_id: run_owner_id(&run_id.to_string()),
            scope: self.scope.clone(),
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: matched.to_string(),
                agent_name: self.agent_name.clone(),
            },
            items: vec![ApprovalItem {
                tool_name: ctx.tool_name.clone(),
                arguments: args.clone(),
                tool_call_intent: ctx.tool_call_intent.clone(),
            }],
        };

        // The store's own register, not the registry's park-anyway one: a
        // fault fails the call closed.
        let parked = ParkedApproval {
            request,
            registered_at: now,
            expires_at,
            egress_headers,
        };
        if let Err(err) = park.registry.register_durable(parked.clone()).await {
            tracing::warn!(
                decision_id = %decision_id,
                error = %err,
                "park-mode approval register failed; failing the gated call closed",
            );
            return Err(ToolError::ToolCallError(
                format!("tool call blocked: approval store register failed: {err}").into(),
            ));
        }

        let call_id = park.cell.take_current_call_id().unwrap_or_else(|| {
            tracing::warn!(
                decision_id = %decision_id,
                tool_name = %ctx.tool_name,
                "parked call has no tool-call id; recording an empty call_id",
            );
            String::new()
        });
        let call = PendingCall {
            decision_id,
            tool_name: ctx.tool_name.clone(),
            arguments: args.clone(),
            call_id,
        };
        // The guard sees the parked call now, so a run dropped before the
        // task returns still sweeps this ticket.
        park.guard.record(std::slice::from_ref(&call));

        // The lifecycle pair goes to the live request's broker, not the owner id.
        crate::approval_event_broker::publish(
            &self.request_id,
            crate::approval_event_broker::ApprovalLifecycleEvent::Requested(
                (&parked.request).into(),
            ),
        )
        .await;
        crate::approval_event_broker::publish(
            &self.request_id,
            crate::approval_event_broker::ApprovalLifecycleEvent::Pending(super::events::pending(
                &parked.request,
                &parked.expires_at,
            )),
        )
        .await;

        park.cell.push(call);
        tracing::info!(
            decision_id = %decision_id,
            tool_name = %ctx.tool_name,
            "parked gated call awaiting human decision",
        );

        Ok(PreCallOutcome::ShortCircuit {
            output: PARK_SENTINEL.to_string(),
        })
    }
}

#[async_trait]
impl ToolWrapper for HitlApprovalWrapper {
    async fn pre_call(
        &self,
        args: &Value,
        ctx: &ToolCallContext,
    ) -> Result<PreCallOutcome, ToolError> {
        let Some(matched) = self.matched_pattern(&ctx.tool_name) else {
            return Ok(PreCallOutcome::Proceed { overrides: None });
        };
        // Recorded-decisions consult: a resumed worker's gated call may
        // already carry a stored decision. A hit maps through the same
        // mapping the live route uses (so an approval proceeds and a denial
        // produces the live path's denial feedback); a miss while the task is
        // strict is a resume fault; a miss otherwise falls through to the park
        // arm (or the live route when park is unset) and re-parks.
        if let Some(recorded) = &self.recorded_decisions {
            // A resumed worker's tools always carry a task id. A gated call
            // without one on the resume path is a wiring fault, and falling
            // through to the park arm would re-ask the human for a decided
            // call.
            let Some(task_id) = ctx.task_id else {
                return Err(ToolError::ToolCallError(
                    "resume mismatch: no task id".to_string().into(),
                ));
            };
            match recorded.take(&CallKey::new(task_id, &ctx.tool_name, args)) {
                Some(resolved) => return recorded_pre_call(&self.route, resolved),
                // A continuation invocation that misses is a resume fault,
                // never a fresh park: the recorded call no longer matches what
                // the chain produced. Fail the call closed and let the resume
                // stream fail.
                None if recorded.is_strict(task_id) => {
                    return Err(ToolError::ToolCallError(
                        "resume mismatch".to_string().into(),
                    ));
                }
                // A model-issued gated call after the continuation re-parks
                // normally: fall through to the park arm (or the live route).
                None => {}
            }
        }
        if let Some(park) = &self.park {
            return self.park_pre_call(park, matched, args, ctx).await;
        }
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            instance_id: self.instance_id.clone(),
            decision_id: DecisionId::generate(),
            request_id: self.request_id.clone(),
            scope: self.scope.clone(),
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: matched.to_string(),
                agent_name: self.agent_name.clone(),
            },
            items: vec![ApprovalItem {
                tool_name: ctx.tool_name.clone(),
                arguments: args.clone(),
                tool_call_intent: ctx.tool_call_intent.clone(),
            }],
        };
        let cancel =
            crate::request_cancellation::RequestCancellation::token_for_id(&self.request_id)
                .unwrap_or_else(crate::request_cancellation::RequestCancelToken::unbound);
        approval_result_to_pre_call(self.route.decide_for_gate(request, &cancel).await)
    }
}

/// Map a gate-scoped decision to a pre-call outcome.
fn approval_result_to_pre_call(
    result: Result<GateDecision, ApprovalError>,
) -> Result<PreCallOutcome, ToolError> {
    match result {
        Ok(GateDecision::Approved { overrides }) => Ok(PreCallOutcome::Proceed { overrides }),
        Ok(GateDecision::Denied { reason }) => Ok(denial_outcome(reason)),
        Ok(GateDecision::TimedOut { .. }) => Err(ToolError::ToolCallError(
            "tool call denied: approval timed out".to_string().into(),
        )),
        Ok(GateDecision::Cancelled(_)) => Err(ToolError::ToolCallError(
            "tool call denied: approval cancelled".to_string().into(),
        )),
        Err(e) => Err(ToolError::ToolCallError(
            format!("tool call blocked: approval channel error: {e}").into(),
        )),
    }
}

/// The live path's denial feedback: the denial is feedback the model can act
/// on, so it short-circuits the call rather than erroring it.
fn denial_outcome(reason: Option<String>) -> PreCallOutcome {
    PreCallOutcome::ShortCircuit {
        output: format!(
            "Tool call blocked by human approval denial: {}. Do not execute this action.",
            reason.unwrap_or_else(|| "no reason provided".to_string())
        ),
    }
}

/// Map a recorded decision to a pre-call outcome — the same surfaces the live
/// gate produces. An approved call re-executes under its recorded identity,
/// riding the same `Proceed.overrides` apply point the sync gate captures
/// into; when the route's identity mapping demands identity and the recorded
/// approval carries none (the poll-200 capture failed closed), reify blocks
/// the approved execution rather than sending it under the requester's
/// credentials.
fn recorded_pre_call(
    route: &DecisionRoute,
    resolved: super::decision::ResolvedDecision,
) -> Result<PreCallOutcome, ToolError> {
    match resolved {
        super::decision::ResolvedDecision::Approved { identity } => {
            if identity.is_none() && route.requires_identity() {
                return Err(ToolError::ToolCallError(
                    "resume mismatch: approved call is missing required approver identity"
                        .to_string()
                        .into(),
                ));
            }
            Ok(PreCallOutcome::Proceed {
                overrides: identity,
            })
        }
        super::decision::ResolvedDecision::Denied { reason } => Ok(denial_outcome(reason)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aura_config::WebhookUrl;

    use super::super::decision::CancelReason;
    use super::super::route::{WebhookClient, build_webhook_client};
    use super::*;

    #[test]
    fn matched_pattern_selects_first_matching_glob_and_excludes_approval_tool() {
        let wrapper = HitlApprovalWrapper::new(
            Arc::from([GlobPattern::new("kubectl_*").unwrap()]),
            Arc::new(DecisionRoute::Webhook {
                client: WebhookClient::new(
                    build_webhook_client(),
                    WebhookUrl::new("http://localhost:9").unwrap(),
                ),
                registry: PendingApprovals::new(),
                timeout: Duration::from_secs(1),
                egress_capture: Ok(()),
            }),
            AgentScope::Single { session_id: None },
            "t".into(),
            "test-agent".to_string(),
            "test-instance-id".to_string(),
        );
        assert_eq!(wrapper.matched_pattern("kubectl_apply"), Some("kubectl_*"));
        assert_eq!(wrapper.matched_pattern("request_approval"), None);
        assert_eq!(wrapper.matched_pattern("ls"), None);
    }

    /// A matching tool whose approval channel is unreachable must fail closed
    /// (the call is blocked), while a non-matching tool stays transparent and
    /// never touches the route. Any channel result — connection refused
    /// (transport) or timeout — maps to a denial here.
    #[tokio::test]
    async fn matching_tool_fails_closed_when_webhook_unreachable() {
        let wrapper = HitlApprovalWrapper::new(
            Arc::from([GlobPattern::new("kubectl_*").unwrap()]),
            Arc::new(DecisionRoute::Webhook {
                client: WebhookClient::new(
                    build_webhook_client(),
                    // Discard port: nothing listens, so the POST fails closed.
                    WebhookUrl::new("http://127.0.0.1:9").unwrap(),
                ),
                registry: PendingApprovals::new(),
                timeout: Duration::from_secs(1),
                egress_capture: Ok(()),
            }),
            AgentScope::Single { session_id: None },
            "req-test".into(),
            "test-agent".to_string(),
            "test-instance-id".to_string(),
        );
        let args = serde_json::json!({});

        let gated = ToolCallContext::new("kubectl_apply");
        assert!(
            wrapper.pre_call(&args, &gated).await.is_err(),
            "gated tool must be blocked when the approval channel is down",
        );

        let ungated = ToolCallContext::new("ls");
        assert!(
            wrapper.pre_call(&args, &ungated).await.is_ok(),
            "non-matching tool must pass through without consulting the route",
        );
    }

    #[test]
    fn approval_result_mapping_proceeds_only_on_approval() {
        assert_eq!(
            approval_result_to_pre_call(Ok(GateDecision::Approved { overrides: None })).unwrap(),
            PreCallOutcome::Proceed { overrides: None }
        );
    }

    /// The mapping is the only path from an approval's captured identity to the call it released; a `Proceed` that dropped the overrides would send the gated call under the requester's identity instead.
    #[test]
    fn approval_result_mapping_carries_captured_overrides_into_the_call() {
        let captured = crate::approver_headers::tests::captured_overrides("authorization", "tok");

        assert_eq!(
            approval_result_to_pre_call(Ok(GateDecision::Approved {
                overrides: Some(captured.clone()),
            }))
            .unwrap(),
            PreCallOutcome::Proceed {
                overrides: Some(captured)
            },
        );
    }

    /// A denial is feedback the model can act on, not a tool error: the
    /// mapping short-circuits the call with the denial reason.
    #[test]
    fn approval_result_mapping_denial_is_feedback_not_error() {
        let outcome = approval_result_to_pre_call(Ok(GateDecision::Denied {
            reason: Some("too risky".to_string()),
        }))
        .unwrap();

        assert_eq!(
            outcome,
            PreCallOutcome::ShortCircuit {
                output: "Tool call blocked by human approval denial: too risky. Do not execute this action."
                    .to_string()
            }
        )
    }

    // ====================================================================
    // Park arm
    // ====================================================================

    mod park {
        use std::sync::Arc;
        use std::time::Duration;

        use super::*;
        use crate::session_store::ApprovalStore;

        fn worker_scope() -> AgentScope {
            AgentScope::Worker {
                run_id: "0191e8c0-1111-7000-8000-000000000042".parse().unwrap(),
                task: crate::orchestration::TaskIdentity::new(1, Some("operations".to_string())),
                session_id: None,
            }
        }

        fn conv_route(timeout: Duration) -> (PendingApprovals, Arc<DecisionRoute>) {
            let registry = PendingApprovals::new();
            let route = Arc::new(DecisionRoute::Conversational {
                registry: registry.clone(),
                timeout,
            });
            (registry, route)
        }

        /// Route over an explicit registry, for tests that need the store.
        fn conv_route_over(registry: PendingApprovals, timeout: Duration) -> Arc<DecisionRoute> {
            Arc::new(DecisionRoute::Conversational { registry, timeout })
        }

        fn parked_gate(
            registry: &PendingApprovals,
            route: &Arc<DecisionRoute>,
            request_id: &str,
            cell: &Arc<crate::orchestration::BlockedCell>,
        ) -> HitlApprovalWrapper {
            HitlApprovalWrapper::new(
                Arc::from([GlobPattern::new("kubectl_*").unwrap()]),
                route.clone(),
                worker_scope(),
                request_id.to_string(),
                "test-agent".to_string(),
                "test-instance".to_string(),
            )
            .with_park(
                registry.clone(),
                cell.clone(),
                ParkGuard::new(
                    registry.clone(),
                    "0191e8c0-1111-7000-8000-000000000042".to_string(),
                    request_id.to_string(),
                ),
            )
        }

        #[tokio::test]
        async fn register_error_fails_closed_with_no_cell_entry_and_no_event() {
            let request_id = format!("req_park_fail_{}", uuid::Uuid::new_v4().simple());
            let mut events = crate::approval_event_broker::subscribe(&request_id).await;
            let store: Arc<dyn crate::session_store::ApprovalStore> =
                Arc::new(crate::session_store::FaultInjectingStore::failing_register());
            let registry = PendingApprovals::with_backend(
                store,
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let route = conv_route_over(registry.clone(), Duration::from_secs(60));
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let gate = parked_gate(&registry, &route, &request_id, &cell);

            let args = serde_json::json!({ "namespace": "prod" });
            let ctx = ToolCallContext::new("kubectl_apply");
            let result = gate.pre_call(&args, &ctx).await;

            let err = result.expect_err("a register fault must fail the call closed");
            assert!(
                err.to_string().contains("approval store register failed"),
                "error must name the register fault, got: {err}"
            );
            assert!(
                err.to_string().contains("disk on fire"),
                "error must carry the store's reason, got: {err}"
            );
            assert!(
                cell.is_empty(),
                "no cell entry may exist after a register fault"
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(50), events.recv())
                    .await
                    .is_err(),
                "no approval event may be published after a register fault"
            );

            crate::approval_event_broker::unsubscribe(&request_id).await;
        }

        #[tokio::test]
        async fn happy_path_registers_publishes_appends_and_short_circuits() {
            let request_id = format!("req_park_ok_{}", uuid::Uuid::new_v4().simple());
            let mut events = crate::approval_event_broker::subscribe(&request_id).await;
            let store: Arc<dyn crate::session_store::ApprovalStore> =
                Arc::new(crate::session_store::InMemoryApprovalStore::new());
            let registry = PendingApprovals::with_backend(
                store.clone(),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let route = conv_route_over(registry.clone(), Duration::from_secs(120));
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            cell.set_current_call_id(Some("call_7".to_string()));
            let gate = parked_gate(&registry, &route, &request_id, &cell);

            let args = serde_json::json!({ "namespace": "prod" });
            let ctx = ToolCallContext::new("kubectl_apply");
            let outcome = gate.pre_call(&args, &ctx).await.unwrap();

            assert_eq!(
                outcome,
                PreCallOutcome::ShortCircuit {
                    output: super::PARK_SENTINEL.to_string()
                },
                "a parked call short-circuits with the inert sentinel"
            );

            // Mirror the hook's snapshot so the cell reports Blocked.
            cell.snapshot_if_pending(
                &[rig::completion::Message::user("do the thing")],
                &rig::completion::Message::user("tool results"),
            );
            match cell.outcome() {
                crate::orchestration::CellOutcome::Blocked { pending } => {
                    assert_eq!(pending.len(), 1);
                    assert_eq!(pending[0].tool_name, "kubectl_apply");
                    assert_eq!(pending[0].call_id, "call_7");
                    assert_eq!(pending[0].arguments, args);

                    // Store: the ticket is parked under the run-scoped owner.
                    let parked = store
                        .get(&pending[0].decision_id)
                        .await
                        .unwrap()
                        .expect("ticket parked in the store");
                    assert_eq!(
                        parked.request.request_id,
                        "run:0191e8c0-1111-7000-8000-000000000042"
                    );
                    assert_eq!(parked.request.items[0].tool_name, "kubectl_apply");
                    assert_eq!(parked.request.items[0].arguments, args);
                }
                other => panic!("expected Blocked, got {other:?}"),
            }

            // SSE: requested then pending, on the live request id.
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
                Ok(Some(crate::approval_event_broker::ApprovalLifecycleEvent::Requested(
                    requested,
                ))) => {
                    assert_eq!(requested.tool_name, "kubectl_apply");
                }
                other => panic!("expected Requested event, got {other:?}"),
            }
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
                Ok(Some(crate::approval_event_broker::ApprovalLifecycleEvent::Pending(
                    pending,
                ))) => {
                    assert_eq!(pending.tool_name, "kubectl_apply");
                    assert_eq!(pending.arguments, args);
                    let scope = serde_json::to_value(&pending.scope).unwrap();
                    assert_eq!(scope["kind"], "worker");
                    assert_eq!(scope["run_id"], "0191e8c0-1111-7000-8000-000000000042");
                }
                other => panic!("expected Pending event, got {other:?}"),
            }

            crate::approval_event_broker::unsubscribe(&request_id).await;
        }

        #[tokio::test]
        async fn two_gated_calls_append_two_cell_entries() {
            let (registry, route) = conv_route(Duration::from_secs(60));
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let gate = parked_gate(&registry, &route, "req-two-calls", &cell);

            let first = gate
                .pre_call(
                    &serde_json::json!({ "namespace": "prod" }),
                    &ToolCallContext::new("kubectl_apply"),
                )
                .await
                .unwrap();
            let second = gate
                .pre_call(
                    &serde_json::json!({ "namespace": "stage" }),
                    &ToolCallContext::new("kubectl_delete"),
                )
                .await
                .unwrap();
            assert!(matches!(first, PreCallOutcome::ShortCircuit { .. }));
            assert!(matches!(second, PreCallOutcome::ShortCircuit { .. }));

            // Inspect without consuming: mirror the cell contents.
            cell.snapshot_if_pending(&[], &rig::completion::Message::user("results"));
            match cell.outcome() {
                crate::orchestration::CellOutcome::Blocked { pending } => {
                    assert_eq!(pending.len(), 2, "both gated calls are recorded");
                    assert_eq!(pending[0].tool_name, "kubectl_apply");
                    assert_eq!(pending[1].tool_name, "kubectl_delete");
                    assert_ne!(pending[0].decision_id, pending[1].decision_id);
                }
                other => panic!("expected Blocked, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn ungated_tool_proceeds_without_parking() {
            let (registry, route) = conv_route(Duration::from_secs(60));
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let gate = parked_gate(&registry, &route, "req-ungated", &cell);

            let outcome = gate
                .pre_call(&serde_json::json!({}), &ToolCallContext::new("ls"))
                .await
                .unwrap();
            assert_eq!(outcome, PreCallOutcome::Proceed { overrides: None });
            assert!(cell.is_empty());
        }

        #[tokio::test]
        async fn guard_learns_the_decision_at_registration() {
            let store: Arc<dyn crate::session_store::ApprovalStore> =
                Arc::new(crate::session_store::InMemoryApprovalStore::new());
            let registry = PendingApprovals::with_backend(
                store.clone(),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let route = conv_route_over(registry.clone(), Duration::from_secs(60));
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let guard = ParkGuard::new(
                registry.clone(),
                "0191e8c0-1111-7000-8000-000000000042".to_string(),
                "req-guard".to_string(),
            );
            let gate = HitlApprovalWrapper::new(
                Arc::from([GlobPattern::new("kubectl_*").unwrap()]),
                route,
                worker_scope(),
                "req-guard".to_string(),
                "test-agent".to_string(),
                "test-instance".to_string(),
            )
            .with_park(registry, cell.clone(), Arc::clone(&guard));

            gate.pre_call(
                &serde_json::json!({}),
                &ToolCallContext::new("kubectl_apply"),
            )
            .await
            .unwrap();
            let decision_id = match cell.outcome() {
                crate::orchestration::CellOutcome::Orphaned { pending } => pending[0].decision_id,
                other => panic!("expected a parked call, got {other:?}"),
            };
            assert!(store.get(&decision_id).await.unwrap().is_some());

            // The run ends unpublished: the guard sweeps the ticket the park
            // arm registered, without any record from the orchestrator.
            drop(gate);
            drop(guard);
            for _ in 0..200 {
                if store.get(&decision_id).await.unwrap().is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(store.get(&decision_id).await.unwrap().is_none());
        }

        /// A webhook route with poll delivery parks the gated call. The
        /// route is wired the production way
        /// ([`crate::hitl::HitlRuntime::from_config`] over a poll config) so
        /// the client carries the poll marker; the park arm registers into
        /// the shared registry without consulting the unreachable webhook.
        /// The reconciler flow itself is the poller's.
        #[tokio::test]
        async fn webhook_poll_route_parks_the_gated_call() {
            let config = aura_config::HitlConfig {
                require_approval: vec![aura_config::GlobPattern::new("kubectl_*").unwrap()],
                park: aura_config::ParkConfig { enabled: true },
                route: aura_config::DecisionRouteConfig::Webhook {
                    url: WebhookUrl::new("http://127.0.0.1:9").unwrap(),
                    timeout_secs: 60,
                    headers: std::collections::HashMap::new(),
                    headers_from_request: std::collections::HashMap::new(),
                    tool_headers_from_response: aura_config::ToolHeaderMappings::default(),
                    delivery: aura_config::WebhookDelivery::Poll,
                    poll_url: None,
                    poll_interval_secs: 10,
                    poll_request_timeout_secs: 30,
                },
            };
            let store: Arc<dyn crate::session_store::ApprovalStore> =
                Arc::new(crate::session_store::InMemoryApprovalStore::new());
            let registry = PendingApprovals::with_backend(
                store.clone(),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let runtime = crate::hitl::HitlRuntime::from_config(&config, &registry, None, None);
            assert!(
                runtime.route.park_registry().is_some(),
                "the poll route arms the park arm"
            );

            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            cell.set_current_call_id(Some("call_poll".to_string()));
            let gate = parked_gate(&registry, &runtime.route, "req-poll-park", &cell);

            let args = serde_json::json!({ "namespace": "prod" });
            let outcome = gate
                .pre_call(&args, &ToolCallContext::new("kubectl_apply"))
                .await
                .unwrap();
            assert_eq!(
                outcome,
                PreCallOutcome::ShortCircuit {
                    output: super::PARK_SENTINEL.to_string()
                },
                "a gated call parks under poll delivery"
            );

            // The registration landed in the shared store under the
            // run-scoped owner.
            cell.snapshot_if_pending(&[], &rig::completion::Message::user("results"));
            match cell.outcome() {
                crate::orchestration::CellOutcome::Blocked { pending } => {
                    assert_eq!(pending.len(), 1);
                    let parked = store
                        .get(&pending[0].decision_id)
                        .await
                        .unwrap()
                        .expect("ticket parked in the store");
                    assert_eq!(
                        parked.request.request_id,
                        "run:0191e8c0-1111-7000-8000-000000000042"
                    );
                }
                other => panic!("expected Blocked, got {other:?}"),
            }
        }

        /// A poll webhook config with `headers_from_request`, built the
        /// production way with `req_headers` supplied by the caller.
        fn poll_route_with_mapping(
            registry: &PendingApprovals,
            req_headers: Option<&std::collections::HashMap<String, String>>,
            static_headers: std::collections::HashMap<String, String>,
        ) -> (
            Arc<dyn crate::session_store::ApprovalStore>,
            Arc<DecisionRoute>,
        ) {
            let store: Arc<dyn crate::session_store::ApprovalStore> =
                Arc::new(crate::session_store::InMemoryApprovalStore::new());
            let config = aura_config::HitlConfig {
                require_approval: vec![aura_config::GlobPattern::new("kubectl_*").unwrap()],
                park: aura_config::ParkConfig { enabled: true },
                route: aura_config::DecisionRouteConfig::Webhook {
                    url: WebhookUrl::new("https://approvals.example.com/hook").unwrap(),
                    timeout_secs: 60,
                    headers: static_headers,
                    headers_from_request: std::collections::HashMap::from([(
                        "authorization".to_string(),
                        "x-incoming-auth".to_string(),
                    )]),
                    tool_headers_from_response: aura_config::ToolHeaderMappings::default(),
                    delivery: aura_config::WebhookDelivery::Poll,
                    poll_url: None,
                    poll_interval_secs: 10,
                    poll_request_timeout_secs: 30,
                },
            };
            let runtime =
                crate::hitl::HitlRuntime::from_config(&config, registry, None, req_headers);
            (store, runtime.route)
        }

        fn req_headers(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        }

        /// Capture failure fails the REGISTRATION closed: a mapped
        /// destination with no usable resolved value and no static fallback
        /// produces no approval row, no pending event, and no blocked-cell
        /// entry — so there is also nothing for the reconciler to notify.
        #[tokio::test]
        async fn egress_capture_failure_fails_the_registration_closed() {
            let request_id = format!("req_egress_fail_{}", uuid::Uuid::new_v4().simple());
            let mut events = crate::approval_event_broker::subscribe(&request_id).await;
            let store = Arc::new(crate::session_store::InMemoryApprovalStore::new());
            let registry = PendingApprovals::with_backend(
                store.clone(),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let (_, route) = poll_route_with_mapping(&registry, None, Default::default());
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let gate = parked_gate(&registry, &route, &request_id, &cell);

            let err = gate
                .pre_call(
                    &serde_json::json!({ "namespace": "prod" }),
                    &ToolCallContext::new("kubectl_apply"),
                )
                .await
                .expect_err("an unresolvable mapped destination must fail the call closed");

            let message = err.to_string();
            assert!(
                message.contains("egress capture failed") && message.contains("authorization"),
                "the error names the capture failure and the destination: {message}"
            );
            assert!(
                store.list_pending().await.unwrap().is_empty(),
                "no approval row may exist"
            );
            assert!(cell.is_empty(), "no blocked-cell entry may exist");
            assert!(
                tokio::time::timeout(Duration::from_millis(50), events.recv())
                    .await
                    .is_err(),
                "no approval event may be published"
            );
            crate::approval_event_broker::unsubscribe(&request_id).await;
        }

        /// The successful capture copies the request-scoped resolved values
        /// onto the parked row: the row carries THIS request's credential,
        /// the input the reconciler will notify with.
        #[tokio::test]
        async fn egress_capture_lands_on_the_parked_row() {
            let store = Arc::new(crate::session_store::InMemoryApprovalStore::new());
            let registry = PendingApprovals::with_backend(
                store.clone(),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let (_, route) = poll_route_with_mapping(
                &registry,
                Some(&req_headers(&[(
                    "x-incoming-auth",
                    "Bearer request-scoped",
                )])),
                Default::default(),
            );
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let gate = parked_gate(&registry, &route, "req-egress-ok", &cell);

            gate.pre_call(
                &serde_json::json!({ "namespace": "prod" }),
                &ToolCallContext::new("kubectl_apply"),
            )
            .await
            .expect("a resolvable mapping parks normally");

            cell.snapshot_if_pending(&[], &rig::completion::Message::user("results"));
            match cell.outcome() {
                crate::orchestration::CellOutcome::Blocked { pending } => {
                    let parked = store
                        .get(&pending[0].decision_id)
                        .await
                        .unwrap()
                        .expect("ticket parked");
                    let row = parked.egress_headers.as_ref().expect("row egress headers");
                    assert_eq!(
                        row.get("authorization").unwrap(),
                        "Bearer request-scoped",
                        "the parked row carries this request's resolved credential"
                    );
                }
                other => panic!("expected Blocked, got {other:?}"),
            }
        }

        /// An explicit valid static fallback keeps the existing resolution
        /// semantics: the absent request header resolves to the static value
        /// and the row parks with it.
        #[tokio::test]
        async fn static_fallback_keeps_registration_open() {
            let store = Arc::new(crate::session_store::InMemoryApprovalStore::new());
            let registry = PendingApprovals::with_backend(
                store.clone(),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let (_, route) = poll_route_with_mapping(
                &registry,
                None,
                std::collections::HashMap::from([(
                    "authorization".to_string(),
                    "Bearer static-fallback".to_string(),
                )]),
            );
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let gate = parked_gate(&registry, &route, "req-egress-fallback", &cell);

            gate.pre_call(
                &serde_json::json!({}),
                &ToolCallContext::new("kubectl_apply"),
            )
            .await
            .expect("a static fallback keeps the registration open");

            cell.snapshot_if_pending(&[], &rig::completion::Message::user("results"));
            match cell.outcome() {
                crate::orchestration::CellOutcome::Blocked { pending } => {
                    let parked = store
                        .get(&pending[0].decision_id)
                        .await
                        .unwrap()
                        .expect("ticket parked");
                    assert_eq!(
                        parked
                            .egress_headers
                            .as_ref()
                            .expect("row egress headers")
                            .get("authorization")
                            .unwrap(),
                        "Bearer static-fallback",
                    );
                }
                other => panic!("expected Blocked, got {other:?}"),
            }
        }
    }

    // ====================================================================
    // Recorded-decisions consult
    // ====================================================================

    mod recorded {
        use std::sync::Arc;
        use std::time::Duration;

        use super::*;
        use crate::hitl::{ApprovalDecision, ResolvedDecision};

        /// A route whose webhook is unreachable, so a fall-through to the
        /// route fails closed rather than hanging. A recorded hit
        /// short-circuits before the route, so it never sees this.
        fn discard_route() -> Arc<DecisionRoute> {
            Arc::new(DecisionRoute::Webhook {
                client: WebhookClient::new(
                    build_webhook_client(),
                    // Discard port: nothing listens, so the POST fails closed.
                    WebhookUrl::new("http://127.0.0.1:9").unwrap(),
                ),
                registry: PendingApprovals::new(),
                timeout: Duration::from_secs(2),
                egress_capture: Ok(()),
            })
        }

        /// A gate armed with `recorded_decisions` and no park arm, so a miss
        /// falls through to the live route (the contract path the brief calls
        /// out for these tests).
        fn recorded_gate(
            recorded: Arc<RecordedDecisions>,
            route: Arc<DecisionRoute>,
        ) -> HitlApprovalWrapper {
            HitlApprovalWrapper::new(
                Arc::from([GlobPattern::new("kubectl_*").unwrap()]),
                route,
                AgentScope::Single { session_id: None },
                "req-recorded".to_string(),
                "test-agent".to_string(),
                "test-instance".to_string(),
            )
            .with_recorded_decisions(recorded)
        }

        fn ctx_for(tool: &str, task_id: Option<usize>) -> ToolCallContext {
            let mut ctx = ToolCallContext::new(tool);
            ctx.task_id = task_id;
            ctx
        }

        /// A route with `tool_headers_from_response` configured, so the
        /// reify-side rule "approved calls must carry identity" is armed.
        fn identity_route() -> Arc<DecisionRoute> {
            let config = aura_config::HitlConfig {
                require_approval: vec![],
                park: aura_config::ParkConfig::default(),
                route: aura_config::DecisionRouteConfig::Webhook {
                    url: WebhookUrl::new("https://approvals.example.com/hook").unwrap(),
                    timeout_secs: 60,
                    headers: Default::default(),
                    headers_from_request: Default::default(),
                    tool_headers_from_response: crate::approver_headers::tests::mappings(&[(
                        "x-forwarded-user",
                        "x-approver-id",
                    )]),
                    delivery: aura_config::WebhookDelivery::Sync,
                    poll_url: None,
                    poll_interval_secs: 10,
                    poll_request_timeout_secs: 30,
                },
            };
            let client =
                crate::hitl::webhook_client_from_config(&config.route, None, None).unwrap();
            Arc::new(DecisionRoute::Webhook {
                client,
                registry: PendingApprovals::new(),
                timeout: Duration::from_secs(2),
                egress_capture: Ok(()),
            })
        }

        fn identity(values: &[(&str, &str)]) -> crate::approver_headers::ApproverHeaders {
            crate::approver_headers::ApproverHeaders::from_pairs(
                values
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
            )
            .expect("identity test pairs are valid headers")
        }

        /// A recorded approval WITH captured identity re-executes under it:
        /// the overrides ride the same `Proceed` apply point the sync gate
        /// captures into.
        #[tokio::test]
        async fn recorded_identity_is_applied_at_reexecution() {
            let recorded = Arc::new(RecordedDecisions::default());
            let args = serde_json::json!({"namespace": "prod"});
            recorded.push(
                CallKey::new(1, "kubectl_apply", &args),
                ResolvedDecision::approved(Some(identity(&[(
                    "x-forwarded-user",
                    "approver-alice",
                )]))),
            );

            let gate = recorded_gate(recorded, identity_route());
            let outcome = gate
                .pre_call(&args, &ctx_for("kubectl_apply", Some(1)))
                .await
                .expect("a recorded approval with its identity re-executes");

            match outcome {
                PreCallOutcome::Proceed {
                    overrides: Some(overrides),
                } => assert_eq!(
                    overrides.captured_names().collect::<Vec<_>>(),
                    ["x-forwarded-user"],
                ),
                other => panic!("expected Proceed with overrides, got {other:?}"),
            }
        }

        /// Reify blocks an approved call whose identity capture failed
        /// (recorded without identity) when the route demands identity:
        /// record-then-block, per the approver identity ADR — the decision
        /// is not lost, but the call never runs under the requester's
        /// credentials.
        #[tokio::test]
        async fn approved_without_required_identity_fails_the_call_closed() {
            let recorded = Arc::new(RecordedDecisions::default());
            let args = serde_json::json!({"namespace": "prod"});
            recorded.push(
                CallKey::new(1, "kubectl_apply", &args),
                ResolvedDecision::approved(None),
            );

            let gate = recorded_gate(recorded, identity_route());
            let err = gate
                .pre_call(&args, &ctx_for("kubectl_apply", Some(1)))
                .await
                .expect_err("an approved call missing required identity must fail closed");

            let message = err.to_string();
            assert!(
                message.contains("required approver identity"),
                "the error names the missing identity, got: {message}"
            );
            assert!(
                !message.contains("approval channel"),
                "this is a reify block, not a route fault: {message}"
            );
        }

        /// Without an identity mapping the same uncaptured approval proceeds:
        /// the block keys off the route's demand, not off identity being
        /// absent per se.
        #[tokio::test]
        async fn approved_without_identity_proceeds_when_route_demands_none() {
            let recorded = Arc::new(RecordedDecisions::default());
            let args = serde_json::json!({"namespace": "prod"});
            recorded.push(
                CallKey::new(1, "kubectl_apply", &args),
                ResolvedDecision::approved(None),
            );

            let gate = recorded_gate(recorded, discard_route());
            let outcome = gate
                .pre_call(&args, &ctx_for("kubectl_apply", Some(1)))
                .await
                .unwrap();
            assert_eq!(outcome, PreCallOutcome::Proceed { overrides: None });
        }

        /// A recorded approval proceeds through the same mapping the live
        /// route uses, without ever consulting the (unreachable) route.
        #[tokio::test]
        async fn recorded_hit_proceeds_without_invoking_route() {
            let recorded = Arc::new(RecordedDecisions::default());
            let args = serde_json::json!({"namespace": "prod"});
            recorded.push(
                CallKey::new(1, "kubectl_apply", &args),
                ApprovalDecision::Approved.into(),
            );

            let gate = recorded_gate(recorded, discard_route());
            let outcome = gate
                .pre_call(&args, &ctx_for("kubectl_apply", Some(1)))
                .await
                .unwrap();

            assert_eq!(outcome, PreCallOutcome::Proceed { overrides: None });
        }

        /// A recorded denial produces the live path's denial feedback string,
        /// through the shared mapping — the denial string is not duplicated
        /// in the consult.
        #[tokio::test]
        async fn recorded_denial_produces_live_path_denial_feedback() {
            let recorded = Arc::new(RecordedDecisions::default());
            let args = serde_json::json!({"namespace": "prod"});
            recorded.push(
                CallKey::new(1, "kubectl_apply", &args),
                ApprovalDecision::Denied {
                    reason: Some("too risky".to_string()),
                }
                .into(),
            );

            let gate = recorded_gate(recorded, discard_route());
            let outcome = gate
                .pre_call(&args, &ctx_for("kubectl_apply", Some(1)))
                .await
                .unwrap();

            assert_eq!(
                outcome,
                PreCallOutcome::ShortCircuit {
                    output: "Tool call blocked by human approval denial: too risky. Do not execute this action."
                        .to_string(),
                },
            );
        }

        /// A miss while the task is strict is a resume fault, never a fresh
        /// park: the route is not consulted (no "approval channel error").
        #[tokio::test]
        async fn recorded_miss_while_strict_fails_closed_with_resume_mismatch() {
            let recorded = Arc::new(RecordedDecisions::default());
            recorded.set_strict(1, true);

            let gate = recorded_gate(recorded, discard_route());
            let args = serde_json::json!({"namespace": "prod"});
            let err = gate
                .pre_call(&args, &ctx_for("kubectl_apply", Some(1)))
                .await
                .expect_err("a strict miss must fail closed");

            let msg = err.to_string();
            assert!(
                msg.contains("resume mismatch"),
                "a strict miss must report a resume mismatch, got: {msg}",
            );
            assert!(
                !msg.contains("approval channel error"),
                "the route must not be consulted on a strict miss, got: {msg}",
            );
        }

        /// A miss when the task is not strict falls through to the live route
        /// (park is unset), proving the consult did not short-circuit. The
        /// contrast with the strict-miss test is the error class.
        #[tokio::test]
        async fn recorded_miss_not_strict_falls_through_to_route() {
            let recorded = Arc::new(RecordedDecisions::default());

            let gate = recorded_gate(recorded, discard_route());
            let args = serde_json::json!({"namespace": "prod"});
            let err = gate
                .pre_call(&args, &ctx_for("kubectl_apply", Some(1)))
                .await
                .expect_err("the unreachable route must fail closed");

            let msg = err.to_string();
            assert!(
                msg.contains("approval channel error"),
                "a non-strict miss must fall through to the route, got: {msg}",
            );
            assert!(
                !msg.contains("resume mismatch"),
                "a non-strict miss must not report a resume mismatch, got: {msg}",
            );
        }

        /// A gated call on the resume path with no task id is a wiring fault,
        /// not a fall-through to the park arm (which would re-ask the human
        /// for a decided call).
        #[tokio::test]
        async fn recorded_consult_without_task_id_is_a_resume_fault() {
            let recorded = Arc::new(RecordedDecisions::default());

            let gate = recorded_gate(recorded, discard_route());
            let args = serde_json::json!({});
            let err = gate
                .pre_call(&args, &ctx_for("kubectl_apply", None))
                .await
                .expect_err("a gated resume call without a task id must fail");

            let msg = err.to_string();
            assert!(
                msg.contains("resume mismatch: no task id"),
                "expected the no-task-id wiring fault, got: {msg}",
            );
            assert!(
                !msg.contains("approval channel error"),
                "the route must not be consulted on the no-task-id fault, got: {msg}",
            );
        }
    }

    #[test]
    fn approval_result_mapping_timeout_cancel_and_channel_fault_are_errors() {
        let timed_out = approval_result_to_pre_call(Ok(GateDecision::TimedOut {
            waited: Duration::from_secs(1),
        }))
        .unwrap_err()
        .to_string();
        assert!(timed_out.contains("approval timed out"));

        let cancelled = approval_result_to_pre_call(Ok(GateDecision::Cancelled(
            CancelReason::ClientDisconnected,
        )))
        .unwrap_err()
        .to_string();
        assert!(cancelled.contains("approval cancelled"));

        let sender_dropped =
            approval_result_to_pre_call(Ok(GateDecision::Cancelled(CancelReason::SenderDropped)))
                .unwrap_err()
                .to_string();
        assert!(sender_dropped.contains("approval cancelled"));

        let channel_fault =
            approval_result_to_pre_call(Err(ApprovalError::BadStatus { status: 500 }))
                .unwrap_err()
                .to_string();
        assert!(channel_fault.contains("approval channel error"));
    }

    /// Trace correlation: a gated call's `execute_tool` span carries the
    /// `decision_id` of the approval that gated it.
    ///
    /// Gated on `otel`: without the feature there is no span data to
    /// assert against.
    #[cfg(feature = "otel")]
    mod decision_id_span {

        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use opentelemetry::trace::TracerProvider as _;

        use opentelemetry_sdk::trace::TracerProvider;
        use rig::completion::ToolDefinition;
        use rig::tool::Tool as RigTool;
        use serde_json::json;
        use tokio::sync::mpsc::Receiver;
        use tracing::Instrument;
        use tracing_subscriber::layer::SubscriberExt;

        use super::super::super::decision::ApprovalDecision;
        use super::super::super::registry::PendingApprovals;
        use super::*;
        use crate::approval_event_broker::{self, ApprovalLifecycleEvent};
        use crate::logging::ATTR_DECISION_ID;
        use crate::test_span_capture::{CapturedSpans, traced_as_execute_tool};
        use crate::tool_wrapper::WrappedTool;

        /// What a trace backend actually receives, assembled the way the binary
        /// assembles it: the real OTel filter, the OpenInference exporter, and
        /// the span reaching the tool through Rig's tool-server task rather
        /// than inline. Each of those can silently drop an attribute — a filter
        /// that stops enabling Rig's span leaves it with nowhere to land, and
        /// the exporter rewrites every span on its way out.
        #[tokio::test]
        async fn decision_id_reaches_the_exporter_through_the_binary_stack() {
            use tracing_subscriber::Layer;

            let captured = CapturedSpans::default();
            let provider = TracerProvider::builder()
                .with_simple_exporter(crate::openinference_exporter::OpenInferenceExporter::new(
                    captured.clone(),
                ))
                .build();
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(
                    tracing_opentelemetry::layer()
                        .with_tracer(provider.tracer("aura"))
                        .with_filter(crate::logging::otel_filter("aura_web_server")),
                ),
            );

            let request_id = unique_request_id();
            let mut events = approval_event_broker::subscribe(&request_id).await;
            let registry = PendingApprovals::new();
            let (tool, ran) = gated_tool(
                DecisionRoute::Conversational {
                    registry: registry.clone(),
                    timeout: Duration::from_secs(60),
                },
                &request_id,
                "kubectl_apply",
            );

            // Rig's streaming loop opens this span and hands it to the tool
            // server, which runs the toolset call instrumented with it.
            let execute_tool = tracing::info_span!(
                target: "rig::agent::prompt_request::streaming",
                "execute_tool",
                gen_ai.operation.name = "execute_tool",
            );
            let call = tokio::spawn(
                async move { tool.call(json!({ "namespace": "prod" })).await }
                    .instrument(execute_tool),
            );

            let payload_id = payload_decision_id(&mut events).await;
            registry
                .resolve(&payload_id, ApprovalDecision::Approved.into())
                .await
                .expect("parked approval resolves");
            call.await
                .expect("tool task did not panic")
                .expect("an approved call proceeds");

            for _ in 0..1_000 {
                if captured.contains("execute_tool") {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(
                ran.load(Ordering::SeqCst),
                "an approved call must reach the inner tool",
            );
            assert_eq!(
                captured
                    .attribute("execute_tool", ATTR_DECISION_ID)
                    .as_deref(),
                Some(payload_id.to_string().as_str()),
                "the exported execute_tool span must carry the decision id",
            );
            // Phoenix keys its TOOL classification off this, and the exporter
            // adds it to the same span the id lands on.
            assert_eq!(
                captured
                    .attribute("execute_tool", "openinference.span.kind")
                    .as_deref(),
                Some("TOOL"),
            );

            approval_event_broker::unsubscribe(&request_id).await;
        }

        /// Inner tool that records whether the gate let it run.
        #[derive(Clone)]
        struct StubTool {
            name: String,
            ran: Arc<AtomicBool>,
        }

        impl RigTool for StubTool {
            const NAME: &'static str = "stub";

            type Error = ToolError;
            type Args = Value;
            type Output = String;

            fn name(&self) -> String {
                self.name.clone()
            }

            async fn definition(&self, _prompt: String) -> ToolDefinition {
                ToolDefinition {
                    name: self.name.clone(),
                    description: String::new(),
                    parameters: json!({ "type": "object" }),
                }
            }

            async fn call(&self, _args: Value) -> Result<String, ToolError> {
                self.ran.store(true, Ordering::SeqCst);
                Ok("done".to_string())
            }
        }

        /// A tool behind the `kubectl_*` gate, wrapped the way production wraps
        /// it, plus the flag that reports whether the inner tool ran.
        fn gated_tool(
            route: DecisionRoute,
            request_id: &str,
            tool_name: &str,
        ) -> (WrappedTool<StubTool>, Arc<AtomicBool>) {
            let ran = Arc::new(AtomicBool::new(false));
            let inner = StubTool {
                name: tool_name.to_string(),
                ran: ran.clone(),
            };
            let gate = HitlApprovalWrapper::new(
                Arc::from([GlobPattern::new("kubectl_*").unwrap()]),
                Arc::new(route),
                AgentScope::Single { session_id: None },
                request_id.to_string(),
                "test-agent".to_string(),
                "test-instance-id".to_string(),
            );
            (
                WrappedTool::new(inner, Arc::new(gate) as Arc<dyn ToolWrapper>),
                ran,
            )
        }

        /// The decision id the approval payload carried, read off the
        /// `Requested` lifecycle event the route publishes for it.
        async fn payload_decision_id(events: &mut Receiver<ApprovalLifecycleEvent>) -> DecisionId {
            match events.recv().await.expect("approval events channel open") {
                ApprovalLifecycleEvent::Requested(event) => {
                    DecisionId::parse(&event.decision_id).expect("valid decision id")
                }
                other => panic!("expected Requested, got {other:?}"),
            }
        }

        fn unique_request_id() -> String {
            format!("req_span_{}", uuid::Uuid::new_v4().simple())
        }

        /// The correlation the whole feature exists for: the id the approver
        /// decided against is the id on the span of the execution it released.
        #[tokio::test]
        async fn approved_gate_stamps_the_payload_decision_id_on_the_execution_span() {
            let request_id = unique_request_id();
            let mut events = approval_event_broker::subscribe(&request_id).await;
            let registry = PendingApprovals::new();
            let (tool, ran) = gated_tool(
                DecisionRoute::Conversational {
                    registry: registry.clone(),
                    timeout: Duration::from_secs(60),
                },
                &request_id,
                "kubectl_apply",
            );

            let ((result, payload_id), span_id) = traced_as_execute_tool(async {
                tokio::join!(tool.call(json!({ "namespace": "prod" })), async {
                    let id = payload_decision_id(&mut events).await;
                    registry
                        .resolve(&id, ApprovalDecision::Approved.into())
                        .await
                        .expect("parked approval resolves");
                    id
                })
            })
            .await;

            assert_eq!(result.expect("an approved call proceeds"), "done");
            assert!(
                ran.load(Ordering::SeqCst),
                "an approved call must reach the inner tool",
            );
            assert_eq!(
                span_id.as_deref(),
                Some(payload_id.to_string().as_str()),
                "the execution span must carry the approval payload's decision id",
            );

            approval_event_broker::unsubscribe(&request_id).await;
        }

        /// An attempt that never reaches a decision is exactly where the
        /// correlation earns its keep: the trace still names the approval the
        /// failed-closed execution was waiting on.
        #[tokio::test(start_paused = true)]
        async fn timed_out_gate_stamps_the_decision_id_on_the_execution_span() {
            let request_id = unique_request_id();
            let mut events = approval_event_broker::subscribe(&request_id).await;
            let (tool, ran) = gated_tool(
                DecisionRoute::Conversational {
                    registry: PendingApprovals::new(),
                    timeout: Duration::from_secs(30),
                },
                &request_id,
                "kubectl_apply",
            );

            let ((result, payload_id), span_id) = traced_as_execute_tool(async {
                tokio::join!(tool.call(json!({})), payload_decision_id(&mut events))
            })
            .await;

            let error = result.expect_err("an undecided approval must fail closed");
            assert!(
                error.to_string().contains("approval timed out"),
                "expected a timeout denial, got: {error}",
            );
            assert!(!ran.load(Ordering::SeqCst));
            assert_eq!(span_id.as_deref(), Some(payload_id.to_string().as_str()));

            approval_event_broker::unsubscribe(&request_id).await;
        }

        /// The webhook route stamps the same id from the same place, so the
        /// correlation does not depend on which route the deployment picked.
        #[tokio::test]
        async fn webhook_gate_stamps_the_decision_id_on_the_execution_span() {
            let request_id = unique_request_id();
            let mut events = approval_event_broker::subscribe(&request_id).await;
            let (tool, ran) = gated_tool(
                DecisionRoute::Webhook {
                    client: WebhookClient::new(
                        build_webhook_client(),
                        // Discard port: nothing listens, so the POST fails closed.
                        WebhookUrl::new("http://127.0.0.1:9").unwrap(),
                    ),
                    registry: PendingApprovals::new(),
                    timeout: Duration::from_secs(2),
                    egress_capture: Ok(()),
                },
                &request_id,
                "kubectl_delete",
            );

            let ((result, payload_id), span_id) = traced_as_execute_tool(async {
                tokio::join!(tool.call(json!({})), payload_decision_id(&mut events))
            })
            .await;

            assert!(
                result.is_err(),
                "an unreachable webhook must block the call",
            );
            assert!(!ran.load(Ordering::SeqCst));
            assert_eq!(span_id.as_deref(), Some(payload_id.to_string().as_str()));

            approval_event_broker::unsubscribe(&request_id).await;
        }

        #[tokio::test]
        async fn ungated_call_records_no_decision_id() {
            let (tool, ran) = gated_tool(
                DecisionRoute::Conversational {
                    registry: PendingApprovals::new(),
                    timeout: Duration::from_secs(60),
                },
                &unique_request_id(),
                "ls",
            );

            let (result, span_id) = traced_as_execute_tool(tool.call(json!({}))).await;

            assert_eq!(result.expect("an ungated call proceeds"), "done");
            assert!(ran.load(Ordering::SeqCst));
            assert_eq!(
                span_id, None,
                "an ungated call must not be correlated to any approval",
            );
        }
    }
}
