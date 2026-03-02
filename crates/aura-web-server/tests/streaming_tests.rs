#![cfg(feature = "integration-streaming")]

//! Integration tests for SSE streaming functionality
//!
//! These tests verify that the aura-web-server correctly implements
//! OpenAI-compatible Server-Sent Events streaming for chat completions.
//!

use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::{extract_openai_chunks, parse_data_line};
use serde_json::{json, Value};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Helper to send a chat completion request
async fn send_chat_request(
    client: &reqwest::Client,
    messages: Vec<Value>,
    stream: bool,
) -> reqwest::Response {
    client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o",
            "messages": messages,
            "stream": stream,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("test-session-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request")
}

/// Test 1: Basic Streaming - Verify token-by-token delivery
#[tokio::test]
async fn test_basic_streaming() {
    let client = reqwest::Client::new();

    // Send streaming request
    let response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "Count from 1 to 5, with each number on its own line."})],
        true,
    )
    .await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream",
        "Expected SSE content type"
    );

    // Read and parse SSE chunks (filtering out aura.* custom events)
    let body = response.text().await.expect("Failed to read response body");
    let (chunks, found_done) = extract_openai_chunks(&body);

    // Verify streaming behavior
    assert!(
        chunks.len() > 1,
        "Expected multiple chunks, got {}",
        chunks.len()
    );
    assert!(found_done, "Expected [DONE] marker at end of stream");

    // Verify chunk structure
    for (i, chunk) in chunks.iter().enumerate() {
        assert!(chunk.get("id").is_some(), "Chunk {i} missing id field");
        assert_eq!(
            chunk.get("object").and_then(|v| v.as_str()),
            Some("chat.completion.chunk"),
            "Chunk {i} has wrong object type"
        );
        assert!(
            chunk.get("choices").and_then(|v| v.as_array()).is_some(),
            "Chunk {i} missing choices array"
        );
    }

    // Verify we got content
    let has_content = chunks.iter().any(|chunk| {
        chunk["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("delta"))
            .and_then(|delta| delta.get("content"))
            .and_then(|content| content.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    });
    assert!(has_content, "Expected at least one chunk with content");

    println!(
        "✓ Basic streaming test passed: {} chunks received",
        chunks.len()
    );
}

/// Test 2: Streaming vs Non-Streaming - Compare outputs
#[tokio::test]
async fn test_streaming_vs_non_streaming() {
    let client = reqwest::Client::new();
    let query = "Say hello in exactly 3 words.";

    // Non-streaming request
    let non_streaming_response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": query})],
        false,
    )
    .await;

    let non_streaming_json: Value = non_streaming_response
        .json()
        .await
        .expect("Failed to parse non-streaming response");

    let non_streaming_content = non_streaming_json["choices"][0]["message"]["content"]
        .as_str()
        .expect("Non-streaming response missing content");

    // Streaming request
    let streaming_response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": query})],
        true,
    )
    .await;

    let body = streaming_response
        .text()
        .await
        .expect("Failed to read streaming response body");

    // Parse streaming chunks and reconstruct content (filtering out aura.* events)
    let mut streaming_content = String::new();
    let mut streaming_final_chunk: Option<Value> = None;
    let mut current_event_type: Option<String> = None;
    for line in body.lines() {
        // Track SSE event type
        if let Some(event) = line.strip_prefix("event: ") {
            current_event_type = Some(event.to_string());
            continue;
        }
        if line == "data: [DONE]" {
            break;
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
                if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                    streaming_content.push_str(content);
                }
                // Capture final chunk (indicated by having populated finish_reason property)
                if json["choices"][0]["finish_reason"].as_str().is_some() {
                    streaming_final_chunk = Some(json);
                }
            }
        }
    }

    // Verify streaming produced content
    assert!(
        !streaming_content.is_empty(),
        "Streaming produced no content"
    );

    // Verify both modes return usage (LOG-22932)
    assert!(
        non_streaming_json.get("usage").is_some(),
        "Non-streaming response should include usage field"
    );

    let streaming_final = streaming_final_chunk.expect("Should have received final chunk");
    assert!(
        streaming_final.get("usage").is_some(),
        "Streaming final chunk should include usage field"
    );

    // Note: Content might differ slightly due to LLM non-determinism,
    // but both should be valid responses
    println!("✓ Non-streaming: {non_streaming_content}");
    println!("✓ Streaming: {streaming_content}");
    println!("✓ Both modes include usage field");
    println!("✓ Both requests completed successfully");
}

