//! The runtime's routes, served over a runtime whose runs use test agents,
//! and the events body read over that runtime's subscriptions.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aura::RunSession;
use aura::journal::{Cursor, JournalItem};
use aura::runtime::{
    AgentForRun, AgentRuntime, AgentState, Attach, ObserverCounts, RunTarget, RuntimeStores,
    SessionSnapshot, StartRun,
};
use aura::session_store::{
    InMemoryJournalStore, InMemoryRunStore, InMemorySkillInvocationStore, JournalStore, RunDetails,
    RunFacts, RunFilter, RunStatus, RunStore,
};
use aura_events::run::{CancelCause, LifecycleEvent, Liveness, SessionEventPayload};
use aura_events::{EmittingAgent, RunId, SequenceNumber, SessionId, Timestamp, TokenUsage};
use aura_test_utils::mock_agent::{MockAgent, Step, items};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tower::ServiceExt;

use crate::types::AppState;

/// The chat id the client knows the test's conversation by.
const CHAT: &str = "cs_runtime_api";
/// The agent most tests run.
const AGENT: &str = "solo";
/// The session id of lane 0 of the conversation between `CHAT` and `AGENT`.
const LANE: &str = "cs_runtime_api:solo:0";
/// `LANE` percent-encoded, as a client puts it in a path.
const LANE_IN_PATH: &str = "cs_runtime_api%3Asolo%3A0";
/// The SSE comment a heartbeat sends.
const HEARTBEAT: &str = ": heartbeat\n\n";
/// How long a test waits for what it expects before it fails.
const PATIENCE: Duration = Duration::from_secs(20);

fn config(name: &str) -> aura_config::Config {
    aura_config::Config {
        agent: aura_config::AgentConfig {
            name: name.to_owned(),
            ..aura_config::AgentConfig::default()
        },
        ..aura_config::Config::default()
    }
}

fn session_id(id: &str) -> SessionId {
    SessionId::new(id).expect("a session id")
}

fn seq(n: u64) -> SequenceNumber {
    SequenceNumber::try_from(n).expect("a sequence number")
}

type AgentOrError = Result<AgentForRun, aura::BuilderError>;

fn agent_for(run: RunId, agent: MockAgent) -> AgentOrError {
    Ok(AgentForRun {
        agent: Arc::new(agent.for_run(run).with_run_context()),
        attribution: EmittingAgent::single_agent(),
    })
}

/// An agent for `run` that answers `answer`.
fn answering(run: RunId, answer: &str) -> AgentOrError {
    agent_for(
        run,
        MockAgent::scripted(vec![Step::item(items::text(answer))]),
    )
}

/// An agent for `run` that never answers and does not notice a stop.
fn pending(run: RunId) -> AgentOrError {
    agent_for(run, MockAgent::pending())
}

/// An agent for `run` that waits for a permit from `gate`, then answers
/// `answer`.
fn gated(gate: &Arc<Semaphore>, run: RunId, answer: &str) -> AgentOrError {
    let gate = Arc::clone(gate);
    agent_for(
        run,
        MockAgent::scripted(vec![
            Step::effect(move |_| {
                let gate = Arc::clone(&gate);
                async move { gate.acquire().await.expect("the gate stays open").forget() }
            }),
            Step::item(items::text(answer)),
        ]),
    )
}

/// The run store and the journal store of a test's runtime.
struct Stores {
    runs: Arc<InMemoryRunStore>,
    journals: Arc<InMemoryJournalStore>,
}

impl Stores {
    fn new() -> Self {
        Self::with_journals(InMemoryJournalStore::new())
    }

    fn with_journals(journals: InMemoryJournalStore) -> Self {
        Self {
            runs: Arc::new(InMemoryRunStore::new()),
            journals: Arc::new(journals),
        }
    }

    fn for_runtime(&self) -> RuntimeStores {
        RuntimeStores {
            runs: Arc::clone(&self.runs) as Arc<dyn RunStore>,
            journals: Arc::clone(&self.journals) as Arc<dyn JournalStore>,
            skills: Arc::new(InMemorySkillInvocationStore::new()),
        }
    }

    /// Writes an event to each of 100 other sessions, so a journal store
    /// that keeps one session's events forgets `session` entirely, its
    /// newest number included.
    async fn forget_journal_of(&self, session: &str) {
        for n in 0..100 {
            self.journals
                .append(
                    &session_id(&format!("other-{n}")),
                    None,
                    None,
                    Timestamp::from_unix_millis(1),
                    SessionEventPayload::Lifecycle(LifecycleEvent::ClaimsExhausted),
                )
                .await
                .expect("the journal keeps the event");
        }
        assert_eq!(
            self.journals
                .latest(&session_id(session))
                .await
                .expect("the journal store answers"),
            None,
            "the journal store forgot the session"
        );
    }
}

/// A state over `configs` whose runtime keeps its runs and journals in
/// `stores`. Each run gets the agent that `agents` returns.
fn state_over<F>(configs: Vec<aura_config::Config>, stores: &Stores, agents: F) -> Arc<AppState>
where
    F: Fn(&aura_config::Config, RunId) -> AgentOrError + Send + Sync + 'static,
{
    let mut state = AppState::for_tests(configs);
    state.runtime = Arc::new(AgentRuntime::over_agents(
        Arc::clone(&state.configs),
        state.default_agent.clone(),
        stores.for_runtime(),
        agents,
    ));
    Arc::new(state)
}

/// A state over the one agent `AGENT`, whose `n`th run answers `answer n`.
fn answering_state(stores: &Stores) -> Arc<AppState> {
    let count_of_runs = AtomicUsize::new(0);
    state_over(vec![config(AGENT)], stores, move |_, run| {
        let n = count_of_runs.fetch_add(1, Ordering::SeqCst) + 1;
        answering(run, &format!("answer {n}"))
    })
}

