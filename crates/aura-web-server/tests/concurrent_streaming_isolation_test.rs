#![cfg(feature = "integration-streaming")]

//! Integration test for concurrent streaming request isolation
//!
//! Verifies that bounded channels provide proper stream isolation when
//! multiple streaming requests are active simultaneously.
//!

use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::parse_data_line;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::sleep;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Extract completion_id from SSE chunks
fn extract_completion_id(chunks: &[Value]) -> Option<String> {
    chunks
        .first()
        .and_then(|chunk| chunk.get("id"))
        .and_then(|id| id.as_str())
        .map(String::from)
}

/// Extract all text content from SSE chunks
fn extract_text_content(chunks: &[Value]) -> String {
    chunks
        .iter()
        .filter_map(|chunk| {
            chunk["choices"]
                .get(0)
                .and_then(|choice| choice["delta"]["content"].as_str())
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Test: Concurrent Streaming Requests with Different Session IDs
///
/// Validates that:
/// 1. Multiple concurrent streaming requests complete successfully
/// 2. Each request has a unique completion_id
/// 3. Responses don't contain data from other concurrent requests
/// 4. Bounded channels prevent stream crossing
#[tokio::test]
async fn test_concurrent_streaming_isolation() {
    let client = reqwest::Client::new();

    // Create 5 concurrent streaming requests with unique identifiers
    let num_requests = 5;
    let mut handles = Vec::new();

    for i in 0..num_requests {
        let client_clone = client.clone();
        let session_id = format!("concurrent-session-{}", i);
        let unique_number = i + 1; // 1-5

        // Spawn each request as a separate async task
        let handle = tokio::spawn(async move {
            // Send streaming request with unique prompt
            let response = client_clone
                .post(format!("{AURA_SERVER}/v1/chat/completions"))
                .json(&json!({
                    "model": "gpt-4o",
                    "messages": [{
                        "role": "user",
                        "content": format!("Say only the number {} and nothing else.", unique_number)
                    }],
                    "stream": true,
                    "metadata": {
                        "account_id": format!("concurrent-test-{}", unique_number),
                        "chat_session_id": session_id
                    }
                }))
                .timeout(TEST_TIMEOUT)
                .send()
                .await
                .expect("Failed to send request");

            assert_eq!(
                response.status(),
                200,
                "Request {} should return 200 OK",
                unique_number
            );

            // Parse SSE stream (filtering out aura.* custom events)
            let body = response.text().await.expect("Failed to read response body");
            let lines: Vec<&str> = body.lines().collect();

            let mut chunks = Vec::new();
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
                        chunks.push(json);
                    }
                }
            }

            (unique_number, chunks)
        });

        handles.push(handle);

        // Small delay to stagger requests slightly (not required, but makes test more realistic)
        sleep(Duration::from_millis(50)).await;
    }

    // Wait for all concurrent requests to complete
    let results = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("Task panicked"))
        .collect::<Vec<_>>();

    // Validate each response independently
    let mut completion_ids = Vec::new();

    for (unique_number, chunks) in results {
        // Verify we got chunks
        assert!(
            !chunks.is_empty(),
            "Request {} should have received chunks",
            unique_number
        );

        // Extract and verify unique completion_id
        let completion_id = extract_completion_id(&chunks)
            .unwrap_or_else(|| panic!("Request {} should have completion_id", unique_number));

        assert!(
            completion_id.starts_with("chatcmpl-"),
            "Request {} should have valid completion_id format, got '{}'",
            unique_number,
            completion_id
        );

        completion_ids.push(completion_id.clone());

        // Extract text content
        let content = extract_text_content(&chunks);

        // Verify content is non-empty
        assert!(
            !content.trim().is_empty(),
            "Request {} should have non-empty content",
            unique_number
        );

        // Verify content matches the expected number (stream isolation check)
        let contains_number = content.contains(&unique_number.to_string());
        assert!(
            contains_number,
            "Request {} should contain '{}' in response, got: '{}'",
            unique_number, unique_number, content
        );
    }

    // CRITICAL: Verify all completion_ids are unique (no stream crossing)
    let unique_ids: std::collections::HashSet<_> = completion_ids.iter().collect();
    assert_eq!(
        unique_ids.len(),
        num_requests,
        "❌ CRITICAL BUG: Found duplicate completion_ids! This indicates stream crossing.\n\
         Expected {} unique IDs, got {}.\n\
         IDs: {:?}",
        num_requests,
        unique_ids.len(),
        completion_ids
    );
}

/// Test: Rapid Sequential Requests (Channel Cleanup Verification)
///
/// Validates that:
/// 1. Channels are properly cleaned up between requests
/// 2. Old producer tasks don't interfere with new requests
/// 3. No resource leaks from spawned tasks
///
/// Note: Uses unique session_id per request to ensure request isolation.
/// Testing channel cleanup doesn't require session reuse.
#[tokio::test]
async fn test_rapid_sequential_streaming() {
    let client = reqwest::Client::new();
    let num_requests = 10;
    let test_run_id = uuid::Uuid::new_v4();

    let mut all_completion_ids = Vec::new();

    for i in 0..num_requests {
        // Use unique session_id per request to ensure request isolation
        let session_id = format!("rapid-sequential-{}-{}", test_run_id, i);

        let response = client
            .post(format!("{AURA_SERVER}/v1/chat/completions"))
            .json(&json!({
                "model": "gpt-4o",
                "messages": [{
                    "role": "user",
                    "content": format!("Say hello #{}", i + 1)
                }],
                "stream": true,
                "metadata": {
                    "account_id": "rapid-test",
                    "chat_session_id": session_id
                }
            }))
            .timeout(TEST_TIMEOUT)
            .send()
            .await
            .unwrap_or_else(|e| panic!("Request {} failed: {}", i + 1, e));

        assert_eq!(
            response.status(),
            200,
            "Request {} should return 200 OK",
            i + 1
        );

        // Parse just the first OpenAI chunk to get completion_id (skip aura.* events)
        let body = response
            .text()
            .await
            .unwrap_or_else(|e| panic!("Request {} failed to read body: {}", i + 1, e));

        let mut current_event_type: Option<String> = None;
        let mut first_openai_chunk: Option<Value> = None;
        for line in body.lines() {
            if let Some(event) = line.strip_prefix("event: ") {
                current_event_type = Some(event.to_string());
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ") {
                let is_aura_event = current_event_type
                    .as_ref()
                    .map(|t| t.starts_with("aura."))
                    .unwrap_or(false);
                current_event_type = None;
                if is_aura_event {
                    continue;
                }
                if let Ok(json) = serde_json::from_str::<Value>(data) {
                    first_openai_chunk = Some(json);
                    break;
                }
            }
        }

        let json =
            first_openai_chunk.unwrap_or_else(|| panic!("Request {} had no OpenAI chunks", i + 1));
        let completion_id = json["id"]
            .as_str()
            .unwrap_or_else(|| panic!("Request {} missing id field", i + 1))
            .to_string();
        all_completion_ids.push(completion_id);
    }

    // Verify all completion_ids are unique
    let unique_ids: std::collections::HashSet<_> = all_completion_ids.iter().collect();
    assert_eq!(
        unique_ids.len(),
        num_requests,
        "All {} requests should have unique completion_ids, got {} unique.\nIDs: {:?}",
        num_requests,
        unique_ids.len(),
        all_completion_ids
    );
}