/// Test 3: Streaming Multi-Turn - Chat history preservation
#[tokio::test]
async fn test_streaming_multi_turn() {
    let client = reqwest::Client::new();

    // Turn 1: Introduce myself
    let turn1_response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "My name is Alice."})],
        true,
    )
    .await;

    let turn1_body = turn1_response
        .text()
        .await
        .expect("Failed to read turn 1 response");

    let mut turn1_content = String::new();
    for line in turn1_body.lines() {
        if let Some(chunk) = parse_data_line(line) {
            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data) {
                if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                    turn1_content.push_str(content);
                }
            }
        }
    }

    assert!(!turn1_content.is_empty(), "Turn 1 produced no content");

    // Turn 2: Ask about my name (requires history)
    let turn2_response = send_chat_request(
        &client,
        vec![
            json!({"role": "user", "content": "My name is Alice."}),
            json!({"role": "assistant", "content": turn1_content}),
            json!({"role": "user", "content": "What is my name?"}),
        ],
        true,
    )
    .await;

    let turn2_body = turn2_response
        .text()
        .await
        .expect("Failed to read turn 2 response");

    let mut turn2_content = String::new();
    for line in turn2_body.lines() {
        if let Some(chunk) = parse_data_line(line) {
            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data) {
                if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                    turn2_content.push_str(content);
                }
            }
        }
    }

    // Verify turn 2 remembers the name
    let turn2_lower = turn2_content.to_lowercase();
    assert!(
        turn2_lower.contains("alice"),
        "Turn 2 should remember the name 'Alice', got: {turn2_content}"
    );

    println!("✓ Multi-turn streaming test passed");
    println!("  Turn 1: {turn1_content}");
    println!("  Turn 2: {turn2_content}");
}

/// Test 5: Error Handling - Verify proper error SSE format
#[tokio::test]
async fn test_streaming_error_handling() {
    let client = reqwest::Client::new();

    // Send malformed request (empty messages array)
    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o",
            "messages": [],  // Invalid: empty messages
            "stream": true
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    // Should get error response
    assert!(
        response.status().is_client_error() || response.status().is_server_error(),
        "Expected error status, got {}",
        response.status()
    );

    println!("✓ Error handling test passed: Invalid request properly rejected");
}