/// A state over the one agent `AGENT`, each of whose runs waits on `gate`.
fn gated_state(stores: &Stores, gate: &Arc<Semaphore>) -> Arc<AppState> {
    let gate = Arc::clone(gate);
    state_over(vec![config(AGENT)], stores, move |_, run| {
        gated(&gate, run, "late answer")
    })
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> Response {
    super::routes()
        .with_state(Arc::clone(state))
        .oneshot(request)
        .await
        .expect("the router answers")
}

async fn get(state: &Arc<AppState>, uri: &str) -> Response {
    send(
        state,
        Request::get(uri).body(Body::empty()).expect("a request"),
    )
    .await
}

/// `POST /v1/runs/{run}/cancel` with no body and no content type.
async fn cancel(state: &Arc<AppState>, run: RunId) -> Response {
    send(
        state,
        Request::post(format!("/v1/runs/{run}/cancel"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await
}

/// `POST /v1/runs/{run}/cancel` with `body`, sent as `content_type`, or
/// with no content type for `None`.
async fn cancel_with(
    state: &Arc<AppState>,
    run: RunId,
    content_type: Option<&str>,
    body: &str,
) -> Response {
    let mut request = Request::post(format!("/v1/runs/{run}/cancel"));
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    send(
        state,
        request
            .body(Body::from(body.to_owned()))
            .expect("a request"),
    )
    .await
}

async fn body_text(response: Response) -> String {
    let bytes = tokio::time::timeout(
        PATIENCE,
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("the body ends in time")
    .expect("the body reads");
    String::from_utf8(bytes.to_vec()).expect("UTF-8")
}

async fn json_body(response: Response) -> Value {
    let text = body_text(response).await;
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("not JSON ({error}): {text}"))
}

/// The status, and the message and type of the OpenAI error envelope.
async fn refusal(response: Response) -> (StatusCode, String, String) {
    let status = response.status();
    let body = json_body(response).await;
    let text_at = |field: &str| {
        body["error"][field]
            .as_str()
            .unwrap_or_else(|| panic!("the error envelope has a `{field}`: {body}"))
            .to_owned()
    };
    (status, text_at("message"), text_at("type"))
}

async fn start(state: &Arc<AppState>, start: StartRun) -> RunId {
    state.runtime.start(start).await.expect("the run starts")
}

/// A run of `q` by the default agent on a lane of the conversation between
/// `chat` and that agent.
fn on_chat(chat: &str) -> StartRun {
    StartRun {
        target: RunTarget::Conversation {
            chat: session_id(chat),
        },
        ..StartRun::new("q")
    }
}

/// A run of `q` by the default agent on the plain session `session`.
fn on_session(session: &str) -> StartRun {
    StartRun {
        target: RunTarget::Session(session_id(session)),
        ..StartRun::new("q")
    }
}

/// A run of `q` by `agent` on the plain session `session`.
fn by_agent_on_session(agent: &str, session: &str) -> StartRun {
    StartRun {
        agent: Some(agent.to_owned()),
        ..on_session(session)
    }
}

/// What the run store holds for `run`.
async fn stored_facts(state: &Arc<AppState>, run: RunId) -> Option<RunFacts> {
    state
        .runtime
        .list(&RunFilter::default())
        .await
        .expect("the run store lists runs")
        .into_iter()
        .find(|facts| facts.run.run_id == run)
}

/// Waits until the run store holds `run` with a status `wanted` accepts,
/// and returns what it holds.
async fn until_stored(
    state: &Arc<AppState>,
    run: RunId,
    wanted: impl Fn(RunStatus) -> bool,
) -> RunFacts {
    tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(facts) = stored_facts(state, run).await
                && wanted(facts.run.status)
            {
                return facts;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the run store has the run's facts in time")
}

async fn running(state: &Arc<AppState>, run: RunId) -> RunFacts {
    until_stored(state, run, |status| status == RunStatus::Running).await
}

async fn ended(state: &Arc<AppState>, run: RunId) -> RunFacts {
    until_stored(state, run, |status| status.is_terminal()).await
}

/// The sequence number of the run's newest event, from its stored facts.
fn latest_event_of(facts: &RunFacts) -> u64 {
    facts.run.latest_event.expect("the run has an event").get()
}

/// Waits until `check` holds.
async fn eventually(what: &str, check: impl Fn() -> bool) {
    tokio::time::timeout(PATIENCE, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}, in time"));
}

/// The run's terminal event, read from the start of its session's stream.
async fn ending_of(state: &Arc<AppState>, session: &str, run: RunId) -> LifecycleEvent {
    let mut witness = state
        .runtime
        .attach(
            &session_id(session),
            Attach::collecting().from(Cursor::Start),
        )
        .await
        .expect("the witness attaches");
    tokio::time::timeout(PATIENCE, async {
        while let Some(item) = futures_util::StreamExt::next(&mut witness).await {
            if let JournalItem::Event(event) = item
                && event.run_id == Some(run)
                && let SessionEventPayload::Lifecycle(lifecycle) = &event.payload
                && lifecycle.ends_run()
            {
                return lifecycle.clone();
            }
        }
        panic!("the session's stream ended before the run did");
    })
    .await
    .expect("the run's ending is on the stream in time")
}

/// The summary of `run` by `AGENT`, which answered and finished, with
/// `started_at` and `ended_at` as `normalised_run` leaves them, and no
/// observers.
fn finished_run_summary(run: RunId, session: &str, client_session: Option<&str>) -> Value {
    json!({
        "run_id": run.to_string(),
        "agent": AGENT,
        "status": "finished",
        "liveness": { "policy": "cancel", "grace_ms": 0 },
        "started_at": "<started_at>",
        "ended_at": "<ended_at>",
        "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
        "first_event": 1,
        "latest_event": 5,
        "pending_approvals": [],
        "checkpoint": null,
        "continues": null,
        "session_id": session,
        "client_session_id": client_session,
        "observers": { "collecting": 0, "claiming": 0, "present": 0 },
    })
}

/// The summary of a session whose one run, `run` by `AGENT`, answered and
/// finished, with no observers.
fn done_session_summary(session: &str, client_session: Option<&str>, run: RunId) -> Value {
    json!({
        "session_id": session,
        "client_session_id": client_session,
        "agent": AGENT,
        "state": "done",
        "current_run": null,
        "last_run": run.to_string(),
        "latest_event": 5,
        "observers": { "collecting": 0, "claiming": 0, "present": 0 },
    })
}

/// A run summary with its `started_at` and `ended_at` replaced at their
/// fields. Panics unless `started_at` is a time and `ended_at` a time or
/// null.
fn normalised_run(mut summary: Value) -> Value {
    assert!(
        summary["started_at"].is_u64(),
        "started_at is a time: {summary}"
    );
    summary["started_at"] = json!("<started_at>");
    let ended_at = &summary["ended_at"];
    assert!(
        ended_at.is_u64() || ended_at.is_null(),
        "ended_at is a time or null: {summary}"
    );
    if ended_at.is_u64() {
        summary["ended_at"] = json!("<ended_at>");
    }
    summary
}

/// A run listing with each run as `normalised_run` leaves it.
fn normalised_runs(mut listing: Value) -> Value {
    let runs = listing["runs"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("a run listing has runs"));
    for run in runs.iter_mut() {
        *run = normalised_run(run.take());
    }
    listing
}

fn session_ids_of(listing: &Value) -> Vec<String> {
    listing["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("a session listing has sessions: {listing}"))
        .iter()
        .map(|session| {
            session["session_id"]
                .as_str()
                .expect("a session id")
                .to_owned()
        })
        .collect()
}

fn run_ids_of(listing: &Value) -> Vec<String> {
    listing["runs"]
        .as_array()
        .unwrap_or_else(|| panic!("a run listing has runs: {listing}"))
        .iter()
        .map(|run| run["run_id"].as_str().expect("a run id").to_owned())
        .collect()
}

/// One SSE message as a client reads it: its `event:` name, if it has one,
/// and its data as JSON.
#[derive(Clone, Debug, PartialEq)]
struct SseMessage {
    event: Option<String>,
    data: Value,
}

/// The value of an SSE line for `name`: what follows its colon, less one
/// leading space.
fn field_of<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let value = line.strip_prefix(name)?.strip_prefix(':')?;
    Some(value.strip_prefix(' ').unwrap_or(value))
}

/// The message in one SSE frame. Panics unless the frame ends in a blank
/// line and holds one `data:` line, after one `event:` line or none.
fn message_of(frame: &str) -> SseMessage {
    let text = frame
        .strip_suffix("\n\n")
        .unwrap_or_else(|| panic!("a frame ends in a blank line: {frame:?}"));
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or_default();
    let (event, data_line) = match field_of(first, "event") {
        Some(name) => (Some(name.to_owned()), lines.next().unwrap_or_default()),
        None => (None, first),
    };
    let data =
        field_of(data_line, "data").unwrap_or_else(|| panic!("a frame has a data line: {frame:?}"));
    assert_eq!(lines.next(), None, "a frame has one data line: {frame:?}");
    SseMessage {
        event,
        data: serde_json::from_str(data)
            .unwrap_or_else(|error| panic!("the data is JSON ({error}): {frame:?}")),
    }
}

/// `message` with an event's time, and an observer's id, replaced at their
/// fields. Panics if an event's time is not a number or an observer's id
/// not a string.
fn normalised(mut message: SseMessage) -> SseMessage {
    if matches!(message.event.as_deref(), None | Some("unknown")) {
        assert!(
            message.data["at"].is_u64(),
            "an event's time is a number: {}",
            message.data
        );
        message.data["at"] = json!("<at>");
    }
    if message.data.pointer("/payload/event/observer/id").is_some() {
        let id = &mut message.data["payload"]["event"]["observer"]["id"];
        assert!(id.is_string(), "an observer's id is a string: {id}");
        *id = json!("<observer>");
    }
    message
}

/// An event's message as `normalised` leaves it.
fn envelope(session: &str, run: Option<RunId>, seq: u64, payload: Value) -> SseMessage {
    let mut data = json!({
        "session_id": session,
        "seq": seq,
        "at": "<at>",
        "payload": payload,
    });
    if let Some(run) = run {
        data["run_id"] = json!(run.to_string());
    }
    SseMessage { event: None, data }
}

fn started_payload(agent: &str) -> Value {
    json!({
        "kind": "lifecycle",
        "event": {
            "type": "started",
            "agent": agent,
            "prompt": "q",
            "liveness": { "policy": "cancel", "grace_ms": 0 },
        },
    })
}

fn finished_payload() -> Value {
    json!({
        "kind": "lifecycle",
        "event": { "type": "finished", "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
    })
}

/// The five events of `run` by `agent` on `session`, which answered
/// `answer` and finished, from its `Started` at `first`.
fn answered_run_by(
    agent: &str,
    session: &str,
    run: RunId,
    first: u64,
    answer: &str,
) -> Vec<SseMessage> {
    let agent_event = |payload: Value| json!({ "kind": "agent", "event": { "agent": { "agent_id": "main" }, "payload": payload } });
    vec![
        envelope(session, Some(run), first, started_payload(agent)),
        envelope(
            session,
            Some(run),
            first + 1,
            agent_event(json!({ "type": "session_info", "model": "test/fake" })),
        ),
        envelope(
            session,
            Some(run),
            first + 2,
            agent_event(json!({ "type": "text_delta", "content": answer })),
        ),
        envelope(
            session,
            Some(run),
            first + 3,
            agent_event(json!({
                "type": "usage",
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "total_tokens": 0,
            })),
        ),
        envelope(session, Some(run), first + 4, finished_payload()),
    ]
}

/// As `answered_run_by`, for a run by `AGENT`.
fn answered_run(session: &str, run: RunId, first: u64, answer: &str) -> Vec<SseMessage> {
    answered_run_by(AGENT, session, run, first, answer)
}

/// The message of an observer attaching to `session`, as `normalised`
/// leaves it.
fn observer_attached(session: &str, run: Option<RunId>, seq: u64) -> SseMessage {
    envelope(
        session,
        run,
        seq,
        json!({
            "kind": "lifecycle",
            "event": {
                "type": "observer_attached",
                "observer": { "id": "<observer>", "kind": "collecting", "presence": false },
            },
        }),
    )
}

/// Reads an SSE body as a client does, one message at a time, leaving out
/// heartbeat comments.
struct SseReader {
    body: Body,
    /// Text read past the last whole frame.
    unread: String,
}

impl SseReader {
    fn new(response: Response) -> Self {
        Self {
            body: response.into_body(),
            unread: String::new(),
        }
    }

    /// The next whole frame, or `None` once the body ends.
    async fn next_frame(&mut self) -> Option<String> {
        loop {
            if let Some(end) = self.unread.find("\n\n") {
                return Some(self.unread.drain(..end + 2).collect());
            }
            let Some(frame) = self.body.frame().await else {
                assert!(
                    self.unread.is_empty(),
                    "the body ended inside a frame: {:?}",
                    self.unread
                );
                return None;
            };
            if let Ok(data) = frame.expect("the body reads").into_data() {
                self.unread
                    .push_str(std::str::from_utf8(&data).expect("UTF-8"));
            }
        }
    }

    /// The next message, or `None` once the body ends.
    async fn next_unbounded(&mut self) -> Option<SseMessage> {
        loop {
            match self.next_frame().await {
                Some(frame) if frame == HEARTBEAT => {}
                Some(frame) => return Some(message_of(&frame)),
                None => return None,
            }
        }
    }

    /// The next message, or `None` once the body ends. Panics if neither
    /// comes within `PATIENCE`.
    async fn next(&mut self) -> Option<SseMessage> {
        tokio::time::timeout(PATIENCE, self.next_unbounded())
            .await
            .expect("the body sends a message or ends in time")
    }

    /// The next `count` messages.
    async fn take(&mut self, count: usize) -> Vec<SseMessage> {
        let mut messages = Vec::with_capacity(count);
        for _ in 0..count {
            messages.push(self.next().await.expect("the body is still open"));
        }
        messages
    }

    /// Every message up to the end of the body. Panics if the body has not
    /// ended within `PATIENCE`.
    async fn rest(&mut self) -> Vec<SseMessage> {
        tokio::time::timeout(PATIENCE, async {
            let mut messages = Vec::new();
            while let Some(message) = self.next_unbounded().await {
                messages.push(message);
            }
            messages
        })
        .await
        .expect("the body ends in time")
    }

    /// Asserts that for `wait` the body neither sends a message nor ends.
    async fn is_quiet_for(&mut self, wait: Duration) {
        match tokio::time::timeout(wait, self.next_unbounded()).await {
            Err(_) => {}
            Ok(Some(message)) => panic!("the body sent {message:?}"),
            Ok(None) => panic!("the body ended"),
        }
    }

    /// Waits for the next heartbeat, leaving out messages, and returns when
    /// it came. `PATIENCE` is longer than the route's 15s heartbeat period,
    /// so under paused time the heartbeat comes before the wait gives up.
    async fn next_heartbeat(&mut self) -> tokio::time::Instant {
        tokio::time::timeout(PATIENCE, async {
            loop {
                match self.next_frame().await {
                    Some(frame) if frame == HEARTBEAT => return tokio::time::Instant::now(),
                    Some(_) => {}
                    None => panic!("the body ended before a heartbeat"),
                }
            }
        })
        .await
        .expect("a heartbeat comes in time")
    }
}

fn assert_event_stream(response: &Response) {
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers()["content-type"]
        .to_str()
        .expect("a text content type");
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    assert_eq!(response.headers()["x-accel-buffering"], "no");
}

mod summaries {
    use super::*;

    /// The run's summary compared key for key, so a key added to or renamed
    /// in the record's details fails it.
    #[tokio::test]
    async fn a_run_on_a_lane_reads_as_its_whole_summary() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_chat(CHAT)).await;
        ended(&state, run).await;

        let response = get(&state, &format!("/v1/runs/{run}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            normalised_run(json_body(response).await),
            json!({
                "run_id": run.to_string(),
                "agent": "solo",
                "status": "finished",
                "liveness": { "policy": "cancel", "grace_ms": 0 },
                "started_at": "<started_at>",
                "ended_at": "<ended_at>",
                "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
                "first_event": 1,
                "latest_event": 5,
                "pending_approvals": [],
                "checkpoint": null,
                "continues": null,
                "session_id": "cs_runtime_api:solo:0",
                "client_session_id": "cs_runtime_api",
                "observers": { "collecting": 0, "claiming": 0, "present": 0 },
            })
        );
    }

    #[tokio::test]
    async fn a_session_on_a_lane_reads_as_its_whole_summary() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_chat(CHAT)).await;
        ended(&state, run).await;

        let response = get(&state, &format!("/v1/sessions/{LANE_IN_PATH}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_body(response).await,
            json!({
                "session_id": "cs_runtime_api:solo:0",
                "client_session_id": "cs_runtime_api",
                "agent": "solo",
                "state": "done",
                "current_run": null,
                "last_run": run.to_string(),
                "latest_event": 5,
                "observers": { "collecting": 0, "claiming": 0, "present": 0 },
            })
        );
    }

    /// A session on a lane and a plain session, in session-id order. The
    /// plain session is not a conversation's, so it has no client session
    /// id.
    #[tokio::test]
    async fn the_session_listing_reads_as_its_whole_body() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let on_lane = start(&state, on_chat(CHAT)).await;
        ended(&state, on_lane).await;
        let on_plain = start(&state, on_session("s1")).await;
        ended(&state, on_plain).await;

        let response = get(&state, "/v1/sessions").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_body(response).await,
            json!({
                "sessions": [
                    done_session_summary(LANE, Some(CHAT), on_lane),
                    done_session_summary("s1", None, on_plain),
                ],
                "next_cursor": null,
            })
        );
    }

    #[tokio::test]
    async fn the_run_listing_reads_as_its_whole_body() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let on_lane = start(&state, on_chat(CHAT)).await;
        ended(&state, on_lane).await;
        let on_plain = start(&state, on_session("s1")).await;
        ended(&state, on_plain).await;

        let response = get(&state, "/v1/runs").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            normalised_runs(json_body(response).await),
            json!({
                "runs": [
                    finished_run_summary(on_lane, LANE, Some(CHAT)),
                    finished_run_summary(on_plain, "s1", None),
                ],
                "next_cursor": null,
            })
        );
    }

    /// The agent's id has a `:` in it, which the lane's session id escapes
    /// as `%3A`. A client percent-encodes the whole id in a path, and the
    /// route decodes it once.
    #[tokio::test]
    async fn a_lane_id_with_an_escaped_agent_reads_through_the_path() {
        let stores = Stores::new();
        let state = state_over(vec![config("my:agent")], &stores, |_, run| {
            answering(run, "answer")
        });
        let run = start(&state, on_chat("c1")).await;
        ended(&state, run).await;
        let in_path = "c1%3Amy%253Aagent%3A0";

        let session = json_body(get(&state, &format!("/v1/sessions/{in_path}")).await).await;
        assert_eq!(session["session_id"], "c1:my%3Aagent:0");
        assert_eq!(session["client_session_id"], "c1");
        assert_eq!(session["agent"], "my:agent");

        let runs = json_body(get(&state, &format!("/v1/runs?session_id={in_path}")).await).await;
        assert_eq!(run_ids_of(&runs), [run.to_string()]);

        let response = get(&state, &format!("/v1/sessions/{in_path}/events")).await;
        assert_event_stream(&response);
        let first = SseReader::new(response).next().await.expect("a message");
        assert_eq!(first.data["session_id"], "c1:my%3Aagent:0");
    }

    /// Once the runtime has let a run go, the run and its session are read
    /// from the stores and show no observers.
    #[tokio::test(start_paused = true)]
    async fn a_run_reads_from_the_store_after_its_retention() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_chat(CHAT)).await;
        ended(&state, run).await;

        tokio::time::sleep(aura::runtime::DEFAULT_RETENTION + Duration::from_secs(1)).await;
        eventually("the runtime lets the run go", || {
            state.runtime.facts(&run).is_none()
        })
        .await;

        let response = get(&state, &format!("/v1/runs/{run}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            normalised_run(json_body(response).await),
            finished_run_summary(run, LANE, Some(CHAT))
        );
        let response = get(&state, &format!("/v1/sessions/{LANE_IN_PATH}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_body(response).await,
            done_session_summary(LANE, Some(CHAT), run)
        );
    }
}

