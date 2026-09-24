//! Projection of approval [`AgentEvent`]s onto the request-scoped broker.
//!
//! Approvals are raised by the config gate, which rig builds before the run
//! exists and invokes on its own tool-server task, so the gate cannot reach the
//! run's channel and its events still travel by request id. Every other payload
//! reaches its observer through [`crate::run_context::RunContext::emit`].

use aura_events::agent::{AgentEvent, AgentEventPayload};

use crate::approval_event_broker::{self, ApprovalLifecycleEvent};

/// The seam producers call, so a real event stream can attach here later
/// without touching the emission sites.
///
/// A payload that finds no subscriber is reported here, so producers that only
/// wanted it logged have nothing to branch on.
pub async fn emit(request_id: &str, event: AgentEvent) -> Routed {
    let payload = std::mem::discriminant(&event.payload);
    let routed = publish_to_brokers(request_id, event).await;

    if routed == Routed::NoSubscriber {
        tracing::debug!(request_id, ?payload, "agent event reached no consumer");
    }

    routed
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routed {
    Delivered,
    NoSubscriber,
    /// The payload has no broker.
    NotSideChannel,
}

/// Publishing is fire-and-forget, so a request whose subscriber has gone away
/// yields `NoSubscriber` rather than an error. Content-bearing payloads have no
/// broker to publish to and fall through to `NotSideChannel`; they reach
/// consumers over the `StreamItem` stream instead.
///
pub(crate) async fn publish_to_brokers(request_id: &str, event: AgentEvent) -> Routed {
    let AgentEvent { payload, .. } = event;
    let payload_kind = std::mem::discriminant(&payload);
    let delivered = match payload {
        AgentEventPayload::ApprovalRequested(approval) => {
            approval_event_broker::publish(request_id, ApprovalLifecycleEvent::Requested(approval))
                .await
        }

        AgentEventPayload::ApprovalPending(approval) => {
            approval_event_broker::publish(request_id, ApprovalLifecycleEvent::Pending(approval))
                .await
        }

        AgentEventPayload::ApprovalCompleted(approval) => {
            approval_event_broker::publish(request_id, ApprovalLifecycleEvent::Completed(approval))
                .await
        }

        // Listed rather than folded into the wildcard: `AgentEventPayload` is
        // `#[non_exhaustive]` across the crate boundary, so the wildcard is
        // mandatory and exhaustiveness checking cannot flag a new side-channel
        // variant that forgets its arm here.
        AgentEventPayload::ToolRequested { .. }
        | AgentEventPayload::ToolStart { .. }
        | AgentEventPayload::ToolProgress { .. }
        | AgentEventPayload::ToolUsage { .. }
        | AgentEventPayload::SessionInfo { .. }
        | AgentEventPayload::McpStatus { .. }
        | AgentEventPayload::TextDelta { .. }
        | AgentEventPayload::Reasoning { .. }
        | AgentEventPayload::ToolComplete { .. }
        | AgentEventPayload::WorkerPhase { .. }
        | AgentEventPayload::Usage { .. }
        | AgentEventPayload::ContextUsage { .. }
        | AgentEventPayload::ScratchpadUsage { .. }
        | AgentEventPayload::PlanCreated { .. }
        | AgentEventPayload::DirectAnswer { .. }
        | AgentEventPayload::ClarificationNeeded { .. }
        | AgentEventPayload::TaskStarted { .. }
        | AgentEventPayload::TaskCompleted { .. }
        | AgentEventPayload::TaskBlocked { .. }
        | AgentEventPayload::RunParked { .. }
        | AgentEventPayload::IterationComplete { .. }
        | AgentEventPayload::ReplanStarted { .. }
        | AgentEventPayload::Synthesizing { .. } => {
            tracing::trace!(
                request_id,
                payload = ?payload_kind,
                "content payload reached no broker; it travels on the StreamItem stream"
            );
            return Routed::NotSideChannel;
        }

        unknown => {
            tracing::warn!(
                payload = ?std::mem::discriminant(&unknown),
                "agent event payload has no broker arm; the event reached no consumer"
            );
            return Routed::NotSideChannel;
        }
    };

    if delivered {
        Routed::Delivered
    } else {
        Routed::NoSubscriber
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_events::agent::ToolOutcome;
    use aura_events::{ToolCallId, ToolName};

    fn approval(decision_id: &str) -> aura_events::ApprovalRequested {
        aura_events::ApprovalRequested {
            decision_id: decision_id.to_string(),
            tool_name: "kubectl_apply".to_string(),
            tool_namespace: None,
            origin: aura_events::ApprovalOriginWire::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "ops".to_string(),
            },
            scope: aura_events::AgentScopeWire::Single { session_id: None },
        }
    }
    use serde_json::json;

    /// A worker context, because `single_agent` is what the SSE handler stamps
    /// when a usage event carries none — asserting on it would pass either way.
    /// The three approval arms publish to one broker and differ only by
    /// lifecycle variant, so an arm wired to the wrong variant is invisible
    /// without driving all three.
    #[tokio::test]
    async fn each_approval_arm_keeps_its_lifecycle_variant() {
        let request_id = "req_adapter_approvals";
        let mut rx = crate::approval_event_broker::subscribe(request_id).await;

        let origin = || aura_events::ApprovalOriginWire::ConfigGate {
            matched_pattern: "kubectl_*".to_string(),
            agent_name: "ops".to_string(),
        };
        let scope = || aura_events::AgentScopeWire::Single { session_id: None };

        let payloads = [
            AgentEventPayload::ApprovalRequested(aura_events::ApprovalRequested {
                decision_id: "d-1".to_string(),
                tool_name: "kubectl_apply".to_string(),
                tool_namespace: None,
                origin: origin(),
                scope: scope(),
            }),
            AgentEventPayload::ApprovalPending(aura_events::ApprovalPending {
                decision_id: "d-1".to_string(),
                tool_name: "kubectl_apply".to_string(),
                tool_namespace: None,
                arguments: json!({ "ns": "prod" }),
                origin: origin(),
                scope: scope(),
                expires_at: "2026-09-02T15:03:11+00:00".to_string(),
            }),
            AgentEventPayload::ApprovalCompleted(aura_events::ApprovalCompleted {
                decision_id: "d-1".to_string(),
                outcome: aura_events::ApprovalOutcomeWire::Errored {
                    message: "boom".to_string(),
                },
                duration_ms: 7,
                scope: scope(),
            }),
        ];

        for payload in payloads {
            let routed = publish_to_brokers(request_id, AgentEvent::single_agent(payload)).await;
            assert_eq!(routed, Routed::Delivered);
        }

        let arrived: Vec<_> = (0..3)
            .map(|_| rx.try_recv().expect("approval should arrive"))
            .collect();
        crate::approval_event_broker::unsubscribe(request_id).await;

        assert!(matches!(arrived[0], ApprovalLifecycleEvent::Requested(_)));
        assert!(matches!(arrived[1], ApprovalLifecycleEvent::Pending(_)));
        assert!(matches!(arrived[2], ApprovalLifecycleEvent::Completed(_)));

        let ApprovalLifecycleEvent::Pending(ref pending) = arrived[1] else {
            unreachable!()
        };
        assert_eq!(pending.expires_at, "2026-09-02T15:03:11+00:00");
    }

    #[tokio::test]
    async fn content_events_are_not_routed_to_a_broker() {
        let routed = publish_to_brokers(
            "req_adapter_text",
            AgentEvent::single_agent(AgentEventPayload::TextDelta {
                content: "hello".to_string(),
            }),
        )
        .await;

        assert_eq!(routed, Routed::NotSideChannel);
    }

    #[tokio::test]
    async fn tool_complete_is_carried_by_the_stream_not_a_broker() {
        let routed = publish_to_brokers(
            "req_adapter_complete",
            AgentEvent::single_agent(AgentEventPayload::ToolComplete {
                task_id: None,
                tool_call_id: ToolCallId::new("call_1"),
                tool_name: ToolName::new("list_files"),
                duration_ms: 3,
                outcome: ToolOutcome::Success {
                    result: "ok".to_string(),
                },
            }),
        )
        .await;

        assert_eq!(routed, Routed::NotSideChannel);
    }

    /// An approval raised for a request nobody is streaming is reported rather
    /// than mistaken for a payload that has no broker at all.
    #[tokio::test]
    async fn an_approval_with_nobody_listening_reports_no_subscriber() {
        let routed = publish_to_brokers(
            "req_adapter_unsubscribed",
            AgentEvent::single_agent(AgentEventPayload::ApprovalRequested(approval("dec_absent"))),
        )
        .await;

        assert_eq!(routed, Routed::NoSubscriber);
    }

    /// A tool payload reaches its observer on the run's channel, so the broker
    /// adapter must report it as none of its business rather than silently
    /// claiming it.
    #[tokio::test]
    async fn a_tool_event_is_not_the_brokers_business() {
        let routed = publish_to_brokers(
            "req_adapter_tool",
            AgentEvent::single_agent(AgentEventPayload::ToolRequested {
                tool_call_id: ToolCallId::new("call_1"),
                tool_name: ToolName::new("list_files"),
                arguments: json!({}),
            }),
        )
        .await;

        assert_eq!(routed, Routed::NotSideChannel);
    }
}
