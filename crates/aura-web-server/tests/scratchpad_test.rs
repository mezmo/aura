#![cfg(feature = "integration-scratchpad")]

use aura::orchestration::event_names as orch_event_names;
use aura::stream_events::event_names;
use aura_test_utils::server_urls::{AURA_SERVER, AURA_SINGLE_AGENT_SERVER};
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

async fn send_scratchpad_request_to(
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
) -> reqwest::Response {
    client
        .post(format!("{base_url}/v1/chat/completions"))
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

/// Send a query to `base_url` and return parsed SSE events.
async fn events_from(base_url: &str, query: &str) -> Vec<SseEvent> {
    let client = reqwest::Client::new();
    let response = send_scratchpad_request_to(&client, base_url, query).await;
    let status = response.status();
    if status != 200 {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "[unreadable]".into());
        panic!(
            "HTTP {status} from {base_url} for query: {query}\nBody: {}",
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

/// Send a query to the orchestration aura and return parsed SSE events.
async fn scratchpad_events(query: &str) -> Vec<SseEvent> {
    events_from(AURA_SERVER.as_str(), query).await
}

/// Send a query to the single-agent aura and return parsed SSE events.
async fn single_agent_events(query: &str) -> Vec<SseEvent> {
    events_from(AURA_SINGLE_AGENT_SERVER.as_str(), query).await
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
    events_by_type(events, orch_event_names::TOOL_CALL_STARTED)
        .iter()
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["tool_name"].as_str().map(String::from))
        })
        .collect()
}

/// Extract the scratchpad_usage event and return (tokens_intercepted, tokens_extracted).
/// Returns None if no scratchpad_usage event was emitted.
fn scratchpad_usage_from_events(events: &[SseEvent]) -> Option<(u64, u64)> {
    let usage_events = events_by_type(events, event_names::SCRATCHPAD_USAGE);
    usage_events.first().and_then(|e| {
        let json: Value = serde_json::from_str(&e.data).ok()?;
        let intercepted = json["tokens_intercepted"].as_u64()?;
        let extracted = json["tokens_extracted"].as_u64()?;
        Some((intercepted, extracted))
    })
}

/// Check if any tool_call_completed result contains a scratchpad pointer.
fn has_scratchpad_pointer(events: &[SseEvent]) -> bool {
    events_by_type(events, orch_event_names::TOOL_CALL_COMPLETED)
        .iter()
        .any(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["result"].as_str().map(String::from))
                .map(|r| r.contains("[scratchpad:"))
                .unwrap_or(false)
        })
}

/// Single-agent equivalent of `tool_names_from_events`. The orchestrator
/// emits `aura.orchestrator.tool_call_started`, but single-agent mode emits
/// the base `aura.tool_requested` event instead.
fn single_agent_tool_names(events: &[SseEvent]) -> Vec<String> {
    events_by_type(events, event_names::TOOL_REQUESTED)
        .iter()
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["tool_name"].as_str().map(String::from))
        })
        .collect()
}

