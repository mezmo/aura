//! The agent-callable surface: `request_approval`, a Rig tool the agent invokes
//! when it judges that an action needs a human.
//!
//! Denial returns `Ok(String)` - the model sees it as policy feedback it can
//! reason about ("do not proceed"). Non-decisions (timeout, cancellation) and
//! channel errors return `Err(ToolError)` - the model sees an infrastructure
//! failure. This aligns with the config-gate path in `gate.rs`.

use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::{Tool, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::decision::{AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalOutcome, DecisionId};
use super::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
use super::route::{ApprovalError, DecisionRoute};

/// The `request_approval` tool. Constructs an
/// [`ApprovalOrigin::AgentRequested`] and dispatches through the shared
/// [`DecisionRoute`].
///
/// [`ApprovalOrigin::AgentRequested`]: super::decision::ApprovalOrigin::AgentRequested
#[derive(Clone)]
pub struct RequestApprovalTool {
    route: Arc<DecisionRoute>,
    scope: AgentScope,
    request_id: String,
}

impl RequestApprovalTool {
    #[must_use]
    pub fn new(route: Arc<DecisionRoute>, scope: AgentScope, request_id: String) -> Self {
        Self {
            route,
            scope,
            request_id,
        }
    }
}

/// Arguments the LLM provides when calling `request_approval`.
#[derive(Debug, Deserialize, Serialize)]
pub struct RequestApprovalArgs {
    /// What the agent wants to do (the action awaiting approval).
    pub action_description: String,
    /// Why the agent is asking for approval.
    pub risk_rationale: String,
    /// Optional structured metadata for the reviewer.
    #[serde(default)]
    pub context: Option<Value>,
    /// The agent's reasoning for requesting approval.
    #[serde(default, rename = "_aura_reasoning", skip_serializing)]
    pub tool_call_intent: Option<String>,
}

/// Map [`DecisionRoute::decide`] outcome to [`Tool::Output`] / [`Tool::Error`].
///
/// Denial -> `Ok` (policy feedback). Timeout/cancel/channel -> `Err` (tool error).
fn approval_outcome_to_tool_result(
    result: Result<ApprovalOutcome, ApprovalError>,
    action: &str,
) -> Result<String, ToolError> {
    match result {
        Ok(ApprovalOutcome::Decided(ApprovalDecision::Approved)) => {
            Ok(format!("Approved. You may proceed with: {action}"))
        }
        Ok(ApprovalOutcome::Decided(ApprovalDecision::Denied { reason })) => Ok(format!(
            "Rejected: {}. Do not proceed with this action.",
            reason.unwrap_or_else(|| "no reason provided".to_string())
        )),
        Ok(ApprovalOutcome::TimedOut { .. }) => Err(ToolError::ToolCallError(
            "Approval timed out. Treat this as not approved; do not proceed."
                .to_string()
                .into(),
        )),
        Ok(ApprovalOutcome::Cancelled(_)) => Err(ToolError::ToolCallError(
            "Approval was cancelled. Treat this as not approved; do not proceed."
                .to_string()
                .into(),
        )),
        Err(e) => Err(ToolError::ToolCallError(
            format!("Approval request failed: {e}. You may try again or notify the user.").into(),
        )),
    }
}

/// Normalize model reasoning into a `tool_call_intent` via
/// [`crate::tool_wrapper::non_blank`].
fn normalize_tool_call_intent(raw: Option<&str>) -> Option<String> {
    raw.and_then(crate::tool_wrapper::non_blank)
        .map(str::to_string)
}

impl Tool for RequestApprovalTool {
    const NAME: &'static str = "request_approval";

    type Error = ToolError;
    type Args = RequestApprovalArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Request human approval before proceeding with a sensitive action. \
                Describe what you want to do, why it's risky, and optionally provide structured \
                context. The call blocks until a human approves or rejects."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action_description": {
                        "type": "string",
                        "description": "What you want to do (the action awaiting approval)."
                    },
                    "risk_rationale": {
                        "type": "string",
                        "description": "Why this action requires human approval."
                    },
                    "context": {
                        "type": "object",
                        "description": "Optional additional structured metadata for the reviewer."
                    },
                    "_aura_reasoning": {
                        "type": "string",
                        "description": "Explain your reasoning for requesting this approval. Why does this specific action need a human decision?"
                    }
                },
                "required": ["action_description", "risk_rationale"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Blank reasoning collapses to absent, consistent with the config_gate path.
        let tool_call_intent = normalize_tool_call_intent(args.tool_call_intent.as_deref());
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: self.request_id.clone(),
            scope: self.scope.clone(),
            origin: ApprovalOrigin::AgentRequested {
                reason: args.risk_rationale.clone(),
            },
            items: vec![ApprovalItem {
                tool_name: Self::NAME.to_string(),
                arguments: serde_json::to_value(&args).unwrap_or_default(),
                tool_call_intent,
            }],
        };
        let cancel =
            crate::request_cancellation::RequestCancellation::token_for_id(&self.request_id)
                .unwrap_or_else(crate::request_cancellation::RequestCancelToken::unbound);
        approval_outcome_to_tool_result(
            self.route.decide(request, &cancel).await,
            &args.action_description,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::decision::{ApprovalOutcome, CancelReason};
    use super::*;

    use crate::approval_event_broker::{self, ApprovalLifecycleEvent};
    use crate::hitl::PendingApprovals;
    use crate::session_store::{ApprovalStore, EventBus, InMemoryApprovalStore, InMemoryEventBus};

    #[test]
    fn mapping_approved_returns_ok_with_action() {
        let result = approval_outcome_to_tool_result(
            Ok(ApprovalOutcome::Decided(ApprovalDecision::Approved)),
            "deploy service",
        );
        assert!(result.is_ok());
        assert!(result.as_ref().unwrap().contains("Approved"));
        assert!(result.as_ref().unwrap().contains("deploy service"));
    }

    #[test]
    fn mapping_denied_returns_ok_with_reason() {
        let result = approval_outcome_to_tool_result(
            Ok(ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: Some("too risky".to_string()),
            })),
            "deploy service",
        );
        assert!(result.is_ok());
        assert!(result.as_ref().unwrap().contains("Rejected"));
        assert!(result.as_ref().unwrap().contains("too risky"));
    }

    #[test]
    fn mapping_denied_with_no_reason_returns_ok() {
        let result = approval_outcome_to_tool_result(
            Ok(ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: None,
            })),
            "deploy service",
        );
        assert!(result.is_ok());
        assert!(result.as_ref().unwrap().contains("no reason provided"));
    }

    #[test]
    fn mapping_timeout_returns_err() {
        let result = approval_outcome_to_tool_result(
            Ok(ApprovalOutcome::TimedOut {
                waited: std::time::Duration::from_secs(5),
            }),
            "deploy service",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timed out"));
    }

    #[test]
    fn mapping_cancelled_returns_err() {
        let result = approval_outcome_to_tool_result(
            Ok(ApprovalOutcome::Cancelled(CancelReason::ClientDisconnected)),
            "deploy service",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("cancelled"));
    }

    #[test]
    fn mapping_channel_error_returns_err() {
        let result = approval_outcome_to_tool_result(
            Err(ApprovalError::Transport("connection refused".to_string())),
            "deploy service",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Approval request failed"));
    }

    #[test]
    fn request_approval_args_serialization_strips_aura_reasoning() {
        let args = RequestApprovalArgs {
            action_description: "deploy".to_string(),
            risk_rationale: "prod change".to_string(),
            context: None,
            tool_call_intent: Some("need to unblock the migration".to_string()),
        };
        let serialized = serde_json::to_value(&args).unwrap();
        // skip_serializing keeps _aura_reasoning OUT of the wire arguments
        assert!(serialized.get("_aura_reasoning").is_none());
        assert!(serialized.get("tool_call_intent").is_none());
    }

    #[test]
    fn request_approval_args_deserializes_aura_reasoning_into_tool_call_intent() {
        let json = serde_json::json!({
            "action_description": "deploy",
            "risk_rationale": "prod change",
            "_aura_reasoning": "need to unblock the migration"
        });
        let args: RequestApprovalArgs = serde_json::from_value(json).unwrap();
        assert_eq!(
            args.tool_call_intent.as_deref(),
            Some("need to unblock the migration")
        );
    }

    #[test]
    fn request_approval_args_deserializes_without_aura_reasoning() {
        let json = serde_json::json!({
            "action_description": "deploy",
            "risk_rationale": "prod change"
        });
        let args: RequestApprovalArgs = serde_json::from_value(json).unwrap();
        assert!(args.tool_call_intent.is_none());
    }

    // --------------------------------------------------------------------
    // agent_requested path: drive `RequestApprovalTool::call` end-to-end
    // through a real `DecisionRoute::Conversational` and inspect the parked
    // `ApprovalItem` the production call site builds. These pin the call
    // site's normalization: the blank case asserts the parked item's
    // `tool_call_intent` is `None`, which only holds when the production
    // path normalizes `Some("")`/`Some("   ")` away.
    // --------------------------------------------------------------------

    /// Drive `RequestApprovalTool::call` through a real conversational route
    /// backed by an inspectable in-memory store, returning the production-built
    /// `ApprovalItem` parked for the request. Approves the request so the
    /// spawned call completes and the caller can await it.
    async fn drive_request_approval_tool(
        store: &Arc<dyn ApprovalStore>,
        registry: &PendingApprovals,
        route: &Arc<DecisionRoute>,
        args: RequestApprovalArgs,
    ) -> ApprovalItem {
        let request_id = format!("req_w2_{}", uuid::Uuid::new_v4().simple());
        let tool = RequestApprovalTool::new(
            route.clone(),
            AgentScope::Single { session_id: None },
            request_id.clone(),
        );

        // Subscribe before the call so the Requested event is captured.
        let mut rx = approval_event_broker::subscribe(&request_id).await;

        let call_handle: tokio::task::JoinHandle<Result<String, ToolError>> =
            tokio::spawn(async move { tool.call(args).await });

        // Learn the decision_id from the Requested event, then read the
        // parked request the production call site registered.
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("requested event should arrive")
            .expect("event channel open");
        let decision_id = match event {
            ApprovalLifecycleEvent::Requested(req) => {
                DecisionId::parse(&req.decision_id).expect("valid decision id")
            }
            other => panic!("expected Requested event, got {:?}", other),
        };

        let parked = store
            .get(&decision_id)
            .await
            .unwrap()
            .expect("parked approval exists");

        // Approve so the spawned tool call completes.
        registry
            .resolve(&decision_id, ApprovalDecision::Approved)
            .await
            .expect("resolve");

        let result = call_handle.await.expect("call task did not panic");
        assert!(result.is_ok(), "approved call should succeed: {:?}", result);

        approval_event_broker::unsubscribe(&request_id).await;

        parked.request.items.into_iter().next().expect("one item")
    }

    #[tokio::test]
    async fn request_approval_normalizes_blank_reasoning_to_absent() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        let registry = PendingApprovals::with_backend(store.clone(), bus);
        let registry_for_resolve = registry.clone();
        let route = Arc::new(DecisionRoute::Conversational {
            registry,
            timeout: std::time::Duration::from_secs(60),
        });

        // Both empty and whitespace-only _aura_reasoning deserialize to
        // Some(...) — the bug precondition. The production call site must
        // normalize them to None before the ApprovalItem is built, so the
        // field is omitted on the wire (never null, never empty string).
        for raw in ["", "   "] {
            let args: RequestApprovalArgs = serde_json::from_value(serde_json::json!({
                "action_description": "deploy",
                "risk_rationale": "prod change",
                "_aura_reasoning": raw,
            }))
            .unwrap();
            // Precondition: serde reads blank _aura_reasoning as Some(...),
            // so only the production path's normalization can collapse it.
            assert_eq!(
                args.tool_call_intent.as_deref(),
                Some(raw),
                "blank _aura_reasoning {:?} must deserialize to Some(...) first",
                raw,
            );

            let item =
                drive_request_approval_tool(&store, &registry_for_resolve, &route, args).await;

            assert!(
                item.tool_call_intent.is_none(),
                "production-built item must have tool_call_intent None for blank _aura_reasoning {:?}, got: {:?}",
                raw,
                item.tool_call_intent,
            );

            let wire = serde_json::to_value(&item).unwrap();
            assert!(
                wire.get("tool_call_intent").is_none(),
                "tool_call_intent must be omitted on the wire for blank _aura_reasoning {:?}, got: {}",
                raw,
                wire,
            );
            assert!(
                item.arguments.get("_aura_reasoning").is_none(),
                "items[].arguments must not contain _aura_reasoning for blank _aura_reasoning {:?}, got: {}",
                raw,
                item.arguments,
            );
        }
    }

    #[tokio::test]
    async fn request_approval_preserves_non_blank_reasoning() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        let registry = PendingApprovals::with_backend(store.clone(), bus);
        let registry_for_resolve = registry.clone();
        let route = Arc::new(DecisionRoute::Conversational {
            registry,
            timeout: std::time::Duration::from_secs(60),
        });

        // Non-blank intent must flow through the production path unchanged,
        // surrounding whitespace included — guards against over-filtering.
        let args: RequestApprovalArgs = serde_json::from_value(serde_json::json!({
            "action_description": "deploy",
            "risk_rationale": "prod change",
            "_aura_reasoning": "  need to unblock the migration  ",
        }))
        .unwrap();

        let item = drive_request_approval_tool(&store, &registry_for_resolve, &route, args).await;

        assert_eq!(
            item.tool_call_intent.as_deref(),
            Some("  need to unblock the migration  "),
            "production-built item must carry non-blank tool_call_intent unchanged",
        );

        let wire = serde_json::to_value(&item).unwrap();
        assert_eq!(
            wire.get("tool_call_intent").and_then(|v| v.as_str()),
            Some("  need to unblock the migration  "),
            "non-blank tool_call_intent must appear on the wire unchanged, got: {}",
            wire,
        );
        assert!(
            item.arguments.get("_aura_reasoning").is_none(),
            "items[].arguments must not contain _aura_reasoning, got: {}",
            item.arguments,
        );
    }

    /// Trace correlation for the agent-callable surface: `request_approval`
    /// dispatches through [`DecisionRoute::decide`] directly, never through
    /// the config gate's `decide_for_gate`, so this surface must stamp the
    /// decision id on its own execution span rather than inherit one from
    /// the gate. Mirrors the equivalent coverage for the config-gate surface
    /// in `gate.rs`'s `decision_id_span` tests.
    ///
    /// Gated on `otel` — without the feature `set_span_attribute` is a
    /// documented no-op and there is no span data to assert against.
    #[cfg(feature = "otel")]
    mod decision_id_span {
        use std::future::Future;
        use std::sync::{Arc, Mutex, MutexGuard};

        use futures::future::BoxFuture;
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
        use opentelemetry_sdk::trace::TracerProvider;
        use tracing::Instrument;
        use tracing_subscriber::layer::SubscriberExt;

        use super::*;
        use crate::logging::ATTR_DECISION_ID;

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

        /// Run `body` inside an `execute_tool` span — the span Rig opens
        /// around a tool call — under a subscriber that exports to memory,
        /// returning the body's output and the `decision_id` the exported
        /// span carries.
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

        #[tokio::test]
        async fn request_approval_tool_stamps_the_decision_id_on_the_execution_span() {
            let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
            let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
            let registry = PendingApprovals::with_backend(store, bus);
            let route = Arc::new(DecisionRoute::Conversational {
                registry: registry.clone(),
                timeout: std::time::Duration::from_secs(60),
            });

            let request_id = format!("req_tool_span_{}", uuid::Uuid::new_v4().simple());
            let mut events = approval_event_broker::subscribe(&request_id).await;
            let tool = RequestApprovalTool::new(
                route,
                AgentScope::Single { session_id: None },
                request_id.clone(),
            );
            let args = RequestApprovalArgs {
                action_description: "delete namespace".to_string(),
                risk_rationale: "touches prod".to_string(),
                context: None,
                tool_call_intent: None,
            };

            let ((result, payload_id), span_id) = traced_as_execute_tool(async {
                tokio::join!(tool.call(args), async {
                    let event = events.recv().await.expect("requested event arrives");
                    let id = match event {
                        ApprovalLifecycleEvent::Requested(req) => {
                            DecisionId::parse(&req.decision_id).expect("valid decision id")
                        }
                        other => panic!("expected Requested event, got {:?}", other),
                    };
                    registry
                        .resolve(&id, ApprovalDecision::Approved)
                        .await
                        .expect("parked approval resolves");
                    id
                })
            })
            .await;

            assert!(
                result.is_ok(),
                "an approved call must succeed: {:?}",
                result
            );
            assert_eq!(
                span_id.as_deref(),
                Some(payload_id.to_string().as_str()),
                "the execution span must carry the request_approval tool's decision id",
            );

            approval_event_broker::unsubscribe(&request_id).await;
        }
    }
}
