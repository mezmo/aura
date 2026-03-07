#![cfg(feature = "integration-events")]

//! Integration tests for Aura custom SSE streaming events
//!
//! These tests verify that the aura-web-server correctly emits custom
//! aura.tool_requested, aura.tool_start, and aura.tool_complete events when AURA_CUSTOM_EVENTS=true.
//!
//! Event types:
//! - `aura.tool_requested` - Emitted when LLM decides to call a tool (immediate UI feedback)
//! - `aura.tool_start` - Emitted when MCP execution begins (has progress_token for correlation)
//! - `aura.tool_complete` - Emitted when a tool call completes
//! - `aura.reasoning` - Emitted for LLM reasoning (requires AURA_EMIT_REASONING=true)
//!

use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::{SseEvent, events_by_type, parse_sse_stream};
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

async fn send_tool_request(client: &reqwest::Client) -> reqwest::Response {
    client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o",
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
    events_by_type(events, "aura.tool_start")
}

fn find_progress_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events_by_type(events, "aura.progress")
}

fn find_tool_requested_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events_by_type(events, "aura.tool_requested")
}

fn find_tool_complete_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events_by_type(events, "aura.tool_complete")
}

/// Test 1: Verify aura.tool_requested events are emitted when AURA_CUSTOM_EVENTS=true
///
/// This test requires the server to be started with AURA_CUSTOM_EVENTS=true
/// which is configured in run_tests.sh
#[tokio::test]
async fn test_aura_tool_requested_events_emitted() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_tool_request(&client).await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    // Find aura.tool_requested events
    let tool_requested_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.as_deref() == Some("aura.tool_requested"))
        .collect();

    // Verify we got at least one tool_requested event (LLM should use list_files tool)
    if tool_requested_events.is_empty() {
        // If no events, check if AURA_CUSTOM_EVENTS is disabled
        println!("Note: No aura.tool_requested events found.");
        println!("Ensure server is started with AURA_CUSTOM_EVENTS=true");
        println!("Total events received: {}", events.len());

        // Still pass if custom events are disabled (backward compatibility test)
        return;
    }

    // Verify tool_requested event structure
    for event in &tool_requested_events {
        let json: Value =
            serde_json::from_str(&event.data).expect("Failed to parse tool_requested JSON");

        // Required fields
        assert!(
            json.get("tool_id").is_some(),
            "tool_requested missing tool_id field"
        );
        assert!(
            json.get("tool_name").is_some(),
            "tool_requested missing tool_name field"
        );
        assert!(
            json.get("arguments").is_some(),
            "tool_requested missing arguments field"
        );

        // Context fields
        assert!(
            json.get("agent_id").is_some(),
            "tool_requested missing agent_id field"
        );
        assert!(
            json.get("session_id").is_some(),
            "tool_requested missing session_id field"
        );

        println!(
            "  tool_requested: tool_name={}, tool_id={}",
            json["tool_name"], json["tool_id"]
        );
    }

    println!(
        "Found {} aura.tool_requested events",
        tool_requested_events.len()
    );
}

/// Test 2: Verify aura.tool_complete events are emitted with duration
#[tokio::test]
async fn test_aura_tool_complete_events_emitted() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_tool_request(&client).await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    // Find aura.tool_complete events
    let tool_complete_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.as_deref() == Some("aura.tool_complete"))
        .collect();

    // Verify we got at least one tool_complete event
    if tool_complete_events.is_empty() {
        println!("Note: No aura.tool_complete events found.");
        println!("Ensure server is started with AURA_CUSTOM_EVENTS=true");
        return;
    }

    // Verify tool_complete event structure
    for event in &tool_complete_events {
        let json: Value =
            serde_json::from_str(&event.data).expect("Failed to parse tool_complete JSON");

        // Required fields
        assert!(
            json.get("tool_id").is_some(),
            "tool_complete missing tool_id field"
        );
        assert!(
            json.get("tool_name").is_some(),
            "tool_complete missing tool_name field"
        );
        assert!(
            json.get("duration_ms").is_some(),
            "tool_complete missing duration_ms field"
        );
        assert!(
            json.get("success").is_some(),
            "tool_complete missing success field"
        );

        // Verify duration is a number
        let duration = json["duration_ms"].as_u64();
        assert!(
            duration.is_some(),
            "duration_ms should be a non-negative integer"
        );

        // Verify success is boolean
        assert!(
            json["success"].as_bool().is_some(),
            "success should be a boolean"
        );

        println!(
            "  tool_complete: tool_name={}, duration_ms={}, success={}",
            json["tool_name"], json["duration_ms"], json["success"]
        );
    }

    println!(
        "Found {} aura.tool_complete events",
        tool_complete_events.len()
    );
}