mod listings {
    use super::*;

    /// A state over the agents `a`, whose runs answer, and `b`, whose runs
    /// wait on `gate`.
    fn two_agents(stores: &Stores, gate: &Arc<Semaphore>) -> Arc<AppState> {
        let gate = Arc::clone(gate);
        state_over(
            vec![config("a"), config("b")],
            stores,
            move |config, run| match config.agent_id() {
                "b" => gated(&gate, run, "late answer"),
                _ => answering(run, "answer"),
            },
        )
    }

    #[tokio::test]
    async fn the_session_listing_selects_by_state_and_agent() {
        let stores = Stores::new();
        let gate = Arc::new(Semaphore::new(0));
        let state = two_agents(&stores, &gate);
        let done = start(&state, by_agent_on_session("a", "s1")).await;
        ended(&state, done).await;
        let live = start(&state, by_agent_on_session("b", "s2")).await;
        running(&state, live).await;

        for (query, expected) in [
            ("state=running", ["s2"]),
            ("state=done", ["s1"]),
            ("agent=a", ["s1"]),
            ("agent=b", ["s2"]),
        ] {
            let response = get(&state, &format!("/v1/sessions?{query}")).await;
            assert_eq!(response.status(), StatusCode::OK, "{query}");
            assert_eq!(
                session_ids_of(&json_body(response).await),
                expected,
                "{query}"
            );
        }
        gate.add_permits(1);
        ended(&state, live).await;
    }

