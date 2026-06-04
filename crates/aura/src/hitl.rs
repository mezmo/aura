//! Human-in-the-loop (HITL) approval gate.
//!
//! This module provides a webhook-based approval workflow that blocks tool
//! execution until an external service approves (or rejects) the call.
//!
//! # Architecture
//!
//! The gate is implemented as a [`ToolWrapper`] that uses the `pre_call` hook.
//! When a tool call matches one of the configured glob patterns, the wrapper
//! sends a JSON payload to the configured webhook URL and blocks until a
//! response arrives (or times out).
//!
//! # Protocol
//!
//! **Request** (`POST` to webhook URL):
//! ```json
//! {
//!   "version": 1,
//!   "request_type": "tool_gate",
//!   "request_id": "<uuid>",
//!   "timestamp": "<ISO 8601>",
//!   "agent": { "name": "...", "run_id": "...", "session_id": "..." },
//!   "items": [{
//!     "tool_name": "...",
//!     "arguments": { ... },
//!     "matched_pattern": "mezmo_*",
//!     "task_id": 3,
//!     "worker_name": "log_worker"
//!   }]
//! }
//! ```
//!
//! **Response** (either shape accepted):
//! ```json
//! { "approved": true }
//! ```
//! ```json
//! { "decisions": [{ "approved": true, "reason": "looks good" }] }
//! ```

use async_trait::async_trait;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use rig::tool::ToolError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::convert::Infallible;
use std::sync::Arc;

use aura_events::RequestType;

use crate::config::glob_match;
use crate::tool_event_broker::{self, ToolLifecycleEvent};
use crate::tool_wrapper::{ToolCallContext, ToolWrapper};

// ---------------------------------------------------------------------------
// Webhook protocol types
// ---------------------------------------------------------------------------

/// Payload sent to the approval webhook.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRequest {
    /// Protocol version (currently `1`).
    pub version: u32,
    /// Discriminator for future request types.
    pub request_type: RequestType,
    /// Unique identifier for this request (UUID v4).
    pub request_id: String,
    /// ISO 8601 timestamp of the request.
    pub timestamp: String,
    /// Agent/worker context.
    pub agent: HitlAgentContext,
    /// One or more items awaiting approval.
    pub items: Vec<ApprovalItem>,
}

/// Agent metadata included in every approval request.
#[derive(Debug, Clone, Serialize)]
pub struct HitlAgentContext {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// A single tool call awaiting approval.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalItem {
    pub tool_name: String,
    pub arguments: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_name: Option<String>,
}

/// Webhook response. Accepts two shapes:
///
/// - **Simple:** `{ "approved": true, "reason": "..." }`
/// - **Batch:**  `{ "decisions": [{ "approved": true, "reason": "..." }] }`
///
/// When both `decisions` and `approved` are present, `decisions` takes
/// precedence and `approved` is ignored.
///
/// Fail-closed: if neither `approved` nor `decisions` is present, or if
/// `decisions` is an empty array, `resolve_single()` returns `(false, reason)`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResponse {
    /// Direct single-decision field.
    pub approved: Option<bool>,
    /// Optional reason (used with the simple shape).
    pub reason: Option<String>,
    /// Per-item decisions (batch shape).
    pub decisions: Option<Vec<ApprovalDecision>>,
}

impl ApprovalResponse {
    /// Resolve a single approval decision from whichever response shape was
    /// used. Returns `(approved, reason)`.
    ///
    /// Precedence: if `decisions` is present, the first decision wins;
    /// otherwise falls back to `approved`. Missing or empty decisions are
    /// treated as rejected.
    pub fn resolve_single(&self) -> (bool, Option<String>) {
        if let Some(ref decisions) = self.decisions {
            decisions
                .first()
                .map(|d| (d.approved, d.reason.clone()))
                .unwrap_or((false, Some("empty decisions array".to_string())))
        } else {
            (self.approved.unwrap_or(false), self.reason.clone())
        }
    }
}

