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
                    }
                },
                "required": ["action_description", "risk_rationale"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
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
}