    #[tokio::test]
    async fn the_run_listing_selects_by_session_status_and_pending_approval() {
        let stores = Stores::new();
        let gate = Arc::new(Semaphore::new(0));
        let state = two_agents(&stores, &gate);
        let first = start(&state, by_agent_on_session("a", "s1")).await;
        ended(&state, first).await;
        let second = start(&state, by_agent_on_session("a", "s1")).await;
        ended(&state, second).await;
        let live = start(&state, by_agent_on_session("b", "s2")).await;
        running(&state, live).await;
        let [first, second, live] = [first, second, live].map(|run| run.to_string());

        for (query, expected) in [
            ("session_id=s1", vec![first.as_str(), second.as_str()]),
            ("session_id=s2", vec![live.as_str()]),
            ("status=running", vec![live.as_str()]),
            ("status=finished", vec![first.as_str(), second.as_str()]),
            (
                "pending_approval=false",
                vec![first.as_str(), second.as_str(), live.as_str()],
            ),
            ("pending_approval=true", vec![]),
        ] {
            let response = get(&state, &format!("/v1/runs?{query}")).await;
            assert_eq!(response.status(), StatusCode::OK, "{query}");
            assert_eq!(run_ids_of(&json_body(response).await), expected, "{query}");
        }
        gate.add_permits(1);
    }

    /// `client_session_id` is the chat id, which the runtime's query calls
    /// `chat`. It selects the lanes of that chat's conversation, and not a
    /// plain session that happens to have the chat id as its id.
    #[tokio::test]
    async fn client_session_id_reaches_both_listings_as_the_chat() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let on_c1 = start(&state, on_chat("c1")).await;
        ended(&state, on_c1).await;
        let on_c2 = start(&state, on_chat("c2")).await;
        ended(&state, on_c2).await;
        let named_c1 = start(&state, on_session("c1")).await;
        ended(&state, named_c1).await;

        let sessions = json_body(get(&state, "/v1/sessions?client_session_id=c1").await).await;
        assert_eq!(session_ids_of(&sessions), ["c1:solo:0"]);
        let runs = json_body(get(&state, "/v1/runs?client_session_id=c1").await).await;
        assert_eq!(run_ids_of(&runs), [on_c1.to_string()]);

        let runs = json_body(get(&state, "/v1/runs?session_id=c1").await).await;
        assert_eq!(run_ids_of(&runs), [named_c1.to_string()]);
    }

    #[tokio::test]
    async fn both_listings_page_by_next_cursor() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let mut runs = Vec::new();
        for session in ["s1", "s2", "s3"] {
            let run = start(&state, on_session(session)).await;
            ended(&state, run).await;
            runs.push(run.to_string());
        }

        let first = json_body(get(&state, "/v1/sessions?limit=2").await).await;
        assert_eq!(session_ids_of(&first), ["s1", "s2"]);
        assert_eq!(first["next_cursor"], "s2");
        let second = json_body(get(&state, "/v1/sessions?limit=2&cursor=s2").await).await;
        assert_eq!(session_ids_of(&second), ["s3"]);
        assert_eq!(second["next_cursor"], Value::Null);

        let first = json_body(get(&state, "/v1/runs?limit=2").await).await;
        assert_eq!(run_ids_of(&first), runs[..2]);
        assert_eq!(first["next_cursor"], runs[1].as_str());
        let second =
            json_body(get(&state, &format!("/v1/runs?limit=2&cursor={}", runs[1])).await).await;
        assert_eq!(run_ids_of(&second), runs[2..]);
        assert_eq!(second["next_cursor"], Value::Null);
    }
}

mod event_streams {
    use super::*;

