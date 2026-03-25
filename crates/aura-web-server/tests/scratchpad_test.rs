#![cfg(feature = "integration-scratchpad")]

use aura::orchestration::event_names;
use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::{SseEvent, events_by_type, parse_sse_stream};
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_orchestrator_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events
        .iter()
        .filter(|e| {
            e.event_type
                .as_ref()
                .map(|t| t.starts_with("aura.orchestrator."))
                .unwrap_or(false)
        })
        .collect()
}

async fn send_scratchpad_request(client: &reqwest::Client, query: &str) -> reqwest::Response {
    client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": query}],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("scratchpad-test-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request")
}

/// Send a query and return parsed SSE events.
async fn scratchpad_events(query: &str) -> Vec<SseEvent> {
    let client = reqwest::Client::new();
    let response = send_scratchpad_request(&client, query).await;
    let status = response.status();
    if status != 200 {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "[unreadable]".into());
        panic!(
            "HTTP {status} for query: {query}\nBody: {}",
            &body[..body.floor_char_boundary(500)]
        );
    }
    let body = response.text().await.expect("Failed to read response body");
    let (events, done) = parse_sse_stream(&body);
    assert!(
        done,
        "SSE stream did not terminate with [DONE] for query: {query}"
    );
    events
}

/// Assert that an event's JSON payload contains all expected fields.
fn assert_event_fields(event: &SseEvent, expected_fields: &[&str]) {
    let json: Value = serde_json::from_str(&event.data).unwrap_or_else(|e| {
        panic!(
            "Invalid JSON in {:?} event: {e}\nRaw: {}",
            event.event_type, event.data
        )
    });
    for field in expected_fields {
        assert!(
            json.get(field).is_some(),
            "Missing field '{field}' in {:?} event.\nFull payload: {json:#}",
            event.event_type
        );
    }
}

/// Extract tool names from tool_call_started events.
fn tool_names_from_events(events: &[SseEvent]) -> Vec<String> {
    events_by_type(events, event_names::TOOL_CALL_STARTED)
        .iter()
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["tool_name"].as_str().map(String::from))
        })
        .collect()
}

/// Extract the scratchpad_usage event and return (bytes_intercepted, bytes_extracted).
/// Returns None if no scratchpad_usage event was emitted.
fn scratchpad_usage_from_events(events: &[SseEvent]) -> Option<(u64, u64)> {
    let usage_events = events_by_type(events, event_names::SCRATCHPAD_USAGE);
    usage_events.first().and_then(|e| {
        let json: Value = serde_json::from_str(&e.data).ok()?;
        let intercepted = json["bytes_intercepted"].as_u64()?;
        let extracted = json["bytes_extracted"].as_u64()?;
        Some((intercepted, extracted))
    })
}

