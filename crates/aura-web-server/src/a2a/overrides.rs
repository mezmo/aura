//! Local overrides for the `a2a-rs-server` REST streaming endpoints.
//!
//! These handlers re-implement [`POST /a2a/v1/message:stream`] and
//! [`GET /a2a/v1/tasks/{id}:subscribe`] with one critical change: they subscribe to the
//! broadcast channel **before** invoking the handler / reading the task snapshot.
//!
//! ## The bug we're working around
//!
//! Upstream `a2a-rs-server` 1.0.26 captures the task snapshot first, then subscribes:
//!
//! ```text
//! task = task_store.get(id).await;     // snapshot
//! rx   = event_tx.subscribe();          // <-- any event broadcast between these
//!                                       //     two lines is dropped
//! ```
//!
//! For synchronous handlers this is mostly harmless. But [`super::AuraMessageHandler`] spawns
//! a background worker and returns immediately — the worker can (and does) emit the terminal
//! `StatusUpdate(Completed)` before the upstream code reaches `event_tx.subscribe()`. The
//! event is persisted to the task store but never reaches the subscriber, which then loops on
//! `rx.recv()` forever waiting for a terminal event that already happened.
//!
//! ## The fix
//!
//! Subscribe to the broadcast channel first, then snapshot. The race window collapses to "may
//! deliver one duplicate event," and A2A clients dedupe by `task_id`/`artifact_id` so that's
//! harmless.
//!
//! JSON-RPC streaming methods (`message/stream`, `tasks/resubscribe`) have the same bug
//! upstream but are out of scope here — overriding them would require intercepting the entire
//! `/a2a/v1/rpc` endpoint and re-dispatching every method. See `docs/a2a-implementation.md`.

use std::convert::Infallible;
use std::sync::Arc;

use a2a_rs_core::{
    Message, SendMessageRequest, SendMessageResponse, StreamResponse, StreamingMessageResult, Task,
    TaskStatus, TaskStatusUpdateEvent, now_iso8601,
};
use a2a_rs_server::{MessageHandler, TaskStore};
use async_stream::stream;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::broadcast;
use tracing::warn;

use crate::a2a::{AuraMessageHandler, extract_auth_context};
use crate::types::AppState;

