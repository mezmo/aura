#![cfg(feature = "integration-a2a")]

//! Integration tests for the local A2A streaming overrides in `a2a/overrides.rs`.
//!
//! ## What these tests can and can't prove
//!
//! The bug is a microsecond-scale race between snapshot and `subscribe()` in the upstream
//! REST handlers. An over-the-network integration test can't deterministically force the
//! worker to emit the terminal event *inside* that window — even with a 0-duration tool,
//! the LLM round-trip dominates and the race window is comfortably empty. So the asserts
//! here verify that the override path:
//!
//! 1. **Functions end-to-end** — the route is wired, SSE events flow, the initial `task`
//!    arrives first, a terminal `statusUpdate` arrives before timeout.
//! 2. **Probabilistically catches the race** when it does occur — both tests use the
//!    shortest reasonable task duration and `:subscribe` runs an inner loop of N attempts
//!    to raise the chance of landing inside the race window across CI runs. A run that
//!    hangs without the fix would be observable as a timeout failure.
//!
//! For a deterministic test of the *fix mechanics* (subscribe-before-snapshot, post-lag
//! terminal break), see the unit tests in `src/a2a/overrides.rs::tests` — those drive a
//! `broadcast::channel` and `TaskStore` directly and don't depend on timing.
//!
//! Requires a running aura-web-server with the test config (`slow_task` MCP tool).

use aura_test_utils::server_urls::AURA_SERVER;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_TIMEOUT: Duration = Duration::from_secs(60);

/// `:stream` happy path.
///
/// `duration_seconds=0` makes the tool return as fast as possible so the worker
/// terminates promptly. We don't expect the race to manifest in CI under network latency,
/// but if it ever does, this test will hang past `STREAM_TIMEOUT` and fail loudly.
#[tokio::test]
async fn test_a2a_stream_happy_path() {
    let message_id = uuid::Uuid::new_v4().to_string();
    let request_text = format!(
        "Call the slow_task tool immediately with message_id='{}' and duration_seconds=0. \
         Do not say anything else, just call the tool.",
        message_id
    );

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/a2a/v1/message:stream", AURA_SERVER))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("A2A-Version", "1.0")
        .json(&json!({
            "message": {
                "messageId": message_id,
                "role": "ROLE_USER",
                "parts": [{ "text": request_text }],
            }
        }))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .expect("network failure - is aura-web-server running?");

    assert_eq!(
        response.status(),
        200,
        ":stream should return 200 OK; got body: {:?}",
        response.text().await.ok()
    );

    let events = collect_until_terminal(response).await;
    assert_terminal(&events, ":stream");
}