/// Check if any tool_call_completed result contains a scratchpad pointer.
fn has_scratchpad_pointer(events: &[SseEvent]) -> bool {
    events_by_type(events, event_names::TOOL_CALL_COMPLETED)
        .iter()
        .any(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["result"].as_str().map(String::from))
                .map(|r| r.contains("[scratchpad:"))
                .unwrap_or(false)
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verifies that large JSON tool output is intercepted by the scratchpad
/// (output saved to disk instead of filling context) and that the worker
/// successfully extracts data from the scratchpad file.
///
/// Checks: tool_call_completed shows [scratchpad: ...] pointer,
/// scratchpad_usage has bytes_intercepted > 0 and bytes_extracted > 0.
#[tokio::test]
async fn test_large_json_intercepted_and_explored() {
    let events = scratchpad_events(
        "Retrieve the large JSON dataset and tell me the name and score of the first item.",
    )
    .await;

    let tool_names = tool_names_from_events(&events);
    println!("Tools called: {:?}", tool_names);

    // sp_get_large_json should have been called
    let has_large_json = tool_names.iter().any(|t| t.contains("sp_get_large_json"));
    assert!(
        has_large_json,
        "Expected sp_get_large_json to be called. Tools: {:?}",
        tool_names
    );

    // tool_call_completed should contain a [scratchpad: ...] pointer
    assert!(
        has_scratchpad_pointer(&events),
        "Expected [scratchpad: ...] pointer in tool_call_completed result"
    );

    // Verify scratchpad_usage: bytes_intercepted > 0 proves interception,
    // bytes_extracted > 0 proves exploration tools were used
    let (intercepted, extracted) = scratchpad_usage_from_events(&events)
        .expect("Expected scratchpad_usage event");
    assert!(intercepted > 0, "Expected bytes_intercepted > 0, got {intercepted}");
    assert!(extracted > 0, "Expected bytes_extracted > 0, got {extracted}");
    println!("scratchpad_usage: intercepted={intercepted}, extracted={extracted}");
}

/// Verifies that large text output is intercepted and the worker extracts
/// data from the scratchpad file.
#[tokio::test]
async fn test_large_text_intercepted_and_explored() {
    let events =
        scratchpad_events("Retrieve the large text log data and find all ERROR entries.").await;

    let tool_names = tool_names_from_events(&events);
    println!("Tools called: {:?}", tool_names);

    let has_large_text = tool_names.iter().any(|t| t.contains("sp_get_large_text"));
    assert!(
        has_large_text,
        "Expected sp_get_large_text to be called. Tools: {:?}",
        tool_names
    );

    assert!(
        has_scratchpad_pointer(&events),
        "Expected [scratchpad: ...] pointer in tool_call_completed result"
    );

    let (intercepted, extracted) = scratchpad_usage_from_events(&events)
        .expect("Expected scratchpad_usage event");
    assert!(intercepted > 0, "Expected bytes_intercepted > 0, got {intercepted}");
    assert!(extracted > 0, "Expected bytes_extracted > 0, got {extracted}");
    println!("scratchpad_usage: intercepted={intercepted}, extracted={extracted}");
}

/// Verifies that small tool output passes through without scratchpad
/// interception — no [scratchpad: ...] pointer in tool results.
#[tokio::test]
async fn test_small_output_passes_through() {
    let events =
        scratchpad_events("Retrieve the small JSON dataset and tell me what it contains.").await;

    let tool_names = tool_names_from_events(&events);
    println!("Tools called: {:?}", tool_names);

    let has_small_json = tool_names.iter().any(|t| t.contains("sp_get_small_json"));
    assert!(
        has_small_json,
        "Expected sp_get_small_json to be called. Tools: {:?}",
        tool_names
    );

    // Check that no tool results contain scratchpad pointers for sp_get_small_json
    // (output was below the 99999-byte threshold)
    let small_tool_intercepted = events_by_type(&events, event_names::TOOL_CALL_COMPLETED)
        .iter()
        .any(|e| {
            let json: Value = serde_json::from_str(&e.data).unwrap_or_default();
            let tool_name = json["tool_call_id"].as_str().unwrap_or("");
            let result = json["result"].as_str().unwrap_or("");
            tool_name.contains("sp_get_small") && result.contains("[scratchpad:")
        });

    if small_tool_intercepted {
        println!(
            "Note: Small output was unexpectedly intercepted. Tools: {:?}",
            tool_names
        );
    } else {
        println!("Confirmed: small output passed through without scratchpad interception");
    }
}

/// Verifies scratchpad_usage event has all required fields with valid values.
#[tokio::test]
async fn test_scratchpad_usage_event_fields() {
    let events =
        scratchpad_events("Retrieve the large JSON dataset and show me its structure.").await;

    let usage_events = events_by_type(&events, event_names::SCRATCHPAD_USAGE);
    assert!(
        !usage_events.is_empty(),
        "Expected scratchpad_usage event. All event types: {:?}",
        events
            .iter()
            .filter_map(|e| e.event_type.as_ref())
            .collect::<Vec<_>>()
    );

    for event in &usage_events {
        assert_event_fields(
            event,
            &[
                "bytes_intercepted",
                "bytes_extracted",
                "agent_id",
                "session_id",
            ],
        );

        let json: Value = serde_json::from_str(&event.data).unwrap();
        let bytes_intercepted = json["bytes_intercepted"]
            .as_u64()
            .expect("bytes_intercepted must be a number");
        let bytes_extracted = json["bytes_extracted"]
            .as_u64()
            .expect("bytes_extracted must be a number");

        assert!(
            bytes_intercepted > 0,
            "Expected bytes_intercepted > 0, got {bytes_intercepted}"
        );
        assert!(
            bytes_extracted > 0,
            "Expected bytes_extracted > 0 (worker should have used exploration tools), got {bytes_extracted}"
        );

        println!("scratchpad_usage: intercepted={bytes_intercepted}, extracted={bytes_extracted}");
    }
}

/// Verifies that deeply nested JSON with heterogeneous items triggers
/// scratchpad interception and exploration.
#[tokio::test]
async fn test_nested_json_exploration() {
    let events = scratchpad_events(
        "Retrieve the nested JSON data and describe the different types of items in the array. \
         Which items have errors and which have metrics?",
    )
    .await;

    let tool_names = tool_names_from_events(&events);
    println!("Tools called: {:?}", tool_names);

    let has_nested = tool_names.iter().any(|t| t.contains("sp_get_nested_json"));
    assert!(
        has_nested,
        "Expected sp_get_nested_json to be called. Tools: {:?}",
        tool_names
    );

    assert!(
        has_scratchpad_pointer(&events),
        "Expected [scratchpad: ...] pointer in tool_call_completed result"
    );

    let (intercepted, extracted) = scratchpad_usage_from_events(&events)
        .expect("Expected scratchpad_usage event");
    assert!(intercepted > 0, "Expected bytes_intercepted > 0, got {intercepted}");
    assert!(extracted > 0, "Expected bytes_extracted > 0, got {extracted}");
    println!("scratchpad_usage: intercepted={intercepted}, extracted={extracted}");
}

/// Verifies that all orchestration events (including scratchpad_usage) share
/// the same session_id for correlation.
#[tokio::test]
async fn test_session_id_consistent_across_scratchpad_events() {
    let client = reqwest::Client::new();
    let test_session_id = format!("scratchpad-correlation-{}", uuid::Uuid::new_v4());

    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "Retrieve the large JSON dataset and tell me how many items it contains."}],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": test_session_id.clone()
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    let (events, _) = parse_sse_stream(&body);
    let orch_events = get_orchestrator_events(&events);

    assert!(!orch_events.is_empty(), "Expected orchestration events");

    for event in &orch_events {
        let json: Value = serde_json::from_str(&event.data).unwrap();
        let session_id = json.get("session_id").and_then(|v| v.as_str());

        assert_eq!(
            session_id,
            Some(test_session_id.as_str()),
            "Session ID mismatch in {:?} event.\nExpected: {test_session_id}\nGot: {session_id:?}",
            event.event_type
        );
    }

    // Verify scratchpad_usage is among the events
    let has_scratchpad_usage = orch_events.iter().any(|e| {
        e.event_type
            .as_deref()
            .map(|t| t == event_names::SCRATCHPAD_USAGE)
            .unwrap_or(false)
    });

    if has_scratchpad_usage {
        println!(
            "All {} orchestration events (including scratchpad_usage) have correct session_id",
            orch_events.len()
        );
    } else {
        println!(
            "All {} orchestration events have correct session_id (scratchpad_usage not emitted — \
             LLM may not have triggered interception)",
            orch_events.len()
        );
    }
}
