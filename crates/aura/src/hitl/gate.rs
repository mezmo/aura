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

use super::decision::{
    AgentScope, ApprovalOrigin, ApprovalOutcome, ApprovalOwner, DecisionId, live_approval_context,
};
use super::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
use super::registry::{ParkedApproval, PendingApprovals};
use super::route::{ApprovalError, DecisionRoute, GateDecision};
use crate::orchestration::{BlockedCell, CallKey, ParkGuard, PendingCall, RecordedDecisions};
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
    /// The run whose observer sees this gate's approvals.
    run: crate::run_context::BoundRun,
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
        agent_name: String,
        instance_id: String,
    ) -> Self {
        Self {
            patterns,
            route,
            scope,
            // A worker's gate is built inside its run; a single agent's is
            // built before one exists and is bound by `stream`.
            run: crate::run_context::BoundRun::captured(),
            agent_name,
            instance_id,
            park: None,
            recorded_decisions: None,
        }
    }

    /// Names the run this gate's approvals belong to.
    pub fn bind_run(&self, run: Arc<crate::run_context::RunContext>) {
        self.run.bind(run);
    }

    fn run(&self) -> Option<Arc<crate::run_context::RunContext>> {
        self.run.get()
    }

    /// Hands an approval to the run's observer, warning when there is no run to
    /// hand it to — an approval nobody sees is a request that stalls unanswered.
    async fn emit(&self, event: aura_events::agent::AgentEvent) {
        match self.run() {
            Some(run) => {
                run.emit(event).await;
            }
            None => {
                tracing::warn!("no run bound to this gate; its approval reaches no observer")
            }
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
    ///
    /// Namespace-aware: a pattern containing `:` (e.g. `github:*`) scopes the
    /// match to `tool_namespace`. `tool_namespace` is `None` for tools with
    /// no known MCP server (e.g. filesystem or client-side tools), which only
    /// patterns without `:` can match.
    fn matched_pattern(&self, tool_name: &str, tool_namespace: Option<&str>) -> Option<&str> {
        if tool_name == "request_approval" {
            return None;
        }
        self.patterns
            .iter()
            .find(|p| p.matches(tool_namespace, tool_name))
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
        let DecisionRoute::Conversational { timeout, .. } = &*self.route else {
            return Err(ToolError::ToolCallError(
                "tool call blocked: park mode requires the conversational route"
                    .to_string()
                    .into(),
            ));
        };

        let now = chrono::Utc::now();
        let expires_at =
            now + chrono::Duration::from_std(*timeout).expect("approval timeout fits in chrono");
        let decision_id = DecisionId::generate();
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            instance_id: self.instance_id.clone(),
            decision_id,
            // Request teardown sweeps the live request's approvals; owning a
            // parked ticket by its run keeps it out of that sweep, and the
            // run's own sweep cancels it.
            owner: ApprovalOwner::Run(*run_id),
            scope: self.scope.clone(),
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: matched.to_string(),
                agent_name: self.agent_name.clone(),
            },
            items: vec![ApprovalItem {
                tool_name: ctx.tool_name.clone(),
                tool_namespace: ctx.tool_namespace.clone(),
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

        // The lifecycle pair goes to the live request, not the owner id.
        self.emit(super::events::requested_event(&parked.request))
            .await;
        self.emit(super::events::pending_event(
            &parked.request,
            &parked.expires_at,
        ))
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
        let Some(matched) = self.matched_pattern(&ctx.tool_name, ctx.tool_namespace.as_deref())
        else {
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
                Some(decision) => {
                    return approval_result_to_pre_call(Ok(GateDecision::without_overrides(
                        ApprovalOutcome::Decided(decision),
                    )));
                }
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
        let run = self.run();
        let (owner, cancel) = live_approval_context(run.as_deref());
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            instance_id: self.instance_id.clone(),
            decision_id: DecisionId::generate(),
            owner,
            scope: self.scope.clone(),
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: matched.to_string(),
                agent_name: self.agent_name.clone(),
            },
            items: vec![ApprovalItem {
                tool_name: ctx.tool_name.clone(),
                tool_namespace: ctx.tool_namespace.clone(),
                arguments: args.clone(),
                tool_call_intent: ctx.tool_call_intent.clone(),
            }],
        };
        // `DecisionRoute` emits the lifecycle itself; the scope is how those
        // events find the run, since rig calls this off it.
        let decision = match run {
            Some(run) => {
                crate::run_context::with_run(run, self.route.decide_for_gate(request, &cancel))
                    .await
            }
            None => self.route.decide_for_gate(request, &cancel).await,
        };
        approval_result_to_pre_call(decision)
    }
}

/// Map a gate-scoped decision to a pre-call outcome.
fn approval_result_to_pre_call(
    result: Result<GateDecision, ApprovalError>,
) -> Result<PreCallOutcome, ToolError> {
    match result {
        Ok(GateDecision::Approved { overrides }) => Ok(PreCallOutcome::Proceed { overrides }),
        Ok(GateDecision::Denied { reason }) => Ok(PreCallOutcome::ShortCircuit {
            output: format!(
                "Tool call blocked by human approval denial: {}. Do not execute this action.",
                reason.unwrap_or_else(|| "no reason provided".to_string())
            ),
        }),
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
            Arc::from(["kubectl_*".into()]),
            Arc::new(DecisionRoute::Webhook {
                client: WebhookClient::new(
                    build_webhook_client(),
                    WebhookUrl::new("http://localhost:9").unwrap(),
                ),
                timeout: Duration::from_secs(1),
            }),
            AgentScope::Single { session_id: None },
            "test-agent".to_string(),
            "test-instance-id".to_string(),
        );
        assert_eq!(
            wrapper.matched_pattern("kubectl_apply", None),
            Some("kubectl_*")
        );
        assert_eq!(wrapper.matched_pattern("request_approval", None), None);
        assert_eq!(wrapper.matched_pattern("ls", None), None);
    }

    #[test]
    fn matched_pattern_is_namespace_aware() {
        let wrapper = HitlApprovalWrapper::new(
            Arc::from(["github:*".into()]),
            Arc::new(DecisionRoute::Webhook {
                client: WebhookClient::new(
                    build_webhook_client(),
                    WebhookUrl::new("http://localhost:9").unwrap(),
                ),
                timeout: Duration::from_secs(1),
            }),
            AgentScope::Single { session_id: None },
            "test-agent".to_string(),
            "test-instance-id".to_string(),
        );
        assert_eq!(
            wrapper.matched_pattern("list_repos", Some("github")),
            Some("github:*"),
            "a namespace-scoped pattern must match a bare tool name paired with its namespace",
        );
        assert_eq!(
            wrapper.matched_pattern("list_repos", Some("gitlab")),
            None,
            "the namespace half of the pattern must still be enforced",
        );
        assert_eq!(
            wrapper.matched_pattern("list_repos", None),
            None,
            "a namespace-scoped pattern must not match a tool with no known namespace",
        );
    }

    /// A matching tool whose approval channel is unreachable must fail closed
    /// (the call is blocked), while a non-matching tool stays transparent and
    /// never touches the route. Any channel result — connection refused
    /// (transport) or timeout — maps to a denial here.
    #[tokio::test]
    async fn matching_tool_fails_closed_when_webhook_unreachable() {
        let wrapper = HitlApprovalWrapper::new(
            Arc::from(["kubectl_*".into()]),
            Arc::new(DecisionRoute::Webhook {
                client: WebhookClient::new(
                    build_webhook_client(),
                    // Discard port: nothing listens, so the POST fails closed.
                    WebhookUrl::new("http://127.0.0.1:9").unwrap(),
                ),
                timeout: Duration::from_secs(2),
            }),
            AgentScope::Single { session_id: None },
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
            cell: &Arc<crate::orchestration::BlockedCell>,
        ) -> HitlApprovalWrapper {
            HitlApprovalWrapper::new(
                Arc::from(["kubectl_*".into()]),
                route.clone(),
                worker_scope(),
                "test-agent".to_string(),
                "test-instance".to_string(),
            )
            .with_park(
                registry.clone(),
                cell.clone(),
                ParkGuard::new(
                    registry.clone(),
                    "0191e8c0-1111-7000-8000-000000000042".parse().unwrap(),
                ),
            )
        }

        #[tokio::test]
        async fn register_error_fails_closed_with_no_cell_entry_and_no_event() {
            let request_id = crate::domain::RequestId::generate();
            let store: Arc<dyn crate::session_store::ApprovalStore> =
                Arc::new(crate::session_store::FaultInjectingStore::failing_register());
            let registry = PendingApprovals::with_backend(
                store,
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let route = conv_route_over(registry.clone(), Duration::from_secs(60));
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let gate = parked_gate(&registry, &route, &cell);

            let args = serde_json::json!({ "namespace": "prod" });
            let ctx = ToolCallContext::new("kubectl_apply");
            let (result, events) =
                crate::run_context::observing(request_id.clone(), gate.pre_call(&args, &ctx)).await;

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
                events.is_empty(),
                "no approval event may reach the run after a register fault"
            );
        }

        #[tokio::test]
        async fn happy_path_registers_publishes_appends_and_short_circuits() {
            let request_id = crate::domain::RequestId::generate();
            let store: Arc<dyn crate::session_store::ApprovalStore> =
                Arc::new(crate::session_store::InMemoryApprovalStore::new());
            let registry = PendingApprovals::with_backend(
                store.clone(),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            let route = conv_route_over(registry.clone(), Duration::from_secs(120));
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            cell.set_current_call_id(Some("call_7".to_string()));
            let gate = parked_gate(&registry, &route, &cell);

            let args = serde_json::json!({ "namespace": "prod" });
            let ctx = ToolCallContext::new("kubectl_apply");
            let (outcome, events) =
                crate::run_context::observing(request_id.clone(), gate.pre_call(&args, &ctx)).await;
            let outcome = outcome.unwrap();

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
                        parked.request.owner.to_string(),
                        "run:0191e8c0-1111-7000-8000-000000000042"
                    );
                    assert_eq!(parked.request.items[0].tool_name, "kubectl_apply");
                    assert_eq!(parked.request.items[0].arguments, args);
                }
                other => panic!("expected Blocked, got {other:?}"),
            }

            // The run sees requested then pending, in that order.
            use aura_events::agent::AgentEventPayload as Payload;
            match events.first().map(|e| &e.payload) {
                Some(Payload::ApprovalRequested(requested)) => {
                    assert_eq!(requested.tool_name, "kubectl_apply");
                }
                other => panic!("expected Requested first, got {other:?}"),
            }
            match events.get(1).map(|e| &e.payload) {
                Some(Payload::ApprovalPending(pending)) => {
                    assert_eq!(pending.tool_name, "kubectl_apply");
                    assert_eq!(pending.arguments, args);
                    let scope = serde_json::to_value(&pending.scope).unwrap();
                    assert_eq!(scope["kind"], "worker");
                    assert_eq!(scope["run_id"], "0191e8c0-1111-7000-8000-000000000042");
                }
                other => panic!("expected Pending second, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn two_gated_calls_append_two_cell_entries() {
            let (registry, route) = conv_route(Duration::from_secs(60));
            let cell = Arc::new(crate::orchestration::BlockedCell::default());
            let gate = parked_gate(&registry, &route, &cell);

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
            let gate = parked_gate(&registry, &route, &cell);

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
                "0191e8c0-1111-7000-8000-000000000042".parse().unwrap(),
            );
            let gate = HitlApprovalWrapper::new(
                Arc::from(["kubectl_*".into()]),
                route,
                worker_scope(),
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
    }

    // ====================================================================
    // Recorded-decisions consult
    // ====================================================================

    mod recorded {
        use std::sync::Arc;
        use std::time::Duration;

        use super::*;
        use crate::hitl::ApprovalDecision;

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
                timeout: Duration::from_secs(2),
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
                Arc::from(["kubectl_*".into()]),
                route,
                AgentScope::Single { session_id: None },
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

        /// A recorded approval proceeds through the same mapping the live
        /// route uses, without ever consulting the (unreachable) route.
        #[tokio::test]
        async fn recorded_hit_proceeds_without_invoking_route() {
            let recorded = Arc::new(RecordedDecisions::default());
            let args = serde_json::json!({"namespace": "prod"});
            recorded.push(
                CallKey::new(1, "kubectl_apply", &args),
                ApprovalDecision::Approved,
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
                },
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

    /// The gate awaits the run's own token, so stopping the run releases a call
    /// waiting on a human. Sourcing the token from anywhere else — an unbound
    /// stand-in, a registry the run never registered with — leaves the call
    /// parked until its approval times out, with nobody left to answer it.
    #[tokio::test]
    async fn stopping_the_run_releases_a_call_waiting_on_approval() {
        use crate::hitl::PendingApprovals;

        let registry = PendingApprovals::new();
        let route = Arc::new(DecisionRoute::Conversational {
            registry,
            // Long enough that only cancellation can end the wait.
            timeout: Duration::from_secs(3_600),
        });
        let gate = Arc::new(HitlApprovalWrapper::new(
            Arc::from(["kubectl_*".into()]),
            route,
            AgentScope::Single { session_id: None },
            "test-agent".to_string(),
            "test-instance-id".to_string(),
        ));

        let cancel = tokio_util::sync::CancellationToken::new();
        let (run, mut events) = crate::run_context::RunContext::channel_on(
            crate::domain::RequestId::generate(),
            cancel.clone(),
        );
        gate.bind_run(run);

        let gated = Arc::clone(&gate);
        let call = tokio::spawn(async move {
            let args = serde_json::json!({ "namespace": "prod" });
            let ctx = ToolCallContext::new("kubectl_apply");
            gated.pre_call(&args, &ctx).await
        });

        // The route registers the approval and raises it before parking on the
        // decision, so the pending event arriving is what says the call is
        // waiting rather than still on its way there.
        for expected in ["requested", "pending"] {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap_or_else(|_| panic!("the gate raises {expected} before parking"))
                .expect("the run's channel stays open");
            let raised = matches!(
                event.payload,
                aura_events::agent::AgentEventPayload::ApprovalRequested(_)
                    | aura_events::agent::AgentEventPayload::ApprovalPending(_)
            );
            assert!(
                raised,
                "expected an approval event, got {:?}",
                event.payload
            );
        }
        cancel.cancel();

        let err = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("stopping the run must release the call, not leave it parked")
            .expect("the call task did not panic")
            .expect_err("a cancelled approval fails the call closed");
        assert!(
            err.to_string().contains("approval cancelled"),
            "the failure must name the cancellation, got: {err}"
        );
    }

    /// A live approval belongs to the request whose run the gate serves, so
    /// sweeping that request cancels it.
    #[tokio::test]
    async fn a_live_approval_is_owned_by_the_request_of_the_gate_s_run() {
        let store = Arc::new(crate::session_store::InMemoryApprovalStore::new());
        let registry = PendingApprovals::with_backend(
            store.clone(),
            Arc::new(crate::session_store::InMemoryEventBus::new()),
        );
        let gate = HitlApprovalWrapper::new(
            Arc::from(["kubectl_*".into()]),
            Arc::new(DecisionRoute::Conversational {
                registry: registry.clone(),
                timeout: Duration::from_secs(3_600),
            }),
            AgentScope::Single { session_id: None },
            "test-agent".to_string(),
            "test-instance-id".to_string(),
        );
        let request_id = crate::domain::RequestId::generate();
        let (run, mut events) = crate::run_context::RunContext::channel(request_id.clone());
        gate.bind_run(run);

        let call = tokio::spawn(async move {
            let args = serde_json::json!({ "namespace": "prod" });
            gate.pre_call(&args, &ToolCallContext::new("kubectl_apply"))
                .await
        });
        let decision_id = match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the gate raises the approval")
            .expect("the run's channel stays open")
            .payload
        {
            aura_events::agent::AgentEventPayload::ApprovalRequested(event) => {
                DecisionId::parse(&event.decision_id).expect("valid decision id")
            }
            other => panic!("expected Requested, got {other:?}"),
        };

        let owner = ApprovalOwner::Request(request_id);
        let parked = crate::session_store::ApprovalStore::get(&*store, &decision_id)
            .await
            .unwrap()
            .expect("the approval is registered");
        assert_eq!(parked.request.owner, owner);

        registry.cancel_request(&owner).await;
        let err = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("sweeping the request releases the call")
            .expect("the call task did not panic")
            .expect_err("a cancelled approval fails the call closed");
        assert!(err.to_string().contains("approval cancelled"), "got: {err}");
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
        use crate::logging::ATTR_DECISION_ID;
        use crate::test_span_capture::{CapturedSpans, traced_as_execute_tool};
        use crate::tool_wrapper::WrappedTool;
        use aura_events::agent::{AgentEvent, AgentEventPayload};

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
            let registry = PendingApprovals::new();
            let (tool, ran, mut events) = gated_tool(
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
                .resolve(&payload_id, ApprovalDecision::Approved)
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
        /// The gate is bound to a run rather than left to find one, because
        /// rig calls a gated tool from its server task — which these tests
        /// reproduce by spawning — and no scope crosses that.
        fn gated_tool(
            route: DecisionRoute,
            request_id: &crate::domain::RequestId,
            tool_name: &str,
        ) -> (WrappedTool<StubTool>, Arc<AtomicBool>, Receiver<AgentEvent>) {
            let ran = Arc::new(AtomicBool::new(false));
            let inner = StubTool {
                name: tool_name.to_string(),
                ran: ran.clone(),
            };
            let gate = HitlApprovalWrapper::new(
                Arc::from(["kubectl_*".into()]),
                Arc::new(route),
                AgentScope::Single { session_id: None },
                "test-agent".to_string(),
                "test-instance-id".to_string(),
            );
            let (run, events) = crate::run_context::RunContext::channel(request_id.clone());
            gate.bind_run(run);
            (
                WrappedTool::new(inner, Arc::new(gate) as Arc<dyn ToolWrapper>),
                ran,
                events,
            )
        }

        /// The decision id the approval payload carried, read off the
        /// `Requested` lifecycle event the route publishes for it.
        async fn payload_decision_id(events: &mut Receiver<AgentEvent>) -> DecisionId {
            match events
                .recv()
                .await
                .expect("the run's events channel open")
                .payload
            {
                AgentEventPayload::ApprovalRequested(event) => {
                    DecisionId::parse(&event.decision_id).expect("valid decision id")
                }
                other => panic!("expected Requested, got {other:?}"),
            }
        }

        fn unique_request_id() -> crate::domain::RequestId {
            crate::domain::RequestId::generate()
        }

        /// The correlation the whole feature exists for: the id the approver
        /// decided against is the id on the span of the execution it released.
        #[tokio::test]
        async fn approved_gate_stamps_the_payload_decision_id_on_the_execution_span() {
            let request_id = unique_request_id();
            let registry = PendingApprovals::new();
            let (tool, ran, mut events) = gated_tool(
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
                        .resolve(&id, ApprovalDecision::Approved)
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
        }

        /// An attempt that never reaches a decision is exactly where the
        /// correlation earns its keep: the trace still names the approval the
        /// failed-closed execution was waiting on.
        #[tokio::test(start_paused = true)]
        async fn timed_out_gate_stamps_the_decision_id_on_the_execution_span() {
            let request_id = unique_request_id();
            let (tool, ran, mut events) = gated_tool(
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
        }

        /// The webhook route stamps the same id from the same place, so the
        /// correlation does not depend on which route the deployment picked.
        #[tokio::test]
        async fn webhook_gate_stamps_the_decision_id_on_the_execution_span() {
            let request_id = unique_request_id();
            let (tool, ran, mut events) = gated_tool(
                DecisionRoute::Webhook {
                    client: WebhookClient::new(
                        build_webhook_client(),
                        // Discard port: nothing listens, so the POST fails closed.
                        WebhookUrl::new("http://127.0.0.1:9").unwrap(),
                    ),
                    timeout: Duration::from_secs(2),
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
        }

        #[tokio::test]
        async fn ungated_call_records_no_decision_id() {
            let (tool, ran, _events) = gated_tool(
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
