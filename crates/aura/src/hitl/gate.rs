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

use super::decision::{AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalRef, DecisionId};
use super::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
use super::route::{ApprovalError, DecisionRoute, GateDecision};
use crate::orchestration::park::{
    ApprovalBinding, ArgsDigest, DispatchError, DispatchEvent, DispatchState, ToolAttemptOutcome,
};
use crate::tool_wrapper::{PreCallOutcome, ToolCallContext, ToolWrapper};

/// Typed side-channel carrying [`ToolAttemptOutcome::Blocked`] across the
/// rig tool stack, whose `Tool` interface types failures as strings and
/// would erase the block into an error. [`Self::deposit`] is the only write
/// path: it stores the projection of the very [`PreCallOutcome`] it hands
/// back to the wrapper chain, and [`Self::take_blocked`] returns the stored
/// value verbatim — the channel and the returned outcome cannot diverge
/// because there is one value.
#[derive(Clone, Default)]
pub struct BlockedSignal(Arc<std::sync::Mutex<Option<ToolAttemptOutcome>>>);

impl BlockedSignal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the blocked projection of `outcome` (a non-blocked outcome
    /// stores nothing) and hand `outcome` back for the wrapper chain.
    #[must_use]
    pub fn deposit(&self, outcome: PreCallOutcome) -> PreCallOutcome {
        if let Some(blocked) = outcome.clone().into_blocked_attempt() {
            *self.0.lock().expect("blocked signal lock poisoned") = Some(blocked);
        }
        outcome
    }

    /// Take the stored blocked outcome, leaving the signal clear for the
    /// next attempt.
    pub fn take_blocked(&self) -> Option<ToolAttemptOutcome> {
        self.0.lock().expect("blocked signal lock poisoned").take()
    }

    /// Whether a block is already deposited for the current attempt.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.0
            .lock()
            .expect("blocked signal lock poisoned")
            .is_some()
    }
}

/// What the gate tells the model once the run is already parking: the
/// attempt is over, and a second gated call would register an approval
/// nothing will ever claim.
const ALREADY_PARKING: &str = "the run is parking pending approval; no further gated calls";

/// The durable-park arming of one gate: the channel a block leaves on, the
/// window an approval is registered for, and the decision this attempt was
/// re-dispatched to consume.
#[derive(Clone)]
struct DurablePark {
    signal: BlockedSignal,
    timeout: std::time::Duration,
    binding: Arc<std::sync::Mutex<Option<ApprovalBinding>>>,
}

impl DurablePark {
    /// Take the pending binding, leaving none behind: one binding dispatches
    /// at most once, so a second gated call in the same attempt raises its
    /// own approval rather than reusing a spent decision.
    fn take_binding(&self) -> Option<ApprovalBinding> {
        self.binding
            .lock()
            .expect("approval binding lock poisoned")
            .take()
    }
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
    /// Present only when the deployment supports durable parking: a gate hit
    /// then parks the approval and ends the attempt as `Blocked` instead of
    /// holding the await for the length of the request.
    durable_park: Option<DurablePark>,
}

impl HitlApprovalWrapper {
    #[must_use]
    pub fn new(
        patterns: Arc<[GlobPattern]>,
        route: Arc<DecisionRoute>,
        scope: AgentScope,
        request_id: String,
        agent_name: String,
    ) -> Self {
        Self {
            patterns,
            route,
            scope,
            request_id,
            agent_name,
            durable_park: None,
        }
    }

    /// Arm the durable-park path: a gate hit registers the approval for
    /// `park_timeout` and reports the block through `signal` instead of
    /// awaiting the decision in-request.
    /// `binding` is the decision a re-dispatched attempt consumes; `None`
    /// raises a fresh approval on the first gated call.
    #[must_use]
    pub fn with_blocked_signal(
        mut self,
        signal: BlockedSignal,
        park_timeout: std::time::Duration,
        binding: Option<ApprovalBinding>,
    ) -> Self {
        self.durable_park = Some(DurablePark {
            signal,
            timeout: park_timeout,
            binding: Arc::new(std::sync::Mutex::new(binding)),
        });
        self
    }

