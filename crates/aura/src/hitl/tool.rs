//! The agent-callable surface: `request_approval`, a Rig tool the agent invokes
//! when it judges that an action needs a human.
//!
//! The tool always returns a string message — even on rejection or error — so
//! the LLM can reason about the outcome; it never returns a `ToolError`.

use std::convert::Infallible;
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::decision::{AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalOutcome, DecisionId};
use super::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
use super::route::DecisionRoute;

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

impl Tool for RequestApprovalTool {
    const NAME: &'static str = "request_approval";

    type Error = Infallible;
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
        match self.route.decide(request, &cancel).await {
            Ok(ApprovalOutcome::Decided(ApprovalDecision::Approved)) => Ok(format!(
                "Approved. You may proceed with: {}",
                args.action_description
            )),
            Ok(ApprovalOutcome::Decided(ApprovalDecision::Denied { reason })) => Ok(format!(
                "Rejected: {}. Do not proceed with this action.",
                reason.unwrap_or_else(|| "no reason provided".to_string())
            )),
            Ok(ApprovalOutcome::TimedOut { .. }) => {
                Ok("Approval timed out. Treat this as not approved; do not proceed.".to_string())
            }
            Ok(ApprovalOutcome::Cancelled(_)) => Ok(
                "Approval was cancelled. Treat this as not approved; do not proceed.".to_string(),
            ),
            Err(e) => Ok(format!(
                "Approval request failed: {e}. Do not proceed — treat this as a rejection."
            )),
        }
    }
}
