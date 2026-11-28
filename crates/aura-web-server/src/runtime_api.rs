//! The runtime's own HTTP routes: list and read sessions and runs, read a
//! session's or a run's events, and cancel a run.
//!
//! Each handler parses its request, calls the [`AgentRuntime`], and maps the
//! answer to a status and a body. What needs deciding is decided in the
//! runtime, so a handler holds no state of its own. Errors use the OpenAI
//! error envelope, as the other routes do.
//!
//! The routes exist only on a server started with `AURA_RUNTIME_API` on
//! (see `server::routes`).
//!
//! [`AgentRuntime`]: aura::runtime::AgentRuntime

mod events;
mod wire;

#[cfg(test)]
mod tests;

use std::future::Future;
use std::ops::ControlFlow;
use std::sync::Arc;

use aura::journal::Cursor;
use aura::runtime::{
    AttachError, EventRange, RunQuery, RunSnapshot, SessionQuery, StopOutcome, Subscription,
};
use aura::session_store::{JournalEntry, SessionStoreError};
use aura_events::run::CancelCause;
use aura_events::{RunId, SequenceNumber, SessionId};
use axum::body::{Body, HttpBody as _};
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::handlers::{error_response, shutting_down_response};
use crate::streaming::HEARTBEAT_INTERVAL;
use crate::types::AppState;

pub use wire::{
    CancelRunRequest, EventsQuery, LastSeenEvent, ListRunsQuery, ListRunsResponse,
    ListSessionsQuery, ListSessionsResponse, RunSummary, SessionSummary,
};

/// The runtime's routes.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/{id}", get(get_session))
        .route(
            "/v1/sessions/{id}/events",
            get(session_events).head(head_session_events),
        )
        .route("/v1/runs", get(list_runs))
        .route("/v1/runs/{id}", get(get_run))
        .route(
            "/v1/runs/{id}/events",
            get(run_events).head(head_run_events),
        )
        .route("/v1/runs/{id}/cancel", post(cancel_run))
}

/// `GET /v1/sessions`: one page of the sessions the query selects, as a
/// [`ListSessionsResponse`].
async fn list_sessions(
    State(state): State<Arc<AppState>>,
    query: Result<Query<ListSessionsQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return refused_query(&rejection),
    };
    match state.runtime.sessions(&SessionQuery::from(query)).await {
        Ok(page) => Json(ListSessionsResponse::from(&page)).into_response(),
        Err(error) => store_fault(&error),
    }
}

/// `GET /v1/sessions/{id}`: the session's [`SessionSummary`], or 404 for a
/// session neither the runtime nor the run store has a run of.
async fn get_session(
    State(state): State<Arc<AppState>>,
    id: Result<Path<SessionId>, PathRejection>,
) -> Response {
    let Path(id) = match id {
        Ok(id) => id,
        Err(rejection) => return refused_path(&rejection),
    };
    match state.runtime.session(&id).await {
        Ok(Some(session)) => Json(SessionSummary::from(&session)).into_response(),
        Ok(None) => no_session(&id),
        Err(error) => store_fault(&error),
    }
}