/// Test 6: Streaming with Tool Execution - THE CRITICAL BUG FIX
///
/// This test verifies the PRIMARY functionality enabled by rig 0.22.0 upgrade:
/// Tools now execute during streaming mode via `.multi_turn()`.
///
/// Before rig 0.22.0: Tools would NOT execute in streaming mode (known limitation)
/// After rig 0.22.0: Tools execute and results are included in streaming response
#[tokio::test]
async fn test_streaming_with_tool_calls() {
    let client = reqwest::Client::new();

    // Request that should trigger filesystem tool execution
    let response = send_chat_request(
        &client,
        vec![json!({
            "role": "user",
            "content": "Please list the files in the current directory. I need to see what configuration files are available."
        })],
        true,
    )
    .await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream",
        "Expected SSE content type"
    );

    // Read and parse SSE chunks
    let body = response.text().await.expect("Failed to read response body");
    let lines: Vec<&str> = body.lines().collect();

    let mut chunks = Vec::new();
    let mut found_done = false;
    let mut combined_content = String::new();
    let mut tool_call_chunks = Vec::new();

    let mut current_event_type: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // Track SSE event type
        if let Some(event) = line.strip_prefix("event: ") {
            current_event_type = Some(event.to_string());
            continue;
        }
        if line == "data: [DONE]" {
            found_done = true;
            break;
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
                // Collect all content from stream
                if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                    combined_content.push_str(content);
                }

                // PHASE 2: Collect tool_calls chunks
                if let Some(_tool_calls) = json["choices"][0]["delta"]["tool_calls"].as_array() {
                    tool_call_chunks.push(json.clone());
                }

                chunks.push(json);
            }
        }
    }

    // Verify streaming behavior
    assert!(
        chunks.len() > 1,
        "Expected multiple chunks, got {}",
        chunks.len()
    );
    assert!(found_done, "Expected [DONE] marker at end of stream");

    // PHASE 1: Verify tool results are incorporated in response content
    // The mock MCP server's list_files tool returns "file1.txt", "file2.txt", "file3.txt"
    // If tools executed correctly, these filenames should appear in the final response
    let has_tool_results = combined_content.contains("file1.txt")
        || combined_content.contains("file2.txt")
        || combined_content.contains("file3.txt")
        || combined_content.to_lowercase().contains("config")
        || combined_content.to_lowercase().contains("cargo");

    assert!(
        has_tool_results,
        "Tool results should be incorporated in response content. \
         Expected references to files or directories. Got: {combined_content}"
    );

    // Verify we got meaningful content
    assert!(
        !combined_content.trim().is_empty(),
        "Expected non-empty content after tool execution"
    );

    // PHASE 2: Verify OpenAI-compatible tool_calls streaming format
    // ✅ FIXED with patched rig-core v0.22.0
    // Rig now correctly emits ToolCall events during multi-turn streaming
    assert!(
        !tool_call_chunks.is_empty(),
        "Expected at least one tool_calls chunk in stream. Got {} total chunks, but no tool_calls chunks.",
        chunks.len()
    );

    // Verify tool_calls chunk structure
    for (i, chunk) in tool_call_chunks.iter().enumerate() {
        let tool_calls = chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .expect("tool_calls should be an array");

        assert!(
            !tool_calls.is_empty(),
            "tool_calls array should not be empty in chunk {i}"
        );

        for (j, tool_call) in tool_calls.iter().enumerate() {
            // Verify required fields
            assert!(
                tool_call.get("index").is_some(),
                "Tool call {j} in chunk {i} missing 'index' field"
            );
            assert!(
                tool_call.get("id").and_then(|v| v.as_str()).is_some(),
                "Tool call {j} in chunk {i} missing 'id' field"
            );
            assert_eq!(
                tool_call.get("type").and_then(|v| v.as_str()),
                Some("function"),
                "Tool call {j} in chunk {i} should have type 'function'"
            );

            // Verify function object
            let function = tool_call
                .get("function")
                .expect("Tool call should have 'function' field");
            assert!(
                function.get("name").and_then(|v| v.as_str()).is_some(),
                "Function in tool call {j} chunk {i} missing 'name' field"
            );
            assert!(
                function.get("arguments").and_then(|v| v.as_str()).is_some(),
                "Function in tool call {j} chunk {i} missing 'arguments' field"
            );
        }
    }

    println!("✓ Streaming with tool execution test passed (PHASE 1 & 2 COMPLETE)");
    println!("  - Total chunks: {}", chunks.len());
    println!("  - Tool calls chunks: {}", tool_call_chunks.len());
    println!("  - Tool results verified in content: ✓");
    println!("  - Tool calls streamed in delta format: ✓");
}