    /// A session's stream from its start: the run's events, then the
    /// reader's own attach, which names no run since the run has ended.
    #[tokio::test]
    async fn the_events_of_a_session_are_its_whole_stream_from_the_start() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_chat(CHAT)).await;
        ended(&state, run).await;

        let response = get(&state, &format!("/v1/sessions/{LANE_IN_PATH}/events")).await;
        assert_event_stream(&response);
        let messages = SseReader::new(response).take(6).await;

        let mut expected = answered_run(LANE, run, 1, "answer 1");
        expected.push(observer_attached(LANE, None, 6));
        assert_eq!(
            messages.into_iter().map(normalised).collect::<Vec<_>>(),
            expected
        );
    }

    /// The body of an ended run is its events from its `Started` to its
    /// ending, and nothing after: the reader's own attach comes after the
    /// ending and is not sent.
    #[tokio::test]
    async fn an_ended_run_replays_from_its_started_and_ends() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_chat(CHAT)).await;
        ended(&state, run).await;

        let response = get(&state, &format!("/v1/runs/{run}/events")).await;
        assert_event_stream(&response);
        let messages = SseReader::new(response).rest().await;

        assert_eq!(
            messages.into_iter().map(normalised).collect::<Vec<_>>(),
            answered_run(LANE, run, 1, "answer 1")
        );
    }

    /// A reader that saw up to 3 and comes back with `after=3` gets 4 next,
    /// and then each number in turn.
    #[tokio::test]
    async fn after_resumes_with_no_duplicate_and_no_hole() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;

        let mut first_reader = SseReader::new(get(&state, "/v1/sessions/s1/events").await);
        let seen = first_reader.take(3).await;
        assert_eq!(
            seen.last().map(|message| &message.data["seq"]),
            Some(&json!(3))
        );
        drop(first_reader);

        let response = get(&state, "/v1/sessions/s1/events?after=3").await;
        assert_event_stream(&response);
        let resumed = SseReader::new(response).take(3).await;
        let numbers: Vec<&Value> = resumed.iter().map(|message| &message.data["seq"]).collect();
        assert_eq!(numbers, [&json!(4), &json!(5), &json!(6)]);
        assert_eq!(
            resumed[..2]
                .iter()
                .cloned()
                .map(normalised)
                .collect::<Vec<_>>(),
            answered_run("s1", run, 1, "answer 1")[3..]
        );
    }

    /// The journal keeps three events of a session, so a reader from the
    /// start finds the earlier ones gone. The gap message says where the
    /// stream resumes, and the next event is that one.
    #[tokio::test]
    async fn a_small_journal_bound_gives_a_gap_message() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(3, 8));
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;

        let mut reader = SseReader::new(get(&state, "/v1/sessions/s1/events").await);
        let gap = reader.next().await.expect("a gap message");
        assert_eq!(gap.event.as_deref(), Some("gap"), "{gap:?}");
        let resumed_at = gap.data["resumed_at"]
            .as_u64()
            .unwrap_or_else(|| panic!("the gap says where the stream resumes: {gap:?}"));
        assert!(resumed_at > 1, "{gap:?}");
        assert_eq!(gap.data, json!({ "resumed_at": resumed_at }));
        let next = reader
            .next()
            .await
            .expect("the event the stream resumes at");
        assert_eq!(next.event, None);
        assert_eq!(next.data["seq"], resumed_at);
    }

    /// Both events hold a lifecycle variant this binary reads as unknown,
    /// so neither can be written as JSON. Each is an `unknown` message with
    /// its envelope and without its payload.
    #[tokio::test]
    async fn an_event_that_cannot_be_written_as_json_is_an_unknown_message() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let session = session_id("s_unknown");
        let run = RunId::mint();
        stores
            .runs
            .put(&RunFacts {
                session: RunSession::Plain(session.clone()),
                run: RunDetails {
                    run_id: run,
                    agent: AGENT.to_owned(),
                    status: RunStatus::Finished,
                    liveness: Liveness::default(),
                    started_at: Timestamp::from_unix_millis(1_763_568_000_000),
                    ended_at: Some(Timestamp::from_unix_millis(1_763_568_000_002)),
                    usage: TokenUsage::default(),
                    first_event: Some(seq(1)),
                    latest_event: Some(seq(2)),
                    pending_approvals: Vec::new(),
                    checkpoint: None,
                    continues: None,
                },
            })
            .await
            .expect("the run store keeps the run");
        let unknown = || SessionEventPayload::Lifecycle(LifecycleEvent::Unknown);
        stores
            .journals
            .append(
                &session,
                None,
                Some(run),
                Timestamp::from_unix_millis(1_763_568_000_001),
                unknown(),
            )
            .await
            .expect("the journal keeps the event");
        stores
            .journals
            .append(
                &session,
                Some(seq(1)),
                None,
                Timestamp::from_unix_millis(1_763_568_000_002),
                unknown(),
            )
            .await
            .expect("the journal keeps the event");

        let response = get(&state, "/v1/sessions/s_unknown/events").await;
        assert_event_stream(&response);
        let messages = SseReader::new(response).take(2).await;
        assert_eq!(
            messages,
            [
                SseMessage {
                    event: Some("unknown".to_owned()),
                    data: json!({
                        "session_id": "s_unknown",
                        "run_id": run.to_string(),
                        "seq": 1,
                        "at": 1_763_568_000_001_u64,
                    }),
                },
                SseMessage {
                    event: Some("unknown".to_owned()),
                    data: json!({
                        "session_id": "s_unknown",
                        "seq": 2,
                        "at": 1_763_568_000_002_u64,
                    }),
                },
            ]
        );
    }

    /// An id nobody has run on is refused before the attach, which would
    /// open the session and write to its journal.
    #[tokio::test]
    async fn the_events_of_an_unknown_session_are_404_and_write_nothing() {
        let stores = Stores::new();
        let state = answering_state(&stores);

        let response = get(&state, "/v1/sessions/nope/events").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let (_, message, kind) = refusal(response).await;
        assert_eq!(kind, "not_found");
        assert!(!message.is_empty());
        assert_eq!(
            stores
                .journals
                .latest(&session_id("nope"))
                .await
                .expect("the journal store answers"),
            None,
            "nothing was written to the session's journal"
        );
        assert_eq!(
            state.runtime.observer_counts(&session_id("nope")),
            None,
            "the runtime opened no session"
        );
    }

    #[tokio::test]
    async fn a_second_claim_is_409_naming_the_claimant() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        let holder = state
            .runtime
            .attach(&session_id("s1"), Attach::claiming())
            .await
            .expect("the holder claims the session");
        let claimant = holder.observer().id.to_string();

        for uri in [
            "/v1/sessions/s1/events?claim=true".to_owned(),
            format!("/v1/runs/{run}/events?claim=true"),
        ] {
            let response = get(&state, &uri).await;
            assert_eq!(response.status(), StatusCode::CONFLICT, "{uri}");
            let (_, message, kind) = refusal(response).await;
            assert_eq!(kind, "session_claimed", "{uri}");
            assert!(message.contains(&claimant), "{uri}: {message}");
        }
        drop(holder);
    }

    /// Each reader is counted by what its query asked for: `presence=true`
    /// adds presence, `claim=true` claims, and neither collects without
    /// presence.
    #[tokio::test]
    async fn presence_and_claim_show_in_the_sessions_counts() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        let mut readers = Vec::new();

        for (query, collecting, claiming, present) in [
            ("presence=true", 1, 0, 1),
            ("claim=true&presence=true", 1, 1, 2),
            ("", 2, 1, 2),
        ] {
            let response = get(&state, &format!("/v1/sessions/s1/events?{query}")).await;
            assert_event_stream(&response);
            readers.push(response);
            let session = json_body(get(&state, "/v1/sessions/s1").await).await;
            assert_eq!(
                session["observers"],
                json!({ "collecting": collecting, "claiming": claiming, "present": present }),
                "after a reader with `{query}`"
            );
        }
    }

    /// The run is the session's second, so the stream holds the first run's
    /// events before it. The body starts at the second run's `Started`, and
    /// ends after its ending.
    #[tokio::test]
    async fn the_run_route_starts_at_the_runs_started_and_ends_after_its_ending() {
        let stores = Stores::new();
        let gate = Arc::new(Semaphore::new(0));
        let count_of_runs = AtomicUsize::new(0);
        let state = state_over(vec![config(AGENT)], &stores, {
            let gate = Arc::clone(&gate);
            move |_, run| match count_of_runs.fetch_add(1, Ordering::SeqCst) {
                0 => answering(run, "answer 1"),
                _ => gated(&gate, run, "answer 2"),
            }
        });
        let first = start(&state, on_session("s1")).await;
        ended(&state, first).await;
        let second = start(&state, on_session("s1")).await;
        running(&state, second).await;

        let response = get(&state, &format!("/v1/runs/{second}/events")).await;
        assert_event_stream(&response);
        let mut reader = SseReader::new(response);
        let opening = reader.next().await.expect("the run's first message");
        assert_eq!(
            normalised(opening),
            envelope("s1", Some(second), 6, started_payload(AGENT))
        );

        gate.add_permits(1);
        let rest = reader.rest().await;
        let facts = ended(&state, second).await;
        assert!(
            rest.iter()
                .all(|message| message.data["run_id"] == second.to_string()),
            "{rest:?}"
        );
        assert_eq!(
            rest.last().cloned().map(normalised),
            Some(envelope(
                "s1",
                Some(second),
                latest_event_of(&facts),
                finished_payload()
            ))
        );
    }

    #[tokio::test]
    async fn on_a_sessions_second_run_after_0_reads_no_event_of_the_first() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let first = start(&state, on_session("s1")).await;
        ended(&state, first).await;
        let second = start(&state, on_session("s1")).await;
        ended(&state, second).await;

        let response = get(&state, &format!("/v1/runs/{second}/events?after=0")).await;
        assert_event_stream(&response);
        let messages = SseReader::new(response).rest().await;

        assert_eq!(
            messages.into_iter().map(normalised).collect::<Vec<_>>(),
            answered_run("s1", second, 6, "answer 2")
        );
    }

    /// A reader that has seen the run's ending has nothing left to read.
    /// The body is empty, and nothing is attached, so nothing is written to
    /// the session's journal.
    #[tokio::test]
    async fn a_run_read_past_its_ending_is_an_empty_stream_without_an_attach() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;

        let response = get(&state, &format!("/v1/runs/{run}/events?after=5")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "");
        assert_eq!(
            stores
                .journals
                .latest(&session_id("s1"))
                .await
                .expect("the journal store answers"),
            Some(seq(5)),
            "nothing was written after the run's ending"
        );
    }

    /// The journal keeps two events of a session: the run's ending and the
    /// reader's own attach. The reader gets a gap and then the ending.
    #[tokio::test]
    async fn an_ended_run_with_only_its_ending_left_is_a_gap_and_the_ending() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(2, 8));
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        let latest = latest_event_of(&ended(&state, run).await);

        let response = get(&state, &format!("/v1/runs/{run}/events")).await;
        assert_event_stream(&response);
        let messages = SseReader::new(response).rest().await;

        let [gap, ending] = messages.as_slice() else {
            panic!("a gap and the run's ending: {messages:?}");
        };
        assert_eq!(gap.event.as_deref(), Some("gap"), "{gap:?}");
        assert_eq!(gap.data, json!({ "resumed_at": latest }));
        assert_eq!(ending.event, None, "{ending:?}");
        assert_eq!(ending.data["seq"], latest, "{ending:?}");
        assert_eq!(
            ending.data["payload"]["event"]["type"], "finished",
            "{ending:?}"
        );
    }

    /// The run ended and the runtime let it go. Then the journal store
    /// forgot the session entirely, its newest number included, so a
    /// reopened journal would number from the start and never reach the
    /// run's last number. The run's stream is one gap past the run, and
    /// nothing is attached.
    #[tokio::test(start_paused = true)]
    async fn an_ended_run_whose_journal_was_forgotten_is_one_gap_past_it() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(16, 1));
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        let facts = ended(&state, run).await;
        tokio::time::sleep(aura::runtime::DEFAULT_RETENTION + Duration::from_secs(1)).await;
        eventually("the runtime lets the session go", || {
            state.runtime.observer_counts(&session_id("s1")).is_none()
        })
        .await;
        stores.forget_journal_of("s1").await;

        let response = get(&state, &format!("/v1/runs/{run}/events")).await;
        assert_event_stream(&response);
        let messages = SseReader::new(response).rest().await;

        assert_eq!(
            messages,
            [SseMessage {
                event: Some("gap".to_owned()),
                data: json!({ "resumed_at": latest_event_of(&facts) + 1 }),
            }]
        );
        assert_eq!(
            stores
                .journals
                .latest(&session_id("s1"))
                .await
                .expect("the journal store answers"),
            None,
            "nothing was attached"
        );
    }

    /// A run whose agent could not be built has one event, its ending. The
    /// runtime let the run go and the journal store forgot its session, so
    /// an attach reopens the session numbering from the start, and the
    /// attach's own event takes the run's number. The run route is left
    /// there when the store forgets the session between the route's first
    /// check and its attach. The check after the attach finds the attach's
    /// event at the run's number and answers one gap past the run.
    #[tokio::test(start_paused = true)]
    async fn after_the_attach_an_observers_event_at_the_runs_number_is_one_gap_past_it() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(16, 1));
        let state = state_over(vec![config(AGENT)], &stores, |_, _| {
            Err(aura::BuilderError::AgentError("no provider".to_owned()))
        });
        let run = start(&state, on_session("s1")).await;
        let facts = ended(&state, run).await;
        assert_eq!(
            (facts.run.first_event, facts.run.latest_event),
            (None, Some(seq(1))),
            "the run's only event is its ending"
        );
        tokio::time::sleep(aura::runtime::DEFAULT_RETENTION + Duration::from_secs(1)).await;
        eventually("the runtime lets the session go", || {
            state.runtime.observer_counts(&session_id("s1")).is_none()
        })
        .await;
        stores.forget_journal_of("s1").await;
        let range = state
            .runtime
            .run(&run)
            .await
            .expect("the stores answer")
            .expect("the run store has the run")
            .events_after(None)
            .expect("the run has an event to read");
        let s1 = session_id("s1");
        let _reader = state
            .runtime
            .attach(&s1, Attach::collecting().from(range.from))
            .await
            .expect("the observer attaches");
        assert_eq!(
            state
                .runtime
                .session(&s1)
                .await
                .expect("the stores answer")
                .as_ref()
                .and_then(SessionSnapshot::latest_event),
            Some(seq(1)),
            "the attach's own event took the run's number"
        );

        let response = crate::runtime_api::lost_events_response(&state, run, &s1, range.to)
            .await
            .expect("the run's events read as lost");
        assert_event_stream(&response);
        let messages = SseReader::new(response).rest().await;

        assert_eq!(
            messages,
            [SseMessage {
                event: Some("gap".to_owned()),
                data: json!({ "resumed_at": 2 }),
            }]
        );
    }

    /// The run ended, the runtime let it go and the journal store forgot
    /// its session. Observers then reopened the session, numbering it from
    /// the start, until its newest number passed the run's last. The run's
    /// number now holds an observer's event, so the run's stream is one gap
    /// past the run.
    #[tokio::test(start_paused = true)]
    async fn a_journal_observers_renumbered_past_the_run_is_one_gap_past_it() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(16, 1));
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        let facts = ended(&state, run).await;
        tokio::time::sleep(aura::runtime::DEFAULT_RETENTION + Duration::from_secs(1)).await;
        eventually("the runtime lets the session go", || {
            state.runtime.observer_counts(&session_id("s1")).is_none()
        })
        .await;
        stores.forget_journal_of("s1").await;
        let s1 = session_id("s1");
        let newest = || async {
            stores
                .journals
                .latest(&s1)
                .await
                .expect("the journal store answers")
                .map_or(0, SequenceNumber::get)
        };
        while newest().await < latest_event_of(&facts) {
            state
                .runtime
                .attach(&s1, Attach::collecting())
                .await
                .expect("the observer attaches")
                .detach()
                .await;
        }

        let response = get(&state, &format!("/v1/runs/{run}/events")).await;
        assert_event_stream(&response);
        let messages = SseReader::new(response).rest().await;

        assert_eq!(
            messages,
            [SseMessage {
                event: Some("gap".to_owned()),
                data: json!({ "resumed_at": latest_event_of(&facts) + 1 }),
            }]
        );
    }

    /// The journal store forgot the session, and an observer then reopened
    /// it, numbering it from the start short of a number a reader saw on the
    /// old stream. A session events request with that `after` is refused, by
    /// `GET` and `HEAD` alike, so the reader reads the session again from
    /// the start rather than wait past the session's new events.
    #[tokio::test(start_paused = true)]
    async fn a_session_read_after_a_number_past_its_newest_event_is_refused() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(16, 1));
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        let seen = latest_event_of(&ended(&state, run).await);
        tokio::time::sleep(aura::runtime::DEFAULT_RETENTION + Duration::from_secs(1)).await;
        eventually("the runtime lets the session go", || {
            state.runtime.observer_counts(&session_id("s1")).is_none()
        })
        .await;
        stores.forget_journal_of("s1").await;
        state
            .runtime
            .attach(&session_id("s1"), Attach::collecting())
            .await
            .expect("the observer attaches")
            .detach()
            .await;
        let newest = stores
            .journals
            .latest(&session_id("s1"))
            .await
            .expect("the journal store answers")
            .map_or(0, SequenceNumber::get);
        assert!(newest < seen, "{newest} is short of {seen}");

        let uri = format!("/v1/sessions/s1/events?after={seen}");
        let (status, message, kind) = refusal(get(&state, &uri).await).await;
        assert_eq!(
            (status, kind.as_str()),
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        );
        assert!(message.contains("after"), "{message}");
        let head = send(
            &state,
            Request::head(&uri).body(Body::empty()).expect("a request"),
        )
        .await;
        assert_eq!(head.status(), StatusCode::BAD_REQUEST);
    }

    /// A live run's reader whose last event was an observer's, past the
    /// run's own newest event, reads on from there and gets nothing a second
    /// time. An `after` past the session's newest event is refused.
    #[tokio::test]
    async fn a_live_run_read_after_an_observers_event_sends_nothing_again() {
        let stores = Stores::new();
        let state = state_over(vec![config(AGENT)], &stores, |_, run| pending(run));
        let run = start(&state, on_session("s1")).await;
        running(&state, run).await;
        let s1 = session_id("s1");
        state
            .runtime
            .attach(&s1, Attach::collecting())
            .await
            .expect("the observer attaches")
            .detach()
            .await;
        let newest = stores
            .journals
            .latest(&s1)
            .await
            .expect("the journal store answers")
            .expect("the session has events")
            .get();

        let mut reader =
            SseReader::new(get(&state, &format!("/v1/runs/{run}/events?after={newest}")).await);
        let first = reader.next().await.expect("the stream sends a message");
        assert!(
            first.data["seq"].as_u64() > Some(newest),
            "after {newest}: {first:?}"
        );
        drop(reader);
        let (status, _, kind) = refusal(
            get(
                &state,
                &format!("/v1/runs/{run}/events?after={}", newest + 10),
            )
            .await,
        )
        .await;
        assert_eq!(
            (status, kind.as_str()),
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        );
        state.runtime.drain(Duration::ZERO).await;
    }

    /// The runtime still holds an ended run whose row the run store dropped
    /// for a newer run. Its journal is whole, so the run route reads the
    /// run's events.
    #[tokio::test]
    async fn a_held_run_whose_row_the_run_store_dropped_reads_its_events() {
        let stores = Stores {
            runs: Arc::new(InMemoryRunStore::with_bounds(1)),
            journals: Arc::new(InMemoryJournalStore::new()),
        };
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        let other = start(&state, on_session("s2")).await;
        ended(&state, other).await;
        assert_eq!(
            stored_facts(&state, run).await,
            None,
            "the run store dropped the run"
        );

        let response = get(&state, &format!("/v1/runs/{run}/events")).await;
        assert_event_stream(&response);
        let messages = SseReader::new(response).rest().await;

        assert_eq!(
            messages.into_iter().map(normalised).collect::<Vec<_>>(),
            answered_run("s1", run, 1, "answer 1")
        );
    }

    /// A HEAD on an events route answers as the GET would before its attach:
    /// the stream's headers for a session or run the server has, and 404
    /// for one it does not. It attaches nothing, so nothing is written to
    /// the session.
    #[tokio::test]
    async fn a_head_on_an_events_route_attaches_nothing() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        let latest = || async {
            stores
                .journals
                .latest(&session_id("s1"))
                .await
                .expect("the journal store answers")
        };
        let before = latest().await;
        let head = |uri: &str| Request::head(uri).body(Body::empty()).expect("a request");

        for uri in [
            "/v1/sessions/s1/events".to_owned(),
            format!("/v1/runs/{run}/events"),
        ] {
            let response = send(&state, head(&uri)).await;
            assert_event_stream(&response);
            assert_eq!(body_text(response).await, "", "{uri}");
        }
        for uri in [
            "/v1/sessions/unknown/events".to_owned(),
            format!("/v1/runs/{}/events", RunId::mint()),
        ] {
            let response = send(&state, head(&uri)).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }

        // A detach is written after its response, so give one time to land.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(latest().await, before, "nothing was written to the session");
    }

    #[tokio::test]
    async fn dropping_the_body_detaches_the_observer() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        let s1 = session_id("s1");

        let response = get(&state, "/v1/sessions/s1/events").await;
        assert_event_stream(&response);
        assert_eq!(
            state.runtime.observer_counts(&s1).unwrap_or_default(),
            ObserverCounts {
                collecting: 1,
                claiming: 0,
                present: 0,
            }
        );

        drop(response);
        eventually("the observer is detached", || {
            state.runtime.observer_counts(&s1).unwrap_or_default() == ObserverCounts::default()
        })
        .await;
    }

    /// The server's first shutdown phase waits for the runtime to be idle
    /// and does not end a stream. With no run going, the stream ends once
    /// the server is shutting down.
    #[tokio::test]
    async fn shutdown_ends_a_stream_once_the_runtime_is_idle() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        let mut reader = SseReader::new(get(&state, "/v1/sessions/s1/events").await);
        reader.take(6).await;
        reader.is_quiet_for(Duration::from_millis(200)).await;

        state.shutdown_token.cancel();
        assert_eq!(reader.rest().await, [], "nothing more is sent");
    }

    /// A run still going when the server starts shutting down keeps the
    /// stream open, and the stream sends the run's ending before it ends.
    #[tokio::test]
    async fn a_run_that_ends_after_shutdown_began_still_sends_its_ending() {
        let stores = Stores::new();
        let gate = Arc::new(Semaphore::new(0));
        let state = gated_state(&stores, &gate);
        let run = start(&state, on_session("s1")).await;
        running(&state, run).await;
        let mut reader = SseReader::new(get(&state, "/v1/sessions/s1/events").await);
        // The run's `Started` and `SessionInfo`, and the reader's own attach.
        reader.take(3).await;

        state.shutdown_token.cancel();
        reader.is_quiet_for(Duration::from_millis(200)).await;
        gate.add_permits(1);
        let messages = reader.rest().await;
        let facts = ended(&state, run).await;

        assert_eq!(
            messages.last().cloned().map(normalised),
            Some(envelope(
                "s1",
                Some(run),
                latest_event_of(&facts),
                finished_payload()
            ))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_events_stream_sends_a_heartbeat_every_15_seconds() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        let mut reader = SseReader::new(get(&state, "/v1/sessions/s1/events").await);
        reader.take(6).await;

        let first = reader.next_heartbeat().await;
        let second = reader.next_heartbeat().await;
        assert_eq!(second - first, Duration::from_secs(15));
    }
}