/// `GET /v1/sessions/{id}/events`: the session's events as an SSE stream,
/// read from the start of what the store holds or from after the query's
/// `after`.
///
/// A session neither the runtime nor the run store has a run of is a 404, as
/// on [`get_session`].
/// Attaching opens a session and writes to its journal, so an id nobody has
/// run on is refused before that. A claim on a session that has a claimant
/// is a 409 that names the claimant.
async fn session_events(
    State(state): State<Arc<AppState>>,
    id: Result<Path<SessionId>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response {
    let (id, query) = match session_for_events(&state, id, query).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    let from = query.after.get().map_or(Cursor::Start, Cursor::After);
    match state.runtime.attach(&id, query.attach(from)).await {
        Ok(subscription) => event_stream(&state, subscription, None, None),
        Err(error) => refused_attach(&error),
    }
}

/// `HEAD /v1/sessions/{id}/events`: what [`session_events`] answers before
/// it attaches, with no body. It attaches nothing, so it writes nothing to
/// the session's journal; axum would otherwise send a `HEAD` to the `GET`
/// handler and drop its body.
async fn head_session_events(
    State(state): State<Arc<AppState>>,
    id: Result<Path<SessionId>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response {
    match session_for_events(&state, id, query).await {
        Ok(_) => sse_response(Body::empty()),
        Err(refusal) => refusal,
    }
}

/// The session and query of a session events request, or the response
/// that refuses it: a path or query that does not parse, a session with no
/// run, or an `after` past the session's newest event.
async fn session_for_events(
    state: &AppState,
    id: Result<Path<SessionId>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<(SessionId, EventsQuery), Response> {
    let Path(id) = id.map_err(|rejection| refused_path(&rejection))?;
    let Query(query) = query.map_err(|rejection| refused_query(&rejection))?;
    let session = match state.runtime.session(&id).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(no_session(&id)),
        Err(error) => return Err(store_fault(&error)),
    };
    match after_past_the_newest_event(query.after.get(), session.latest_event()) {
        Some(refusal) => Err(refusal),
        None => Ok((id, query)),
    }
}

/// `GET /v1/runs`: one page of the runs the query selects, as a
/// [`ListRunsResponse`].
async fn list_runs(
    State(state): State<Arc<AppState>>,
    query: Result<Query<ListRunsQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return refused_query(&rejection),
    };
    match state.runtime.runs(&RunQuery::from(query)).await {
        Ok(page) => Json(ListRunsResponse::from(&page)).into_response(),
        Err(error) => store_fault(&error),
    }
}

/// `GET /v1/runs/{id}`: the run's [`RunSummary`], or 404 for a run neither
/// the runtime nor the run store holds.
async fn get_run(
    State(state): State<Arc<AppState>>,
    id: Result<Path<RunId>, PathRejection>,
) -> Response {
    let Path(run) = match id {
        Ok(id) => id,
        Err(rejection) => return refused_path(&rejection),
    };
    match state.runtime.run(&run).await {
        Ok(Some(snapshot)) => Json(RunSummary::from(&snapshot)).into_response(),
        Ok(None) => no_run(run),
        Err(error) => store_fault(&error),
    }
}

/// `GET /v1/runs/{id}/events`: the run's events as an SSE stream that ends
/// with the run. What it reads is `RunSnapshot::events_after` for the
/// query's `after`, so it never reads the runs before this one, and a run
/// with nothing left to read gets an empty stream without an attach. A run
/// whose last event the session's journal no longer holds as the run's gets
/// one gap past the run ([`lost_events_response`]), checked before the
/// attach and again after it, and [`events::events_body`] sends the same
/// gap when the numbers it reads turn out to be another run's. An unknown
/// run is a 404, and a refused claim a 409, as on [`session_events`].
async fn run_events(
    State(state): State<Arc<AppState>>,
    id: Result<Path<RunId>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response {
    let (run, query, range, session) = match run_events_before_attach(&state, id, query).await {
        ControlFlow::Continue(request) => request,
        ControlFlow::Break(response) => return response,
    };
    let subscription = match state
        .runtime
        .attach(&session, query.attach(range.from))
        .await
    {
        Ok(subscription) => subscription,
        Err(error) => return refused_attach(&error),
    };
    // The store can forget the session between the check above and the
    // attach, which then reopens it numbering from the start.
    if let Some(response) = lost_events_response(&state, run, &session, range.to).await {
        return response;
    }
    event_stream(&state, subscription, Some(run), range.to)
}

/// `HEAD /v1/runs/{id}/events`: what [`run_events`] answers before it
/// attaches, with no body. It attaches nothing, so it writes nothing to the
/// session's journal.
async fn head_run_events(
    State(state): State<Arc<AppState>>,
    id: Result<Path<RunId>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response {
    match run_events_before_attach(&state, id, query).await {
        ControlFlow::Continue(_) => sse_response(Body::empty()),
        ControlFlow::Break(response) => response,
    }
}

/// A run events request up to its attach. It breaks with the response the
/// request gets without one, checked in this order: a path or query that
/// does not parse, a run neither the runtime nor the run store holds, an
/// empty stream for a run with nothing left to read, the gap for a run
/// whose events the journal lost, or the refusal of an `after` past the
/// session's newest event. A run with nothing left to read is answered
/// before `after` is checked against the stream, since its stream then
/// ends at once and waits for nothing. Otherwise it goes on with the run, the query, the stretch of the
/// session's stream to read and the run's session.
async fn run_events_before_attach(
    state: &AppState,
    id: Result<Path<RunId>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> ControlFlow<Response, (RunId, EventsQuery, EventRange, SessionId)> {
    let Path(run) = match id {
        Ok(id) => id,
        Err(rejection) => return ControlFlow::Break(refused_path(&rejection)),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ControlFlow::Break(refused_query(&rejection)),
    };
    // The session's newest number is read before the run that the stretch
    // to read comes from. A run that ended at or before that number then
    // reads as ended, since `AgentRuntime::run` waits for an ending already
    // on the stream, and an `after` the check below takes cannot pass the
    // run's ending unread.
    let session = match run_snapshot(state, run).await {
        ControlFlow::Continue(snapshot) => snapshot.facts().session_id(),
        ControlFlow::Break(response) => return ControlFlow::Break(response),
    };
    let latest = match state.runtime.latest_event(&session).await {
        Ok(latest) => latest,
        Err(error) => return ControlFlow::Break(store_fault(&error)),
    };
    let snapshot = match run_snapshot(state, run).await {
        ControlFlow::Continue(snapshot) => snapshot,
        ControlFlow::Break(response) => return ControlFlow::Break(response),
    };
    let Some(range) = snapshot.events_after(query.after.get()) else {
        return ControlFlow::Break(sse_response(Body::empty()));
    };
    if let Some(response) = lost_events_response(state, run, &session, range.to).await {
        return ControlFlow::Break(response);
    }
    if let Some(refusal) = after_past_the_newest_event(query.after.get(), latest) {
        return ControlFlow::Break(refusal);
    }
    ControlFlow::Continue((run, query, range, session))
}

/// The run's snapshot, or the 404 or 500 that answers for it.
async fn run_snapshot(state: &AppState, run: RunId) -> ControlFlow<Response, RunSnapshot> {
    match state.runtime.run(&run).await {
        Ok(Some(snapshot)) => ControlFlow::Continue(snapshot),
        Ok(None) => ControlFlow::Break(no_run(run)),
        Err(error) => ControlFlow::Break(store_fault(&error)),
    }
}

/// `POST /v1/runs/{id}/cancel`: stops the run with the cause `External`
/// and the `reason` [`reason_from_cancel_request`] reads from the body.
///
/// The answer is [`AgentRuntime::stop`](aura::runtime::AgentRuntime::stop)'s
/// outcome, whose doc says when each comes back:
///
/// - 202 with the run's [`RunSummary`] for [`StopOutcome::Stopping`].
/// - 200 with the run's [`RunSummary`] for [`StopOutcome::AlreadyEnded`].
/// - 404 for [`StopOutcome::NotHere`].
async fn cancel_run(
    State(state): State<Arc<AppState>>,
    id: Result<Path<RunId>, PathRejection>,
    request: Request,
) -> Response {
    let Path(run) = match id {
        Ok(id) => id,
        Err(rejection) => return refused_path(&rejection),
    };
    let reason = match reason_from_cancel_request(request).await {
        Ok(reason) => reason,
        Err(refusal) => return refusal,
    };
    match state
        .runtime
        .stop(&run, CancelCause::External, reason)
        .await
    {
        Ok(StopOutcome::Stopping(snapshot)) => {
            (StatusCode::ACCEPTED, Json(RunSummary::from(&snapshot))).into_response()
        }
        Ok(StopOutcome::AlreadyEnded(snapshot)) => {
            Json(RunSummary::from(&snapshot)).into_response()
        }
        Ok(StopOutcome::NotHere) => no_run(run),
        Err(error) => store_fault(&error),
    }
}

/// The `reason` a cancel request gives, or the response that refuses it.
/// A request with no content type whose body is known to be empty gives
/// none. Any other request is a [`CancelRunRequest`] sent as JSON, which
/// axum's `Json` reads: a body sent with no content type, or with one other
/// than JSON, is a 415 before any of it is read, and JSON that is not a
/// [`CancelRunRequest`] is a 400.
async fn reason_from_cancel_request(request: Request) -> Result<Option<String>, Response> {
    if !request.headers().contains_key(header::CONTENT_TYPE) && request.body().is_end_stream() {
        return Ok(None);
    }
    Json::<CancelRunRequest>::from_request(request, &())
        .await
        .map(|Json(request)| request.reason)
        .map_err(|rejection| refused_body(&rejection))
}

/// The response for a run that ends at `to` when the session's journal no
/// longer holds the run's last event as the run's: the journal store let
/// the event go or forgot the session, or a journal reopened after that
/// numbered the session from the start and gave `to` to another event. The
/// run's events are gone, so the stream is one `gap` message that resumes
/// after `to`, and then it ends. Otherwise a stream could wait for a number
/// a reopened journal will not reach, or end at `to` on another event. A
/// store fault is a 500. `None` when the run has no end, or the event at
/// `to` is still the run's.
async fn lost_events_response(
    state: &AppState,
    run: RunId,
    session: &SessionId,
    to: Option<SequenceNumber>,
) -> Option<Response> {
    let to = to?;
    match state.runtime.journal_entry(session, to).await {
        Ok(Some(JournalEntry::Event(event))) if event.run_id == Some(run) => None,
        Ok(_) => Some(sse_response(Body::from(events::gap_message(to.next())))),
        Err(error) => Some(store_fault(&error)),
    }
}

/// The 400 for an events request whose `after` is past `latest`, the
/// session's newest event. Such a number is not on the session's stream:
/// the stream may have been numbered from the start again since the reader
/// saw it, when the journal store forgot the session, or the number never
/// came from this stream. Read from there, the stream would pass over the
/// events numbered up to `after` and wait for later ones, so the reader is
/// told to read the session again without `after`. `None` when there is no
/// `after`, or it is on the stream.
fn after_past_the_newest_event(
    after: Option<SequenceNumber>,
    latest: Option<SequenceNumber>,
) -> Option<Response> {
    let after = after?;
    if latest.is_some_and(|latest| after <= latest) {
        return None;
    }
    let newest = latest.map_or_else(
        || "has no events".to_owned(),
        |latest| format!("ends at {latest}"),
    );
    Some(error_response(
        StatusCode::BAD_REQUEST,
        format!(
            "`after` is {after}, past the session's newest event: its stream {newest}. Read the session again without `after`"
        ),
        "invalid_request_error",
    ))
}

/// The SSE response of an events route: [`events::events_body`] over
/// `subscription`, with the envelope frame and the server's heartbeat.
fn event_stream(
    state: &AppState,
    subscription: Subscription,
    run: Option<RunId>,
    to: Option<SequenceNumber>,
) -> Response {
    let body = events::events_body(
        subscription,
        events::envelope_frame,
        run,
        to,
        shutdown_with_no_run_going(state),
        Some(HEARTBEAT_INTERVAL),
    );
    sse_response(Body::from_stream(body))
}

/// Resolves once the server has begun shutting down and the runtime has no
/// run going. A clean first phase of the shutdown never ends a stream on
/// its own, and the server waits for every connection, so an events body
/// ends on this.
fn shutdown_with_no_run_going(state: &AppState) -> impl Future<Output = ()> + Send + 'static {
    let shutting_down = state.shutdown_token.clone();
    let runtime = Arc::clone(&state.runtime);
    async move {
        shutting_down.cancelled().await;
        runtime.idle().await;
    }
}

fn sse_response(body: Body) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        body,
    )
        .into_response()
}

fn refused_query(rejection: &QueryRejection) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        rejection.body_text(),
        "invalid_request_error",
    )
}