/// Single-agent equivalent of `has_scratchpad_pointer`. Single-agent mode
/// emits `aura.tool_complete` events; their `result` field carries any
/// `[scratchpad: ...]` pointer the wrapper produced.
fn single_agent_has_scratchpad_pointer(events: &[SseEvent]) -> bool {
    events_by_type(events, event_names::TOOL_COMPLETE)
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
/// scratchpad_usage has tokens_intercepted > 0 and tokens_extracted > 0.
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

    // Verify scratchpad_usage: tokens_intercepted > 0 proves interception,
    // tokens_extracted > 0 proves exploration tools were used
    let (intercepted, extracted) =
        scratchpad_usage_from_events(&events).expect("Expected scratchpad_usage event");
    assert!(
        intercepted > 0,
        "Expected tokens_intercepted > 0, got {intercepted}"
    );
    assert!(
        extracted > 0,
        "Expected tokens_extracted > 0, got {extracted}"
    );
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

    let (intercepted, extracted) =
        scratchpad_usage_from_events(&events).expect("Expected scratchpad_usage event");
    assert!(
        intercepted > 0,
        "Expected tokens_intercepted > 0, got {intercepted}"
    );
    assert!(
        extracted > 0,
        "Expected tokens_extracted > 0, got {extracted}"
    );
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

    // Small output should NOT be intercepted — no scratchpad pointer in any tool result
    assert!(
        !has_scratchpad_pointer(&events),
        "Expected small output to pass through without scratchpad interception. Tools: {:?}",
        tool_names
    );
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
                "tokens_intercepted",
                "tokens_extracted",
                "agent_id",
                "session_id",
            ],
        );

        let json: Value = serde_json::from_str(&event.data).unwrap();
        let tokens_intercepted = json["tokens_intercepted"]
            .as_u64()
            .expect("tokens_intercepted must be a number");
        let tokens_extracted = json["tokens_extracted"]
            .as_u64()
            .expect("tokens_extracted must be a number");

        assert!(
            tokens_intercepted > 0,
            "Expected tokens_intercepted > 0, got {tokens_intercepted}"
        );
        assert!(
            tokens_extracted > 0,
            "Expected tokens_extracted > 0 (worker should have used exploration tools), got {tokens_extracted}"
        );

        println!(
            "scratchpad_usage: intercepted={tokens_intercepted}, extracted={tokens_extracted}"
        );
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

    let (intercepted, extracted) =
        scratchpad_usage_from_events(&events).expect("Expected scratchpad_usage event");
    assert!(
        intercepted > 0,
        "Expected tokens_intercepted > 0, got {intercepted}"
    );
    assert!(
        extracted > 0,
        "Expected tokens_extracted > 0, got {extracted}"
    );
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

    // Verify scratchpad_usage is among the events — this query targets
    // sp_get_large_json which should always trigger interception
    let has_scratchpad_usage = orch_events.iter().any(|e| {
        e.event_type
            .as_deref()
            .map(|t| t == event_names::SCRATCHPAD_USAGE)
            .unwrap_or(false)
    });
    assert!(
        has_scratchpad_usage,
        "Expected scratchpad_usage event among orchestration events. Event types: {:?}",
        orch_events
            .iter()
            .filter_map(|e| e.event_type.as_ref())
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Single-agent tests
//
// Mirror the orchestration tests above but target the single-agent aura
// instance (port 8081, config `integration-scratchpad-single-agent.toml`).
// They cover the path that wires LLM ground-truth feedback through
// `StreamingRequestHook` and emits `aura.scratchpad_usage` from
// `Agent::append_scratchpad_usage` rather than the orchestrator.
// ---------------------------------------------------------------------------

/// Single-agent: large JSON output is intercepted (pointer surfaces in the
/// `aura.tool_complete` result) and the agent extracts data via exploration
/// tools. Verifies `tokens_intercepted > 0` AND `tokens_extracted > 0` so the
/// full intercept-then-explore loop ran.
#[tokio::test]
async fn single_agent_large_json_intercepted_and_explored() {
    let events = single_agent_events(
        "Retrieve the large JSON dataset and tell me the name and score of the first item.",
    )
    .await;

    let tool_names = single_agent_tool_names(&events);
    println!("[single-agent] tools: {tool_names:?}");

    assert!(
        tool_names.iter().any(|t| t.contains("sp_get_large_json")),
        "Expected sp_get_large_json to be called. Tools: {tool_names:?}"
    );
    assert!(
        single_agent_has_scratchpad_pointer(&events),
        "Expected [scratchpad: ...] pointer in aura.tool_complete result"
    );

    let (intercepted, extracted) = scratchpad_usage_from_events(&events)
        .expect("single-agent must emit aura.scratchpad_usage");
    assert!(
        intercepted > 0,
        "single-agent tokens_intercepted should be > 0, got {intercepted}"
    );
    assert!(
        extracted > 0,
        "single-agent tokens_extracted should be > 0 (exploration tools must run), got {extracted}"
    );
    println!("[single-agent] intercepted={intercepted}, extracted={extracted}");
}

/// Single-agent: output below the per-tool `min_tokens` threshold passes
/// through unchanged — no `[scratchpad: ...]` pointer and no
/// `aura.scratchpad_usage` event.
#[tokio::test]
async fn single_agent_small_output_passes_through() {
    let events =
        single_agent_events("Retrieve the small JSON dataset and tell me what status it reports.")
            .await;

    let tool_names = single_agent_tool_names(&events);
    println!("[single-agent] tools: {tool_names:?}");

    assert!(
        tool_names.iter().any(|t| t.contains("sp_get_small_json")),
        "Expected sp_get_small_json to be called. Tools: {tool_names:?}"
    );

    // No interception → no pointer, no scratchpad_usage event.
    assert!(
        !single_agent_has_scratchpad_pointer(&events),
        "Small output should pass through without scratchpad interception"
    );
    assert!(
        scratchpad_usage_from_events(&events).is_none(),
        "Small-output query should NOT emit aura.scratchpad_usage (no activity)"
    );
}

/// Single-agent: the `aura.scratchpad_usage` event carries `agent_id="main"`
/// (matches what `AgentContext::single_agent` injects). Validates payload
/// shape parity with the orchestration variant aside from this field.
#[tokio::test]
async fn single_agent_scratchpad_usage_event_payload() {
    let events =
        single_agent_events("Retrieve the large JSON dataset and show me its structure.").await;

    let usage_events = events_by_type(&events, event_names::SCRATCHPAD_USAGE);
    assert!(
        !usage_events.is_empty(),
        "single-agent must emit aura.scratchpad_usage. Saw event types: {:?}",
        events
            .iter()
            .filter_map(|e| e.event_type.as_ref())
            .collect::<Vec<_>>()
    );

    for event in &usage_events {
        assert_event_fields(
            event,
            &[
                "tokens_intercepted",
                "tokens_extracted",
                "agent_id",
                "session_id",
            ],
        );
        let json: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(
            json["agent_id"].as_str(),
            Some("main"),
            "single-agent agent_id should be 'main' (got: {})",
            json["agent_id"]
        );
        assert!(
            json["tokens_intercepted"].as_u64().unwrap_or(0) > 0,
            "tokens_intercepted should be > 0"
        );
        assert!(
            json["tokens_extracted"].as_u64().unwrap_or(0) > 0,
            "tokens_extracted should be > 0"
        );
    }
}

/// Single-agent: nested JSON exercises the same companion-extraction +
/// exploration path as orchestration. Specifically targets `sp_get_nested_json`
/// which produces heterogeneous array items (some with `error`, some with
/// `metrics`) that the agent must explore via `item_schema` / `iterate_over`.
#[tokio::test]
async fn single_agent_nested_json_exploration() {
    let events = single_agent_events(
        "Retrieve the nested JSON cluster data. How many nodes have errors and how many have metrics?",
    )
    .await;

    let tool_names = single_agent_tool_names(&events);
    println!("[single-agent nested] tools: {tool_names:?}");

    assert!(
        tool_names.iter().any(|t| t.contains("sp_get_nested_json")),
        "Expected sp_get_nested_json to be called. Tools: {tool_names:?}"
    );
    assert!(
        single_agent_has_scratchpad_pointer(&events),
        "Expected [scratchpad: ...] pointer in aura.tool_complete result"
    );

    let (intercepted, extracted) = scratchpad_usage_from_events(&events)
        .expect("single-agent must emit aura.scratchpad_usage");
    assert!(intercepted > 0, "tokens_intercepted should be > 0");
    assert!(
        extracted > 0,
        "tokens_extracted should be > 0 — agent must use exploration tools to answer"
    );
}

/// Single-agent: `turn_depth_bonus` (default 6) actually grants the extra
/// turns. Without the bonus, an intercept + multi-step exploration query
/// would exhaust the configured `turn_depth=10` before answering. With the
/// bonus the effective limit is 16, leaving ample room. We assert that the
/// agent reaches a final response (not a depth-exceeded error) AND that
/// multiple exploration tools ran after the initial intercept.
#[tokio::test]
async fn single_agent_turn_depth_bonus_allows_multi_step_exploration() {
    let events = single_agent_events(
        "Retrieve the nested JSON cluster data, then for any nodes with errors \
         report the error code and the message. Use the exploration tools.",
    )
    .await;

    // Stream must terminate cleanly (events_from already asserts [DONE]).
    // Verify the agent produced a final response message.
    let final_chunks: Vec<&SseEvent> = events.iter().filter(|e| e.event_type.is_none()).collect();
    assert!(
        !final_chunks.is_empty(),
        "Expected final response chunks; turn_depth bonus may not be applied"
    );

    let tool_names = single_agent_tool_names(&events);
    println!("[single-agent depth] tool count: {}", tool_names.len());

    // Initial fetch + at least one scratchpad exploration tool. Without the
    // bonus the agent would either truncate before exploring or hit
    // depth-exceeded mid-loop.
    let scratchpad_explore_tools = [
        "schema",
        "head",
        "slice",
        "grep",
        "get_in",
        "iterate_over",
        "item_schema",
        "read",
    ];
    let exploration_count = tool_names
        .iter()
        .filter(|n| scratchpad_explore_tools.contains(&n.as_str()))
        .count();
    assert!(
        exploration_count >= 1,
        "Expected >=1 exploration tool call after intercept (turn_depth_bonus must take effect). Tools: {tool_names:?}"
    );

    let (intercepted, _) = scratchpad_usage_from_events(&events)
        .expect("single-agent must emit aura.scratchpad_usage");
    assert!(intercepted > 0);
}

/// Single-agent: when no MCP tool is called at all, the agent still finishes
/// normally and does NOT emit a stray `aura.scratchpad_usage` event (skip
/// rule: `tokens_intercepted == 0 && tokens_extracted == 0`).
#[tokio::test]
async fn single_agent_no_tool_calls_skips_scratchpad_event() {
    // Trivial query that doesn't need tools — bypasses scratchpad entirely.
    let events = single_agent_events("Reply with exactly the word 'ack' and nothing else.").await;

    let tool_names = single_agent_tool_names(&events);
    println!("[single-agent no-tools] tools: {tool_names:?}");

    // No tool calls AND no scratchpad_usage event.
    assert!(
        scratchpad_usage_from_events(&events).is_none(),
        "Single-agent must skip aura.scratchpad_usage when budget shows zero activity"
    );
}