mod cancels {
    use super::*;

    /// The run's agent never notices the stop, so the run ends once the
    /// runtime gives up on its stream. Until then the session reads
    /// `stopping`.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_is_202_and_the_session_reads_stopping_until_the_run_ends() {
        let stores = Stores::new();
        let state = state_over(vec![config(AGENT)], &stores, |_, run| pending(run));
        let run = start(&state, on_session("s1")).await;
        running(&state, run).await;

        let response = cancel(&state, run).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let summary = json_body(response).await;
        assert_eq!(summary["run_id"], run.to_string());
        assert_eq!(summary["status"], "running");

        let session = json_body(get(&state, "/v1/sessions/s1").await).await;
        assert_eq!(session["state"], "stopping");
        assert_eq!(session["current_run"], run.to_string());
        let stopping = json_body(get(&state, "/v1/sessions?state=stopping").await).await;
        assert_eq!(session_ids_of(&stopping), ["s1"]);

        let facts = ended(&state, run).await;
        assert_eq!(facts.run.status, RunStatus::Cancelled);
        let session = json_body(get(&state, "/v1/sessions/s1").await).await;
        assert_eq!(session["state"], "done");
        let ending = ending_of(&state, "s1", run).await;
        assert!(
            matches!(
                ending,
                LifecycleEvent::Cancelled {
                    cause: CancelCause::External,
                    reason: None,
                    ..
                }
            ),
            "a cancel with no body has no reason: {ending:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_cancel_is_202_and_the_first_reason_stands() {
        let stores = Stores::new();
        let state = state_over(vec![config(AGENT)], &stores, |_, run| pending(run));
        let run = start(&state, on_session("s1")).await;
        running(&state, run).await;

        for reason in ["first", "second"] {
            let response = cancel_with(
                &state,
                run,
                Some("application/json"),
                &json!({ "reason": reason }).to_string(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::ACCEPTED, "{reason}");
        }

        ended(&state, run).await;
        let ending = ending_of(&state, "s1", run).await;
        assert!(
            matches!(
                &ending,
                LifecycleEvent::Cancelled {
                    cause: CancelCause::External,
                    reason: Some(reason),
                    ..
                } if reason == "first"
            ),
            "{ending:?}"
        );
    }

    #[tokio::test]
    async fn cancelling_an_ended_run_is_200_with_its_status() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;

        let response = cancel(&state, run).await;
        assert_eq!(response.status(), StatusCode::OK);
        let summary = json_body(response).await;
        assert_eq!(summary["run_id"], run.to_string());
        assert_eq!(summary["status"], "finished");
    }