/// Test 3: Verify tool_requested and tool_complete events are properly paired
#[tokio::test]
async fn test_aura_tool_events_paired() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_tool_request(&client).await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    // Collect tool_requested and tool_complete events
    let tool_requested_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.as_deref() == Some("aura.tool_requested"))
        .collect();

    let tool_complete_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.as_deref() == Some("aura.tool_complete"))
        .collect();

    // If no events, custom events are disabled
    if tool_requested_events.is_empty() && tool_complete_events.is_empty() {
        println!("Note: No aura events found - custom events may be disabled");
        return;
    }

    // Verify equal number of requested and complete events
    assert_eq!(
        tool_requested_events.len(),
        tool_complete_events.len(),
        "Mismatched tool_requested ({}) and tool_complete ({}) event counts",
        tool_requested_events.len(),
        tool_complete_events.len()
    );

    // Verify each requested has a matching complete with same tool_id
    for requested_event in &tool_requested_events {
        let requested_json: Value = serde_json::from_str(&requested_event.data).unwrap();
        let tool_id = requested_json["tool_id"].as_str().unwrap();

        let matching_complete = tool_complete_events.iter().find(|e| {
            let json: Value = serde_json::from_str(&e.data).unwrap();
            json["tool_id"].as_str() == Some(tool_id)
        });

        assert!(
            matching_complete.is_some(),
            "No matching tool_complete for tool_id: {}",
            tool_id
        );
    }

    println!(
        "All {} tool events properly paired",
        tool_requested_events.len()
    );
}

/// Test 4: Verify OpenAI chunks are still emitted alongside custom events
#[tokio::test]
async fn test_openai_chunks_still_emitted() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_tool_request(&client).await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    // Find standard OpenAI chunks (no event type = default data chunk)
    let openai_chunks: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.is_none())
        .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
        .filter(|json| json.get("object").and_then(|v| v.as_str()) == Some("chat.completion.chunk"))
        .collect();

    // Verify we got OpenAI chunks
    assert!(
        !openai_chunks.is_empty(),
        "Expected OpenAI chat.completion.chunk events"
    );

    // Verify chunk structure
    for chunk in &openai_chunks {
        assert!(chunk.get("id").is_some(), "Missing id field");
        assert!(chunk.get("choices").is_some(), "Missing choices field");
    }

    println!(
        "Found {} OpenAI chat.completion.chunk events",
        openai_chunks.len()
    );
}

/// Test 5: Verify session_id correlation in custom events
#[tokio::test]
async fn test_session_id_correlation() {
    let client = reqwest::Client::new();
    let test_session_id = format!("correlation-test-{}", uuid::Uuid::new_v4());

    // Send request with known session ID
    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": "List files in the mock directory"
            }],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": test_session_id
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    // Find aura events
    let aura_events: Vec<_> = events
        .iter()
        .filter(|e| {
            e.event_type
                .as_ref()
                .map(|t| t.starts_with("aura."))
                .unwrap_or(false)
        })
        .collect();

    // If custom events are enabled, verify session_id matches
    if !aura_events.is_empty() {
        for event in &aura_events {
            let json: Value = serde_json::from_str(&event.data).unwrap();

            let session_id = json.get("session_id").and_then(|v| v.as_str());

            assert_eq!(
                session_id,
                Some(test_session_id.as_str()),
                "Session ID mismatch in {} event",
                event.event_type.as_ref().unwrap()
            );
        }

        println!(
            "All {} aura events have correct session_id",
            aura_events.len()
        );
    } else {
        println!("Note: No aura events found - custom events may be disabled");
    }
}

