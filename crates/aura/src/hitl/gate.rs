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
use super::route::{ApprovalError, DecisionRoute, GateDecision};
use crate::tool_wrapper::{PreCallOutcome, ToolCallContext, ToolWrapper};

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
        }
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

            /// The single `key` attribute on the span named `name`, or `None`
            /// if the span carries no such attribute. Panics if the span
            /// carries more than one — a double-stamp regression would
            /// otherwise pass unnoticed, since `find` would silently return
            /// only the first entry.
            fn attribute(&self, name: &str, key: &str) -> Option<String> {
                let spans = self.spans();
                let span = spans.iter().find(|span| span.name == name)?;
                let matches: Vec<_> = span
                    .attributes
                    .iter()
                    .filter(|kv| kv.key.as_str() == key)
                    .collect();
                assert!(
                    matches.len() <= 1,
                    "span {name:?} must carry at most one {key} attribute, found {}: \
                     a regression is double-stamping the same span",
                    matches.len(),
                );
                matches.first().map(|kv| kv.value.to_string())
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