/// A single per-item decision within a batch response.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecision {
    pub approved: bool,
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// ApprovalDispatch trait + errors
// ---------------------------------------------------------------------------

/// Errors that can occur during the approval workflow.
#[derive(thiserror::Error, Debug, Clone)]
pub enum ApprovalError {
    /// The webhook did not respond within the configured timeout.
    #[error("approval webhook timed out")]
    Timeout,
    /// An HTTP-level error (connection refused, non-2xx status, etc.).
    #[error("approval webhook HTTP error: {0}")]
    HttpError(String),
    /// The webhook responded but the body could not be parsed.
    #[error("could not parse approval response: {0}")]
    ParseError(String),
}

impl From<ApprovalError> for ToolError {
    fn from(e: ApprovalError) -> Self {
        ToolError::ToolCallError(format!("Tool call blocked: {e}").into())
    }
}

/// Abstraction over how approval requests are dispatched.
///
/// The default implementation ([`HttpApprovalDispatch`]) sends a POST to a
/// webhook URL. Tests can substitute a mock implementation.
#[async_trait]
pub trait ApprovalDispatch: Send + Sync {
    async fn request_approval(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalResponse, ApprovalError>;
}

// ---------------------------------------------------------------------------
// HttpApprovalDispatch
// ---------------------------------------------------------------------------

/// Sends approval requests to a webhook URL via HTTP POST.
pub struct HttpApprovalDispatch {
    webhook_url: String,
    client: reqwest::Client,
    timeout: std::time::Duration,
}

impl HttpApprovalDispatch {
    pub fn new(client: reqwest::Client, webhook_url: String, timeout_secs: u64) -> Self {
        Self {
            client,
            timeout: std::time::Duration::from_secs(timeout_secs),
            webhook_url,
        }
    }
}

#[async_trait]
impl ApprovalDispatch for HttpApprovalDispatch {
    async fn request_approval(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalResponse, ApprovalError> {
        let response = self
            .client
            .post(&self.webhook_url)
            .json(request)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ApprovalError::Timeout
                } else {
                    ApprovalError::HttpError(e.to_string())
                }
            })?;

        if !response.status().is_success() {
            return Err(ApprovalError::HttpError(format!(
                "webhook returned {}",
                response.status()
            )));
        }

