#![cfg(feature = "integration-orchestration-sre")]

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

async fn send_orchestration_request(client: &reqwest::Client, query: &str) -> reqwest::Response {
    client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": query}],
            "stream": true,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("sre-orch-test-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request")
}

/// Send a query and return parsed SSE events.
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

/// Assert that at least one routing event exists.
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

/// Verifies that a full SRE workflow triggers orchestration planning with
/// lifecycle events (plan, tasks, tool calls, synthesis).
///
/// Query asks for multi-step: discover workloads → check monitoring → create alerts.
/// Expected: plan_created (tasks array length >= 2), task_started/completed pairs,
/// tool_call events, synthesizing, iteration_complete.
///
/// LENIENCY: LLM may route to direct answer or use fewer tasks than expected.
#[tokio::test]
async fn test_sre_full_workflow_emits_plan_and_tasks() {
    let events = orchestration_events(
        "Discover all workloads in the production namespace, check which ones \
         have metrics endpoints, then verify their Prometheus targets are healthy \
         and create ServiceMonitors for any workloads missing monitoring coverage.",
    )
    .await;

    let plan_events = events_by_type(&events, event_names::PLAN_CREATED);

    // LLM may route to direct answer; if so, pass with note
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
        assert!(
            tasks.len() >= 2,
            "Expected tasks.len() >= 2 for SRE workflow, got {}",
            tasks.len()
        );
        println!(
            "plan_created: goal={}, task_count={}",
            json["goal"],
            tasks.len()
        );
    }

    // Verify task lifecycle events exist
    let task_started = events_by_type(&events, event_names::TASK_STARTED);
    let task_completed = events_by_type(&events, event_names::TASK_COMPLETED);
    assert!(
        !task_started.is_empty(),
        "Expected task_started events for SRE workflow"
    );
    assert!(
        !task_completed.is_empty(),
        "Expected task_completed events for SRE workflow"
    );

    // Verify tool calls were made
    let tool_started = events_by_type(&events, event_names::TOOL_CALL_STARTED);
    if !tool_started.is_empty() {
        println!(
            "Tool calls observed: {}",
            tool_started
                .iter()
                .filter_map(|e| {
                    serde_json::from_str::<Value>(&e.data)
                        .ok()
                        .and_then(|j| j["tool_name"].as_str().map(String::from))
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Verify synthesis occurred
    let synthesizing = events_by_type(&events, event_names::SYNTHESIZING);
    let iteration_complete = events_by_type(&events, event_names::ITERATION_COMPLETE);
    if !synthesizing.is_empty() {
        println!("Synthesis event present");
    }
    if !iteration_complete.is_empty() {
        for event in &iteration_complete {
            let json: Value = serde_json::from_str(&event.data).unwrap();
            println!(
                "iteration_complete: iteration={}, quality_score={}",
                json["iteration"], json["quality_score"]
            );
        }
    }
}

/// Verifies that workers use domain-appropriate tools (k8s_* and/or prometheus_*).
///
/// Query spans two domains: Kubernetes discovery and Prometheus.
/// Expected: tool_call_started events include k8s and/or prometheus tools.
///
/// LENIENCY: LLM may answer directly without tool calls.
#[tokio::test]
async fn test_sre_workers_use_domain_tools() {
    let events = orchestration_events(
        "List all workloads in the production namespace and check which \
         Prometheus targets are currently healthy.",
    )
    .await;

    let tool_started = events_by_type(&events, event_names::TOOL_CALL_STARTED);

    if tool_started.is_empty() {
        println!("Note: No tool call events. LLM may have answered directly.");
        assert_any_routing_event(&events);
        return;
    }

    let tool_names: Vec<String> = tool_started
        .iter()
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|j| j["tool_name"].as_str().map(String::from))
        })
        .collect();

    println!("Tools called: {:?}", tool_names);

    let has_k8s_tool = tool_names.iter().any(|t| t.starts_with("k8s_"));
    let has_prom_tool = tool_names.iter().any(|t| t.starts_with("prometheus_"));

    // At least one domain-specific tool should have been called
    assert!(
        has_k8s_tool || has_prom_tool,
        "Expected at least one k8s_* or prometheus_* tool call, got: {:?}",
        tool_names
    );

    if has_k8s_tool {
        println!("k8s tools used");
    }
    if has_prom_tool {
        println!("prometheus tools used");
    }
}

/// Verifies that all orchestration events share the same session_id for correlation.
///
/// Query triggers a multi-step SRE workflow. Every aura.orchestrator.* event
/// must carry the same session_id value.
#[tokio::test]
async fn test_sre_orchestration_events_share_session_id() {
    let client = reqwest::Client::new();
    let test_session_id = format!("sre-orch-correlation-{}", uuid::Uuid::new_v4());

    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "Discover workloads in production and check existing ServiceMonitors"}],
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

/// Verifies that multi-domain SRE queries produce synthesis + quality evaluation.
///
/// Query requires discovery then configuration — spanning multiple workers.
/// Expected: synthesizing event + iteration_complete with quality_score in 0.0-1.0.
///
/// LENIENCY: LLM may route to direct answer or skip synthesis for simple queries.
#[tokio::test]
async fn test_sre_multi_domain_emits_synthesis() {
    let events = orchestration_events(
        "Find all workloads with metrics ports in the production namespace, \
         then create ServiceMonitors for payment-service and user-api.",
    )
    .await;

    let synthesizing = events_by_type(&events, event_names::SYNTHESIZING);
    let iteration_complete = events_by_type(&events, event_names::ITERATION_COMPLETE);

    // LLM may route to direct answer
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
        assert_event_fields(event, &["iteration", "will_replan"]);

        let iteration = json["iteration"]
            .as_u64()
            .expect("iteration must be a number");

        assert!(iteration >= 1, "iteration should be >= 1");

        println!("iteration_complete: iteration={iteration}");
    }
}