    /// Register the gated call durably and end the attempt blocked (ADR
    /// 2026-07-21, decisions 1 and 11).
    ///
    /// Registration precedes the block: a store fault fails the call closed
    /// as a tool error with nothing published, so the run never parks on an
    /// approval that was not written. On success the one
    /// [`PreCallOutcome::Blocked`] built here returns THROUGH
    /// [`BlockedSignal::deposit`], which stores its projection and hands the
    /// outcome back — the side channel and the wrapper-chain value are one.
    async fn park_instead_of_awaiting(
        &self,
        request: ApprovalRequest,
        args: &Value,
        park: &DurablePark,
    ) -> Result<PreCallOutcome, ToolError> {
        let AgentScope::Worker { task, .. } = request.scope.clone() else {
            return Err(ToolError::ToolCallError(
                "durable park requires worker scope".to_string().into(),
            ));
        };
        // A task re-dispatched after its approval was decided consumes that
        // decision here; only an undecided or absent binding registers.
        if let Some(binding) = park.take_binding() {
            if let Some(consumed) = self.consume_binding(&binding, args).await {
                return consumed;
            }
            // Still undecided: the run blocks on the approval it already has,
            // never on a second one for the same call.
            return Ok(park
                .signal
                .deposit(PreCallOutcome::Blocked(binding.approval)));
        }
        let approval_ref = ApprovalRef {
            decision_id: request.decision_id,
            task,
        };
        self.route
            .park(request, park.timeout)
            .await
            .map_err(|e| ToolError::ToolCallError(format!("tool call blocked: {e}").into()))?;
        Ok(park.signal.deposit(PreCallOutcome::Blocked(approval_ref)))
    }

    /// Dispatch the decision `binding` names, or report that it has not
    /// arrived (`None`) so the caller blocks on it again.
    ///
    /// The claim revalidates this call's arguments against the digest
    /// recorded when the human decided: only the call they actually saw may
    /// run, and a mismatch leaves the decision unconsumed for the arguments
    /// it does cover (ADR 2026-07-21, decision 9). A dispatched decision is
    /// removed with its record, so it cannot be claimed twice.
    async fn consume_binding(
        &self,
        binding: &ApprovalBinding,
        args: &Value,
    ) -> Option<Result<PreCallOutcome, ToolError>> {
        let registry = self.route.registry()?;
        let id = binding.approval.decision_id;
        let decision = match registry.store().decision(&id).await {
            Ok(Some(decision)) => decision,
            Ok(None) => return None,
            Err(e) => {
                return Some(Err(ToolError::ToolCallError(
                    format!("tool call blocked: approval decision unreadable: {e}").into(),
                )));
            }
        };

        let outcome = match decision {
            ApprovalDecision::Denied { reason } => denial_short_circuit(reason),
            ApprovalDecision::Approved => {
                let claim = DispatchEvent::Claim {
                    generation: binding.generation,
                    presented: ArgsDigest::compute(args),
                    at: chrono::Utc::now(),
                };
                match DispatchState::Unclaimed.apply(claim, &binding.args_digest) {
                    Ok(_) => PreCallOutcome::Proceed { overrides: None },
                    Err(DispatchError::DigestMismatch { bound, presented }) => {
                        return Some(Err(ToolError::ToolCallError(
                            format!(
                                "tool call denied: arguments differ from the approved call \
                                 (approved digest {}, presented {})",
                                bound.as_str(),
                                presented.as_str(),
                            )
                            .into(),
                        )));
                    }
                    Err(DispatchError::Illegal { from, event }) => {
                        return Some(Err(ToolError::ToolCallError(
                            format!("tool call denied: dispatch rejected {event:?} from {from:?}")
                                .into(),
                        )));
                    }
                }
            }
        };
        registry.remove(&id).await;
        Some(Ok(outcome))
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
        // The block is sticky for the rest of the attempt: the run is
        // already draining toward its park, so a further gated call
        // registers nothing and tells the model to stop.
        if let Some(park) = &self.durable_park
            && park.signal.is_blocked()
        {
            return Ok(PreCallOutcome::ShortCircuit {
                output: ALREADY_PARKING.to_string(),
            });
        }
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
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
        if let Some(park) = &self.durable_park {
            return self.park_instead_of_awaiting(request, args, park).await;
        }
        let cancel =
            crate::request_cancellation::RequestCancellation::token_for_id(&self.request_id)
                .unwrap_or_else(crate::request_cancellation::RequestCancelToken::unbound);
        approval_result_to_pre_call(self.route.decide_for_gate(request, &cancel).await)
    }
}

