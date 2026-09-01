//! Proves the agent event schema reaches consumers unchanged.
//!
//! Each case drives `process_sse_stream_full` twice over the same logical
//! sequence — once publishing to the request-scoped brokers directly, once
//! publishing [`AgentEvent`]s through [`aura::agent_events`] — and asserts the
//! SSE frames are byte-identical. Only the producer side differs; both runs
//! subscribe the way `handlers::stream_chat_completion` does, so the broker
//! registry and its request-id routing are under test rather than stubbed.
//!
//! A variant that loses a field in translation fails here.

use std::sync::Arc;
use std::time::Duration;

use aura::agent_events::{Routed, publish_to_brokers};
use aura::tool_event_broker::publish_tool_requested;
use aura::{
    ApprovalLifecycleEvent, NumberOrString, ProgressNotification, ProgressToken, ResponseContent,
    StreamError, StreamItem, StreamingAgent, UsageState,
};
use aura::{
    approval_event_subscribe, approval_event_unsubscribe, publish_tool_start, publish_tool_usage,
    request_progress_subscribe, request_progress_unsubscribe, tool_event_subscribe,
    tool_event_unsubscribe, tool_usage_subscribe, tool_usage_unsubscribe,
};
use aura_events::agent::{AgentEvent, AgentEventPayload};
use aura_test_utils::mock_agent::{MockAgent, Step, items};
use aura_test_utils::sse::{SseEvent, parse_sse_stream};
use aura_web_server::streaming::{
    StreamConfig, StreamTermination, StreamingCallbacks, ToolResultMode, TurnContext,
    process_sse_stream_full,
};
use bytes::Bytes;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

const TOOL_ID: &str = "call_abc123";
const TOOL_NAME: &str = "list_files";
const TOOL_ARGS: &str = r#"{"path":"/mock"}"#;
const SESSION_ID: &str = "cs-differential";

fn token() -> ProgressToken {
    ProgressToken(NumberOrString::Number(7))
}

fn args() -> serde_json::Value {
    serde_json::json!({ "path": "/mock" })
}

/// Subscribes exactly as the production handler does, so events reach the
/// stream through the global brokers keyed by `request_id`.
async fn callbacks_for(request_id: &str) -> StreamingCallbacks {
    StreamingCallbacks {
        request_id: request_id.to_string(),
        agent: Arc::new(MockAgent::pending()),
        tool_event_rx: tool_event_subscribe(request_id).await,
        progress_rx: request_progress_subscribe(request_id).await,
        tool_usage_rx: tool_usage_subscribe(request_id).await,
        approval_event_rx: approval_event_subscribe(request_id).await,
        usage_state: UsageState::new(),
        response_content: ResponseContent::new(),
        model_name: "test/fake".to_string(),
        stream_shutdown_token: CancellationToken::new(),
    }
}

async fn unsubscribe_all(request_id: &str) {
    tool_event_unsubscribe(request_id).await;
    request_progress_unsubscribe(request_id).await;
    tool_usage_unsubscribe(request_id).await;
    approval_event_unsubscribe(request_id).await;
}

async fn run(request_id: &str, steps: Vec<Step>) -> Vec<SseEvent> {
    let callbacks = callbacks_for(request_id).await;
    let config = StreamConfig::new(true, false, ToolResultMode::Aura, 0);
    let ctx = TurnContext::new(
        "chatcmpl-test".to_string(),
        "test/fake".to_string(),
        1_700_000_000,
        None,
        SESSION_ID,
    );

    let stream = MockAgent::scripted(steps)
        .stream("q", vec![], CancellationToken::new(), request_id)
        .await
        .expect("mock stream should start");

    let (chunk_tx, mut chunk_rx) = mpsc::channel::<Result<Bytes, String>>(64);
    let collector = tokio::spawn(async move {
        let mut body = String::new();
        while let Some(chunk) = chunk_rx.recv().await {
            body.push_str(std::str::from_utf8(&chunk.expect("SSE chunk")).expect("UTF-8"));
        }
        body
    });
    let (cancel_tx, _cancel_rx) = watch::channel(false);

    let termination = process_sse_stream_full(
        &config,
        &ctx,
        stream,
        chunk_tx,
        cancel_tx,
        Duration::from_secs(900),
        // Far enough out that heartbeats never interleave with the script.
        Duration::from_secs(86_400),
        None,
        None,
        callbacks,
    )
    .await;
    assert_eq!(termination, StreamTermination::Complete);

    let body = collector.await.expect("collector should not panic");
    unsubscribe_all(request_id).await;

    let (events, done) = parse_sse_stream(&body);
    assert!(done, "stream should terminate with [DONE]");
    events
}

