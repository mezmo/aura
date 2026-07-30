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

use super::decision::{AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalOutcome, DecisionId};
use super::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
use super::route::{ApprovalError, DecisionRoute};
use crate::orchestration::park::ToolAttemptOutcome;
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
    /// Present only when the deployment supports durable parking: a gate hit
    /// then parks the approval and ends the attempt as `Blocked` instead of
    /// holding the await for the length of the request.
    blocked_signal: Option<BlockedSignal>,
}

impl HitlApprovalWrapper {
    #[must_use]
    pub fn new(
        patterns: Arc<[GlobPattern]>,
        route: Arc<DecisionRoute>,
        scope: AgentScope,
        request_id: String,
    ) -> Self {
        Self {
            patterns,
            route,
            scope,
            request_id,
            blocked_signal: None,
        }
    }

    /// Arm the durable-park path: a gate hit parks the approval and reports
    /// the block through `signal` instead of awaiting the decision in-request.
    #[must_use]
    pub fn with_blocked_signal(mut self, signal: BlockedSignal) -> Self {
        self.blocked_signal = Some(signal);
        self
    }

    /// Park the gated call durably and end the attempt blocked (ADR
    /// 2026-07-21, decisions 1 and 11). The fill builds one
    /// [`PreCallOutcome::Blocked`] and returns it THROUGH
    /// [`BlockedSignal::deposit`], which stores its projection and hands the
    /// outcome back — the side channel and the wrapper-chain value are one.
    #[expect(unused_variables, reason = "staged for #271: durable gate park")]
    fn park_instead_of_awaiting(
        &self,
        request: ApprovalRequest,
        signal: &BlockedSignal,
    ) -> Result<PreCallOutcome, ToolError> {
        todo!("staged for #271: durable gate park -> ToolAttemptOutcome::Blocked")
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
            return Ok(PreCallOutcome::Proceed);
        };
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: self.request_id.clone(),
            scope: self.scope.clone(),
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: matched.to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: ctx.tool_name.clone(),
                arguments: args.clone(),
                tool_call_intent: ctx.tool_call_intent.clone(),
            }],
        };
        if let Some(signal) = &self.blocked_signal {
            return self.park_instead_of_awaiting(request, signal);
        }
        let cancel =
            crate::request_cancellation::RequestCancellation::token_for_id(&self.request_id)
                .unwrap_or_else(crate::request_cancellation::RequestCancelToken::unbound);
        approval_result_to_pre_call(self.route.decide(request, &cancel).await)
    }
}

fn approval_result_to_pre_call(
    result: Result<ApprovalOutcome, ApprovalError>,
) -> Result<PreCallOutcome, ToolError> {
    match result {
        Ok(ApprovalOutcome::Decided(ApprovalDecision::Approved)) => Ok(PreCallOutcome::Proceed),
        Ok(ApprovalOutcome::Decided(ApprovalDecision::Denied { reason })) => {
            Ok(PreCallOutcome::ShortCircuit {
                output: format!(
                    "Tool call blocked by human approval denial: {}. Do not execute this action.",
                    reason.unwrap_or_else(|| "no reason provided".to_string())
                ),
            })
        }
        Ok(ApprovalOutcome::TimedOut { .. }) => Err(ToolError::ToolCallError(
            "tool call denied: approval timed out".to_string().into(),
        )),
        Ok(ApprovalOutcome::Cancelled(_)) => Err(ToolError::ToolCallError(
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
            approval_result_to_pre_call(Ok(ApprovalOutcome::Decided(ApprovalDecision::Approved)))
                .unwrap(),
            PreCallOutcome::Proceed
        );
    }

    #[test]
    fn approval_result_mapping_denial_is_feedback_not_error() {
        let outcome =
            approval_result_to_pre_call(Ok(ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: Some("too risky".to_string()),
            })))
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
        let timed_out = approval_result_to_pre_call(Ok(ApprovalOutcome::TimedOut {
            waited: Duration::from_secs(1),
        }))
        .unwrap_err()
        .to_string();
        assert!(timed_out.contains("approval timed out"));

        let cancelled = approval_result_to_pre_call(Ok(ApprovalOutcome::Cancelled(
            CancelReason::ClientDisconnected,
        )))
        .unwrap_err()
        .to_string();
        assert!(cancelled.contains("approval cancelled"));

        let sender_dropped = approval_result_to_pre_call(Ok(ApprovalOutcome::Cancelled(
            CancelReason::SenderDropped,
        )))
        .unwrap_err()
        .to_string();
        assert!(sender_dropped.contains("approval cancelled"));

        let channel_fault =
            approval_result_to_pre_call(Err(ApprovalError::BadStatus { status: 500 }))
                .unwrap_err()
                .to_string();
        assert!(channel_fault.contains("approval channel error"));
    }
}
