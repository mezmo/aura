#![cfg(feature = "integration-events")]

//! Tool-event properties that only a real provider and MCP server establish:
//! progress tokens minted during MCP execution correlating with the
//! `aura.tool_start` that announced them, and the broker's FIFO `tool_call_id`
//! correlation across a multi-tool turn — which holds only while rig executes
//! streamed tool calls sequentially.
//!
//! Requires `AURA_CUSTOM_EVENTS=true` (set in `compose/base.yml`); without it
//! no `aura.*` events are emitted and these find nothing to assert on.
//!
//! Individual `aura.tool_*` frame shapes are covered without a provider in
//! `tests/tool_event_stream_test.rs`.

use aura_events::event_names;
use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::{SseEvent, events_by_type, parse_sse_stream};
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

async fn send_tool_request(client: &reqwest::Client) -> reqwest::Response {
    client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "test-assistant",
            "messages": [{
                "role": "user",
                "content": "List all the available files in the mock directory. Use the list_files tool."
            }],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("aura-events-test-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request")
}

fn find_tool_start_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events_by_type(events, event_names::TOOL_START)
}

fn find_progress_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events_by_type(events, event_names::PROGRESS)
}

fn find_tool_requested_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events_by_type(events, event_names::TOOL_REQUESTED)
}

fn find_tool_complete_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events_by_type(events, event_names::TOOL_COMPLETE)
}

/// Verify progress_token correlation between tool_start and progress events:
/// - aura.tool_start events include a progress_token
/// - All aura.progress events have a progress_token
/// - Progress tokens can be used to correlate events
#[tokio::test]
async fn test_progress_token_correlation() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_tool_request(&client).await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    let tool_start_events = find_tool_start_events(&events);
    let progress_events = find_progress_events(&events);

    assert!(
        !tool_start_events.is_empty(),
        "No aura.tool_start events found. LLM may not have called a tool."
    );

    let start_tokens: Vec<Value> = tool_start_events
        .iter()
        .map(|event| {
            let json: Value = serde_json::from_str(&event.data).unwrap();
            assert!(
                json.get("progress_token").is_some(),
                "tool_start missing progress_token"
            );
            json["progress_token"].clone()
        })
        .collect();

    for event in &progress_events {
        let json: Value = serde_json::from_str(&event.data).unwrap();

        assert!(
            json.get("progress_token").is_some(),
            "progress event missing progress_token"
        );

        let token = &json["progress_token"];
        assert!(
            start_tokens.iter().any(|t| t == token),
            "progress_token {} does not match any tool_start token",
            token
        );
    }
}

/// Verify FIFO ordering when multiple tools are called in sequence.
/// Uses chain_tool which triggers multiple sequential tool calls.
#[tokio::test]
async fn test_multiple_tools_fifo_ordering() {
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "test-assistant",
            "messages": [{
                "role": "user",
                "content": "Call chain_tool with steps=3 to trigger multiple sequential tool calls."
            }],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("fifo-test-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    let tool_requested = find_tool_requested_events(&events);
    let tool_start = find_tool_start_events(&events);
    let tool_complete = find_tool_complete_events(&events);

    if tool_requested.len() < 2 {
        assert!(
            !tool_requested.is_empty(),
            "No tool calls detected. LLM may not have called a tool."
        );
        return;
    }

    let requested_ids: Vec<String> = tool_requested
        .iter()
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["tool_id"].as_str().map(String::from))
        })
        .collect();

    let start_ids: Vec<String> = tool_start
        .iter()
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["tool_id"].as_str().map(String::from))
        })
        .collect();

    let complete_ids: Vec<String> = tool_complete
        .iter()
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["tool_id"].as_str().map(String::from))
        })
        .collect();

    assert_eq!(
        requested_ids, start_ids,
        "tool_requested and tool_start should have same tool_id order (FIFO)"
    );
    assert_eq!(
        start_ids, complete_ids,
        "tool_start and tool_complete should have same tool_id order (FIFO)"
    );
}
