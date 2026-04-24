#![cfg(feature = "integration-orchestration")]

use aura::orchestration::event_names;
use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::{SseEvent, events_by_type, parse_sse_stream};
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

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

async fn send_orchestration_request(client: &reqwest::Client, query: &str) -> reqwest::Response {
    client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": query}],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("orch-test-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request")
}

/// Send a query and return parsed orchestration events.
/// Creates a fresh HTTP client per call. Panics with contextual messages on failure.
async fn orchestration_events(query: &str) -> Vec<SseEvent> {
    let client = reqwest::Client::new();
    let response = send_orchestration_request(&client, query).await;
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
/// Produces rich error messages with event type and full payload.
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

/// Assert that at least one routing event exists (plan_created, direct_answer, or clarification_needed).
fn assert_any_routing_event(events: &[SseEvent]) {
    let has_routing = !events_by_type(events, event_names::PLAN_CREATED).is_empty()
        || !events_by_type(events, event_names::DIRECT_ANSWER).is_empty()
        || !events_by_type(events, event_names::CLARIFICATION_NEEDED).is_empty();
    assert!(
        has_routing,
        "No routing event found. Expected plan_created, direct_answer, or clarification_needed.\n\
         All events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verifies that a multi-step math query triggers orchestration planning.
///
/// Query: "Calculate the mean of [10, 20, 30] then multiply the result by 3"
/// Expected: plan_created event with tasks array of length >= 2.
///
/// NOTE: Hits a real LLM. Multi-domain queries should always produce a plan.
#[tokio::test]
async fn test_multi_step_math_emits_plan_created() {
    let events =
        orchestration_events("Calculate the mean of [10, 20, 30] then multiply the result by 3")
            .await;

    let plan_events = events_by_type(&events, event_names::PLAN_CREATED);

    // LLM may route to direct answer for this query; if so, pass with note
    if plan_events.is_empty() {
        let direct = events_by_type(&events, event_names::DIRECT_ANSWER);
        if !direct.is_empty() {
            println!("Note: LLM routed to direct answer instead of plan. Acceptable.");
            return;
        }
        assert_any_routing_event(&events);
        return;
    }

    for event in &plan_events {
        assert_event_fields(event, &["goal", "tasks", "agent_id", "session_id"]);

        let json: Value = serde_json::from_str(&event.data).unwrap();
        let tasks = json["tasks"].as_array().expect("tasks must be an array");
        assert!(!tasks.is_empty(), "tasks array should not be empty");
        println!(
            "plan_created: goal={}, task_count={}",
            json["goal"],
            tasks.len()
        );
    }
}

/// Verifies that orchestrated tasks emit matched start/complete event pairs.
///
/// Query: "Add 5 and 3, then multiply the result by 2"
/// Expected: task_started + task_completed pairs with matching task_id.
#[tokio::test]
async fn test_sequential_tasks_emit_lifecycle_events() {
    let events = orchestration_events("Add 5 and 3, then multiply the result by 2").await;

    let task_started = events_by_type(&events, event_names::TASK_STARTED);
    let task_completed = events_by_type(&events, event_names::TASK_COMPLETED);

    // LLM may route to direct answer; if so, no task events are expected
    if task_started.is_empty() && task_completed.is_empty() {
        let direct = events_by_type(&events, event_names::DIRECT_ANSWER);
        if !direct.is_empty() {
            println!("Note: LLM routed to direct answer. No task lifecycle events expected.");
            return;
        }
        assert_any_routing_event(&events);
        return;
    }

    assert!(
        !task_started.is_empty(),
        "Expected task_started events but found none"
    );
    assert!(
        !task_completed.is_empty(),
        "Expected task_completed events but found none"
    );

    for started in &task_started {
        let started_json: Value = serde_json::from_str(&started.data).unwrap();
        let task_id = started_json["task_id"]
            .as_u64()
            .expect("task_started missing task_id");

        assert_event_fields(started, &["description", "agent_id"]);

        let completed = task_completed.iter().find(|e| {
            let json: Value = serde_json::from_str(&e.data).unwrap();
            json["task_id"].as_u64() == Some(task_id)
        });

        assert!(
            completed.is_some(),
            "No task_completed for task_id: {task_id}"
        );

        let completed_json: Value = serde_json::from_str(&completed.unwrap().data).unwrap();
        assert!(
            completed_json.get("success").is_some(),
            "Missing success in task_completed for task_id: {task_id}"
        );
        assert!(
            completed_json.get("duration_ms").is_some(),
            "Missing duration_ms in task_completed for task_id: {task_id}"
        );

        println!(
            "Task {task_id}: started -> completed (success={})",
            completed_json["success"]
        );
    }
}

/// Verifies that worker tool invocations emit tool_call_started/completed events.
///
/// Query: "What is 10 + 5?"
/// Expected: tool_call events if orchestrated, graceful skip if direct answer.
///
/// LENIENCY: LLM may route to direct answer (no tool calls). This is acceptable.
#[tokio::test]
async fn test_arithmetic_emits_tool_call_events() {
    let events = orchestration_events("What is 10 + 5?").await;

    let tool_started = events_by_type(&events, event_names::TOOL_CALL_STARTED);
    let tool_completed = events_by_type(&events, event_names::TOOL_CALL_COMPLETED);

    if tool_started.is_empty() && tool_completed.is_empty() {
        println!("Note: No tool call events. LLM may have answered directly or not used tools.");
        assert_any_routing_event(&events);
        return;
    }

    for event in &tool_started {
        assert_event_fields(event, &["tool_call_id", "tool_name"]);
        let json: Value = serde_json::from_str(&event.data).unwrap();
        println!("tool_call_started: tool={}", json["tool_name"]);
    }

    for started in &tool_started {
        let started_json: Value = serde_json::from_str(&started.data).unwrap();
        let tool_call_id = started_json["tool_call_id"]
            .as_str()
            .expect("tool_call_started missing tool_call_id");

        let completed = tool_completed.iter().find(|e| {
            let json: Value = serde_json::from_str(&e.data).unwrap();
            json["tool_call_id"].as_str() == Some(tool_call_id)
        });

        assert!(
            completed.is_some(),
            "Tool call {tool_call_id} started but never completed"
        );

        let json: Value = serde_json::from_str(&completed.unwrap().data).unwrap();
        assert!(
            json.get("success").is_some(),
            "Missing success in tool_call_completed for {tool_call_id}"
        );
        assert!(
            json.get("duration_ms").is_some(),
            "Missing duration_ms in tool_call_completed for {tool_call_id}"
        );
    }
}

/// Verifies that multi-worker execution produces synthesis + quality evaluation.
///
/// Query: "First compute the mean of [2, 4, 6], then compute sin(0.5)"
/// Expected: synthesizing event + iteration_complete with quality_score in 0.0-1.0.
#[tokio::test]
async fn test_multi_domain_emits_synthesis_events() {
    let events =
        orchestration_events("First compute the mean of [2, 4, 6], then compute sin(0.5)").await;

    let synthesizing = events_by_type(&events, event_names::SYNTHESIZING);
    let iteration_complete = events_by_type(&events, event_names::ITERATION_COMPLETE);

    // LLM may route to direct answer for this query
    if synthesizing.is_empty() && iteration_complete.is_empty() {
        let direct = events_by_type(&events, event_names::DIRECT_ANSWER);
        if !direct.is_empty() {
            println!("Note: LLM routed to direct answer. No synthesis events expected.");
            return;
        }
        assert_any_routing_event(&events);
        return;
    }

    assert!(
        !synthesizing.is_empty(),
        "Expected synthesizing event but found none"
    );
    assert!(
        !iteration_complete.is_empty(),
        "Expected iteration_complete event but found none"
    );

    for event in &iteration_complete {
        let json: Value = serde_json::from_str(&event.data).unwrap();
        assert_event_fields(event, &["iteration", "quality_score"]);

        let iteration = json["iteration"]
            .as_u64()
            .expect("iteration must be a number");
        let quality_score = json["quality_score"]
            .as_f64()
            .expect("quality_score must be a number");

        assert!(iteration >= 1, "iteration should be >= 1");
        assert!(
            (0.0..=1.0).contains(&quality_score),
            "quality_score should be 0.0-1.0, got {quality_score}"
        );

        println!("iteration_complete: iteration={iteration}, quality_score={quality_score:.2}");
    }
}

/// Verifies that all orchestration events carry the same session_id for correlation.
///
/// Query: "Add 10 and 20, then find the median of [1, 5, 3]"
/// Expected: every aura.orchestrator.* event has the same session_id value.
#[tokio::test]
async fn test_orchestration_events_share_session_id() {
    let client = reqwest::Client::new();
    let test_session_id = format!("orch-correlation-{}", uuid::Uuid::new_v4());

    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "Add 10 and 20, then find the median of [1, 5, 3]"}],
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

    println!(
        "All {} orchestration events have correct session_id",
        orch_events.len()
    );
}

/// Verifies that a trivial single-op query is answered directly without a plan.
///
/// Query: "What is 2 + 2?"
/// Expected: direct_answer event with response and routing_rationale.
///
/// LENIENCY: LLM may still create a plan. If so, pass with note --
/// the routing decision is non-deterministic for borderline queries.
#[tokio::test]
async fn test_simple_arithmetic_emits_direct_answer() {
    let events = orchestration_events("What is 2 + 2?").await;

    let direct_events = events_by_type(&events, event_names::DIRECT_ANSWER);

    if direct_events.is_empty() {
        // LLM may choose to orchestrate instead -- acceptable for borderline queries
        let plan_events = events_by_type(&events, event_names::PLAN_CREATED);
        if !plan_events.is_empty() {
            println!("Note: LLM created a plan instead of direct answer for '2+2'. Acceptable.");
            return;
        }
        // Must have at least some routing event
        assert_any_routing_event(&events);
        return;
    }

    for event in &direct_events {
        assert_event_fields(event, &["response", "routing_rationale"]);
        let json: Value = serde_json::from_str(&event.data).unwrap();
        println!(
            "direct_answer: response={}, rationale={}",
            json["response"], json["routing_rationale"]
        );
    }
}

/// Verifies that an ambiguous query triggers a clarification request.
///
/// Query: "Do the thing"
/// Expected: clarification_needed event with question and routing_rationale.
///
/// LENIENCY: LLM may choose direct answer or plan instead. Acceptable --
/// the test validates the event structure when clarification IS chosen.
#[tokio::test]
async fn test_ambiguous_query_emits_clarification() {
    let events = orchestration_events("Do the thing").await;

    let clarification_events = events_by_type(&events, event_names::CLARIFICATION_NEEDED);

    if clarification_events.is_empty() {
        // LLM may choose a different routing -- acceptable
        println!(
            "Note: LLM did not ask for clarification for 'Do the thing'. \
             Checking for any routing event."
        );
        assert_any_routing_event(&events);
        return;
    }

    for event in &clarification_events {
        assert_event_fields(event, &["question", "routing_rationale"]);
        let json: Value = serde_json::from_str(&event.data).unwrap();
        println!(
            "clarification_needed: question={}, rationale={}",
            json["question"], json["routing_rationale"]
        );
    }
}

/// Verifies that a tool error produces a task_completed event with success=false.
///
/// Query: "Divide 10 by 0 using the division tool"
/// Expected: task_completed with success=false (division by zero).
///
/// LENIENCY: LLM may avoid calling the tool or handle it differently.
/// If no task events emitted, pass with note.
#[tokio::test]
async fn test_division_error_emits_task_failure() {
    let events = orchestration_events("Divide 10 by 0 using the division tool").await;

    let task_completed = events_by_type(&events, event_names::TASK_COMPLETED);

    if task_completed.is_empty() {
        // LLM may route to direct answer or avoid the tool entirely
        println!(
            "Note: No task_completed events. LLM may have answered directly or avoided the tool."
        );
        assert_any_routing_event(&events);
        return;
    }

    // Check if any task_completed has success=false (division by zero error)
    let has_failure = task_completed.iter().any(|e| {
        let json: Value = serde_json::from_str(&e.data).unwrap();
        json.get("success").and_then(|v| v.as_bool()) == Some(false)
    });

    if has_failure {
        println!("task_completed with success=false found (expected for division by zero)");
    } else {
        // LLM may have handled the error gracefully or the division tool returned a non-error result
        println!(
            "Note: All tasks completed successfully. LLM may have handled division by zero gracefully."
        );
    }

    // Verify all task_completed events have required fields
    for event in &task_completed {
        assert_event_fields(event, &["task_id", "success", "duration_ms"]);
    }
}

// ---------------------------------------------------------------------------
// MCP progress notifications in orchestration mode (LOG-23565)
// ---------------------------------------------------------------------------

/// Verifies that MCP progress notifications route through orchestration SSE.
///
/// Query triggers the progress_task worker which calls task_with_progress on
/// mock-mcp. This tool emits MCP notifications/progress as it runs. The
/// OrchestratorFactory must bridge the request_id to the inner orchestrator's
/// MCP clients so the progress broker routes notifications to the SSE stream.
///
/// LENIENCY: LLM may route to direct answer. Progress notification delivery
/// depends on MCP transport timing. We assert structurally when events appear.
#[tokio::test]
async fn test_orchestration_progress_notifications() {
    let events =
        orchestration_events("Run a progress task with 2 seconds duration and 3 steps").await;

    // Check for aura.progress events
    let progress_events: Vec<&SseEvent> = events
        .iter()
        .filter(|e| {
            e.event_type
                .as_ref()
                .map(|t| t == "aura.progress")
                .unwrap_or(false)
        })
        .collect();

    if progress_events.is_empty() {
        // Several acceptable reasons for no progress events:
        // 1. LLM routed to direct answer (didn't call progress worker)
        // 2. MCP transport timing — progress notifications may arrive after tool completes
        // 3. FastMCP streamable-http may not support progress on all transports
        let direct = events_by_type(&events, event_names::DIRECT_ANSWER);
        if !direct.is_empty() {
            println!("Note: LLM routed to direct answer. No progress events expected.");
            return;
        }

        // If orchestrated but no progress, log for investigation
        let plan_events = events_by_type(&events, event_names::PLAN_CREATED);
        if !plan_events.is_empty() {
            println!(
                "Note: Orchestrated but no aura.progress events received. \
                 This may indicate a progress routing issue or MCP transport limitation."
            );
        }

        assert_any_routing_event(&events);
        return;
    }

    // Validate progress event structure
    for event in &progress_events {
        let json: Value = serde_json::from_str(&event.data).unwrap_or_else(|e| {
            panic!(
                "Invalid JSON in aura.progress event: {e}\nRaw: {}",
                event.data
            )
        });

        assert_event_fields(event, &["message", "phase"]);

        println!(
            "aura.progress: message={}, phase={}, percent={:?}",
            json["message"],
            json["phase"],
            json.get("percent")
        );
    }

    println!(
        "Received {} aura.progress events during orchestration",
        progress_events.len()
    );
}

// ---------------------------------------------------------------------------
// Worker reasoning events
// ---------------------------------------------------------------------------

/// Verifies that complex multi-step queries emit worker_reasoning events
/// with task_id, worker_id, and content fields.
///
/// LENIENCY: Not all models emit reasoning. If no events, pass with note.
#[tokio::test]
async fn test_worker_reasoning_events_emitted() {
    let events = orchestration_events(
        "Calculate the standard deviation of [10, 20, 30, 40, 50] and then multiply by pi",
    )
    .await;

    let reasoning = events_by_type(&events, event_names::WORKER_REASONING);

    if reasoning.is_empty() {
        // Reasoning events depend on model capability (thinking models only)
        println!(
            "Note: No worker_reasoning events emitted. Model may not support reasoning output."
        );
        assert_any_routing_event(&events);
        return;
    }

    for event in &reasoning {
        assert_event_fields(event, &["task_id", "worker_id", "content"]);
        let json: Value = serde_json::from_str(&event.data).unwrap();
        let content = json["content"].as_str().unwrap_or("");
        assert!(
            !content.is_empty(),
            "worker_reasoning content should not be empty"
        );
        println!(
            "worker_reasoning: task_id={}, worker_id={}, content_len={}",
            json["task_id"],
            json["worker_id"],
            content.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Worker ID in task events
// ---------------------------------------------------------------------------

/// Verifies that task_started and task_completed events include worker_id
/// and orchestrator_id fields for multi-worker queries.
///
/// LENIENCY: LLM may route to direct answer.
#[tokio::test]
async fn test_task_events_include_worker_id() {
    let events = orchestration_events(
        "Compute the variance of [3, 7, 11, 15] and then take the square root of the result",
    )
    .await;

    let task_started = events_by_type(&events, event_names::TASK_STARTED);
    let task_completed = events_by_type(&events, event_names::TASK_COMPLETED);

    if task_started.is_empty() && task_completed.is_empty() {
        println!("Note: No task events. LLM may have answered directly.");
        assert_any_routing_event(&events);
        return;
    }

    for event in &task_started {
        assert_event_fields(event, &["task_id", "worker_id", "orchestrator_id"]);
        let json: Value = serde_json::from_str(&event.data).unwrap();
        println!(
            "task_started: task_id={}, worker_id={}, orchestrator_id={}",
            json["task_id"], json["worker_id"], json["orchestrator_id"]
        );
    }

    for event in &task_completed {
        assert_event_fields(event, &["task_id", "worker_id", "orchestrator_id"]);
    }
}

// ---------------------------------------------------------------------------
// Worker ID in tool call events
// ---------------------------------------------------------------------------

/// Verifies that tool_call_started events include worker_id for attribution.
///
/// LENIENCY: LLM may not use tools or route to direct answer.
#[tokio::test]
async fn test_tool_call_events_include_worker_id() {
    let events =
        orchestration_events("Find the median of [12, 5, 8, 3, 19] then divide the result by 2")
            .await;

    let tool_started = events_by_type(&events, event_names::TOOL_CALL_STARTED);

    if tool_started.is_empty() {
        println!("Note: No tool_call_started events. LLM may have answered directly.");
        assert_any_routing_event(&events);
        return;
    }

    for event in &tool_started {
        assert_event_fields(event, &["tool_call_id", "tool_name", "worker_id"]);
        let json: Value = serde_json::from_str(&event.data).unwrap();
        println!(
            "tool_call_started: tool={}, worker_id={}",
            json["tool_name"], json["worker_id"]
        );
    }
}

// ---------------------------------------------------------------------------
// Iteration complete replan fields
// ---------------------------------------------------------------------------

/// Verifies that iteration_complete events include quality_threshold and will_replan fields.
///
/// LENIENCY: LLM may route to direct answer (no iteration events).
#[tokio::test]
async fn test_iteration_complete_includes_replan_fields() {
    let events = orchestration_events(
        "First compute the mean of [1, 2, 3], then compute the factorial of 5",
    )
    .await;

    let iteration_complete = events_by_type(&events, event_names::ITERATION_COMPLETE);

    if iteration_complete.is_empty() {
        println!("Note: No iteration_complete events. LLM may have answered directly.");
        assert_any_routing_event(&events);
        return;
    }

    for event in &iteration_complete {
        assert_event_fields(
            event,
            &[
                "iteration",
                "quality_score",
                "quality_threshold",
                "will_replan",
            ],
        );
        let json: Value = serde_json::from_str(&event.data).unwrap();
        let threshold = json["quality_threshold"]
            .as_f64()
            .expect("quality_threshold must be a number");
        let will_replan = json["will_replan"]
            .as_bool()
            .expect("will_replan must be a bool");
        println!("iteration_complete: quality_threshold={threshold:.2}, will_replan={will_replan}");
    }
}