/// Test 8: Multiple Sequential Tool Calls Have Unique Tool Indices
///
/// This test verifies the BUG FIX for tool call index concatenation (Session 31 + 33):
/// - Session 31: Fixed all tool calls having index=0 → unique indices (0, 1, 2, ...)
/// - Session 33: Added same-index tool results → each tool call produces 2 deltas with same index
///
/// Expected behavior: Each tool produces 2 deltas with SAME index, but DIFFERENT tools have different indices
/// Example: 3 tools → 6 deltas with indices [0, 0, 1, 1, 2, 2]
///
/// Uses chain_tool to trigger multiple sequential tool calls in a single request.
#[tokio::test]
async fn test_multiple_sequential_tool_calls_unique_indices() {
    let client = reqwest::Client::new();

    // Use chain_tool to trigger multiple sequential tool calls
    let response = send_chat_request(
        &client,
        vec![json!({
            "role": "user",
            "content": "Please call chain_tool with steps=2, then follow its instructions exactly to complete the chain."
        })],
        true,
    )
    .await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream",
        "Expected SSE content type"
    );

    // Read and parse SSE chunks
    let body = response.text().await.expect("Failed to read response body");
    let lines: Vec<&str> = body.lines().collect();

    let mut tool_call_chunks = Vec::new();
    let mut all_tool_indices = Vec::new();
    let mut all_tool_names = Vec::new();

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
                // Collect tool_calls chunks
                if let Some(tool_calls) = json["choices"][0]["delta"]["tool_calls"].as_array() {
                    for tool_call in tool_calls {
                        if let Some(index) = tool_call["index"].as_u64() {
                            all_tool_indices.push(index);
                        }
                        if let Some(name) = tool_call["function"]["name"].as_str() {
                            if !name.is_empty() {
                                // Session 33: Second delta has empty name
                                all_tool_names.push(name.to_string());
                            }
                        }
                        tool_call_chunks.push(tool_call.clone());
                    }
                }
            }
        }
    }

    // Verify we got multiple tool call deltas (2 deltas per tool due to Session 33)
    assert!(
        tool_call_chunks.len() >= 4, // At least 2 tools × 2 deltas each
        "Expected at least 4 deltas (2 tools × 2 deltas), got {}. \
         Tool names: {:?}",
        tool_call_chunks.len(),
        all_tool_names
    );

    // Session 33: Each tool produces 2 deltas with same index
    // So unique_indices should be HALF of total deltas
    let unique_indices: std::collections::HashSet<_> = all_tool_indices.iter().collect();
    let expected_unique = all_tool_indices.len() / 2; // Session 33: pairs of same-index deltas

    assert_eq!(
        unique_indices.len(),
        expected_unique,
        "Expected {} unique indices (half of {} total deltas due to Session 33 same-index pairs). \
         Got {} unique indices. \
         Indices: {:?}",
        expected_unique,
        all_tool_indices.len(),
        unique_indices.len(),
        all_tool_indices
    );

    // Verify each index appears exactly twice (Session 33: call + result use same index)
    let mut index_counts = std::collections::HashMap::new();
    for &index in &all_tool_indices {
        *index_counts.entry(index).or_insert(0) += 1;
    }

    for (&index, &count) in &index_counts {
        assert_eq!(
            count, 2,
            "Index {} should appear exactly twice (call + result). Got {} occurrences. \
             All indices: {:?}",
            index, count, all_tool_indices
        );
    }

    println!("✓ Multiple sequential tool calls have correct index pattern (Session 31 + 33)");
    println!("  - Total deltas: {}", tool_call_chunks.len());
    println!("  - Tool names: {:?}", all_tool_names);
    println!("  - Indices: {:?}", all_tool_indices);
    println!("  - Unique indices: {}", unique_indices.len());
    println!("  - ✅ Each tool has unique index, each index appears twice (call + result)");
}

