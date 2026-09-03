//! Projection of [`AgentEvent`]s back onto the request-scoped brokers.
//!
//! Producers move onto the [`aura_events::agent`] schema one at a time. This
//! adapter lets them do that without any consumer changing: it republishes an
//! agent's events into the same brokers the SSE handler, A2A executor, and CLI
//! already subscribe to, so both paths converge on identical output.
//!
//! Scope: the side channels only — tool lifecycle, MCP progress, tool usage,
//! and HITL approvals. Content-bearing events ([`AgentEventPayload::TextDelta`]
//! and friends) reach consumers through the `StreamItem` stream rather than a
//! broker, and gain their projection alongside the producer that emits them.

use aura_events::agent::{AgentEvent, AgentEventPayload};

use crate::approval_event_broker::{self, ApprovalLifecycleEvent};
use crate::env_flags::bool_env;
use crate::request_progress::{self, ProgressNotification};
use crate::tool_event_broker::{publish_tool_requested, publish_tool_start, publish_tool_usage};

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

/// Publishing is fire-and-forget: a request whose subscriber has gone away
/// yields `NoSubscriber` rather than an error. Content-bearing payloads have no
/// broker to publish to and fall through to `NotSideChannel`; they reach
/// consumers over the `StreamItem` stream instead.
pub async fn publish_to_brokers(request_id: &str, event: &AgentEvent) -> Routed {
    let delivered = match &event.payload {
        AgentEventPayload::ToolRequested {
            tool_call_id,
            tool_name,
            arguments,
        } => {
            publish_tool_requested(
                request_id,
                tool_call_id.clone(),
                tool_name.clone(),
                arguments.clone(),
            )
            .await
        }

        AgentEventPayload::ToolStart {
            tool_call_id,
            tool_name,
            progress_token,
        } => {
            publish_tool_start(
                request_id,
                tool_call_id.clone(),
                tool_name.clone(),
                progress_token.clone(),
            )
            .await
        }

        AgentEventPayload::ToolProgress {
            progress_token,
            progress,
            total,
            message,
        } => {
            request_progress::publish(
                request_id,
                ProgressNotification {
                    progress_token: progress_token.clone(),
                    progress: *progress,
                    total: *total,
                    message: message.clone(),
                },
            )
            .await
        }

        AgentEventPayload::ToolUsage {
            tool_call_ids,
            prompt_tokens,
            completion_tokens,
            total_tokens,
        } => {
            publish_tool_usage(
                request_id,
                tool_call_ids.clone(),
                *prompt_tokens,
                *completion_tokens,
                *total_tokens,
            )
            .await
        }

        AgentEventPayload::ApprovalRequested(approval) => {
            approval_event_broker::publish(
                request_id,
                ApprovalLifecycleEvent::Requested(approval.clone()),
            )
            .await
        }

        AgentEventPayload::ApprovalPending(approval) => {
            approval_event_broker::publish(
                request_id,
                ApprovalLifecycleEvent::Pending(approval.clone()),
            )
            .await
        }

        AgentEventPayload::ApprovalCompleted(approval) => {
            approval_event_broker::publish(
                request_id,
                ApprovalLifecycleEvent::Completed(approval.clone()),
            )
            .await
        }

        _ => return Routed::NotSideChannel,
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
    use aura_events::{NumberOrString, ProgressToken};
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
        let request_id = "req_adapter_requested";
        let mut rx = tool_event_subscribe(request_id).await;

        let routed = publish_to_brokers(
            request_id,
            &AgentEvent::single_agent(AgentEventPayload::ToolRequested {
                tool_call_id: "call_1".to_string(),
                tool_name: "list_files".to_string(),
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
        let request_id = "req_adapter_start";
        let mut rx = tool_event_subscribe(request_id).await;

        publish_to_brokers(
            request_id,
            &AgentEvent::single_agent(AgentEventPayload::ToolStart {
                tool_call_id: "call_1".to_string(),
                tool_name: "list_files".to_string(),
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
        let request_id = "req_adapter_progress";
        let mut rx = progress_subscribe(request_id).await;

        publish_to_brokers(
            request_id,
            &AgentEvent::single_agent(AgentEventPayload::ToolProgress {
                progress_token: token(7),
                progress: 50.0,
                total: Some(100.0),
                message: Some("halfway".to_string()),
            }),
        )
        .await;

        let notification = rx.recv().await.expect("notification should arrive");
        assert_eq!(notification.progress, 50.0);
        assert_eq!(notification.total, Some(100.0));
        assert_eq!(notification.message.as_deref(), Some("halfway"));
        assert_eq!(notification.percent(), Some(50));
    }

    #[tokio::test]
    async fn tool_usage_reaches_the_usage_broker() {
        let request_id = "req_adapter_usage";
        let mut rx = tool_usage_subscribe(request_id).await;

        publish_to_brokers(
            request_id,
            &AgentEvent::single_agent(AgentEventPayload::ToolUsage {
                tool_call_ids: vec!["call_1".to_string()],
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
        )
        .await;

        let usage = rx.recv().await.expect("usage should arrive");
        assert_eq!(usage.tool_ids, vec!["call_1".to_string()]);
        assert_eq!(usage.total_tokens, 15);
    }

    #[tokio::test]
    async fn content_events_are_not_routed_to_a_broker() {
        let routed = publish_to_brokers(
            "req_adapter_text",
            &AgentEvent::single_agent(AgentEventPayload::TextDelta {
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
            &AgentEvent::single_agent(AgentEventPayload::ToolComplete {
                tool_call_id: "call_1".to_string(),
                tool_name: "list_files".to_string(),
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
            "req_adapter_unsubscribed",
            &AgentEvent::single_agent(AgentEventPayload::ToolRequested {
                tool_call_id: "call_1".to_string(),
                tool_name: "list_files".to_string(),
                arguments: json!({}),
            }),
        )
        .await;

        assert_eq!(routed, Routed::NoSubscriber);
    }

    #[tokio::test]
    async fn the_flag_is_off_unless_set() {
        assert!(!agent_events_enabled());
    }
}
