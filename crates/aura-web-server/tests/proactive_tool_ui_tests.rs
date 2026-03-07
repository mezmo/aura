#![cfg(feature = "integration-events")]

//! Integration tests for proactive tool call UI
//!
//! Tests status messages and finish_reason compliance with OpenAI spec
//!

use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::parse_data_line;
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: usize = 2;

/// Helper to send chat completion request
async fn send_chat_request(
    client: &reqwest::Client,
    messages: Vec<Value>,
    stream: bool,
) -> reqwest::Response {
    client
        .post(format!("{}/v1/chat/completions", AURA_SERVER))
        .json(&json!({
            "model": "gpt-4o",
            "messages": messages,
            "stream": stream,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("proactive-ui-test-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request")
}

/// Verifies tool calls stream without extra status messages like "Calling tool...".
#[tokio::test]
async fn test_tool_call_streaming_clean() {
    let client = reqwest::Client::new();

    let response = send_chat_request(
        &client,
        vec![json!({
            "role": "user",
            "content": "Call mock_tool with message 'test_clean'"
        })],
        true,
    )
    .await;

    assert_eq!(response.status(), 200);

    let body = response.text().await.expect("Failed to read response");
    let lines: Vec<&str> = body.lines().collect();

    let mut found_tool_call = false;
    let mut found_status_message = false;
    let mut current_event_type: Option<String> = None;

    for line in lines {
        if line.is_empty() || line == "data: [DONE]" {
            continue;
        }
        // Track SSE event type
        if let Some(event) = line.strip_prefix("event: ") {
            current_event_type = Some(event.to_string());
            continue;
        }
        if let Some(chunk) = parse_data_line(line) {
            // Skip aura.* custom events
            let is_aura_event = current_event_type
                .as_ref()
                .map(|t| t.starts_with("aura."))
                .unwrap_or(false);
            current_event_type = None;
            if is_aura_event {
                continue;
            }

            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data) {
                // Check that we DON'T have status messages
                if let Some(content) = json["choices"][0]["delta"]["content"].as_str()
                    && (content.contains("Calling") || content.contains("completed"))
                {
                    found_status_message = true;
                }

                // Verify tool call exists
                if json["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .is_some()
                {
                    found_tool_call = true;
                }
            }
        }
    }

    assert!(
        !found_status_message,
        "Should NOT have found status messages (clean streaming)"
    );
    assert!(found_tool_call, "Should have found tool call");
}

/// Verifies OpenAI API finish_reason compliance with tool calls:
/// - finish_reason: null during streaming
/// - finish_reason: "stop" at the end (server-side tool execution, not client-side)
/// - Empty delta in final chunk
///
/// Note: Unlike OpenAI's client-side tool calling (which returns "tool_calls"),
/// Aura executes tools server-side and returns "stop" since no client action is needed.
#[tokio::test]
async fn test_finish_reason_stop_with_server_side_tools() {
    let result =
        aura_test_utils::retry_test(MAX_RETRIES, || async { run_finish_reason_test().await }).await;

    if let Err(e) = result {
        panic!("Test failed after {} retries: {}", MAX_RETRIES + 1, e);
    }
}

async fn run_finish_reason_test() -> Result<(), String> {
    let client = reqwest::Client::new();

    let response = send_chat_request(
        &client,
        vec![json!({
            "role": "user",
            "content": "Call mock_tool with message 'test_finish_reason'"
        })],
        true,
    )
    .await;

    if response.status() != 200 {
        return Err(format!("Request failed with status: {}", response.status()));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;
    let lines: Vec<&str> = body.lines().collect();

    let mut finish_reasons: Vec<(Option<String>, bool)> = Vec::new(); // (finish_reason, has_empty_delta)
    let mut current_event_type: Option<String> = None;

    for line in lines {
        if line.is_empty() || line == "data: [DONE]" {
            continue;
        }
        // Track SSE event type
        if let Some(event) = line.strip_prefix("event: ") {
            current_event_type = Some(event.to_string());
            continue;
        }
        if let Some(chunk) = parse_data_line(line) {
            // Skip aura.* custom events
            let is_aura_event = current_event_type
                .as_ref()
                .map(|t| t.starts_with("aura."))
                .unwrap_or(false);
            current_event_type = None;
            if is_aura_event {
                continue;
            }

            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data) {
                let finish_reason = json["choices"][0]["finish_reason"]
                    .as_str()
                    .map(|s| s.to_string());

                // Check if delta is empty (final chunk pattern)
                let delta = &json["choices"][0]["delta"];
                let is_empty_delta = delta["content"].is_null()
                    && delta["tool_calls"].is_null()
                    && finish_reason.is_some();

                finish_reasons.push((finish_reason, is_empty_delta));
            }
        }
    }

    // During streaming, all finish_reasons should be null except the last one
    let non_null_finish_reasons: Vec<_> = finish_reasons
        .iter()
        .filter(|(fr, _)| fr.is_some())
        .collect();

    if non_null_finish_reasons.len() != 1 {
        return Err(format!(
            "Should have exactly one non-null finish_reason (the final chunk). Got: {:?}",
            non_null_finish_reasons
        ));
    }

    // The final chunk should have finish_reason: "stop" (server-side tool execution)
    let final_finish_reason = non_null_finish_reasons[0].0.as_ref().unwrap();
    if final_finish_reason != "stop" {
        return Err(format!(
            "Final finish_reason should be 'stop' for server-side tool execution. Got: '{}'",
            final_finish_reason
        ));
    }

    // The final chunk should have an empty delta
    if !non_null_finish_reasons[0].1 {
        return Err("Final chunk should have empty delta (OpenAI spec compliance)".to_string());
    }

    Ok(())
}

/// Verifies finish_reason is "stop" when no tools are called.
#[tokio::test]
async fn test_finish_reason_stop_without_tools() {
    let client = reqwest::Client::new();

    // Request that won't trigger tool calls
    let response = send_chat_request(
        &client,
        vec![json!({
            "role": "user",
            "content": "Say hello in exactly 3 words"
        })],
        true,
    )
    .await;

    assert_eq!(response.status(), 200);

    let body = response.text().await.expect("Failed to read response");
    let lines: Vec<&str> = body.lines().collect();

    let mut final_finish_reason: Option<String> = None;
    let mut current_event_type: Option<String> = None;

    for line in lines {
        if line.is_empty() || line == "data: [DONE]" {
            continue;
        }
        // Track SSE event type
        if let Some(event) = line.strip_prefix("event: ") {
            current_event_type = Some(event.to_string());
            continue;
        }
        if let Some(chunk) = parse_data_line(line) {
            // Skip aura.* custom events
            let is_aura_event = current_event_type
                .as_ref()
                .map(|t| t.starts_with("aura."))
                .unwrap_or(false);
            current_event_type = None;
            if is_aura_event {
                continue;
            }

            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data)
                && let Some(fr) = json["choices"][0]["finish_reason"].as_str()
            {
                final_finish_reason = Some(fr.to_string());
            }
        }
    }

    assert_eq!(
        final_finish_reason,
        Some("stop".to_string()),
        "Should have finish_reason: 'stop' when no tools are called"
    );
}