fn frames(events: &[SseEvent]) -> Vec<(Option<String>, String)> {
    events
        .iter()
        .map(|e| (e.event_type.clone(), e.data.clone()))
        .collect()
}

/// `broker` and `schema` must describe the same logical sequence. Both run with
/// distinct request ids so their broker registrations cannot alias.
async fn assert_paths_agree(
    case: &str,
    broker: impl FnOnce(&str) -> Vec<Step>,
    schema: impl FnOnce(&str) -> Vec<Step>,
) {
    let broker_id = format!("req_broker_{case}");
    let schema_id = format!("req_schema_{case}");

    let via_broker = run(&broker_id, broker(&broker_id)).await;
    let via_schema = run(&schema_id, schema(&schema_id)).await;

    assert_eq!(
        frames(&via_broker),
        frames(&via_schema),
        "{case}: schema path diverged from broker path"
    );
    assert!(
        !via_broker.is_empty(),
        "{case}: no frames captured, so agreement proves nothing"
    );
}

/// Publishing through the adapter must reach a subscriber; a silent
/// `NoSubscriber` would make both paths agree on emptiness.
fn emit(
    event: AgentEvent,
) -> impl Fn(String) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
    move |request_id: String| {
        let event = event.clone();
        Box::pin(async move {
            let routed = publish_to_brokers(&request_id, &event).await;
            assert_eq!(
                routed,
                Routed::Delivered,
                "adapter should reach a subscriber"
            );
        })
    }
}

fn tool_result_ok() -> Result<StreamItem, StreamError> {
    items::tool_result(TOOL_ID, "README.md\nsrc/")
}