fn refused_path(rejection: &PathRejection) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        rejection.body_text(),
        "invalid_request_error",
    )
}

/// A 415 for a body that is not sent as JSON, and a 400 for JSON that is
/// not the body the route takes. Any other refusal keeps axum's status.
fn refused_body(rejection: &JsonRejection) -> Response {
    let status = match rejection {
        JsonRejection::MissingJsonContentType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        JsonRejection::JsonDataError(_) | JsonRejection::JsonSyntaxError(_) => {
            StatusCode::BAD_REQUEST
        }
        other => other.status(),
    };
    error_response(status, rejection.body_text(), "invalid_request_error")
}

fn refused_attach(error: &AttachError) -> Response {
    match error {
        AttachError::AlreadyClaimed { .. } => {
            error_response(StatusCode::CONFLICT, error.to_string(), "session_claimed")
        }
        AttachError::ShuttingDown => shutting_down_response(),
        AttachError::Store(error) => store_fault(error),
        other => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            other.to_string(),
            "internal_error",
        ),
    }
}

fn store_fault(error: &SessionStoreError) -> Response {
    tracing::error!(%error, "the session store failed a runtime API request");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("the session store failed: {error}"),
        "internal_error",
    )
}

fn no_session(id: &SessionId) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        format!("no session `{id}`"),
        "not_found",
    )
}

fn no_run(run: RunId) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        format!("no run `{run}`"),
        "not_found",
    )
}