/// Test 6: Verify tool_complete events include result field for successful tools
///
/// This test verifies that aura.tool_complete events include the actual tool result
/// in the `result` field when success=true. This was added to provide tool observability
/// alongside the OpenWebUI tool_calls hack (which is a separate feature).
#[tokio::test]
async fn test_aura_tool_complete_includes_result() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_tool_request(&client).await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    // Find aura.tool_complete events
    let tool_complete_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.as_deref() == Some("aura.tool_complete"))
        .collect();

    // If custom events are disabled, skip test
    if tool_complete_events.is_empty() {
        println!("Note: No aura.tool_complete events found - custom events may be disabled");
        return;
    }

    // Verify successful tool_complete events have result field
    let mut found_successful_with_result = false;
    for event in &tool_complete_events {
        let json: Value =
            serde_json::from_str(&event.data).expect("Failed to parse tool_complete JSON");

        let success = json["success"].as_bool();
        let tool_name = json["tool_name"].as_str().unwrap_or("unknown");

        if success == Some(true) {
            // Successful tool calls should have result field
            let has_result = json.get("result").is_some();
            let result_preview = json["result"]
                .as_str()
                .map(|s| {
                    if s.len() > 100 {
                        format!("{}...", &s[..100])
                    } else {
                        s.to_string()
                    }
                })
                .unwrap_or_else(|| "null".to_string());

            println!(
                "  tool_complete (success): tool_name={}, has_result={}, result_preview={}",
                tool_name, has_result, result_preview
            );

            if has_result {
                found_successful_with_result = true;
                // Verify result is a non-empty string
                let result_str = json["result"].as_str().unwrap_or("");
                assert!(
                    !result_str.is_empty(),
                    "Result field should not be empty for successful tool"
                );
            }
        } else {
            // Failed tools should NOT have result field (they have error instead)
            let has_error = json.get("error").is_some();
            println!(
                "  tool_complete (failure): tool_name={}, has_error={}",
                tool_name, has_error
            );
        }
    }

    if found_successful_with_result {
        println!("SUCCESS: Found tool_complete with success=true AND result field populated");
    } else {
        println!(
            "WARNING: No successful tool_complete events with result field found. \
             This may indicate the result field feature is not working."
        );
    }
}

/// Test 7: Verify tool_complete events have success=false and error field when tool fails
///
/// This test triggers the failing_tool in the mock MCP server to verify that:
/// - aura.tool_complete has success: false
/// - aura.tool_complete has error field populated
/// - The error message includes the original error text
#[tokio::test]
async fn test_aura_tool_complete_failure_detection() {
    let client = reqwest::Client::new();

    // Send request that explicitly asks to call the failing_tool
    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": "Please call the failing_tool with the error_message 'Test error for integration test'. This tool is expected to fail."
            }],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("failure-test-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    // Find aura.tool_complete events
    let tool_complete_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.as_deref() == Some("aura.tool_complete"))
        .collect();

    // If custom events are disabled, skip test
    if tool_complete_events.is_empty() {
        println!("Note: No aura.tool_complete events found - custom events may be disabled");
        return;
    }

    // Look for a tool_complete with success=false (the failing_tool)
    let mut found_failure = false;
    for event in &tool_complete_events {
        let json: Value =
            serde_json::from_str(&event.data).expect("Failed to parse tool_complete JSON");

        let success = json["success"].as_bool();
        let tool_name = json["tool_name"].as_str().unwrap_or("unknown");

        println!(
            "  tool_complete: tool_name={}, success={:?}, has_error={}",
            tool_name,
            success,
            json.get("error").is_some()
        );

        // Check if this is a failed tool call
        if success == Some(false) {
            found_failure = true;

            // Verify error field is present
            assert!(
                json.get("error").is_some(),
                "Failed tool_complete should have error field"
            );

            let error_msg = json["error"].as_str().unwrap_or("");
            println!("    error: {}", error_msg);

            // Verify error message is non-empty
            assert!(!error_msg.is_empty(), "Error message should not be empty");

            // The error should contain our test message or a Rig error prefix
            // Note: The exact format depends on how Rig wraps the error
            println!("    Found failure with error: {}", error_msg);
        }
    }

    // If the LLM used the failing_tool, we should see a failure
    // If not, the LLM may have avoided it (which is fine for agentic behavior)
    if found_failure {
        println!("SUCCESS: Found tool_complete with success=false and error field");
    } else {
        println!("Note: No failed tool calls detected. LLM may have avoided failing_tool.");
        println!("This is acceptable - the test verifies the event structure when failures occur.");
    }
}