/// Test 9: Tool Names Not Concatenated in Streaming (Session 31 + 33)
///
/// This test specifically verifies that tool names remain separate and are not
/// concatenated when multiple tools are called sequentially.
///
/// Session 31 fix: Unique indices prevent concatenation
/// Session 33 fix: Second delta has empty name to avoid duplication during accumulation
///
/// Before Session 31: "chain_toolmock_toollist_files" (all index=0, concatenated)
/// After Session 31+33: ["chain_tool", "mock_tool", "list_files"] (separate, with empty names in result deltas)
#[tokio::test]
async fn test_tool_names_not_concatenated_in_streaming() {
    let client = reqwest::Client::new();

    // Trigger multi-turn with explicit tool sequence
    let response = send_chat_request(
        &client,
        vec![json!({
            "role": "user",
            "content": "First call mock_tool with message 'step1', then call list_files with path '/tmp', then call mock_tool again with message 'step3'."
        })],
        true,
    )
    .await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Read and parse SSE chunks
    let body = response.text().await.expect("Failed to read response body");
    let lines: Vec<&str> = body.lines().collect();

    let mut tool_names = Vec::new();

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
                if let Some(tool_calls) = json["choices"][0]["delta"]["tool_calls"].as_array() {
                    for tool_call in tool_calls {
                        if let Some(name) = tool_call["function"]["name"].as_str() {
                            // Session 33: Skip empty names (second delta with results)
                            if !name.is_empty() {
                                tool_names.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Verify we got multiple tool calls
    assert!(
        tool_names.len() >= 2,
        "Expected at least 2 tool calls, got {}. Tool names: {:?}",
        tool_names.len(),
        tool_names
    );

    // Verify no concatenation - each name should be a valid tool name
    let valid_tool_names = [
        "mock_tool",
        "list_files",
        "chain_tool",
        "echo_headers",
        "list_metrics",
    ];
    for (i, name) in tool_names.iter().enumerate() {
        let is_valid = valid_tool_names.contains(&name.as_str());
        assert!(
            is_valid,
            "❌ BUG: Tool call {i} has invalid/concatenated name: '{name}'. \
             Expected one of: {:?}. All names: {:?}",
            valid_tool_names, tool_names
        );
    }

    // Verify tool names are distinct (not all the same)
    let unique_names: std::collections::HashSet<_> = tool_names.iter().collect();
    assert!(
        unique_names.len() >= 2 || tool_names.len() >= 3,
        "Expected diverse tool calls (at least 2 different tools or 3+ total calls), \
         got {} unique names out of {} total. Names: {:?}",
        unique_names.len(),
        tool_names.len(),
        tool_names
    );

    println!("✓ Tool names not concatenated in streaming test passed (Session 31 + 33)");
    println!("  - Total tool calls: {}", tool_names.len());
    println!("  - Tool names: {:?}", tool_names);
    println!("  - Unique tools: {}", unique_names.len());
    println!("  - ✅ All names valid and separate (no concatenation, no duplication)");
}

/// Test 10: Tool Results Visible in Streaming (Same-Index Accumulation)
///
/// Verifies that tool execution results are emitted to the SSE stream using
/// OpenWebUI's accumulation pattern: same-index tool_calls deltas.
///
/// **Problem (Session 32)**: Tool results were only used internally by the LLM.
/// Clients received tool calls (with arguments) but never saw the actual results.
///
/// **Session 32 Fix**: Rig fork emits ToolResult events, sent as content chunks.
///
/// **Problem (Session 33)**: "View Results" showed arguments instead of results.
/// Content chunks cluttered conversation.
///
/// **Session 33 Fix**: Send two tool_calls deltas with SAME index:
/// - First delta: tool call with empty arguments ("")
/// - Second delta: tool result as arguments field (same index)
/// - OpenWebUI accumulates: "" + result_json = valid JSON result ✅
#[tokio::test]
async fn test_tool_results_visible_in_streaming() {
    let client = reqwest::Client::new();

    // Send request that triggers tool execution
    let response = send_chat_request(
        &client,
        vec![json!({
            "role": "user",
            "content": "Call mock_tool with message 'hi'"
        })],
        true,
    )
    .await;

    assert_eq!(response.status(), 200);

    // Parse SSE stream
    let body = response.text().await.expect("Failed to read response");
    let lines: Vec<&str> = body.lines().collect();

    let mut tool_call_deltas: Vec<Value> = Vec::new();

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
                // Collect all tool_calls deltas
                if let Some(tool_calls) = json["choices"][0]["delta"]["tool_calls"].as_array() {
                    for tool_call in tool_calls {
                        tool_call_deltas.push(tool_call.clone());
                    }
                }
            }
        }
    }

    // Verify we have at least 2 deltas (first with empty args, second with result)
    assert!(
        tool_call_deltas.len() >= 2,
        "Should have at least 2 tool_calls deltas (call + result). Got: {}",
        tool_call_deltas.len()
    );

    // First delta: tool call with empty arguments
    let first_delta = &tool_call_deltas[0];
    assert!(
        first_delta["function"]["name"].as_str().is_some(),
        "First delta should have tool name"
    );
    let first_args = first_delta["function"]["arguments"]
        .as_str()
        .unwrap_or("not found");
    assert_eq!(
        first_args, "",
        "First delta should have EMPTY arguments (Session 33 fix). Got: '{}'",
        first_args
    );

    // Second delta: tool result with same index
    let second_delta = &tool_call_deltas[1];
    let second_index = second_delta["index"].as_u64();
    let first_index = first_delta["index"].as_u64();
    assert_eq!(
        second_index, first_index,
        "Second delta should have SAME index as first (Session 33 fix)"
    );

    let result_args = second_delta["function"]["arguments"]
        .as_str()
        .expect("Second delta should have result in arguments field");

    assert!(
        result_args.contains("Mock tool executed successfully"),
        "Tool result should contain actual result from mock_tool. Got: {}",
        result_args
    );
    assert!(
        result_args.contains("hi"),
        "Tool result should contain the message argument. Got: {}",
        result_args
    );

    println!("✓ Tool results visible in streaming test passed (Session 33)");
    println!("  - Tool call found: ✓");
    println!("  - First delta has empty arguments: ✓");
    println!("  - Second delta has same index: ✓");
    println!("  - Result in arguments field: ✓");
    println!(
        "  - Result preview: {}",
        result_args.chars().take(100).collect::<String>()
    );
    println!("  - ✅ OpenWebUI 'View Results' format validated");
}

/// Verify session metadata in HTTP response headers (X-Chat-Session-Id, etc.)
#[tokio::test]
async fn test_streaming_response_headers() {
    let client = reqwest::Client::new();
    let session_id = format!("header-test-{}", uuid::Uuid::new_v4());

    // Send streaming request with explicit session ID
    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Say hello"}],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": session_id.clone()
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Verify SSE content type
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream",
        "Expected SSE content type"
    );

    // Verify session metadata headers (extract to owned strings before consuming response)
    let session_header = response
        .headers()
        .get("x-chat-session-id")
        .expect("Missing X-Chat-Session-Id header")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        session_header, session_id,
        "X-Chat-Session-Id should match request session ID"
    );

    // Consume the response body to complete the stream
    let _ = response.text().await;

    println!("✓ Streaming response headers test passed");
    println!("  - X-Chat-Session-Id: {}", session_id);
}

