//! Proves an approval [`AgentEvent`] reaches consumers unchanged.
//!
//! Approvals are the one family still projected onto a request-scoped broker:
//! the config gate is built before the run exists and runs on rig's tool-server
//! task, so it cannot reach the run's channel. Every other payload travels the
//! run's own channel and is projected by `run_event_sse`, which its own tests
//! cover.
//!
//! The case here drives `process_sse_stream_full` twice over the same logical
//! sequence — once publishing to the broker directly, once publishing an
//! [`AgentEvent`] through [`aura::agent_events`] — and asserts the SSE frames
//! are byte-identical. Only the producer side differs, and the run subscribes
//! the way `handlers::stream_chat_completion` does, so the broker registry and
//! its request-id routing are under test rather than stubbed.
//!
//! A variant that loses a field in translation fails here.

use std::sync::Arc;
use std::time::Duration;

use aura::agent_events::Routed;
use aura::{
    ApprovalLifecycleEvent, ResponseContent, StreamingAgent, UsageState, approval_event_subscribe,
    approval_event_unsubscribe,
};
use aura_events::agent::{AgentEvent, AgentEventPayload};
use aura_test_utils::mock_agent::{MockAgent, Step, items};
use aura_test_utils::sse::{SseEvent, parse_sse_stream};
use aura_web_server::streaming::{
    StreamConfig, StreamTermination, StreamingCallbacks, ToolResultMode, TurnContext,
    process_sse_stream_full,
};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const TOOL_NAME: &str = "list_files";
const SESSION_ID: &str = "cs-differential";

/// Subscribes exactly as the production handler does, so an approval reaches
/// the stream through the global broker keyed by `request_id`.
async fn callbacks_for(request_id: &str) -> StreamingCallbacks {
    StreamingCallbacks {
        request_id: request_id.to_string(),
        agent: Arc::new(MockAgent::pending()),
        // The mock's run has no observer; this case is about the broker path.
        agent_events: None,
        approval_event_rx: approval_event_subscribe(request_id).await,
        usage_state: UsageState::new(),
        response_content: ResponseContent::new(),
        model_name: "test/fake".to_string(),
        stream_shutdown_token: CancellationToken::new(),
        // Both runs of a differential case share this, so the stream-start
        // frames stay identical; rehydration is server-side request setup and
        // has no agent-event producer.
        rehydrated_skills: vec![],
    }
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
        .stream(
            "q",
            vec![],
            aura::streaming::RunOptions::default(),
            request_id,
        )
        .await
        .into_events();

    let (chunk_tx, mut chunk_rx) = mpsc::channel::<Result<Bytes, String>>(64);
    let collector = tokio::spawn(async move {
        let mut body = String::new();
        while let Some(chunk) = chunk_rx.recv().await {
            body.push_str(std::str::from_utf8(&chunk.expect("SSE chunk")).expect("UTF-8"));
        }
        body
    });
    let cancel_tx = CancellationToken::new();

    let termination = process_sse_stream_full(
        &config,
        &ctx,
        stream,
        chunk_tx,
        cancel_tx,
        Some(Duration::from_secs(900)),
        // Far enough out that heartbeats never interleave with the script.
        Duration::from_secs(86_400),
        None,
        None,
        callbacks,
    )
    .await;
    assert_eq!(termination, StreamTermination::Complete);

    let body = collector.await.expect("collector should not panic");
    approval_event_unsubscribe(request_id).await;

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
/// distinct request ids so their broker registrations cannot alias; each step
/// receives the id it runs under from [`Step::effect`].
async fn assert_paths_agree(case: &str, broker: Vec<Step>, schema: Vec<Step>) {
    let broker_id = format!("req_broker_{case}");
    let schema_id = format!("req_schema_{case}");

    let via_broker = run(&broker_id, broker).await;
    let via_schema = run(&schema_id, schema).await;

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

/// The `Delivered` assert matters because a silent drop would make both paths
/// agree on emptiness.
fn emit(
    event: AgentEvent,
) -> impl Fn(String) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
    move |request_id: String| {
        let event = event.clone();
        Box::pin(async move {
            let routed = aura::agent_events::emit(&request_id, event).await;
            assert_eq!(routed, Routed::Delivered, "event should reach a consumer");
        })
    }
}

#[tokio::test(start_paused = true)]
async fn approval_lifecycle_matches() {
    let requested = aura_events::ApprovalRequested {
        decision_id: "dec_1".to_string(),
        tool_name: TOOL_NAME.to_string(),
        tool_namespace: None,
        origin: aura_events::ApprovalOriginWire::ConfigGate {
            matched_pattern: "list_*".to_string(),
            agent_name: "main".to_string(),
        },
        scope: aura_events::AgentScopeWire::Single {
            session_id: Some(SESSION_ID.to_string()),
        },
    };

    let for_broker = requested.clone();
    let for_schema = requested;

    assert_paths_agree(
        "approval",
        vec![
            Step::effect(move |request_id: String| {
                let event = ApprovalLifecycleEvent::Requested(for_broker.clone());
                async move {
                    aura::approval_event_broker::publish(&request_id, event).await;
                }
            }),
            Step::item(items::text("done")),
        ],
        vec![
            Step::effect(emit(AgentEvent::single_agent(
                AgentEventPayload::ApprovalRequested(for_schema),
            ))),
            Step::item(items::text("done")),
        ],
    )
    .await;
}