    /// The body is JSON, but sent as plain text or with no content type, so
    /// the cancel is refused and the run goes on.
    #[tokio::test]
    async fn a_cancel_body_sent_as_other_than_json_is_refused_and_stops_nothing() {
        let stores = Stores::new();
        let state = state_over(vec![config(AGENT)], &stores, |_, run| pending(run));
        let run = start(&state, on_session("s1")).await;
        running(&state, run).await;

        for content_type in [Some("text/plain"), None] {
            let response = cancel_with(&state, run, content_type, r#"{"reason":"x"}"#).await;
            let (status, message, kind) = refusal(response).await;
            assert_eq!(
                status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{content_type:?}"
            );
            assert_eq!(kind, "invalid_request_error", "{content_type:?}");
            assert!(!message.is_empty(), "{content_type:?}");
        }
        assert_eq!(
            state.runtime.session_state(&session_id("s1")),
            Some(AgentState::Running)
        );
        state.runtime.drain(Duration::ZERO).await;
    }

    /// A body sent as other than JSON is refused before it is read, so one
    /// that never ends is refused at once.
    #[tokio::test]
    async fn a_cancel_body_sent_as_other_than_json_is_refused_unread() {
        let stores = Stores::new();
        let state = state_over(vec![config(AGENT)], &stores, |_, run| pending(run));
        let run = start(&state, on_session("s1")).await;
        running(&state, run).await;

        for content_type in [Some("text/plain"), None] {
            let mut request = Request::post(format!("/v1/runs/{run}/cancel"));
            if let Some(content_type) = content_type {
                request = request.header("content-type", content_type);
            }
            let unending = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
            let response = tokio::time::timeout(
                PATIENCE,
                send(
                    &state,
                    request
                        .body(Body::from_stream(unending))
                        .expect("a request"),
                ),
            )
            .await
            .unwrap_or_else(|_| panic!("refused without reading the body: {content_type:?}"));
            assert_eq!(
                response.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{content_type:?}"
            );
        }
        assert_eq!(
            state.runtime.session_state(&session_id("s1")),
            Some(AgentState::Running)
        );
        state.runtime.drain(Duration::ZERO).await;
    }

    /// A JSON body that does not parse, or that has a field a cancel does
    /// not take, is refused, and the run goes on.
    #[tokio::test]
    async fn a_cancel_body_that_is_not_a_cancel_request_is_400_and_stops_nothing() {
        let stores = Stores::new();
        let state = state_over(vec![config(AGENT)], &stores, |_, run| pending(run));
        let run = start(&state, on_session("s1")).await;
        running(&state, run).await;

        for body in [r#"{"reason":"#, r#"{"message":"x"}"#] {
            let response = cancel_with(&state, run, Some("application/json"), body).await;
            let (status, message, kind) = refusal(response).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(kind, "invalid_request_error", "{body}");
            assert!(!message.is_empty(), "{body}");
        }
        assert_eq!(
            state.runtime.session_state(&session_id("s1")),
            Some(AgentState::Running)
        );
        state.runtime.drain(Duration::ZERO).await;
    }
}

mod refusals {
    use super::*;

    /// A missing route is axum's empty 404. A missing session or run is a
    /// 404 in the error envelope.
    #[tokio::test]
    async fn an_unknown_session_or_run_is_404_in_the_error_envelope() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let unknown = RunId::mint();

        for response in [
            get(&state, "/v1/sessions/nope").await,
            get(&state, &format!("/v1/runs/{unknown}")).await,
            get(&state, &format!("/v1/runs/{unknown}/events")).await,
            cancel(&state, unknown).await,
        ] {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let (_, message, kind) = refusal(response).await;
            assert_eq!(kind, "not_found");
            assert!(!message.is_empty());
        }
    }

    /// After a drain the runtime attaches no observer, so either events
    /// route answers as the server does while it shuts down.
    #[tokio::test]
    async fn an_events_route_on_a_drained_runtime_is_503() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        state.runtime.drain(Duration::ZERO).await;

        for uri in [
            "/v1/sessions/s1/events".to_owned(),
            format!("/v1/runs/{run}/events"),
        ] {
            let (status, message, kind) = refusal(get(&state, &uri).await).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(message, "Server is shutting down", "{uri}");
            assert_eq!(kind, "service_unavailable", "{uri}");
        }
    }

    #[tokio::test]
    async fn a_query_or_id_that_cannot_be_parsed_is_400_in_the_error_envelope() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;

        let mut responses = Vec::new();
        for uri in [
            "/v1/sessions?limit=abc".to_owned(),
            "/v1/sessions?cursor=".to_owned(),
            "/v1/sessions?state=runing".to_owned(),
            "/v1/sessions?staet=running".to_owned(),
            "/v1/runs?status=nope".to_owned(),
            "/v1/runs?cursor=nope".to_owned(),
            "/v1/runs?limit=-1".to_owned(),
            "/v1/runs/not-a-run".to_owned(),
            "/v1/runs/not-a-run/events".to_owned(),
            "/v1/sessions/s1/events?after=x".to_owned(),
            format!("/v1/runs/{run}/events?format=chat"),
        ] {
            responses.push((uri.clone(), get(&state, &uri).await));
        }
        responses.push((
            "POST /v1/runs/not-a-run/cancel".to_owned(),
            send(
                &state,
                Request::post("/v1/runs/not-a-run/cancel")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await,
        ));

        for (request, response) in responses {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{request}");
            let (_, message, kind) = refusal(response).await;
            assert_eq!(kind, "invalid_request_error", "{request}");
            assert!(!message.is_empty(), "{request}");
        }
    }
}

