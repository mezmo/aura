#![cfg(feature = "integration-mcp")]

//! Integration tests for FastMCP structured_content support
//!
//! Verifies handling of MCP tools with `x-fastmcp-wrap-result: true` that return
//! JSON in structured_content field rather than text content.
//!

use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::parse_data_line;
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Test: Verify list_metrics tool works in streaming mode
///
/// Triggers list_metrics tool with x-fastmcp-wrap-result outputSchema.
#[tokio::test]
async fn test_list_metrics_streaming_no_json_error() {
    let client = reqwest::Client::new();
    let session_id = uuid::Uuid::new_v4().to_string();

    // Request that triggers list_metrics tool call
    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .header("X-OpenWebUI-User-Id", &session_id)
        .header("X-OpenWebUI-Session-Id", &session_id)
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [
                {
                    "role": "user",
                    "content": "Call the list_metrics tool with no arguments. Just call it and tell me what you got back."
                }
            ],
            "stream": true
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        200,
        "Expected 200 OK status for list_metrics streaming request"
    );
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream",
        "Expected SSE content type"
    );

    // Parse SSE stream
    let body = response.text().await.expect("Failed to read response body");
    let lines: Vec<&str> = body.lines().collect();

    let mut chunks = Vec::new();
    let mut found_done = false;
    let mut found_tool_call = false;
    let mut found_json_error = false;
    let mut tool_call_name = String::new();

    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line == "data: [DONE]" {
            found_done = true;
            break;
        }
        if let Some(chunk) = parse_data_line(line) {
            if chunk.data.contains("JsonError") || chunk.data.contains("EOF while parsing") {
                found_json_error = true;
            }

            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data) {
                // Check for tool calls
                if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
                    for choice in choices {
                        if let Some(tool_calls) = choice
                            .get("delta")
                            .and_then(|d| d.get("tool_calls"))
                            .and_then(|tc| tc.as_array())
                        {
                            for tool_call in tool_calls {
                                if let Some(function) = tool_call.get("function")
                                    && let Some(name) =
                                        function.get("name").and_then(|n| n.as_str())
                                    && name == "list_metrics"
                                {
                                    found_tool_call = true;
                                    tool_call_name = name.to_string();
                                }
                            }
                        }
                    }
                }
                chunks.push(json);
            }
        }
    }

    // Assertions
    assert!(
        !found_json_error,
        "❌ FAILED: JsonError found in streaming response. The structured_content fix did not work!"
    );
    assert!(
        found_done,
        "Expected [DONE] marker at end of stream (stream may have terminated early due to error)"
    );
    assert!(
        found_tool_call,
        "Expected list_metrics tool to be called by LLM"
    );
    assert_eq!(
        tool_call_name, "list_metrics",
        "Expected tool name to be 'list_metrics'"
    );
    assert!(
        chunks.len() > 5,
        "Expected multiple streaming chunks, got {}",
        chunks.len()
    );
}

/// Test: Verify non-streaming mode also works with structured_content
///
/// This ensures backward compatibility - structured_content handling works
/// in both streaming and non-streaming modes.
#[tokio::test]
async fn test_list_metrics_non_streaming() {
    let client = reqwest::Client::new();
    let session_id = uuid::Uuid::new_v4().to_string();

    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .header("X-OpenWebUI-User-Id", &session_id)
        .header("X-OpenWebUI-Session-Id", &session_id)
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [
                {
                    "role": "user",
                    "content": "Call the list_metrics tool and tell me how many metrics are available."
                }
            ],
            "stream": false
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    let result: Value = response
        .json()
        .await
        .expect("Failed to parse JSON response");

    // Verify response structure
    assert!(
        result.get("choices").is_some(),
        "Response should have choices"
    );
    let message = result["choices"][0]["message"].clone();
    assert!(
        message.get("content").is_some() || message.get("tool_calls").is_some(),
        "Message should have content or tool_calls"
    );

    // If there were tool calls, verify list_metrics was called
    if let Some(tool_calls) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
        assert!(
            tool_calls.iter().any(|tc| {
                tc.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("list_metrics")
            }),
            "Expected list_metrics tool to be called in non-streaming mode"
        );
    }
}

/// Test: Verify error handling when tool execution fails
///
/// Ensures that even with structured_content support, we properly handle
/// tool execution errors.
#[tokio::test]
async fn test_structured_content_error_handling() {
    let client = reqwest::Client::new();
    let session_id = uuid::Uuid::new_v4().to_string();

    // Request with invalid tool arguments (to trigger error)
    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .header("X-OpenWebUI-User-Id", &session_id)
        .header("X-OpenWebUI-Session-Id", &session_id)
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [
                {
                    "role": "user",
                    "content": "Try to execute a PromQL query that is intentionally malformed: 'up{invalid syntax'"
                }
            ],
            "stream": true
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        200,
        "Server should handle errors gracefully"
    );

    let body = response.text().await.expect("Failed to read response body");

    // Should complete without crashing (may contain error in content)
    assert!(
        body.contains("[DONE]"),
        "Stream should complete even with tool errors"
    );

    // Should not have JsonError (our bug)
    assert!(
        !body.contains("JsonError"),
        "Should not have JsonError even when tool fails"
    );
}