#[tokio::test(start_paused = true)]
async fn tool_requested_matches() {
    assert_paths_agree(
        "tool_requested",
        |_| {
            vec![
                Step::effect(|request_id: String| async move {
                    publish_tool_requested(
                        &request_id,
                        TOOL_ID.to_string(),
                        TOOL_NAME.to_string(),
                        args(),
                    )
                    .await;
                }),
                Step::item(items::text("done")),
            ]
        },
        |_| {
            vec![
                Step::effect(emit(AgentEvent::single_agent(
                    AgentEventPayload::ToolRequested {
                        tool_call_id: TOOL_ID.to_string(),
                        tool_name: TOOL_NAME.to_string(),
                        arguments: args(),
                    },
                ))),
                Step::item(items::text("done")),
            ]
        },
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_full_tool_turn_matches() {
    assert_paths_agree(
        "full_turn",
        |_| {
            vec![
                Step::effect(|request_id: String| async move {
                    publish_tool_requested(
                        &request_id,
                        TOOL_ID.to_string(),
                        TOOL_NAME.to_string(),
                        args(),
                    )
                    .await;
                }),
                Step::item(items::tool_call(TOOL_ID, TOOL_NAME, TOOL_ARGS)),
                Step::effect(|request_id: String| async move {
                    publish_tool_start(
                        &request_id,
                        TOOL_ID.to_string(),
                        TOOL_NAME.to_string(),
                        Some(token()),
                    )
                    .await;
                }),
                Step::effect(|request_id: String| async move {
                    aura::request_progress::publish(
                        &request_id,
                        ProgressNotification {
                            progress_token: token(),
                            progress: 50.0,
                            total: Some(100.0),
                            message: Some("halfway".to_string()),
                        },
                    )
                    .await;
                }),
                Step::item(tool_result_ok()),
                Step::item(items::text("Here are the files.")),
            ]
        },
        |_| {
            vec![
                Step::effect(emit(AgentEvent::single_agent(
                    AgentEventPayload::ToolRequested {
                        tool_call_id: TOOL_ID.to_string(),
                        tool_name: TOOL_NAME.to_string(),
                        arguments: args(),
                    },
                ))),
                Step::item(items::tool_call(TOOL_ID, TOOL_NAME, TOOL_ARGS)),
                Step::effect(emit(AgentEvent::single_agent(
                    AgentEventPayload::ToolStart {
                        arguments: None,
                        task_id: None,
                        tool_call_id: TOOL_ID.to_string(),
                        tool_name: TOOL_NAME.to_string(),
                        progress_token: Some(token()),
                    },
                ))),
                Step::effect(emit(AgentEvent::single_agent(
                    AgentEventPayload::ToolProgress {
                        progress_token: token(),
                        progress: 50.0,
                        total: Some(100.0),
                        message: Some("halfway".to_string()),
                    },
                ))),
                Step::item(tool_result_ok()),
                Step::item(items::text("Here are the files.")),
            ]
        },
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn progress_without_a_message_matches() {
    let build_broker = |_: &str| {
        vec![
            Step::effect(|request_id: String| async move {
                aura::request_progress::publish(
                    &request_id,
                    ProgressNotification {
                        progress_token: token(),
                        progress: 3.0,
                        total: None,
                        message: None,
                    },
                )
                .await;
            }),
            Step::item(items::text("done")),
        ]
    };
    let build_schema = |_: &str| {
        vec![
            Step::effect(emit(AgentEvent::single_agent(
                AgentEventPayload::ToolProgress {
                    progress_token: token(),
                    progress: 3.0,
                    total: None,
                    message: None,
                },
            ))),
            Step::item(items::text("done")),
        ]
    };

    assert_paths_agree("progress_no_message", build_broker, build_schema).await;
}

#[tokio::test(start_paused = true)]
async fn tool_usage_matches() {
    assert_paths_agree(
        "tool_usage",
        |_| {
            vec![
                Step::effect(|request_id: String| async move {
                    publish_tool_usage(&request_id, vec![TOOL_ID.to_string()], 10, 5, 15).await;
                }),
                Step::item(items::text("done")),
            ]
        },
        |_| {
            vec![
                Step::effect(emit(AgentEvent::single_agent(
                    AgentEventPayload::ToolUsage {
                        tool_call_ids: vec![TOOL_ID.to_string()],
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        total_tokens: 15,
                    },
                ))),
                Step::item(items::text("done")),
            ]
        },
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn approval_lifecycle_matches() {
    let requested = aura_events::ApprovalRequested {
        decision_id: "dec_1".to_string(),
        tool_name: TOOL_NAME.to_string(),
        origin: aura_events::ApprovalOriginWire::ConfigGate {
            matched_pattern: "list_*".to_string(),
            agent_name: "main".to_string(),
        },
        scope: aura_events::AgentScopeWire::Single {
            session_id: Some(SESSION_ID.to_string()),
        },
    };

    let for_broker = requested.clone();
    let for_schema = requested.clone();

    assert_paths_agree(
        "approval",
        move |_| {
            vec![
                Step::effect(move |request_id: String| {
                    let event = ApprovalLifecycleEvent::Requested(for_broker.clone());
                    async move {
                        aura::approval_event_broker::publish(&request_id, event).await;
                    }
                }),
                Step::item(items::text("done")),
            ]
        },
        move |_| {
            vec![
                Step::effect(emit(AgentEvent::single_agent(
                    AgentEventPayload::ApprovalRequested(for_schema),
                ))),
                Step::item(items::text("done")),
            ]
        },
    )
    .await;
}