/// The events body read over subscriptions the runtime hands out, with a
/// frame that names each item, so what ends the body is seen apart from
/// what the envelope projection sends.
mod events_body {
    use bytes::Bytes;
    use futures_util::StreamExt;
    use futures_util::stream::BoxStream;

    use super::super::events::events_body;
    use super::*;

    /// One line naming the item.
    fn named_frame(item: &JournalItem) -> Option<Bytes> {
        let line = match item {
            JournalItem::Event(event) => format!("event {}\n", event.seq),
            JournalItem::Gap { resumed_at } => format!("gap {resumed_at}\n"),
            JournalItem::Unknown { seq, .. } => format!("unknown {seq}\n"),
        };
        Some(Bytes::from(line))
    }

    /// Everything the body sends, up to its end.
    async fn text_of(body: BoxStream<'static, Result<Bytes, std::io::Error>>) -> String {
        tokio::time::timeout(
            PATIENCE,
            body.map(|chunk| {
                String::from_utf8(chunk.expect("a body chunk").to_vec()).expect("UTF-8")
            })
            .collect::<String>(),
        )
        .await
        .expect("the body ends in time")
    }

    /// A collecting observer of `session`, reading from the start.
    async fn reader_of(state: &Arc<AppState>, session: &str) -> aura::runtime::Subscription {
        state
            .runtime
            .attach(
                &session_id(session),
                Attach::collecting().from(Cursor::Start),
            )
            .await
            .expect("the observer attaches")
    }

    /// A state with one run on `s1`, which answered and ended: events 1 to
    /// 5 on its stream.
    async fn one_ended_run(stores: &Stores) -> (Arc<AppState>, RunId) {
        let state = answering_state(stores);
        let run = start(&state, on_session("s1")).await;
        ended(&state, run).await;
        (state, run)
    }

    #[tokio::test]
    async fn the_body_ends_after_sending_the_item_at_to() {
        let (state, _) = one_ended_run(&Stores::new()).await;
        let body = events_body(
            reader_of(&state, "s1").await,
            named_frame,
            None,
            Some(seq(3)),
            std::future::pending(),
            None,
        );
        assert_eq!(text_of(body).await, "event 1\nevent 2\nevent 3\n");
    }

    /// The reader attaches while the run is going, so its own attach is
    /// among the run's events. The body ends with the run's ending.
    #[tokio::test]
    async fn the_body_ends_after_the_runs_terminal_event() {
        let stores = Stores::new();
        let gate = Arc::new(Semaphore::new(0));
        let state = gated_state(&stores, &gate);
        let run = start(&state, on_session("s1")).await;
        running(&state, run).await;
        let body = events_body(
            reader_of(&state, "s1").await,
            named_frame,
            Some(run),
            None,
            std::future::pending(),
            None,
        );

        gate.add_permits(1);
        let text = text_of(body).await;
        let latest = latest_event_of(&ended(&state, run).await);
        let expected: String = (1..=latest).map(|n| format!("event {n}\n")).collect();
        assert_eq!(text, expected);
    }

    /// The journal keeps three events of a session, so the first item a
    /// reader from the start gets is a gap past `to`. The gap is sent, and
    /// the body ends.
    #[tokio::test]
    async fn a_gap_past_to_is_sent_and_ends_the_body() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(3, 8));
        let (state, _) = one_ended_run(&stores).await;
        let body = events_body(
            reader_of(&state, "s1").await,
            named_frame,
            None,
            Some(seq(2)),
            std::future::pending(),
            None,
        );

        let text = text_of(body).await;
        let resumed_at: u64 = text
            .strip_prefix("gap ")
            .and_then(|rest| rest.strip_suffix('\n'))
            .and_then(|number| number.parse().ok())
            .unwrap_or_else(|| panic!("one gap line: {text:?}"));
        assert!(resumed_at > 2, "{text:?}");
    }

    /// The journal keeps two events of a session: the run's ending and the
    /// reader's own attach. The gap resumes at `to`, so the event at `to`
    /// follows it, and the body sends both.
    #[tokio::test]
    async fn a_gap_that_resumes_at_to_is_followed_by_the_event_at_to() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(2, 8));
        let (state, run) = one_ended_run(&stores).await;
        let body = events_body(
            reader_of(&state, "s1").await,
            named_frame,
            Some(run),
            Some(seq(5)),
            std::future::pending(),
            None,
        );
        assert_eq!(text_of(body).await, "gap 5\nevent 5\n");
    }

    /// The journal keeps three events of a session, so a reader of the
    /// first run never sees its ending: a gap, then events of the second
    /// run. The body ends at the second run's first event without sending
    /// it.
    #[tokio::test]
    async fn the_body_of_a_run_ends_at_an_event_of_another_run() {
        let stores = Stores::with_journals(InMemoryJournalStore::with_bounds(3, 8));
        let (state, first) = one_ended_run(&stores).await;
        let second = start(&state, on_session("s1")).await;
        ended(&state, second).await;
        let body = events_body(
            reader_of(&state, "s1").await,
            named_frame,
            Some(first),
            None,
            std::future::pending(),
            None,
        );

        let text = text_of(body).await;
        let resumed_at: u64 = text
            .strip_prefix("gap ")
            .and_then(|rest| rest.strip_suffix('\n'))
            .and_then(|number| number.parse().ok())
            .unwrap_or_else(|| panic!("one gap line and nothing else: {text:?}"));
        assert!(resumed_at > 5, "{text:?}");
    }

    /// With the run's last number given, an event of another run at or
    /// before it means the journal's numbers belong to another run: the
    /// store forgot the session and a later run numbered it again. The body
    /// sends one gap past the run's last number, and ends.
    #[tokio::test]
    async fn an_event_of_another_run_before_to_is_one_gap_past_to() {
        let stores = Stores::new();
        let (state, _) = one_ended_run(&stores).await;
        let body = events_body(
            reader_of(&state, "s1").await,
            named_frame,
            Some(RunId::mint()),
            Some(seq(5)),
            std::future::pending(),
            None,
        );

        assert_eq!(text_of(body).await, "gap 6\n");
    }

    /// A drain lets go of every session and its journal, which ends the
    /// subscription.
    #[tokio::test]
    async fn the_body_ends_when_its_subscription_ends() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let body = events_body(
            reader_of(&state, "s_idle").await,
            named_frame,
            None,
            None,
            std::future::pending(),
            None,
        );

        state.runtime.drain(Duration::from_millis(10)).await;
        text_of(body).await;
    }

    /// The run's five events are in the store and the reader's own attach
    /// is on its way, so all six are ready when the body is first read, and
    /// all six go out before the body ends.
    #[tokio::test]
    async fn shutdown_ends_the_body_after_the_items_ready_at_the_same_time() {
        let (state, _) = one_ended_run(&Stores::new()).await;
        let body = events_body(
            reader_of(&state, "s1").await,
            named_frame,
            None,
            None,
            std::future::ready(()),
            None,
        );

        let expected: String = (1..=6).map(|n| format!("event {n}\n")).collect();
        assert_eq!(text_of(body).await, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_goes_out_every_period() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let mut body = events_body(
            reader_of(&state, "s_idle").await,
            named_frame,
            None,
            None,
            std::future::pending(),
            Some(Duration::from_secs(5)),
        );

        let mut heartbeats = Vec::new();
        while heartbeats.len() < 3 {
            let chunk = tokio::time::timeout(PATIENCE, body.next())
                .await
                .expect("the body sends something in time")
                .expect("the body is still open")
                .expect("a body chunk");
            if chunk == HEARTBEAT.as_bytes() {
                heartbeats.push(tokio::time::Instant::now());
            }
        }
        assert_eq!(heartbeats[1] - heartbeats[0], Duration::from_secs(5));
        assert_eq!(heartbeats[2] - heartbeats[1], Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn with_no_period_no_heartbeat_goes_out() {
        let stores = Stores::new();
        let state = answering_state(&stores);
        let mut body = events_body(
            reader_of(&state, "s_idle").await,
            named_frame,
            None,
            None,
            std::future::pending(),
            None,
        );

        let mut sent = Vec::new();
        while let Ok(chunk) = tokio::time::timeout(Duration::from_secs(60), body.next()).await {
            sent.push(
                chunk
                    .expect("the body is still open")
                    .expect("a body chunk"),
            );
        }
        assert!(
            !sent.iter().any(|chunk| chunk == HEARTBEAT.as_bytes()),
            "{sent:?}"
        );
    }
}