/// `:subscribe` mid-task across `ATTEMPTS` iterations.
///
/// Each iteration: POST `:send` to start a near-instant task, then immediately GET
/// `:subscribe`. The shorter the task, the higher the chance the worker's terminal event
/// fires inside the upstream snapshot-then-subscribe window. Running the loop bumps the
/// cumulative probability of hitting the race in a single CI invocation. If any iteration
/// hangs we'll exceed `STREAM_TIMEOUT` and the test fails.
///
/// We don't assert that the race *does* occur — we assert that the stream survives it
/// (terminal `statusUpdate` arrives) every iteration.
#[tokio::test]
async fn test_a2a_subscribe_mid_task_receives_terminal() {
    const ATTEMPTS: usize = 5;

    for attempt in 0..ATTEMPTS {
        let message_id = uuid::Uuid::new_v4().to_string();
        let request_text = format!(
            "Call the slow_task tool immediately with message_id='{}' and duration_seconds=0. \
             Do not say anything else, just call the tool.",
            message_id
        );

        let client = reqwest::Client::new();
        let send_response = client
            .post(format!("{}/a2a/v1/message:send", AURA_SERVER))
            .header("Content-Type", "application/json")
            .header("A2A-Version", "1.0")
            .json(&json!({
                "message": {
                    "messageId": message_id,
                    "role": "ROLE_USER",
                    "parts": [{ "text": request_text }],
                }
            }))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .expect("network failure - is aura-web-server running?");

        assert_eq!(send_response.status(), 200);
        let send_body: Value = send_response
            .json()
            .await
            .expect(":send response was not JSON");
        let task_id = send_body
            .get("task")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .expect(":send response missing task.id")
            .to_string();

        // Subscribe with no delay — this is the moment the upstream race can drop the
        // terminal event between snapshot and subscribe. The override subscribes first,
        // so any event from this point on (including ones already buffered) is delivered.
        let subscribe_response = client
            .get(format!(
                "{}/a2a/v1/tasks/{}:subscribe",
                AURA_SERVER, task_id
            ))
            .header("Accept", "text/event-stream")
            .header("A2A-Version", "1.0")
            .send()
            .await
            .expect("network failure subscribing to task");

        // The task may have already terminated before we got here. In that case upstream
        // (and the override) return 400 "cannot subscribe to terminal state" — that's a
        // valid outcome that proves the fix works (no hang), so we skip to the next
        // attempt rather than failing.
        if subscribe_response.status() == 400 {
            continue;
        }
        assert_eq!(
            subscribe_response.status(),
            200,
            "attempt {attempt}: :subscribe should return 200 or 400; got body: {:?}",
            subscribe_response.text().await.ok()
        );

        let events = collect_until_terminal(subscribe_response).await;
        assert_terminal(&events, &format!(":subscribe attempt {attempt}"));
    }
}

fn assert_terminal(events: &[(String, Value)], context: &str) {
    let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();

    assert_eq!(
        kinds.first().copied(),
        Some("task"),
        "{context}: must emit the initial `task` event first; got {:?}",
        kinds
    );

    let terminal = events.iter().rev().find(|(k, _)| k == "statusUpdate");
    assert!(
        terminal.is_some(),
        "{context}: did not deliver a terminal `statusUpdate` before timeout — this is \
         exactly the symptom of the upstream race the override fixes. Events seen: {:?}",
        kinds
    );

    let state = terminal
        .and_then(|(_, v)| v.get("statusUpdate"))
        .and_then(|v| v.get("status"))
        .and_then(|v| v.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        is_terminal_state(state),
        "{context}: final status state should be terminal; got {state:?}"
    );
}

/// Drain the SSE response, returning `(variant_tag, full_event_json)` for each
/// `StreamingMessageResult` event, stopping after the first terminal status update or when
/// `STREAM_TIMEOUT` elapses.
async fn collect_until_terminal(response: reqwest::Response) -> Vec<(String, Value)> {
    use std::sync::{Arc, Mutex};
    let out: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut stream = response.bytes_stream().eventsource();
    let drain_out = out.clone();
    let drain = async move {
        while let Some(event) = stream.next().await {
            let Ok(event) = event else { break };
            if event.data.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            // StreamingMessageResult is externally tagged camelCase: `{"task": {...}}` etc.
            let Some(tag) = value.as_object().and_then(|o| o.keys().next().cloned()) else {
                continue;
            };

            let is_terminal = tag == "statusUpdate"
                && value
                    .get("statusUpdate")
                    .and_then(|s| s.get("status"))
                    .and_then(|s| s.get("state"))
                    .and_then(|s| s.as_str())
                    .map(is_terminal_state)
                    .unwrap_or(false);

            drain_out.lock().unwrap().push((tag, value));

            if is_terminal {
                break;
            }
        }
    };

    let _ = timeout(STREAM_TIMEOUT, drain).await;
    let guard = out.lock().unwrap();
    guard.clone()
}

fn is_terminal_state(state: &str) -> bool {
    matches!(
        state,
        "TASK_STATE_COMPLETED"
            | "TASK_STATE_FAILED"
            | "TASK_STATE_CANCELED"
            | "TASK_STATE_REJECTED"
            | "completed"
            | "failed"
            | "canceled"
            | "rejected"
    )
}
