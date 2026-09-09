//! Projection of [`AgentEvent`]s back onto the request-scoped brokers.
//!
//! Producers move onto the [`aura_events::agent`] schema one at a time. This
//! adapter lets them do that without any consumer changing, because it
//! republishes an agent's events into the same brokers the SSE handler already
//! subscribes to, so both paths converge on identical output.
//!
//! Scope: the side channels only — tool lifecycle, MCP progress, tool usage,
//! and HITL approvals. Content-bearing events ([`AgentEventPayload::TextDelta`]
//! and friends) reach consumers through the `StreamItem` stream rather than a
//! broker, and gain their projection alongside the producer that emits them.

use aura_events::agent::{AgentEvent, AgentEventPayload};

use crate::RequestId;
use crate::approval_event_broker::{self, ApprovalLifecycleEvent};
use crate::env_flags::bool_env;
use crate::request_progress::{self, ProgressNotification};
use crate::tool_event_broker::{self, ToolLifecycleEvent, publish_tool_usage};

pub const ENV_AGENT_EVENTS: &str = "AURA_AGENT_EVENTS";

/// Defaults off until the schema reaches parity with the broker path.
pub fn agent_events_enabled() -> bool {
    bool_env(ENV_AGENT_EVENTS, false)
}

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
/// The event's [`AgentContext`] rides along on every broker event that carries
/// one, so a worker's tool call stays attributed to the worker rather than to
/// the stream's own agent.
pub async fn publish_to_brokers(request_id: &RequestId, event: AgentEvent) -> Routed {
    let AgentEvent { agent, payload } = event;
    let delivered = match payload {
        AgentEventPayload::ToolRequested {
            tool_call_id,
            tool_name,
            arguments,
        } => {
            tool_event_broker::publish(
                request_id,
                ToolLifecycleEvent::Requested {
                    tool_id: tool_call_id,
                    tool_name,
                    arguments,
                    agent: Some(agent),
                },
            )
            .await
        }

        AgentEventPayload::ToolStart {
            tool_call_id,
            tool_name,
            progress_token,
        } => {
            tool_event_broker::publish(
                request_id,
                ToolLifecycleEvent::Start {
                    tool_id: tool_call_id,
                    tool_name,
                    progress_token,
                    agent: Some(agent),
                },
            )
            .await
        }

        AgentEventPayload::ToolProgress {
            progress_token,
            progress,
            message,
        } => {
            request_progress::publish(
                request_id,
                ProgressNotification {
                    progress_token,
                    progress,
                    message,
                    agent: Some(agent),
                },
            )
            .await
        }

        AgentEventPayload::ToolUsage {
            tool_call_ids,
            usage,
        } => publish_tool_usage(request_id, tool_call_ids, usage).await,

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
        AgentEventPayload::SessionInfo { .. }
        | AgentEventPayload::McpStatus { .. }
        | AgentEventPayload::TextDelta { .. }
        | AgentEventPayload::Reasoning { .. }
        | AgentEventPayload::ToolComplete { .. }
        | AgentEventPayload::WorkerPhase { .. }
        | AgentEventPayload::Usage { .. }
        | AgentEventPayload::ContextUsage { .. }
        | AgentEventPayload::ScratchpadUsage { .. } => return Routed::NotSideChannel,

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
    use aura_events::{
        AgentContext, NumberOrString, Progress, ProgressToken, TokenCount, TokenUsage, ToolCallId,
        ToolName,
    };
    use serde_json::json;

    use crate::request_progress::subscribe as progress_subscribe;

    use crate::tool_event_broker::{
        ToolLifecycleEvent, subscribe as tool_event_subscribe, tool_usage_subscribe,
    };

    fn token(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    #[tokio::test]
    async fn tool_requested_reaches_the_tool_event_broker() {
        let request_id = RequestId::new("req_adapter_requested");
        let mut rx = tool_event_subscribe(&request_id).await;

        let routed = publish_to_brokers(
            &request_id,
            AgentEvent::single_agent(AgentEventPayload::ToolRequested {
                tool_call_id: ToolCallId::new("call_1"),
                tool_name: ToolName::new("list_files"),
                arguments: json!({ "path": "/mock" }),
            }),
        )
        .await;

        assert_eq!(routed, Routed::Delivered);
        let event = rx.recv().await.expect("event should arrive");
        let ToolLifecycleEvent::Requested {
            tool_id,
            tool_name,
            arguments,
            ..
        } = event
        else {
            panic!("expected Requested");
        };
        assert_eq!(tool_id, "call_1");
        assert_eq!(tool_name, "list_files");
        assert_eq!(arguments, json!({ "path": "/mock" }));
    }

    #[tokio::test]
    async fn tool_start_carries_its_progress_token() {
        let request_id = RequestId::new("req_adapter_start");
        let mut rx = tool_event_subscribe(&request_id).await;

        publish_to_brokers(
            &request_id,
            AgentEvent::single_agent(AgentEventPayload::ToolStart {
                tool_call_id: ToolCallId::new("call_1"),
                tool_name: ToolName::new("list_files"),
                progress_token: Some(token(7)),
            }),
        )
        .await;

        let ToolLifecycleEvent::Start { progress_token, .. } =
            rx.recv().await.expect("event should arrive")
        else {
            panic!("expected Start");
        };
        assert_eq!(progress_token, Some(token(7)));
    }

    #[tokio::test]
    async fn progress_keeps_the_raw_values_the_handler_derives_percent_from() {
        let request_id = RequestId::new("req_adapter_progress");
        let mut rx = progress_subscribe(&request_id).await;

        publish_to_brokers(
            &request_id,
            AgentEvent::single_agent(AgentEventPayload::ToolProgress {
                progress_token: token(7),
                progress: Progress::ratio(50.0, 100.0),
                message: Some("halfway".to_string()),
            }),
        )
        .await;

        let notification = rx.recv().await.expect("notification should arrive");
        assert_eq!(notification.progress.current, 50.0);
        assert_eq!(notification.progress.total, Some(100.0));
        assert_eq!(notification.message.as_deref(), Some("halfway"));
        assert_eq!(notification.percent(), Some(50));
    }

    #[tokio::test]
    async fn tool_usage_reaches_the_usage_broker() {
        let request_id = RequestId::new("req_adapter_usage");
        let mut rx = tool_usage_subscribe(&request_id).await;

        publish_to_brokers(
            &request_id,
            AgentEvent::single_agent(AgentEventPayload::ToolUsage {
                tool_call_ids: vec![ToolCallId::new("call_1")],
                usage: TokenUsage {
                    prompt_tokens: TokenCount::new(10),
                    completion_tokens: TokenCount::new(5),
                    total_tokens: TokenCount::new(15),
                },
            }),
        )
        .await;

        let usage = rx.recv().await.expect("usage should arrive");
        assert_eq!(usage.tool_ids, vec![ToolCallId::new("call_1")]);
        assert_eq!(usage.usage.total_tokens.get(), 15);
    }

    #[tokio::test]
    async fn content_events_are_not_routed_to_a_broker() {
        let routed = publish_to_brokers(
            &RequestId::new("req_adapter_text"),
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
            &RequestId::new("req_adapter_complete"),
            AgentEvent::single_agent(AgentEventPayload::ToolComplete {
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

    #[tokio::test]
    async fn a_side_channel_event_with_nobody_listening_reports_no_subscriber() {
        let routed = publish_to_brokers(
            &RequestId::new("req_adapter_unsubscribed"),
            AgentEvent::single_agent(AgentEventPayload::ToolRequested {
                tool_call_id: ToolCallId::new("call_1"),
                tool_name: ToolName::new("list_files"),
                arguments: json!({}),
            }),
        )
        .await;

        assert_eq!(routed, Routed::NoSubscriber);
    }

    /// A worker's tool call stays the worker's. Every other adapter test uses
    /// [`AgentEvent::single_agent`], where dropping the context and stamping
    /// the stream's own agent are indistinguishable.
    #[tokio::test]
    async fn a_workers_context_survives_the_broker_hop() {
        let request_id = RequestId::new("req_adapter_worker_context");
        let mut rx = tool_event_subscribe(&request_id).await;
        let worker = AgentContext::worker("log_worker", None, "orchestrator");

        publish_to_brokers(
            &request_id,
            AgentEvent::new(
                worker.clone(),
                AgentEventPayload::ToolRequested {
                    tool_call_id: ToolCallId::new("call_1"),
                    tool_name: ToolName::new("list_files"),
                    arguments: json!({}),
                },
            ),
        )
        .await;

        let ToolLifecycleEvent::Requested { agent, .. } =
            rx.recv().await.expect("event should arrive")
        else {
            panic!("expected Requested");
        };
        assert_eq!(agent, Some(worker));
    }

    /// Probes an unset name rather than [`ENV_AGENT_EVENTS`] itself, because
    /// reading the real var asserts on the ambient environment and fails for
    /// anyone who has the flag exported.
    #[test]
    fn the_flag_is_off_unless_set() {
        assert_eq!(ENV_AGENT_EVENTS, "AURA_AGENT_EVENTS");
        assert!(!bool_env("AURA_AGENT_EVENTS_UNSET_PROBE", false));
    }
}