/// Verifies tools work across consecutive requests with fresh agent builds.
///
/// Each request builds a fresh agent with fresh MCP tool discovery.
/// Ensures that consecutive requests both successfully discover and execute tools.
#[tokio::test]
async fn test_consecutive_requests_tool_execution() {
    // Longer timeout - needs tool discovery + tool execution twice
    let test_timeout = Duration::from_secs(60);
    let client = reqwest::Client::new();

    let session_id_1 = format!("test-session-1-{}", uuid::Uuid::new_v4());
    let session_id_2 = format!("test-session-2-{}", uuid::Uuid::new_v4());

    // Request 1: Fresh agent build with tool discovery
    println!("Request 1: Fresh agent, tool should work...");
    let response1 = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": "Please list the files in the current directory."
            }],
            "stream": true,
            "metadata": {
                "chat_session_id": session_id_1
            }
        }))
        .timeout(test_timeout)
        .send()
        .await
        .expect("Failed to send request 1");

    assert_eq!(response1.status(), 200, "Request 1 should succeed");

    // Parse Request 1 and verify tool execution
    let body1 = response1.text().await.expect("Failed to read response 1");
    let mut tool_call_chunks_1 = Vec::new();
    let mut combined_content_1 = String::new();

    for line in body1.lines() {
        if let Some(chunk) = parse_data_line(line) {
            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data) {
                if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                    combined_content_1.push_str(content);
                }
                if json["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .is_some()
                {
                    tool_call_chunks_1.push(json.clone());
                }
            }
        }
    }

    // Verify Request 1 had tool execution
    assert!(
        !tool_call_chunks_1.is_empty(),
        "Request 1: Expected tool_calls chunks in stream. \
         This indicates tools were not discovered."
    );
    // Check for either mock server files (file1.txt, file2.txt, file3.txt)
    // or actual project files (.rs, .toml, cargo, config)
    let has_tool_results_1 = combined_content_1.to_lowercase().contains("cargo")
        || combined_content_1.to_lowercase().contains("config")
        || combined_content_1.contains(".rs")
        || combined_content_1.contains(".toml")
        || combined_content_1.contains("file1.txt") // Mock server output
        || combined_content_1.contains("file2.txt")
        || combined_content_1.contains("file3.txt");
    assert!(
        has_tool_results_1,
        "Request 1: Tool results should be in content. Got: {}",
        combined_content_1
    );

    println!("  - Tool calls chunks: {}", tool_call_chunks_1.len());

    // Request 2: Another fresh agent build with tool discovery
    println!("Request 2: Fresh agent, tool should still work...");
    let response2 = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": "Please list the files in the current directory."
            }],
            "stream": true,
            "metadata": {
                "chat_session_id": session_id_2
            }
        }))
        .timeout(test_timeout)
        .send()
        .await
        .expect("Failed to send request 2");

    assert_eq!(response2.status(), 200, "Request 2 should succeed");

    // Parse Request 2 and verify tool execution
    let body2 = response2.text().await.expect("Failed to read response 2");
    let mut tool_call_chunks_2 = Vec::new();
    let mut combined_content_2 = String::new();

    for line in body2.lines() {
        if let Some(chunk) = parse_data_line(line) {
            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data) {
                if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                    combined_content_2.push_str(content);
                }
                if json["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .is_some()
                {
                    tool_call_chunks_2.push(json.clone());
                }
            }
        }
    }

    assert!(
        !tool_call_chunks_2.is_empty(),
        "Request 2: Expected tool_calls chunks in stream. \
         Tools should work on consecutive requests with fresh agent builds."
    );

    // Accept either mock server or real project file output
    let has_tool_results_2 = combined_content_2.to_lowercase().contains("cargo")
        || combined_content_2.to_lowercase().contains("config")
        || combined_content_2.contains(".rs")
        || combined_content_2.contains(".toml")
        || combined_content_2.contains("file1.txt")  // Mock server output
        || combined_content_2.contains("file2.txt")
        || combined_content_2.contains("file3.txt");
    assert!(
        has_tool_results_2,
        "Request 2: Tool results should be in content. Got: {}",
        combined_content_2
    );

    println!("  - Tool calls chunks: {}", tool_call_chunks_2.len());
    println!("Consecutive requests tool execution test PASSED");
}