        response
            .json::<ApprovalResponse>()
            .await
            .map_err(|e| ApprovalError::ParseError(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// HitlContext — shared between wrapper and (future) tool
// ---------------------------------------------------------------------------

/// Shared context that both [`HitlApprovalWrapper`] and
/// `RequestApprovalTool` use to dispatch approval requests.
pub struct HitlContext {
    pub dispatch: Arc<dyn ApprovalDispatch>,
    pub agent_name: String,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    /// Request-scoped identifier for routing SSE events via the tool_event_broker.
    /// Same value in single-agent and orchestration mode so worker approval
    /// events reach the client stream the handler subscribed on.
    pub request_id: String,
}

impl HitlContext {
    /// Send an approval request to the webhook and emit SSE events for
    /// the request/response lifecycle. Returns the outcome
    /// (approved/rejected + reason + duration) or an error for the
    /// caller to map into its own return type.
    async fn dispatch_and_emit(
        &self,
        request: ApprovalRequest,
        event_tool_name: &str,
        event_matched_pattern: Option<String>,
        event_task_id: Option<usize>,
        event_worker_name: Option<String>,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        let _ = tool_event_broker::publish(
            &self.request_id,
            ToolLifecycleEvent::ApprovalRequested {
                tool_name: event_tool_name.to_string(),
                matched_pattern: event_matched_pattern.clone(),
                request_type: request.request_type,
                task_id: event_task_id,
                worker_name: event_worker_name.clone(),
            },
        )
        .await;

        let start = std::time::Instant::now();
        let response = self.dispatch.request_approval(&request).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        let (approved, reason) = match &response {
            Ok(resp) => resp.resolve_single(),
            Err(e) => {
                let _ = tool_event_broker::publish(
                    &self.request_id,
                    ToolLifecycleEvent::ApprovalCompleted {
                        tool_name: event_tool_name.to_string(),
                        approved: false,
                        reason: Some(format!("webhook error: {e}")),
                        duration_ms,
                        task_id: event_task_id,
                    },
                )
                .await;
                return Err(e.clone());
            }
        };

        let reason_for_event = reason.clone().or_else(|| {
            if !approved {
                Some("webhook error".to_string())
            } else {
                None
            }
        });
        let _ = tool_event_broker::publish(
            &self.request_id,
            ToolLifecycleEvent::ApprovalCompleted {
                tool_name: event_tool_name.to_string(),
                approved,
                reason: reason_for_event,
                duration_ms,
                task_id: event_task_id,
            },
        )
        .await;

        Ok(ApprovalOutcome {
            approved,
            reason,
            duration_ms,
        })
    }
}

/// Result of an approval dispatch, for callers to map into their own
/// return types (`Result<(), ToolError>` vs `Result<String, Infallible>`).
struct ApprovalOutcome {
    approved: bool,
    reason: Option<String>,
    duration_ms: u64,
}

// ---------------------------------------------------------------------------
// HitlApprovalWrapper
// ---------------------------------------------------------------------------

/// A [`ToolWrapper`] that gates tool execution behind an external approval
/// webhook.
///
/// When a tool call matches one of the configured glob patterns, a JSON
/// payload is sent to the webhook URL. The tool call proceeds only if the
/// webhook responds with `approved: true`; otherwise a [`ToolError`] is
/// returned to the LLM so it can retry or explain the rejection.
pub struct HitlApprovalWrapper {
    patterns: Arc<[String]>,
    hitl: Arc<HitlContext>,
}

impl HitlApprovalWrapper {
    pub fn new(patterns: Arc<[String]>, hitl: Arc<HitlContext>) -> Self {
        Self { patterns, hitl }
    }

    /// Check whether `tool_name` matches any of the configured patterns.
    /// Returns the first matching pattern, if any.
    ///
    /// The `request_approval` tool is always excluded — gating the approval
    /// tool itself would create a double-approval loop.
    fn matches(&self, tool_name: &str) -> Option<&str> {
        if tool_name == RequestApprovalTool::NAME {
            return None;
        }
        self.patterns
            .iter()
            .find(|p| glob_match(p, tool_name))
            .map(|p| p.as_str())
    }
}

#[async_trait]
impl ToolWrapper for HitlApprovalWrapper {
    async fn pre_call(&self, args: &Value, ctx: &ToolCallContext) -> Result<(), ToolError> {
        let Some(matched_pattern) = self.matches(&ctx.tool_name) else {
            return Ok(());
        };

        tracing::info!(
            tool = %ctx.tool_name,
            pattern = %matched_pattern,
            "HITL approval required"
        );

        let request = ApprovalRequest {
            version: 1,
            request_type: RequestType::ToolGate,
            request_id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            agent: HitlAgentContext {
                name: self.hitl.agent_name.clone(),
                run_id: self.hitl.run_id.clone(),
                session_id: self.hitl.session_id.clone(),
            },
            items: vec![ApprovalItem {
                tool_name: ctx.tool_name.clone(),
                arguments: args.clone(),
                matched_pattern: Some(matched_pattern.to_string()),
                task_id: ctx.task_id,
                worker_name: if ctx.tool_initiator_id.is_empty() {
                    None
                } else {
                    Some(ctx.tool_initiator_id.clone())
                },
            }],
        };

        let outcome = self
            .hitl
            .dispatch_and_emit(
                request,
                &ctx.tool_name,
                Some(matched_pattern.to_string()),
                ctx.task_id,
                if ctx.tool_initiator_id.is_empty() {
                    None
                } else {
                    Some(ctx.tool_initiator_id.clone())
                },
            )
            .await
            .map_err(ToolError::from)?;

        if outcome.approved {
            tracing::info!(tool = %ctx.tool_name, duration_ms = outcome.duration_ms, "HITL approved");
            Ok(())
        } else {
            let reason = outcome
                .reason
                .unwrap_or_else(|| "no reason provided".to_string());
            tracing::warn!(tool = %ctx.tool_name, reason = %reason, duration_ms = outcome.duration_ms, "HITL rejected");
            Err(ToolError::ToolCallError(
                format!("Tool call rejected by approval gate: {reason}").into(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// RequestApprovalTool — agent-initiated approval requests
// ---------------------------------------------------------------------------

/// A Rig tool that agents call explicitly to request human approval before
/// proceeding with a sensitive action. Unlike [`HitlApprovalWrapper`] (which
/// gates tool calls by pattern), this tool lets the agent *itself* decide when
/// approval is needed.
///
/// The tool always returns a string message — even on rejection or dispatch
/// error — so the LLM can reason about the outcome. It never returns a
/// `ToolError`; Rig handles deserialization errors before `call()` is reached.
#[derive(Clone)]
pub struct RequestApprovalTool {
    hitl: Arc<HitlContext>,
}

impl RequestApprovalTool {
    pub fn new(hitl: Arc<HitlContext>) -> Self {
        Self { hitl }
    }
}

/// Arguments the LLM provides when calling `request_approval`.
#[derive(Debug, Deserialize, Serialize)]
pub struct RequestApprovalArgs {
    /// What the agent wants to do.
    pub action_description: String,
    /// Why the agent is asking for approval.
    pub risk_rationale: String,
    /// Optional structured metadata for the reviewer.
    #[serde(default)]
    pub context: Option<Value>,
}

impl Tool for RequestApprovalTool {
    const NAME: &'static str = "request_approval";

    type Error = Infallible;
    type Args = RequestApprovalArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Request human approval before proceeding with a sensitive action. \
                Describe what you want to do, why it's risky, and optionally provide \
                structured context. The tool blocks until a human approves or rejects."
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
                    }
                },
                "required": ["action_description", "risk_rationale"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<String, Infallible> {
        tracing::info!(
            action = %args.action_description,
            rationale = %args.risk_rationale,
            "Agent requested human approval"
        );

        let arguments = serde_json::to_value(&args).unwrap_or_default();

        let request = ApprovalRequest {
            version: 1,
            request_type: RequestType::ApprovalRequest,
            request_id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            agent: HitlAgentContext {
                name: self.hitl.agent_name.clone(),
                run_id: self.hitl.run_id.clone(),
                session_id: self.hitl.session_id.clone(),
            },
            items: vec![ApprovalItem {
                tool_name: Self::NAME.to_string(),
                arguments,
                matched_pattern: None,
                task_id: None,
                worker_name: None,
            }],
        };

        let outcome = match self
            .hitl
            .dispatch_and_emit(request, Self::NAME, None, None, None)
            .await
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    action = %args.action_description,
                    error = %e,
                    "Approval request failed"
                );
                return Ok(format!(
                    "Approval request failed: {e}. Do not proceed — treat this as a rejection."
                ));
            }
        };

        if outcome.approved {
            tracing::info!(action = %args.action_description, duration_ms = outcome.duration_ms, "Approval granted");
            Ok(format!(
                "Approved. You may proceed with: {}",
                args.action_description
            ))
        } else {
            let reason = outcome
                .reason
                .unwrap_or_else(|| "no reason provided".to_string());
            tracing::warn!(action = %args.action_description, reason = %reason, duration_ms = outcome.duration_ms, "Approval rejected");
            Ok(format!(
                "Rejected: {reason}. Do not proceed with this action."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    struct MockDispatch {
        response: Mutex<Result<ApprovalResponse, ApprovalError>>,
        last_request: Mutex<Option<ApprovalRequest>>,
    }

    impl MockDispatch {
        fn approved() -> Self {
            Self {
                response: Mutex::new(Ok(ApprovalResponse {
                    approved: Some(true),
                    reason: None,
                    decisions: None,
                })),
                last_request: Mutex::new(None),
            }
        }

        fn rejected(reason: &str) -> Self {
            Self {
                response: Mutex::new(Ok(ApprovalResponse {
                    approved: Some(false),
                    reason: Some(reason.to_string()),
                    decisions: None,
                })),
                last_request: Mutex::new(None),
            }
        }

        fn batch_decisions(decisions: Vec<ApprovalDecision>) -> Self {
            Self {
                response: Mutex::new(Ok(ApprovalResponse {
                    approved: None,
                    reason: None,
                    decisions: Some(decisions),
                })),
                last_request: Mutex::new(None),
            }
        }

        fn timeout() -> Self {
            Self {
                response: Mutex::new(Err(ApprovalError::Timeout)),
                last_request: Mutex::new(None),
            }
        }

        fn last_request(&self) -> Option<ApprovalRequest> {
            self.last_request.try_lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ApprovalDispatch for MockDispatch {
        async fn request_approval(
            &self,
            request: &ApprovalRequest,
        ) -> Result<ApprovalResponse, ApprovalError> {
            *self.last_request.lock().await = Some(request.clone());
            let mut guard = self.response.lock().await;
            std::mem::replace(
                &mut *guard,
                Err(ApprovalError::HttpError("already consumed".into())),
            )
        }
    }

    fn make_ctx(dispatch: impl ApprovalDispatch + 'static) -> Arc<HitlContext> {
        Arc::new(HitlContext {
            dispatch: Arc::new(dispatch),
            agent_name: "test-agent".to_string(),
            run_id: Some("run-123".to_string()),
            session_id: Some("sess-456".to_string()),
            request_id: "test-req-123".to_string(),
        })
    }

    fn make_tool_ctx(tool_name: &str) -> ToolCallContext {
        ToolCallContext::new(tool_name)
    }

    #[tokio::test]
    async fn test_no_match_passes_through() {
        let hitl = make_ctx(MockDispatch::approved());
        let wrapper = HitlApprovalWrapper::new(Arc::from(vec!["kubectl_*".to_string()]), hitl);

        let ctx = make_tool_ctx("echo");
        let result = wrapper.pre_call(&serde_json::json!({}), &ctx).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_approved_passes() {
        let hitl = make_ctx(MockDispatch::approved());
        let wrapper = HitlApprovalWrapper::new(Arc::from(vec!["kubectl_*".to_string()]), hitl);

        let ctx = make_tool_ctx("kubectl_delete");
        let result = wrapper.pre_call(&serde_json::json!({}), &ctx).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rejected_returns_error() {
        let hitl = make_ctx(MockDispatch::rejected("too risky"));
        let wrapper = HitlApprovalWrapper::new(Arc::from(vec!["kubectl_*".to_string()]), hitl);

        let ctx = make_tool_ctx("kubectl_delete");
        let result = wrapper.pre_call(&serde_json::json!({}), &ctx).await;

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("too risky"), "expected reason in error: {msg}");
    }

    #[tokio::test]
    async fn test_timeout_returns_error() {
        let hitl = make_ctx(MockDispatch::timeout());
        let wrapper = HitlApprovalWrapper::new(Arc::from(vec!["kubectl_*".to_string()]), hitl);

        let ctx = make_tool_ctx("kubectl_delete");
        let result = wrapper.pre_call(&serde_json::json!({}), &ctx).await;

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("timed out"),
            "expected timeout in error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_batch_response_approved() {
        let decisions = vec![ApprovalDecision {
            approved: true,
            reason: Some("lgtm".to_string()),
        }];
        let hitl = make_ctx(MockDispatch::batch_decisions(decisions));
        let wrapper = HitlApprovalWrapper::new(Arc::from(vec!["kubectl_*".to_string()]), hitl);

        let ctx = make_tool_ctx("kubectl_apply");
        let result = wrapper.pre_call(&serde_json::json!({}), &ctx).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_batch_response_rejected() {
        let decisions = vec![ApprovalDecision {
            approved: false,
            reason: Some("not now".to_string()),
        }];
        let hitl = make_ctx(MockDispatch::batch_decisions(decisions));
        let wrapper = HitlApprovalWrapper::new(Arc::from(vec!["kubectl_*".to_string()]), hitl);

        let ctx = make_tool_ctx("kubectl_scale");
        let result = wrapper.pre_call(&serde_json::json!({}), &ctx).await;

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not now"), "expected reason in error: {msg}");
    }

    #[tokio::test]
    async fn test_webhook_payload_shape() {
        let mock = Arc::new(MockDispatch::approved());
        let mock_ref = mock.clone();
        let hitl = Arc::new(HitlContext {
            dispatch: mock as Arc<dyn ApprovalDispatch>,
            agent_name: "test-agent".to_string(),
            run_id: Some("run-123".to_string()),
            session_id: Some("sess-456".to_string()),
            request_id: "test-req-123".to_string(),
        });
        let wrapper = HitlApprovalWrapper::new(Arc::from(vec!["scale_*".to_string()]), hitl);

        let args = serde_json::json!({"replicas": 3});
        let ctx = make_tool_ctx("scale_deployment");
        let _ = wrapper.pre_call(&args, &ctx).await;

        let req = mock_ref.last_request().expect("request was captured");

        assert_eq!(req.version, 1);
        assert_eq!(req.request_type, RequestType::ToolGate);
        assert!(!req.request_id.is_empty(), "request_id must be non-empty");
        assert_eq!(req.agent.name, "test-agent");
        assert_eq!(req.agent.run_id.as_deref(), Some("run-123"));
        assert_eq!(req.agent.session_id.as_deref(), Some("sess-456"));
        assert_eq!(req.items.len(), 1);
        assert_eq!(req.items[0].tool_name, "scale_deployment");
        assert_eq!(req.items[0].matched_pattern.as_deref(), Some("scale_*"));
        assert_eq!(req.items[0].arguments, serde_json::json!({"replicas": 3}));
    }

    #[test]
    fn test_resolve_single_simple() {
        let resp = ApprovalResponse {
            approved: Some(true),
            reason: Some("all good".to_string()),
            decisions: None,
        };
        let (approved, reason) = resp.resolve_single();
        assert!(approved);
        assert_eq!(reason.as_deref(), Some("all good"));

        let resp_rejected = ApprovalResponse {
            approved: Some(false),
            reason: Some("nope".to_string()),
            decisions: None,
        };
        let (approved, reason) = resp_rejected.resolve_single();
        assert!(!approved);
        assert_eq!(reason.as_deref(), Some("nope"));
    }

    #[test]
    fn test_resolve_single_batch() {
        let resp = ApprovalResponse {
            approved: None,
            reason: None,
            decisions: Some(vec![ApprovalDecision {
                approved: true,
                reason: Some("batch ok".to_string()),
            }]),
        };
        let (approved, reason) = resp.resolve_single();
        assert!(approved);
        assert_eq!(reason.as_deref(), Some("batch ok"));

        // Empty decisions array falls back to rejected
        let resp_empty = ApprovalResponse {
            approved: None,
            reason: None,
            decisions: Some(vec![]),
        };
        let (approved, reason) = resp_empty.resolve_single();
        assert!(!approved);
        assert_eq!(reason.as_deref(), Some("empty decisions array"));
    }

    #[tokio::test]
    async fn test_request_approval_tool_excluded_from_glob() {
        let hitl = make_ctx(MockDispatch::rejected("should not reach webhook"));
        let wrapper = HitlApprovalWrapper::new(Arc::from(vec!["*".to_string()]), hitl);

        let ctx = make_tool_ctx("request_approval");
        let result = wrapper.pre_call(&serde_json::json!({}), &ctx).await;

        assert!(
            result.is_ok(),
            "request_approval must bypass glob matching even with wildcard pattern"
        );
    }
}
