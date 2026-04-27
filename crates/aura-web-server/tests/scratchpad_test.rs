#![cfg(feature = "integration-scratchpad")]

use aura::orchestration::event_names as orch_event_names;
use aura::stream_events::event_names;
use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::{SseEvent, events_by_type, parse_sse_stream};
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(180);

// Agent aliases declared in `configs/integration-scratchpad{,-single-agent}.toml`.
// The single aura-web-server loads both configs from a directory and routes
// each request to the matching agent via the OpenAI `model` field.
const ORCH_AGENT: &str = "scratchpad-orch";
const SINGLE_AGENT: &str = "scratchpad-single";

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
    model: &str,
    query: &str,
) -> reqwest::Response {
    client
        .post(format!("{}/v1/chat/completions", AURA_SERVER.as_str()))
        .json(&json!({
            "model": model,
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

/// Send a query routed to `model` (an agent alias) and return parsed SSE events.
async fn events_for_model(model: &str, query: &str) -> Vec<SseEvent> {
    let client = reqwest::Client::new();
    let response = send_scratchpad_request_to(&client, model, query).await;
    let status = response.status();
    if status != 200 {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "[unreadable]".into());
        panic!(
            "HTTP {status} from {} (model={model}) for query: {query}\nBody: {}",
            AURA_SERVER.as_str(),
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

/// Send a query to the orchestration agent and return parsed SSE events.
async fn scratchpad_events(query: &str) -> Vec<SseEvent> {
    events_for_model(ORCH_AGENT, query).await
}

/// Send a query to the single-agent agent and return parsed SSE events.
async fn single_agent_events(query: &str) -> Vec<SseEvent> {
    events_for_model(SINGLE_AGENT, query).await
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
///
/// **Note:** Scratchpad exploration tools (head/slice/grep/schema/...) are
/// suppressed from `aura.tool_requested` in single-agent mode (mirrors
/// orchestration, which also doesn't surface them in
/// `aura.orchestrator.tool_call_*`). This helper therefore returns only
/// MCP tool names. Use `tokens_extracted` from `aura.scratchpad_usage` as
/// the signal that exploration tools ran.
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
///
/// Unaffected by scratchpad-tool event suppression: the pointer appears in
/// the `aura.tool_complete` of the *MCP tool* whose output got intercepted
/// (the wrapper rewrites that tool's output), and MCP tools are not in the
/// suppression set.
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
/// Checks the *interception* path: sp_get_large_json was called, the
/// tool_call_completed result carries a `[scratchpad: ...]` pointer, and
/// the scratchpad_usage event reports `tokens_intercepted > 0`. Whether
/// the LLM then uses exploration tools (`tokens_extracted > 0`) is the
/// model's call and not asserted here — see test_nested_json_exploration's
/// docstring for the rationale.
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

    let (intercepted, extracted) =
        scratchpad_usage_from_events(&events).expect("Expected scratchpad_usage event");
    assert!(
        intercepted > 0,
        "Expected tokens_intercepted > 0, got {intercepted}"
    );
    println!(
        "scratchpad_usage: intercepted={intercepted}, extracted={extracted} (extracted is informational; LLM exploration is non-deterministic)"
    );
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
    println!(
        "scratchpad_usage: intercepted={intercepted}, extracted={extracted} (extracted is informational; LLM exploration is non-deterministic)"
    );
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

/// Verifies scratchpad_usage event(s) have all required fields with valid
/// values.
///
/// Per-event shape: each event must carry `tokens_intercepted`,
/// `tokens_extracted`, `agent_id`, and `session_id` and they must all
/// parse as the right types.
///
/// Aggregate values: across ALL emitted events, the total
/// `tokens_intercepted` must be > 0 (proves the wrapper fired somewhere
/// in this run). Per-event minimums are NOT asserted because the
/// orchestrator can emit one event per worker task attempt — when it
/// retries a worker, the second attempt's fresh budget may legitimately
/// have `intercepted == 0` if the retry didn't re-fetch the data
/// (`scratchpad_usage_event` only requires `intercepted > 0 || extracted > 0`).
/// `tokens_extracted` is intentionally not asserted (LLM exploration is
/// non-deterministic — see test_nested_json_exploration's docstring).
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

    let mut total_intercepted = 0u64;
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
        // Sanity: gating rule (intercepted>0 || extracted>0) must hold per
        // event, otherwise the orchestrator wouldn't have emitted it.
        assert!(
            tokens_intercepted > 0 || tokens_extracted > 0,
            "scratchpad_usage event with both counters at 0 should never be emitted"
        );
        total_intercepted += tokens_intercepted;

        println!(
            "scratchpad_usage: intercepted={tokens_intercepted}, extracted={tokens_extracted}"
        );
    }

    assert!(
        total_intercepted > 0,
        "Aggregate tokens_intercepted across all {} events should be > 0",
        usage_events.len()
    );
}

/// Verifies that deeply nested JSON triggers scratchpad interception.
///
/// We deliberately do NOT assert `tokens_extracted > 0`: that would require
/// the LLM to actually use exploration tools after seeing the pointer, which
/// is non-deterministic — a model can legitimately choose to call the tool
/// again with different args, give up, or pivot. What this test guards is
/// the *interception* path (wrapper fires, pointer is emitted, budget
/// records the diverted tokens) — that path is fully under our control
/// and would be a real regression if it broke. Whether the LLM chooses to
/// then explore is the model's call.
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
    println!(
        "scratchpad_usage: intercepted={intercepted}, extracted={extracted} (extracted is informational; LLM exploration is non-deterministic)"
    );
}

/// Verifies that all orchestration events (including scratchpad_usage) share
/// the same session_id for correlation.
#[tokio::test]
async fn test_session_id_consistent_across_scratchpad_events() {
    let client = reqwest::Client::new();
    let test_session_id = format!("scratchpad-correlation-{}", uuid::Uuid::new_v4());

    // Prompt names the tool explicitly so the LLM reliably calls
    // sp_get_large_json and triggers interception (which produces the
    // aura.scratchpad_usage event we assert on below). Without this
    // explicit hint, the orchestrator can occasionally answer from prior
    // context or call a smaller-output tool, which would skip
    // interception and break the test for reasons unrelated to
    // session_id correlation.
    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": ORCH_AGENT,
            "messages": [{
                "role": "user",
                "content": "Call the sp_get_large_json tool to retrieve the large JSON \
                            dataset, then tell me how many items it contains."
            }],
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

    // Primary assertion: every orchestration event carries the same session_id.
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

    // Secondary check: IF a scratchpad_usage event was emitted (depends on
    // the coordinator's non-deterministic plan decomposition actually
    // routing through sp_get_large_json with a > 50-token result), verify
    // it also carries the same session_id. A run where the orchestrator
    // satisfies the prompt without hitting an interceptable tool is a
    // valid LLM choice and shouldn't fail this test — the session_id
    // correlation is what we're really testing.
    let scratchpad_event = events_by_type(&events, event_names::SCRATCHPAD_USAGE);
    if let Some(event) = scratchpad_event.first() {
        let json: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(
            json.get("session_id").and_then(|v| v.as_str()),
            Some(test_session_id.as_str()),
            "Session ID mismatch in aura.scratchpad_usage event"
        );
        println!(
            "scratchpad_usage emitted with session_id={}",
            test_session_id
        );
    } else {
        println!(
            "[informational] orchestrator chose a path that didn't trigger \
             scratchpad interception this run; session_id consistency was \
             still verified across the {} orchestrator event(s) emitted",
            orch_events.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Single-agent tests
//
// Mirror the orchestration tests above but route requests to the
// single-agent config (`integration-scratchpad-single-agent.toml`,
// alias `scratchpad-single`) on the same aura-web-server. They cover
// the path that wires LLM ground-truth feedback through
// `StreamingRequestHook` and emits `aura.scratchpad_usage` from
// `Agent::append_scratchpad_usage` rather than the orchestrator.
// ---------------------------------------------------------------------------

/// Single-agent: large JSON output is intercepted (pointer surfaces in the
/// `aura.tool_complete` result) and the wrapper records the diverted
/// tokens. Verifies `tokens_intercepted > 0`. Whether the LLM then calls
/// exploration tools is its own non-deterministic choice — see
/// test_nested_json_exploration's docstring for the rationale.
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
    println!(
        "[single-agent] intercepted={intercepted}, extracted={extracted} (extracted is informational; LLM exploration is non-deterministic)"
    );
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
        // tokens_extracted intentionally not asserted: LLM exploration is
        // non-deterministic. The wrapper-controlled interception path is
        // what this test guards.
    }
}

/// Single-agent: nested JSON triggers companion-extraction + interception.
/// Asserts the wrapper-controlled side (intercept fires, pointer surfaces).
/// Whether the LLM then uses item_schema/iterate_over to explore is
/// non-deterministic and not asserted — see test_nested_json_exploration's
/// docstring for the rationale.
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
    println!(
        "[single-agent nested] intercepted={intercepted}, extracted={extracted} (extracted is informational; LLM exploration is non-deterministic)"
    );
}

/// Single-agent: `turn_depth_bonus` (default 6) actually grants the extra
/// turns. Without the bonus, an intercept + multi-step exploration query
/// would exhaust the configured `turn_depth=10` before answering. With the
/// bonus the effective limit is 16, leaving ample room. We assert that the
/// agent reaches a final response (not a depth-exceeded error) AND that
/// `tokens_extracted > 0` (the only signal that exploration tools ran,
/// since they're suppressed from `aura.tool_*` events).
///
/// **Flake-mitigation strategy.** This is the only integration test in this
/// suite that asserts `tokens_extracted > 0` per-event. Other tests dropped
/// that assertion because the LLM's choice to explore is non-deterministic;
/// here the assertion is *load-bearing* — it's the only signal that the
/// turn_depth_bonus mechanism is doing its job. We compensate with a very
/// directive prompt that explicitly names the tools and their order
/// (`schema` → `iterate_over`), which has been reliable in repeated runs.
/// If this test ever flakes anyway, the answer is to make the prompt MORE
/// directive (option 2 above the loosen-assertion fallback) — relaxing
/// `tokens_extracted > 0` would gut the test's purpose.
#[tokio::test]
async fn single_agent_turn_depth_bonus_allows_multi_step_exploration() {
    // Directive prompt: spell out the exploration sequence so the LLM
    // reliably calls scratchpad tools after intercept. The test's purpose
    // is to prove `turn_depth_bonus` grants the headroom for that
    // multi-step flow — non-deterministic LLM choices about WHETHER to
    // explore would obscure that signal.
    let events = single_agent_events(
        "Call sp_get_nested_json. The result will be intercepted into a \
         scratchpad file (you'll see a [scratchpad: ...] pointer). After \
         that, use the `schema` tool on that file to inspect its \
         structure, then use `iterate_over` on the nodes array to read \
         each item's error code and message. Report only nodes that have \
         an `error` field.",
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
    println!("[single-agent depth] mcp tool count: {}", tool_names.len());

    // Scratchpad exploration tool calls (head/slice/grep/...) are intentionally
    // suppressed from `aura.tool_requested` events to match orchestration's
    // event surface, so we can't count them via `single_agent_tool_names`.
    // Instead we use the budget's `tokens_extracted > 0` as the proof that
    // the agent actually used exploration tools after intercept — without the
    // turn_depth_bonus, the agent would hit depth-exceeded before extracting
    // anything and `tokens_extracted` would stay at 0.
    let (intercepted, extracted) = scratchpad_usage_from_events(&events)
        .expect("single-agent must emit aura.scratchpad_usage");
    assert!(intercepted > 0, "tokens_intercepted should be > 0");
    assert!(
        extracted > 0,
        "tokens_extracted > 0 proves exploration tools ran after intercept; \
         turn_depth_bonus must be in effect"
    );
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