/// `GET /a2a/v1/tasks/{id}:subscribe`
///
/// Re-implementation of `rest_subscribe_to_task` from `a2a-rs-server-1.0.26/src/rest.rs:613`
/// that subscribes to the broadcast channel before snapshotting the task.
pub async fn subscribe_to_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let (event_tx, task_store) = match resolve_a2a_state(&state) {
        ResolvedState::Ready(parts) => parts,
        ResolvedState::Error(resp) => return resp,
    };

    // SUBSCRIBE FIRST. Any event emitted between this line and the snapshot below is
    // captured by `rx` and replayed in the stream loop, which is the whole point of this
    // override.
    let mut rx = event_tx.subscribe();

    let task = match task_store.get_flexible(&id).await {
        Some(t) => t,
        None => return aip_error(StatusCode::NOT_FOUND, "task not found"),
    };

    if task.status.state.is_terminal() {
        return aip_error(
            StatusCode::BAD_REQUEST,
            "cannot subscribe to a task in terminal state",
        );
    }

    let target_task_id = task.id.clone();
    // Cache `context_id` from the snapshot we just read so we can match
    // `StreamResponse::Message` events without re-fetching the store. The upstream code
    // uses a non-blocking store fetch here, which silently drops events under contention.
    let target_context_id = task.context_id.clone();
    let task_store_for_stream = task_store.clone();

    let body = stream! {
        if let Some(ev) = task_to_event(&task) {
            yield Ok::<_, Infallible>(ev);
        }

        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !event_matches_task(&event, &target_task_id, &target_context_id) {
                        continue;
                    }
                    let is_terminal = is_terminal_event(&event);
                    if let Some(ev) = stream_response_to_event(event) {
                        yield Ok(ev);
                    }
                    if is_terminal {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // The upstream crate silently swallows Lagged. We surface it so clients
                    // know events were dropped — A2A clients ignore unknown metadata keys.
                    warn!(
                        task_id = %target_task_id,
                        dropped = n,
                        "broadcast receiver lagged; events dropped"
                    );
                    let outcome =
                        handle_lagged(&target_task_id, &task_store_for_stream, n).await;
                    if let Some(ev) = outcome.event {
                        yield Ok(ev);
                    }
                    // If the dropped events included the terminal StatusUpdate the channel
                    // will never deliver another event for this task and the loop would
                    // hang. Break on terminal state read from the store post-lag.
                    if outcome.is_terminal {
                        break;
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(body)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `POST /a2a/v1/message:stream`
///
/// Re-implementation of `rest_send_streaming_message` from
/// `a2a-rs-server-1.0.26/src/rest.rs:397`. Subscribes before invoking the handler so events
/// emitted by the handler's spawned worker (see [`AuraMessageHandler::handle_message`]) are
/// caught in the stream.
pub async fn send_streaming_message(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(params): Json<SendMessageRequest>,
) -> Response {
    let (event_tx, task_store) = match resolve_a2a_state(&state) {
        ResolvedState::Ready(parts) => parts,
        ResolvedState::Error(resp) => return resp,
    };

    if params.message.parts.is_empty() {
        return aip_error(StatusCode::BAD_REQUEST, "message parts must not be empty");
    }

    let auth = extract_auth_context(&headers);

    // SUBSCRIBE FIRST. `handle_message` immediately spawns a worker that can broadcast
    // before this function returns; without an outstanding subscriber, tokio would drop
    // those events.
    let mut rx = event_tx.subscribe();

    let handler = AuraMessageHandler::new(
        state.clone(),
        state.a2a_event_tx.clone(),
        state.a2a_task_store.clone(),
    );

    let response = match handler.handle_message(params.message, auth).await {
        Ok(r) => r,
        Err(e) => return handler_error_response(e),
    };

    let task = match response {
        SendMessageResponse::Task(t) => t,
        SendMessageResponse::Message(m) => return single_message_sse(m),
    };

    // The handler already inserted the task; this is an idempotent overwrite that mirrors
    // the upstream `rest_send_streaming_message` flow.
    task_store.insert(task.clone()).await;

    // Broadcast the initial Task event so any *other* subscribers (e.g. webhooks) see it.
    // Our own `rx` will receive a copy too — the SSE loop below filters it by id, so we
    // skip it there and yield the locally-held `task` as the first SSE event below.
    let _ = event_tx.send(StreamResponse::Task(task.clone()));

    let target_task_id = task.id.clone();
    let target_context_id = task.context_id.clone();
    let task_store_for_stream = task_store.clone();
    let initial_is_terminal = task.status.state.is_terminal();

    let body = stream! {
        if let Some(ev) = task_to_event(&task) {
            yield Ok::<_, Infallible>(ev);
        }

        if initial_is_terminal {
            return;
        }

        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Skip the initial Task echo we just broadcast — already yielded above.
                    if matches!(&event, StreamResponse::Task(t) if t.id == target_task_id) {
                        continue;
                    }
                    if !event_matches_task(&event, &target_task_id, &target_context_id) {
                        continue;
                    }
                    let is_terminal = is_terminal_event(&event);
                    if let Some(ev) = stream_response_to_event(event) {
                        yield Ok(ev);
                    }
                    if is_terminal {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        task_id = %target_task_id,
                        dropped = n,
                        "broadcast receiver lagged; events dropped"
                    );
                    let outcome =
                        handle_lagged(&target_task_id, &task_store_for_stream, n).await;
                    if let Some(ev) = outcome.event {
                        yield Ok(ev);
                    }
                    // Same reasoning as in `subscribe_to_task`: if the dropped events
                    // included the terminal StatusUpdate we must break, not continue.
                    if outcome.is_terminal {
                        break;
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(body)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Two-state enum (rather than `Result`) to keep the large `Response` value out of a
/// `Result::Err`, which clippy flags as `result_large_err`.
enum ResolvedState {
    Ready((broadcast::Sender<StreamResponse>, TaskStore)),
    Error(Response),
}

fn resolve_a2a_state(state: &Arc<AppState>) -> ResolvedState {
    let Some(event_tx) = state.a2a_event_tx.get().cloned() else {
        return ResolvedState::Error(aip_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "a2a streaming not initialized",
        ));
    };
    let Some(task_store) = state.a2a_task_store.get().cloned() else {
        return ResolvedState::Error(aip_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "a2a task store not initialized",
        ));
    };
    ResolvedState::Ready((event_tx, task_store))
}

fn single_message_sse(message: Message) -> Response {
    let body = stream! {
        if let Ok(val) = serde_json::to_value(StreamingMessageResult::Message(message)) {
            let text = serde_json::to_string(&val).unwrap_or_default();
            yield Ok::<_, Infallible>(Event::default().data(text));
        }
    };
    Sse::new(body)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn task_to_event(task: &Task) -> Option<Event> {
    let val = serde_json::to_value(StreamingMessageResult::Task(task.clone())).ok()?;
    let body = serde_json::to_string(&val).ok()?;
    Some(Event::default().data(body))
}

fn stream_response_to_event(event: StreamResponse) -> Option<Event> {
    let val = match event {
        StreamResponse::Task(t) => serde_json::to_value(StreamingMessageResult::Task(t)),
        StreamResponse::Message(m) => serde_json::to_value(StreamingMessageResult::Message(m)),
        StreamResponse::StatusUpdate(e) => {
            serde_json::to_value(StreamingMessageResult::StatusUpdate(e))
        }
        StreamResponse::ArtifactUpdate(e) => {
            serde_json::to_value(StreamingMessageResult::ArtifactUpdate(e))
        }
    }
    .ok()?;
    let body = serde_json::to_string(&val).ok()?;
    Some(Event::default().data(body))
}

fn event_matches_task(event: &StreamResponse, target: &str, target_context_id: &str) -> bool {
    match event {
        StreamResponse::Task(t) => t.id == target,
        StreamResponse::StatusUpdate(e) => e.task_id == target,
        StreamResponse::ArtifactUpdate(e) => e.task_id == target,
        // `StreamResponse::Message` events are never emitted by `AuraMessageHandler`'s
        // worker (it only broadcasts `ArtifactUpdate` / `StatusUpdate`) and a `Message` has
        // no `task_id` field, so we match against the *cached* context_id of the task we
        // snapshotted at subscribe time.
        //
        // Upstream `a2a-rs-server` 1.0.26 does this comparison with a non-blocking task-store
        // fetch (`store.get(target).now_or_never()`); under any lock contention that fetch
        // returns `None` and the Message is silently dropped. Caching `context_id` from the
        // initial snapshot avoids the race entirely.
        StreamResponse::Message(m) => m.context_id.as_deref() == Some(target_context_id),
    }
}

fn is_terminal_event(event: &StreamResponse) -> bool {
    match event {
        StreamResponse::Task(t) => t.status.state.is_terminal(),
        StreamResponse::StatusUpdate(e) => e.status.state.is_terminal(),
        _ => false,
    }
}

/// What to do after a `RecvError::Lagged` on the broadcast channel.
struct LaggedOutcome {
    /// Synthetic SSE event carrying the lag counter and the task's *current* (post-lag) state.
    event: Option<Event>,
    /// If true, the task store now shows a terminal state — meaning the dropped events
    /// almost certainly included the terminal `StatusUpdate`, so the SSE loop must break
    /// rather than wait forever for an event that will never arrive.
    is_terminal: bool,
}

async fn handle_lagged(task_id: &str, store: &TaskStore, dropped: u64) -> LaggedOutcome {
    let Some(task) = store.get_flexible(task_id).await else {
        // Task store has no record — can't synthesize; assume terminal so we exit cleanly.
        return LaggedOutcome {
            event: None,
            is_terminal: true,
        };
    };
    let is_terminal = task.status.state.is_terminal();
    let synthetic = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        kind: "status-update".to_string(),
        task_id: task.id.clone(),
        context_id: task.context_id.clone(),
        status: TaskStatus {
            state: task.status.state,
            message: None,
            timestamp: Some(now_iso8601()),
        },
        metadata: Some(json!({
            "lagged_events": dropped,
            "note": "subscriber fell behind broadcast channel; events were dropped",
        })),
    });
    LaggedOutcome {
        event: stream_response_to_event(synthetic),
        is_terminal,
    }
}

fn aip_error(status: StatusCode, message: &str) -> Response {
    let reason = match status.as_u16() {
        400 => "INVALID_ARGUMENT",
        404 => "NOT_FOUND",
        409 => "FAILED_PRECONDITION",
        500 => "INTERNAL",
        503 => "UNAVAILABLE",
        _ => "UNKNOWN",
    };
    let grpc_status = match status.as_u16() {
        400 => "INVALID_ARGUMENT",
        404 => "NOT_FOUND",
        409 => "FAILED_PRECONDITION",
        500 => "INTERNAL",
        503 => "UNAVAILABLE",
        _ => "UNKNOWN",
    };
    (
        status,
        Json(json!({
            "error": {
                "code": status.as_u16(),
                "message": message,
                "status": grpc_status,
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": reason,
                    "domain": "a2a-protocol.org",
                }],
            }
        })),
    )
        .into_response()
}

fn handler_error_response(e: a2a_rs_server::HandlerError) -> Response {
    use a2a_rs_server::HandlerError as H;
    let (status, msg) = match &e {
        H::InvalidInput(m) => (StatusCode::BAD_REQUEST, m.clone()),
        H::AuthRequired(m) => (StatusCode::UNAUTHORIZED, m.clone()),
        H::BackendUnavailable { message, .. } => (StatusCode::BAD_GATEWAY, message.clone()),
        H::ProcessingFailed { message, .. } => (StatusCode::INTERNAL_SERVER_ERROR, message.clone()),
        H::Internal(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    aip_error(status, &msg)
}

#[cfg(test)]
mod tests {
    //! Deterministic unit tests for the parts of the race fix that don't depend on
    //! network timing or a running LLM:
    //!
    //! - `event_matches_task` correctly routes by `task_id` and falls back to the cached
    //!   `context_id` for `Message` variants (the bit that replaces the upstream
    //!   `now_or_never()` store fetch).
    //! - `handle_lagged` returns `is_terminal = true` when the task store shows a terminal
    //!   state after a lag, so the SSE loop will break instead of hanging on the broadcast
    //!   channel after the terminal event was already dropped.
    //!
    //! The end-to-end timing race is exercised separately in
    //! `tests/a2a_streaming_test.rs`.

    use super::*;
    use a2a_rs_core::{
        Artifact, Message, Part, Role, Task, TaskArtifactUpdateEvent, TaskState, TaskStatus,
    };

    fn make_task(id: &str, context_id: &str, state: TaskState) -> Task {
        Task {
            kind: "task".to_string(),
            id: id.to_string(),
            context_id: context_id.to_string(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: Some(now_iso8601()),
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn make_message(context_id: Option<&str>) -> Message {
        Message {
            kind: "message".to_string(),
            message_id: "msg-1".to_string(),
            role: Role::User,
            parts: vec![Part::text("hi")],
            context_id: context_id.map(str::to_string),
            task_id: None,
            reference_task_ids: None,
            extensions: vec![],
            metadata: None,
        }
    }

    #[test]
    fn matches_by_task_id_on_status_and_artifact_updates() {
        let target_task = "task-1";
        let target_ctx = "ctx-1";

        let su = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            kind: "status-update".to_string(),
            task_id: target_task.to_string(),
            context_id: target_ctx.to_string(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        assert!(event_matches_task(&su, target_task, target_ctx));

        let au = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            kind: "artifact-update".to_string(),
            task_id: target_task.to_string(),
            context_id: target_ctx.to_string(),
            artifact: Artifact {
                artifact_id: "a".to_string(),
                name: None,
                description: None,
                parts: vec![],
                metadata: None,
                extensions: vec![],
            },
            append: None,
            last_chunk: None,
            metadata: None,
        });
        assert!(event_matches_task(&au, target_task, target_ctx));

        // Different task_id — should not match.
        let other_su = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            kind: "status-update".to_string(),
            task_id: "other-task".to_string(),
            context_id: target_ctx.to_string(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        assert!(!event_matches_task(&other_su, target_task, target_ctx));
    }

    #[test]
    fn matches_message_by_cached_context_id() {
        // Regression test: upstream uses `task_store.get(...).now_or_never()` here; under
        // lock contention that returns None and silently drops the Message. We compare
        // against the context_id we captured from the initial task snapshot instead, so
        // the answer is independent of any concurrent task-store activity.
        let m = StreamResponse::Message(make_message(Some("ctx-target")));
        assert!(event_matches_task(&m, "task-1", "ctx-target"));

        let wrong = StreamResponse::Message(make_message(Some("ctx-other")));
        assert!(!event_matches_task(&wrong, "task-1", "ctx-target"));

        let no_ctx = StreamResponse::Message(make_message(None));
        assert!(!event_matches_task(&no_ctx, "task-1", "ctx-target"));
    }

    #[tokio::test]
    async fn lagged_with_terminal_store_state_breaks_loop() {
        // The whole reason `handle_lagged` returns `is_terminal`: if the dropped events
        // included the terminal `StatusUpdate`, the broadcast channel will never deliver
        // another event for this task. The SSE loop has to break itself.
        let store = TaskStore::new();
        store
            .insert(make_task("t1", "c1", TaskState::Completed))
            .await;

        let outcome = handle_lagged("t1", &store, 3).await;
        assert!(
            outcome.is_terminal,
            "post-lag terminal store state must surface so the SSE loop breaks"
        );
        assert!(
            outcome.event.is_some(),
            "should still emit an informational synthetic event"
        );
    }

    #[tokio::test]
    async fn lagged_with_non_terminal_store_state_keeps_loop_alive() {
        let store = TaskStore::new();
        store
            .insert(make_task("t1", "c1", TaskState::Working))
            .await;

        let outcome = handle_lagged("t1", &store, 1).await;
        assert!(
            !outcome.is_terminal,
            "non-terminal state should let the loop keep listening for more events"
        );
        assert!(outcome.event.is_some());
    }

    #[tokio::test]
    async fn lagged_with_missing_task_breaks_loop_cleanly() {
        // No record of the task: we can't synthesize a sensible event, but we definitely
        // shouldn't loop forever waiting for one.
        let store = TaskStore::new();

        let outcome = handle_lagged("nonexistent", &store, 5).await;
        assert!(
            outcome.is_terminal,
            "missing task should terminate the loop rather than spin"
        );
        assert!(outcome.event.is_none());
    }
}