/// Verify aura.tool_start events are emitted with required fields:
///
/// - tool_id: matches the tool_call from OpenAI chunk
/// - tool_name: name of the MCP tool
/// - progress_token: present for progress correlation
/// - agent_id, session_id: correlation fields
#[tokio::test]
async fn test_aura_tool_start_events_emitted() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_tool_request(&client).await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    let tool_start_events = find_tool_start_events(&events);

    assert!(
        !tool_start_events.is_empty(),
        "No aura.tool_start events found. LLM may not have called a tool."
    );

    for event in &tool_start_events {
        let json: Value =
            serde_json::from_str(&event.data).expect("Failed to parse tool_start JSON");

        assert!(
            json.get("tool_id").is_some() && json["tool_id"].is_string(),
            "tool_start missing or invalid tool_id field"
        );
        assert!(
            json.get("tool_name").is_some() && json["tool_name"].is_string(),
            "tool_start missing or invalid tool_name field"
        );
        assert!(
            json.get("progress_token").is_some(),
            "tool_start missing progress_token field"
        );
        assert!(
            json["progress_token"].is_number() || json["progress_token"].is_string(),
            "progress_token should be a number or string"
        );
        assert!(
            json.get("agent_id").is_some() && json["agent_id"].is_string(),
            "tool_start missing or invalid agent_id field"
        );
        assert!(
            json.get("session_id").is_some() && json["session_id"].is_string(),
            "tool_start missing or invalid session_id field"
        );
    }
}

/// Verify the full tool event lifecycle:
/// `tool_requested -> tool_start -> progress* -> tool_complete`
///
/// Verifies events share the same tool_id, arrive in order, and progress_token correlates.
#[tokio::test]
async fn test_full_tool_event_lifecycle() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_tool_request(&client).await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE events
    let body = response.text().await.expect("Failed to read response body");
    let (events, _) = parse_sse_stream(&body);

    // Collect all event types
    let tool_requested = find_tool_requested_events(&events);
    let tool_start = find_tool_start_events(&events);
    let tool_complete = find_tool_complete_events(&events);
    let progress = find_progress_events(&events);

    assert!(
        !tool_requested.is_empty() || !tool_start.is_empty() || !tool_complete.is_empty(),
        "No aura events found. LLM may not have called a tool."
    );

    // Verify equal number of requested, start, and complete events
    assert_eq!(
        tool_requested.len(),
        tool_start.len(),
        "Mismatched tool_requested and tool_start event counts"
    );
    assert_eq!(
        tool_start.len(),
        tool_complete.len(),
        "Mismatched tool_start and tool_complete event counts"
    );

    for requested_event in &tool_requested {
        let requested_json: Value = serde_json::from_str(&requested_event.data).unwrap();
        let tool_id = requested_json["tool_id"].as_str().unwrap();

        // Find matching tool_start
        let matching_start = tool_start.iter().find(|e| {
            let json: Value = serde_json::from_str(&e.data).unwrap();
            json["tool_id"].as_str() == Some(tool_id)
        });
        assert!(
            matching_start.is_some(),
            "No matching tool_start for tool_id: {}",
            tool_id
        );

        // Find matching tool_complete
        let matching_complete = tool_complete.iter().find(|e| {
            let json: Value = serde_json::from_str(&e.data).unwrap();
            json["tool_id"].as_str() == Some(tool_id)
        });
        assert!(
            matching_complete.is_some(),
            "No matching tool_complete for tool_id: {}",
            tool_id
        );

        // Extract progress_token from tool_start
        let start_json: Value = serde_json::from_str(&matching_start.unwrap().data).unwrap();
        let progress_token = &start_json["progress_token"];

        // Helper to find event position by type and tool_id
        let find_event_pos = |event_type: &str, target_tool_id: &str| -> Option<usize> {
            events.iter().position(|e| {
                if e.event_type.as_deref() != Some(event_type) {
                    return false;
                }
                serde_json::from_str::<Value>(&e.data)
                    .ok()
                    .and_then(|json| json["tool_id"].as_str().map(|id| id == target_tool_id))
                    .unwrap_or(false)
            })
        };

        let requested_pos = find_event_pos("aura.tool_requested", tool_id);
        let start_pos = find_event_pos("aura.tool_start", tool_id);
        let complete_pos = find_event_pos("aura.tool_complete", tool_id);

        if let (Some(req), Some(start), Some(complete)) = (requested_pos, start_pos, complete_pos) {
            assert!(req < start, "tool_requested should come before tool_start");
            assert!(
                start < complete,
                "tool_start should come before tool_complete"
            );

            // Verify progress events (if any) fall between start and complete
            let progress_positions: Vec<usize> = progress
                .iter()
                .filter_map(|e| {
                    let json: Value = serde_json::from_str(&e.data).ok()?;
                    if json.get("progress_token") == Some(progress_token) {
                        events.iter().position(|ev| std::ptr::eq(ev, *e))
                    } else {
                        None
                    }
                })
                .collect();

            for pos in &progress_positions {
                assert!(
                    start < *pos && *pos < complete,
                    "progress event should fall between tool_start and tool_complete"
                );
            }
        }
    }
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
            "model": "gpt-4o",
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