/// Report a denial to the model as tool output rather than an error: a
/// refusal is an answer the agent must reason about, not a fault.
fn denial_short_circuit(reason: Option<String>) -> PreCallOutcome {
    PreCallOutcome::ShortCircuit {
        output: format!(
            "Tool call blocked by human approval denial: {}. Do not execute this action.",
            reason.unwrap_or_else(|| "no reason provided".to_string())
        ),
    }
}

/// Map a gate-scoped decision to a pre-call outcome.
fn approval_result_to_pre_call(
    result: Result<GateDecision, ApprovalError>,
) -> Result<PreCallOutcome, ToolError> {
    match result {
        Ok(GateDecision::Approved { overrides }) => Ok(PreCallOutcome::Proceed { overrides }),
        Ok(GateDecision::Denied { reason }) => Ok(denial_short_circuit(reason)),
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
            Arc::from([GlobPattern::new("kubectl_*").unwrap()]),
            Arc::new(DecisionRoute::Webhook {
                client: WebhookClient::new(
                    build_webhook_client(),
                    WebhookUrl::new("http://localhost:9").unwrap(),
                ),
                timeout: Duration::from_secs(1),
            }),
            AgentScope::Single { session_id: None },
            "t".into(),
            "test-agent".to_string(),
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
                timeout: Duration::from_secs(2),
            }),
            AgentScope::Single { session_id: None },
            "req-test".into(),
            "test-agent".to_string(),
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
        );
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

    /// The durable-park arm of the gate: what it writes before it blocks,
    /// and what it refuses to write once the run is already parking.
    mod durable_park {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use rig::completion::ToolDefinition;
        use rig::tool::Tool as RigTool;
        use serde_json::json;

        use super::super::super::decision::ApprovalDecision;
        use super::super::super::registry::{ParkedApproval, PendingApprovals, ResolveError};
        use super::super::super::teardown::ApprovalOwnership;
        use super::*;
        use crate::approval_event_broker;
        use crate::orchestration::park::{FencingGeneration, WakeReason};
        use crate::orchestration::{RunId, TaskIdentity};
        use crate::session_store::{
            ApprovalStore, InMemoryApprovalStore, InMemoryEventBus, SessionStoreError,
        };
        use crate::tool_wrapper::WrappedTool;

        /// An approval store that counts registrations, and can stage what
        /// the park's one durable write runs into: a store outage, or a
        /// request that tears down while the write is in flight.
        struct CountingApprovalStore {
            inner: InMemoryApprovalStore,
            registers: AtomicUsize,
            refuse: bool,
            /// Request whose ownership marker is dropped mid-`register`.
            teardown_during_register: Option<String>,
        }

        impl CountingApprovalStore {
            fn new(refuse: bool) -> Self {
                Self {
                    inner: InMemoryApprovalStore::new(),
                    registers: AtomicUsize::new(0),
                    refuse,
                    teardown_during_register: None,
                }
            }

            /// Tear `request_id` down from inside the write, landing the race
            /// the bracketing lookups in `DecisionRoute::park` exist for.
            fn tearing_down(request_id: &str) -> Self {
                Self {
                    teardown_during_register: Some(request_id.to_string()),
                    ..Self::new(false)
                }
            }

            fn registers(&self) -> usize {
                self.registers.load(Ordering::SeqCst)
            }
        }

        #[async_trait]
        impl ApprovalStore for CountingApprovalStore {
            async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError> {
                self.registers.fetch_add(1, Ordering::SeqCst);
                if self.refuse {
                    return Err(SessionStoreError::Request {
                        reason: "approval store offline".to_string(),
                    });
                }
                if let Some(request_id) = &self.teardown_during_register {
                    ApprovalOwnership::unregister(request_id);
                }
                self.inner.register(parked).await
            }

            async fn get(
                &self,
                id: &DecisionId,
            ) -> Result<Option<ParkedApproval>, SessionStoreError> {
                self.inner.get(id).await
            }

            async fn resolve(
                &self,
                id: &DecisionId,
                decision: ApprovalDecision,
            ) -> Result<(), ResolveError> {
                self.inner.resolve(id, decision).await
            }

            async fn decision(
                &self,
                id: &DecisionId,
            ) -> Result<Option<ApprovalDecision>, SessionStoreError> {
                self.inner.decision(id).await
            }

            async fn resolve_durable(
                &self,
                id: &DecisionId,
                decision: ApprovalDecision,
            ) -> Result<WakeReason, ResolveError> {
                self.inner.resolve_durable(id, decision).await
            }

            async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
                self.inner.remove(id).await
            }

            async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError> {
                self.inner.cancel_request(request_id).await
            }
        }

        #[derive(Clone)]
        struct StubTool;

        impl RigTool for StubTool {
            const NAME: &'static str = "kubectl_apply";
            type Error = ToolError;
            type Args = Value;
            type Output = String;

            fn name(&self) -> String {
                "kubectl_apply".to_string()
            }

            async fn definition(&self, _prompt: String) -> ToolDefinition {
                ToolDefinition {
                    name: "kubectl_apply".to_string(),
                    description: String::new(),
                    parameters: json!({ "type": "object" }),
                }
            }

            async fn call(&self, _args: Value) -> Result<String, ToolError> {
                panic!("a gated tool must never execute")
            }
        }

        fn armed_gate(registry: &PendingApprovals, request_id: &str) -> HitlApprovalWrapper {
            bound_gate(registry, request_id, None)
        }

        /// The gate as `create_worker` arms it, optionally re-dispatching a
        /// task that already holds `binding`.
        fn bound_gate(
            registry: &PendingApprovals,
            request_id: &str,
            binding: Option<ApprovalBinding>,
        ) -> HitlApprovalWrapper {
            HitlApprovalWrapper::new(
                Arc::from([GlobPattern::new("kubectl_*").unwrap()]),
                Arc::new(DecisionRoute::Conversational {
                    registry: registry.clone(),
                    timeout: Duration::from_secs(60),
                }),
                AgentScope::Worker {
                    run_id: uuid::Uuid::new_v4()
                        .to_string()
                        .parse::<RunId>()
                        .expect("a v4 uuid is a run id"),
                    task: TaskIdentity::new(0, None),
                    session_id: None,
                },
                request_id.to_string(),
                "test-agent".to_string(),
            )
            .with_blocked_signal(
                BlockedSignal::new(),
                Duration::from_secs(3600),
                binding,
            )
        }

        fn unique_request_id() -> String {
            format!("req_gate_{}", uuid::Uuid::new_v4().simple())
        }

        /// Park an approval over `args`, settle it with `decision`, and hand
        /// back the binding the released task carries into its next attempt.
        async fn settled_binding(
            registry: &PendingApprovals,
            request_id: &str,
            args: &Value,
            decision: ApprovalDecision,
        ) -> ApprovalBinding {
            let gate = armed_gate(registry, request_id);
            let outcome = gate
                .pre_call(args, &ToolCallContext::new("kubectl_apply"))
                .await
                .expect("the first attempt parks");
            let PreCallOutcome::Blocked(approval) = outcome else {
                panic!("expected a parked call, got {outcome:?}");
            };
            registry
                .resolve(&approval.decision_id, decision)
                .await
                .expect("the parked approval resolves durably");
            ApprovalBinding {
                approval,
                args_digest: ArgsDigest::compute(args),
                generation: FencingGeneration::INITIAL,
            }
        }

        async fn approved_binding(
            registry: &PendingApprovals,
            request_id: &str,
            args: &Value,
        ) -> ApprovalBinding {
            settled_binding(registry, request_id, args, ApprovalDecision::Approved).await
        }

        async fn denied_binding(
            registry: &PendingApprovals,
            request_id: &str,
            args: &Value,
            reason: &str,
        ) -> ApprovalBinding {
            settled_binding(
                registry,
                request_id,
                args,
                ApprovalDecision::Denied {
                    reason: Some(reason.to_string()),
                },
            )
            .await
        }

        /// A store outage at registration fails the call closed with nothing
        /// announced: no approver may see a decision id whose record the run
        /// could never resolve.
        #[tokio::test]
        async fn a_store_fault_at_registration_publishes_nothing() {
            let store = Arc::new(CountingApprovalStore::new(true));
            let registry =
                PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
            let request_id = unique_request_id();
            let mut events = approval_event_broker::subscribe(&request_id).await;
            let _ownership = ApprovalOwnership::register(&request_id);

            let tool = WrappedTool::new(
                StubTool,
                Arc::new(armed_gate(&registry, &request_id)) as Arc<dyn ToolWrapper>,
            );
            let err = tool
                .call(json!({}))
                .await
                .expect_err("a store fault must block the call");

            assert!(
                err.to_string().contains("approval store offline"),
                "the tool error must name the store fault, got: {err}",
            );
            assert_eq!(store.registers(), 1, "the store was asked exactly once");
            assert!(
                events.try_recv().is_err(),
                "a refused registration announces nothing",
            );

            approval_event_broker::unsubscribe(&request_id).await;
        }

        /// The park's write and the request's teardown race: whichever side
        /// lands first, no approval may outlive the request without a run
        /// checkpoint naming it.
        #[tokio::test]
        async fn teardown_during_the_write_leaves_no_orphan_approval() {
            let request_id = unique_request_id();
            let store = Arc::new(CountingApprovalStore::tearing_down(&request_id));
            let registry =
                PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
            let mut events = approval_event_broker::subscribe(&request_id).await;
            let _ownership = ApprovalOwnership::register(&request_id);

            let tool = WrappedTool::new(
                StubTool,
                Arc::new(armed_gate(&registry, &request_id)) as Arc<dyn ToolWrapper>,
            );
            let err = tool
                .call(json!({}))
                .await
                .expect_err("a torn-down request must not park");

            assert!(
                err.to_string().contains("teardown already began"),
                "the tool error must name the race, got: {err}",
            );
            assert_eq!(store.registers(), 1, "the write happened, then was undone");
            assert!(
                events.try_recv().is_err(),
                "an undone registration announces nothing",
            );

            approval_event_broker::unsubscribe(&request_id).await;
        }

        /// A request already torn down never reaches the store at all.
        #[tokio::test]
        async fn a_torn_down_request_registers_nothing() {
            let store = Arc::new(CountingApprovalStore::new(false));
            let registry =
                PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
            let request_id = unique_request_id();

            // No `ApprovalOwnership::register`: the marker teardown drops is
            // already gone by the time the detached pre_call runs.
            let tool = WrappedTool::new(
                StubTool,
                Arc::new(armed_gate(&registry, &request_id)) as Arc<dyn ToolWrapper>,
            );
            let err = tool
                .call(json!({}))
                .await
                .expect_err("a torn-down request must not park");

            assert!(err.to_string().contains("teardown already began"), "{err}");
            assert_eq!(store.registers(), 0, "nothing was written");
        }

        /// A re-dispatched attempt whose arguments differ from the ones the
        /// human saw is denied, and the decision stays claimable for the
        /// call it does cover (ADR decision 9).
        #[tokio::test]
        async fn a_digest_mismatch_is_denied_and_consumes_nothing() {
            let store = Arc::new(CountingApprovalStore::new(false));
            let registry =
                PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
            let request_id = unique_request_id();
            let _ownership = ApprovalOwnership::register(&request_id);
            let approved = json!({ "namespace": "prod" });
            let binding = approved_binding(&registry, &request_id, &approved).await;

            let gate = bound_gate(&registry, &request_id, Some(binding.clone()));
            let err = gate
                .pre_call(
                    &json!({ "namespace": "staging" }),
                    &ToolCallContext::new("kubectl_apply"),
                )
                .await
                .expect_err("tampered arguments are denied");

            assert!(
                err.to_string().contains("differ from the approved call"),
                "{err}"
            );
            assert!(
                store
                    .decision(&binding.approval.decision_id)
                    .await
                    .expect("store readable")
                    .is_some(),
                "a rejected claim leaves the decision unconsumed",
            );
        }

        /// A denial reaches the model as tool output, and the decision it
        /// settles is dropped with its record.
        #[tokio::test]
        async fn a_denied_binding_short_circuits_and_is_consumed() {
            let store = Arc::new(CountingApprovalStore::new(false));
            let registry =
                PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
            let request_id = unique_request_id();
            let _ownership = ApprovalOwnership::register(&request_id);
            let args = json!({ "namespace": "prod" });
            let binding = denied_binding(&registry, &request_id, &args, "too risky").await;

            let gate = bound_gate(&registry, &request_id, Some(binding.clone()));
            let outcome = gate
                .pre_call(&args, &ToolCallContext::new("kubectl_apply"))
                .await
                .expect("a denial is feedback, not a fault");

            assert_eq!(outcome, denial_short_circuit(Some("too risky".to_string())));
            assert_eq!(store.registers(), 1, "a denial raises no new approval");
            assert!(
                store
                    .get(&binding.approval.decision_id)
                    .await
                    .expect("store readable")
                    .is_none(),
                "a settled denial is dropped with its record",
            );
        }

        /// Once the run is parking, a further gated call in the same attempt
        /// registers nothing: a second approval would have no task waiting on
        /// it and nothing would ever claim it.
        #[tokio::test]
        async fn a_second_gated_hit_in_one_attempt_registers_nothing() {
            let store = Arc::new(CountingApprovalStore::new(false));
            let registry =
                PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
            let request_id = unique_request_id();
            let _ownership = ApprovalOwnership::register(&request_id);
            let gate = armed_gate(&registry, &request_id);
            let args = json!({});
            let ctx = ToolCallContext::new("kubectl_apply");

            let first = gate.pre_call(&args, &ctx).await.expect("the gate parks");
            assert!(matches!(first, PreCallOutcome::Blocked(_)));
            assert_eq!(store.registers(), 1);

            let second = gate
                .pre_call(&args, &ctx)
                .await
                .expect("a parking run short-circuits rather than erroring");
            assert_eq!(
                second,
                PreCallOutcome::ShortCircuit {
                    output: ALREADY_PARKING.to_string()
                },
            );
            assert_eq!(store.registers(), 1, "the second hit registered nothing");

            registry.cancel_request(&request_id).await;
        }
    }

    /// Trace correlation: a gated call's `execute_tool` span carries the
    /// `decision_id` of the approval that gated it.
    ///
    /// Gated on `otel` — without the feature `set_span_attribute` is a
    /// documented no-op and there is no span data to assert against.
    #[cfg(feature = "otel")]
    mod decision_id_span {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex, MutexGuard};

        use futures::future::BoxFuture;
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
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
        use crate::tool_wrapper::WrappedTool;

        /// Spans the test subscriber has exported.
        #[derive(Debug, Clone, Default)]
        struct CapturedSpans(Arc<Mutex<Vec<SpanData>>>);

        impl CapturedSpans {
            fn spans(&self) -> MutexGuard<'_, Vec<SpanData>> {
                self.0.lock().expect("captured spans mutex")
            }

            fn contains(&self, name: &str) -> bool {
                self.spans().iter().any(|span| span.name == name)
            }

            fn attribute(&self, name: &str, key: &str) -> Option<String> {
                self.spans()
                    .iter()
                    .find(|span| span.name == name)
                    .and_then(|span| span.attributes.iter().find(|kv| kv.key.as_str() == key))
                    .map(|kv| kv.value.to_string())
            }
        }

        impl SpanExporter for CapturedSpans {
            fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
                self.spans().extend(batch);
                Box::pin(std::future::ready(Ok(())))
            }
        }

        /// Run `body` inside an `execute_tool` span — the span Rig opens around
        /// a tool call — under a subscriber that exports to memory, returning
        /// the body's output and the `decision_id` the exported span carries.
        async fn traced_as_execute_tool<T>(body: impl Future<Output = T>) -> (T, Option<String>) {
            let captured = CapturedSpans::default();
            let provider = TracerProvider::builder()
                .with_simple_exporter(captured.clone())
                .build();
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry()
                    .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test"))),
            );

            let output = body.instrument(tracing::info_span!("execute_tool")).await;

            // The registry instruments a parked approval's wake task with the
            // same span, so the export lands once that task has released it too.
            for _ in 0..1_000 {
                if captured.contains("execute_tool") {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(
                captured.contains("execute_tool"),
                "the execute_tool span was never exported",
            );

            (output, captured.attribute("execute_tool", ATTR_DECISION_ID))
        }

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

            approval_event_broker::unsubscribe(&request_id).await;
        }

        /// A call no glob matches allocates no decision, so its span must not
        /// claim one.
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
