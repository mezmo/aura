#![cfg(feature = "integration-orchestration-sre")]

use aura::orchestration::event_names;
use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::sse::{SseEvent, events_by_type, parse_sse_stream};
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(300);

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

/// Helper to extract all tool calls with their names and arguments from events.
fn get_tool_calls(events: &[SseEvent]) -> Vec<(String, Value)> {
    events_by_type(events, event_names::TOOL_CALL_STARTED)
        .iter()
        .filter_map(|e| {
            let json: Value = serde_json::from_str(&e.data).ok()?;
            let tool_name = json["tool_name"].as_str()?.to_string();
            let arguments = json.get("arguments").cloned().unwrap_or(Value::Null);
            Some((tool_name, arguments))
        })
        .collect()
}

/// Helper to extract the telemetry query string from tool arguments.
fn extract_telemetry_query(arguments: &Value) -> Option<String> {
    if let Some(s) = arguments.as_str() {
        if let Ok(json) = serde_json::from_str::<Value>(s)
            && let Some(q) = json.get("query").and_then(|v| v.as_str())
        {
            return Some(q.to_string());
        }
        return Some(s.to_string());
    }
    if let Some(q) = arguments.get("query").and_then(|v| v.as_str()) {
        return Some(q.to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verifies that a full SRE workflow triggers orchestration planning with
/// lifecycle events (plan, tasks, tool calls, continuation).
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

    // Post-execute coordinator picks a terminal routing tool after the
    // tasks execute.
    let iteration_complete = events_by_type(&events, event_names::ITERATION_COMPLETE);
    let direct = events_by_type(&events, event_names::DIRECT_ANSWER);
    let clarification = events_by_type(&events, event_names::CLARIFICATION_NEEDED);
    assert!(
        !iteration_complete.is_empty(),
        "Expected iteration_complete event after orchestration"
    );
    assert!(
        !direct.is_empty() || !clarification.is_empty(),
        "Expected direct_answer or clarification_needed from post-execute continuation"
    );
    for event in &iteration_complete {
        let json: Value = serde_json::from_str(&event.data).unwrap();
        println!("iteration_complete: iteration={}", json["iteration"]);
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

    // Inspect tool call arguments when telemetry queries are executed
    let tool_calls = get_tool_calls(&events);
    for (name, args) in &tool_calls {
        if name == "prometheus_query" {
            let query_str = extract_telemetry_query(args).unwrap_or_default();
            println!("prometheus_query arguments inspected: {query_str}");
            assert!(
                !query_str.is_empty(),
                "prometheus_query argument 'query' should not be empty"
            );
        }
    }
}

/// Verifies that telemetry investigations without an explicit timeframe
/// inspect a 5-minute lookback window in query arguments by default.
///
/// Query: asks to investigate recent error rates and request count metrics for payment-service in production.
/// Expected: prometheus_query tool arguments contain a 5-minute lookback interval (e.g. "[5m]" or "5m").
///
/// LENIENCY: LLM may answer directly without tool calls.
#[tokio::test]
async fn test_sre_telemetry_query_defaults_to_five_minute_window() {
    let events = orchestration_events(
        "Query Prometheus metrics to inspect the request rate and error rate for the payment-service workload in production.",
    )
    .await;

    let tool_started = events_by_type(&events, event_names::TOOL_CALL_STARTED);

    if tool_started.is_empty() {
        println!("Note: No tool call events. LLM may have answered directly.");
        assert_any_routing_event(&events);
        return;
    }

    let tool_calls = get_tool_calls(&events);
    let prom_queries: Vec<String> = tool_calls
        .iter()
        .filter(|(name, _)| name == "prometheus_query")
        .filter_map(|(_, args)| extract_telemetry_query(args))
        .collect();

    println!("Default window queries: {:?}", prom_queries);

    if !prom_queries.is_empty() {
        // Inspect telemetry query arguments: verify default 5m lookback window
        let has_5m_window = prom_queries
            .iter()
            .any(|q| q.contains("5m") || q.contains("[5m]") || q.contains("300"));
        assert!(
            has_5m_window,
            "Expected prometheus_query arguments to inspect default 5-minute telemetry window (e.g. '[5m]'), got: {:?}",
            prom_queries
        );

        // Ensure the worker did not query an arbitrary wide window by default
        for q in &prom_queries {
            assert!(
                !q.contains("[1h]") && !q.contains("[24h]") && !q.contains("[7d]"),
                "Telemetry query should not use a wide window when 5m is expected: {q}"
            );
        }
    } else {
        assert_any_routing_event(&events);
    }
}

/// Verifies that an explicit user-requested timeframe is respected in telemetry query
/// arguments and NOT overwritten by the default 5-minute window.
///
/// Query: asks to investigate error rates and request latency for payment-service in production over the last 1 hour.
/// Expected: prometheus_query tool arguments contain a 1-hour lookback interval (e.g. "[1h]" or "1h")
/// and do not overwrite it with the 5-minute default.
///
/// LENIENCY: LLM may answer directly without tool calls.
#[tokio::test]
async fn test_sre_telemetry_query_respects_explicit_override_window() {
    let events = orchestration_events(
        "Query Prometheus metrics to inspect the request rate and error rate for the payment-service workload in production over the last 1 hour.",
    )
    .await;

    let tool_started = events_by_type(&events, event_names::TOOL_CALL_STARTED);

    if tool_started.is_empty() {
        println!("Note: No tool call events. LLM may have answered directly.");
        assert_any_routing_event(&events);
        return;
    }

    let tool_calls = get_tool_calls(&events);
    let prom_queries: Vec<String> = tool_calls
        .iter()
        .filter(|(name, _)| name == "prometheus_query")
        .filter_map(|(_, args)| extract_telemetry_query(args))
        .collect();

    println!("Override window queries: {:?}", prom_queries);

    if !prom_queries.is_empty() {
        // Inspect telemetry query arguments: verify user-requested 1h override is used
        let has_1h_window = prom_queries.iter().any(|q| {
            q.contains("1h") || q.contains("[1h]") || q.contains("60m") || q.contains("3600")
        });
        assert!(
            has_1h_window,
            "Expected prometheus_query arguments to reflect user-requested 1-hour lookback window (e.g. '[1h]'), got: {:?}",
            prom_queries
        );

        // Verify the 5m default did NOT overwrite the user's explicit 1h request
        let mistakenly_used_5m = prom_queries
            .iter()
            .all(|q| q.contains("[5m]") && !q.contains("1h"));
        assert!(
            !mistakenly_used_5m,
            "Telemetry query should not overwrite explicit 1-hour request with default 5m window: {:?}",
            prom_queries
        );
    } else {
        assert_any_routing_event(&events);
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

/// Verifies multi-domain SRE queries orchestrate and terminate via a routing
/// tool emitted by the post-execute continuation coordinator.
///
/// Query requires discovery then configuration — spanning multiple workers.
/// Expected: plan_created + iteration_complete + a terminal routing event
/// (direct_answer on the happy path; clarification_needed is also valid).
#[tokio::test]
async fn test_sre_multi_domain_routes_to_terminal() {
    let events = orchestration_events(
        "Find all workloads with metrics ports in the production namespace, \
         then create ServiceMonitors for payment-service and user-api.",
    )
    .await;

    let plan_created = events_by_type(&events, event_names::PLAN_CREATED);
    let iteration_complete = events_by_type(&events, event_names::ITERATION_COMPLETE);
    let direct = events_by_type(&events, event_names::DIRECT_ANSWER);
    let clarification = events_by_type(&events, event_names::CLARIFICATION_NEEDED);

    // The coordinator may route directly on iter-1 (no orchestration).
    if plan_created.is_empty() {
        assert!(
            !direct.is_empty() || !clarification.is_empty(),
            "Expected a terminal routing event (direct_answer or clarification_needed)"
        );
        println!("Note: coordinator routed without orchestration.");
        return;
    }

    assert!(
        !iteration_complete.is_empty(),
        "Expected iteration_complete event but found none"
    );
    assert!(
        !direct.is_empty() || !clarification.is_empty(),
        "Expected direct_answer or clarification_needed from post-execute continuation"
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