/// Test: Non-streaming response includes token usage (LOG-22932)
///
/// Verifies that the non-streaming API returns valid usage statistics
/// including prompt_tokens, completion_tokens, and total_tokens.
#[tokio::test]
async fn test_non_streaming_includes_usage() {
    let client = reqwest::Client::new();

    let response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "Say hello."})],
        false, // Non-streaming
    )
    .await;

    assert_eq!(response.status(), 200);

    let json: Value = response.json().await.expect("Failed to parse response");

    // Verify usage field is present
    let usage = json
        .get("usage")
        .expect("Response should include usage field");

    // Verify all required fields exist and are positive
    let prompt_tokens = usage["prompt_tokens"]
        .as_u64()
        .expect("prompt_tokens should be u64");
    let completion_tokens = usage["completion_tokens"]
        .as_u64()
        .expect("completion_tokens should be u64");
    let total_tokens = usage["total_tokens"]
        .as_u64()
        .expect("total_tokens should be u64");

    assert!(prompt_tokens > 0, "prompt_tokens should be > 0");
    assert!(completion_tokens > 0, "completion_tokens should be > 0");
    assert_eq!(
        total_tokens,
        prompt_tokens + completion_tokens,
        "total_tokens should equal prompt + completion"
    );

    println!("✓ Non-streaming usage test passed");
    println!("  - prompt_tokens: {}", prompt_tokens);
    println!("  - completion_tokens: {}", completion_tokens);
    println!("  - total_tokens: {}", total_tokens);
}

/// Test: Streaming final chunk includes token usage (LOG-22932)
///
/// Verifies that the streaming API returns valid usage statistics
/// in the final SSE chunk (indicated by having populated finish_reason property).
#[tokio::test]
async fn test_streaming_final_chunk_includes_usage() {
    let client = reqwest::Client::new();

    let response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "Say hello."})],
        true, // Streaming
    )
    .await;

    assert_eq!(response.status(), 200);

    let body = response.text().await.expect("Failed to read response");

    let mut final_chunk: Option<Value> = None;

    for line in body.lines() {
        if line == "data: [DONE]" {
            break;
        }
        if let Some(chunk) = parse_data_line(line) {
            if let Ok(json) = serde_json::from_str::<Value>(&chunk.data) {
                // Look for chunk with finish_reason (the final chunk)
                if json["choices"][0]["finish_reason"].as_str().is_some() {
                    final_chunk = Some(json);
                }
            }
        }
    }

    let final_chunk = final_chunk.expect("Should have received final chunk");

    // Verify usage field in final chunk
    let usage = final_chunk
        .get("usage")
        .expect("Final streaming chunk should include usage field");

    let prompt_tokens = usage["prompt_tokens"]
        .as_u64()
        .expect("prompt_tokens should be u64");
    let completion_tokens = usage["completion_tokens"]
        .as_u64()
        .expect("completion_tokens should be u64");
    let total_tokens = usage["total_tokens"]
        .as_u64()
        .expect("total_tokens should be u64");

    assert!(prompt_tokens > 0, "prompt_tokens should be > 0");
    assert!(completion_tokens > 0, "completion_tokens should be > 0");
    assert_eq!(
        total_tokens,
        prompt_tokens + completion_tokens,
        "total_tokens should equal prompt + completion"
    );

    println!("✓ Streaming final chunk usage test passed");
    println!("  - prompt_tokens: {}", prompt_tokens);
    println!("  - completion_tokens: {}", completion_tokens);
    println!("  - total_tokens: {}", total_tokens);
}
