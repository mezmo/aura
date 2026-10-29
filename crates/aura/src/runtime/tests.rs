//! Runtime tests, with two kinds of agent. A real prepared agent over a
//! scripted model shows what a run journals. A fake agent that follows a list
//! of steps drives a run into each way it can end.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use aura_events::agent::{AgentEvent, AgentEventPayload};
use aura_events::run::{LifecycleEvent, SessionEvent, SessionEventPayload};
use aura_events::{
    AgentScopeWire, ApprovalCompleted, ApprovalOriginWire, ApprovalOutcomeWire, ApprovalRequested,
    SequenceNumber, TokenCount, TokenUsage,
};
use rig::completion::Message;
use tokio::sync::Notify;

use aura_events::run::RunCancelReason;

use super::prepare::{PrepareRequest, PreparedForRun};
use super::record::RunRecord;
use super::task::STOP_GRACE;
use super::*;
use crate::builder::test_support::prepared_agent;
use crate::error::BuilderError;
use crate::forwarded_headers::ForwardedHeaders;
use crate::hitl::DecisionId;
use crate::orchestration::{ScriptedCompletionModel, ScriptedTurn};
use crate::passthrough_tool::PASSTHROUGH_MARKER;
use crate::provider_agent::{
    ProviderAgent, StreamError, StreamItem, StreamedUserContent, ToolResult,
};
use crate::run_context::RunContext;
use crate::session_store::JournalEntry;
use crate::streaming::{AgentRun, RunOptions, StreamingAgent};
use crate::streaming_request_hook::UsageState;

// ---------------------------------------------------------------------------
// Test agents
// ---------------------------------------------------------------------------

/// One step of a [`FakeAgent`]'s run.
#[derive(Clone)]
enum Step {
    /// Emits an event on the run's channel.
    Emit(Box<AgentEventPayload>),
    /// Waits for the gate, or for the run to be cancelled.
    Wait(Arc<Notify>),
    /// Waits forever, whatever happens to the run.
    Stall,
    /// Makes an MCP call, which returns only once the run's MCP calls are
    /// cancelled.
    McpCall,
    /// Calls a tool the client executes.
    ClientToolCall,
    /// Panics.
    Panic,
}

/// A streaming agent that follows a script of steps.
struct FakeAgent {
    run_id: RunId,
    steps: Vec<Step>,
    /// Every `cancel_and_close_mcp` call: the request id and the reason.
    closed: Arc<Mutex<Vec<(String, String)>>>,
    /// Woken by `cancel_and_close_mcp`.
    mcp_cancelled: Arc<Notify>,
}

#[async_trait]
impl StreamingAgent for FakeAgent {
    fn get_provider_info(&self) -> (&str, &str) {
        ("fake", "model")
    }

    fn run_id(&self) -> RunId {
        self.run_id
    }

    async fn stream(
        &self,
        _query: &str,
        _chat_history: Vec<Message>,
        options: RunOptions,
        _request_id: &str,
    ) -> AgentRun {
        let (_, cancel) = options.into_parts();
        let cancel = cancel.unwrap_or_default();
        let (run, events) = RunContext::channel_on(self.run_id, cancel.clone());
        let steps = self.steps.clone();
        let token = cancel.clone();
        let mcp_cancelled = Arc::clone(&self.mcp_cancelled);
        let items = async_stream::stream! {
            for step in steps {
                match step {
                    Step::Emit(payload) => {
                        run.emit(AgentEvent::single_agent(*payload)).await;
                    }
                    Step::Wait(gate) => {
                        // Rig ends a cancelled run's stream with an error item,
                        // so a run cancelled before the gate opens ends that way.
                        tokio::select! {
                            () = gate.notified() => {}
                            () = token.cancelled() => {
                                let error: StreamError = "fake run cancelled".into();
                                yield Err(error);
                                return;
                            }
                        }
                    }
                    Step::Stall => std::future::pending::<()>().await,
                    // A tool call waits on its server whatever happens to the
                    // run's token, and returns, with an error, once the run's
                    // MCP calls are cancelled.
                    Step::McpCall => {
                        mcp_cancelled.notified().await;
                        let error: StreamError = "MCP call cancelled".into();
                        yield Err(error);
                    }
                    // The passthrough tool returns its marker, and the agent's
                    // hook then ends the stream with an error so the client
                    // can run the tool.
                    Step::ClientToolCall => {
                        yield Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(
                            ToolResult {
                                id: "call-1".to_owned(),
                                call_id: None,
                                result: PASSTHROUGH_MARKER.to_owned(),
                            },
                        )));
                        let error: StreamError = "prompt cancelled".into();
                        yield Err(error);
                        return;
                    }
                    Step::Panic => panic!("the fake agent panicked"),
                }
            }
        };
        let items: futures::stream::BoxStream<'static, Result<StreamItem, StreamError>> =
            Box::pin(items);
        AgentRun::new(items, cancel, UsageState::new()).observed_by(events)
    }

    async fn cancel_and_close_mcp(&self, request_id: &str, reason: &str) -> usize {
        self.closed
            .lock()
            .unwrap()
            .push((request_id.to_owned(), reason.to_owned()));
        self.mcp_cancelled.notify_one();
        0
    }
}

/// What one `prepare` call hands back.
enum Prepared {
    /// A prepared agent over a model playing these turns.
    Scripted(Vec<ScriptedTurn>),
    /// A fake agent following these steps.
    Fake(Vec<Step>),
    /// A failure to prepare.
    Fail(String),
}

/// A preparer with one config per agent name, handing back the agents a test
/// queued.
struct TestPreparer {
    configs: Vec<Config>,
    queued: Mutex<VecDeque<Prepared>>,
    /// A gate `prepare` waits on.
    gate: Option<Arc<Notify>>,
    /// The agent ids prepared, in order.
    prepared: Mutex<Vec<String>>,
    /// What every fake agent's `cancel_and_close_mcp` was called with.
    closed: Arc<Mutex<Vec<(String, String)>>>,
}

impl TestPreparer {
    fn new(agents: &[&str], queued: Vec<Prepared>) -> Self {
        Self {
            configs: agents.iter().map(|name| config(name)).collect(),
            queued: Mutex::new(queued.into()),
            gate: None,
            prepared: Mutex::new(Vec::new()),
            closed: Arc::default(),
        }
    }

    fn gated(mut self, gate: Arc<Notify>) -> Self {
        self.gate = Some(gate);
        self
    }
}

fn config(name: &str) -> Config {
    Config {
        agent: aura_config::AgentConfig {
            name: name.to_owned(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[async_trait]
impl AgentPreparer for TestPreparer {
    fn config(&self, agent: Option<&str>) -> Result<Config, StartError> {
        let name = agent.unwrap_or("a");
        self.configs
            .iter()
            .find(|config| config.agent_id() == name)
            .cloned()
            .ok_or_else(|| StartError::NoSuchAgent(name.to_owned()))
    }

    /// Waits for the gate, if there is one, then hands back the next queued
    /// agent.
    async fn prepare(
        &self,
        config: &Config,
        request: PrepareRequest<'_>,
    ) -> Result<PreparedForRun, BuilderError> {
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        self.prepared
            .lock()
            .unwrap()
            .push(config.agent_id().to_owned());
        let queued = self
            .queued
            .lock()
            .unwrap()
            .pop_front()
            .expect("the test queued an agent for this run");
        match queued {
            Prepared::Scripted(turns) => {
                let model = ScriptedCompletionModel::new(turns);
                let mut prepared = prepared_agent(
                    ProviderAgent::Scripted(rig::agent::AgentBuilder::new(model).build()),
                    ForwardedHeaders::default(),
                    None,
                );
                prepared.turn_nudge = None;
                Ok(PreparedForRun::Prepared(Arc::new(prepared)))
            }
            Prepared::Fake(steps) => Ok(PreparedForRun::Built(Arc::new(FakeAgent {
                run_id: request.run_id,
                steps,
                closed: Arc::clone(&self.closed),
                mcp_cancelled: Arc::default(),
            }))),
            Prepared::Fail(error) => Err(BuilderError::AgentError(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    runtime: AgentRuntime,
    preparer: Arc<TestPreparer>,
    runs: Arc<InMemoryRunStore>,
    journals: Arc<InMemoryJournalStore>,
}

fn harness(preparer: TestPreparer) -> Harness {
    harness_retaining(preparer, DEFAULT_RETENTION)
}

fn harness_retaining(preparer: TestPreparer, retention: Duration) -> Harness {
    let runs = Arc::new(InMemoryRunStore::new());
    harness_over(
        preparer,
        retention,
        Arc::clone(&runs) as Arc<dyn RunStore>,
        runs,
    )
}

/// A harness whose runtime writes runs to `store`. `runs` is the in-memory
/// store behind `store`, for the test to read.
fn harness_over(
    preparer: TestPreparer,
    retention: Duration,
    store: Arc<dyn RunStore>,
    runs: Arc<InMemoryRunStore>,
) -> Harness {
    let preparer = Arc::new(preparer);
    let journals = Arc::new(InMemoryJournalStore::new());
    let runtime = AgentRuntime::with_preparer(
        Arc::clone(&preparer) as Arc<dyn AgentPreparer>,
        RuntimeStores {
            runs: store,
            journals: Arc::clone(&journals) as Arc<dyn JournalStore>,
            skills: Arc::new(InMemorySkillInvocationStore::new()),
        },
        PendingApprovals::new(),
        retention,
    );
    Harness {
        runtime,
        preparer,
        runs,
        journals,
    }
}

/// A run store with a gate on `finish`.
struct GatedRunStore {
    inner: Arc<InMemoryRunStore>,
    gate: Arc<Notify>,
    /// How many `finish` calls have begun.
    finishing: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl RunStore for GatedRunStore {
    async fn put(&self, facts: &RunFacts) -> Result<(), SessionStoreError> {
        self.inner.put(facts).await
    }

    async fn get(&self, run: &RunId) -> Result<Option<RunFacts>, SessionStoreError> {
        self.inner.get(run).await
    }

    async fn list(&self, filter: &RunFilter) -> Result<Vec<RunFacts>, SessionStoreError> {
        self.inner.list(filter).await
    }

    /// Waits for the gate before finishing, so a test can hold a run in its
    /// ending.
    async fn finish(
        &self,
        terminal: &SessionEvent,
        facts: &RunFacts,
    ) -> Result<(), SessionStoreError> {
        self.finishing.fetch_add(1, Ordering::SeqCst);
        self.gate.notified().await;
        self.inner.finish(terminal, facts).await
    }
}

/// A faulty run store.
struct PanickingRunStore {
    inner: Arc<InMemoryRunStore>,
}

#[async_trait]
impl RunStore for PanickingRunStore {
    async fn put(&self, facts: &RunFacts) -> Result<(), SessionStoreError> {
        self.inner.put(facts).await
    }

    async fn get(&self, run: &RunId) -> Result<Option<RunFacts>, SessionStoreError> {
        self.inner.get(run).await
    }

    async fn list(&self, filter: &RunFilter) -> Result<Vec<RunFacts>, SessionStoreError> {
        self.inner.list(filter).await
    }

    /// Panics instead of finishing the run.
    async fn finish(&self, _: &SessionEvent, _: &RunFacts) -> Result<(), SessionStoreError> {
        panic!("the run store panicked while finishing")
    }
}

/// A journal store whose `latest` call for one session waits on a gate.
struct StallingJournalStore {
    inner: Arc<InMemoryJournalStore>,
    session: SessionId,
    gate: Arc<Notify>,
    /// How many `latest` calls for `session` have begun.
    stalled: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl JournalStore for StallingJournalStore {
    async fn append(
        &self,
        session: &SessionId,
        expected: Option<SequenceNumber>,
        run_id: Option<RunId>,
        at: aura_events::Timestamp,
        payload: SessionEventPayload,
    ) -> Result<Arc<SessionEvent>, SessionStoreError> {
        self.inner
            .append(session, expected, run_id, at, payload)
            .await
    }

    async fn read(
        &self,
        session: &SessionId,
        from: SequenceNumber,
        to: SequenceNumber,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        self.inner.read(session, from, to, limit).await
    }

    /// Waits for the gate before reading the stalled session's newest number.
    async fn latest(
        &self,
        session: &SessionId,
    ) -> Result<Option<SequenceNumber>, SessionStoreError> {
        if *session == self.session {
            self.stalled.fetch_add(1, Ordering::SeqCst);
            self.gate.notified().await;
        }
        self.inner.latest(session).await
    }
}

fn on_session(session: &str, prompt: &str) -> StartRun {
    StartRun {
        session: Some(SessionId::new(session).unwrap()),
        ..StartRun::new(prompt)
    }
}

const PATIENCE: Duration = Duration::from_secs(10);

impl Harness {
    /// Waits for `run` to end and returns its final facts.
    async fn ended(&self, run: RunId) -> RunFacts {
        let record = self
            .runtime
            .shared
            .run(&run)
            .expect("the runtime holds the run");
        let mut watch = record.watch();
        tokio::time::timeout(PATIENCE, watch.wait_for(|facts| facts.status.is_terminal()))
            .await
            .expect("the run ends in time")
            .expect("the run's record is alive")
            .clone()
    }

    /// Everything the store holds for `session`.
    async fn journal(&self, session: &SessionId) -> Vec<Arc<SessionEvent>> {
        self.journals
            .read(
                session,
                SequenceNumber::FIRST,
                SequenceNumber::try_from(u64::MAX).unwrap(),
                usize::MAX,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|entry| match entry {
                JournalEntry::Event(event) => event,
                JournalEntry::Undecodable { seq, .. } => panic!("undecodable entry at {seq}"),
            })
            .collect()
    }
}

/// Waits until `check` holds.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(PATIENCE, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// A short name for what an event is, for asserting on a journal's shape.
fn kind(event: &SessionEvent) -> &'static str {
    match &event.payload {
        SessionEventPayload::Lifecycle(lifecycle) => match lifecycle {
            LifecycleEvent::Started { .. } => "started",
            LifecycleEvent::Finished { .. } => "finished",
            LifecycleEvent::Failed { .. } => "failed",
            LifecycleEvent::Cancelled { .. } => "cancelled",
            LifecycleEvent::Parked { .. } => "parked",
            _ => "lifecycle",
        },
        SessionEventPayload::Agent(agent) => match &agent.payload {
            AgentEventPayload::SessionInfo { .. } => "session_info",
            AgentEventPayload::McpStatus { .. } => "mcp_status",
            AgentEventPayload::TextDelta { .. } => "text_delta",
            AgentEventPayload::Completed { .. } => "completed",
            AgentEventPayload::ApprovalRequested(_) => "approval_requested",
            AgentEventPayload::ApprovalCompleted(_) => "approval_completed",
            AgentEventPayload::RunParked { .. } => "run_parked",
            _ => "agent",
        },
        _ => "other",
    }
}

fn kinds(events: &[Arc<SessionEvent>]) -> Vec<&'static str> {
    events.iter().map(|event| kind(event)).collect()
}

fn requested(id: DecisionId) -> AgentEventPayload {
    AgentEventPayload::ApprovalRequested(ApprovalRequested {
        decision_id: id.to_string(),
        tool_name: "deploy".to_owned(),
        tool_namespace: None,
        origin: ApprovalOriginWire::ConfigGate {
            matched_pattern: "deploy".to_owned(),
            agent_name: "a".to_owned(),
        },
        scope: AgentScopeWire::Single { session_id: None },
    })
}

fn completed(id: DecisionId) -> AgentEventPayload {
    AgentEventPayload::ApprovalCompleted(ApprovalCompleted {
        decision_id: id.to_string(),
        outcome: ApprovalOutcomeWire::Approved,
        duration_ms: 1,
        scope: AgentScopeWire::Single { session_id: None },
    })
}

// ---------------------------------------------------------------------------
// Starting a run
// ---------------------------------------------------------------------------

/// `start` returns once the run is recorded; preparing the agent happens on
/// the run's task, and nothing reaches the journal until the run streams.
#[tokio::test]
async fn start_returns_before_the_agent_is_prepared() {
    let gate = Arc::new(Notify::new());
    let h = harness(
        TestPreparer::new(
            &["a"],
            vec![Prepared::Scripted(vec![ScriptedTurn::text("hi")])],
        )
        .gated(Arc::clone(&gate)),
    );

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let session = SessionId::new("s1").unwrap();

    let facts = h.runtime.facts(&run).unwrap();
    assert_eq!(facts.status, RunStatus::Preparing);
    assert_eq!(facts.first_seq, None);
    assert_eq!(
        h.runtime.session_state(&session),
        Some(AgentState::Preparing)
    );
    assert!(h.journal(&session).await.is_empty());
    tokio::time::timeout(PATIENCE, async {
        loop {
            let stored = h.runs.get(&run).await.unwrap().map(|facts| facts.status);
            if stored == Some(RunStatus::Preparing) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the store has the run before the agent is prepared");

    gate.notify_one();
    h.ended(run).await;
    assert_eq!(kind(&h.journal(&session).await[0]), "started");
}

/// A run of a prepared agent journals `Started`, the agent's session info,
/// the agent's own events, and `Finished`, with no gaps in the sequence
/// numbers. Its facts hold the first and last sequence numbers and the run's
/// usage.
#[tokio::test]
async fn a_run_journals_its_lifecycle_around_the_agents_events() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Scripted(vec![ScriptedTurn::text("hello")])],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;
    let events = h.journal(&SessionId::new("s1").unwrap()).await;

    let kinds = kinds(&events);
    assert_eq!(kinds.first(), Some(&"started"), "{kinds:?}");
    assert_eq!(kinds.get(1), Some(&"session_info"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"finished"), "{kinds:?}");
    assert!(kinds.contains(&"text_delta"), "{kinds:?}");
    assert!(kinds.contains(&"completed"), "{kinds:?}");
    for (n, event) in events.iter().enumerate() {
        assert_eq!(event.seq.get(), n as u64 + 1, "numbers are dense");
        assert_eq!(event.run_id, Some(run));
    }
    match &events[0].payload {
        SessionEventPayload::Lifecycle(LifecycleEvent::Started { agent, prompt, .. }) => {
            assert_eq!(agent, "a");
            assert_eq!(prompt, "q");
        }
        other => panic!("not Started: {other:?}"),
    }
    match &events[1].payload {
        SessionEventPayload::Agent(event) => assert!(matches!(
            &event.payload,
            AgentEventPayload::SessionInfo { model, .. } if model.ends_with("/scripted")
        )),
        other => panic!("not SessionInfo: {other:?}"),
    }

    let usage = TokenUsage {
        prompt_tokens: TokenCount::new(10),
        completion_tokens: TokenCount::new(5),
        total_tokens: TokenCount::new(15),
    };
    match &events.last().unwrap().payload {
        SessionEventPayload::Lifecycle(LifecycleEvent::Finished { usage: finished }) => {
            assert_eq!(*finished, usage);
        }
        other => panic!("not Finished: {other:?}"),
    }
    assert_eq!(facts.status, RunStatus::Finished);
    assert_eq!(facts.usage, usage);
    assert_eq!(facts.first_seq, Some(events[0].seq));
    assert_eq!(facts.latest_seq, Some(events.last().unwrap().seq));
    assert!(facts.ended_at.is_some());
    assert_eq!(h.runs.get(&run).await.unwrap(), Some(facts));
    assert_eq!(
        h.runtime.session_state(&SessionId::new("s1").unwrap()),
        Some(AgentState::Done)
    );
}

/// A run whose agent cannot be prepared never starts: the journal holds only
/// its failure, and the session is free for the next run.
#[tokio::test]
async fn a_run_whose_agent_cannot_be_prepared_fails_without_starting() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fail("no provider".to_owned()),
            Prepared::Scripted(vec![ScriptedTurn::text("hi")]),
        ],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Failed);
    assert_eq!(facts.first_seq, None);
    let events = h.journal(&SessionId::new("s1").unwrap()).await;
    assert_eq!(kinds(&events), vec!["failed"]);
    match &events[0].payload {
        SessionEventPayload::Lifecycle(LifecycleEvent::Failed { error, .. }) => {
            assert!(error.contains("no provider"), "{error}");
        }
        other => panic!("not Failed: {other:?}"),
    }
    assert_eq!(
        h.runs.get(&run).await.unwrap().map(|facts| facts.status),
        Some(RunStatus::Failed)
    );

    let next = h.runtime.start(on_session("s1", "again")).await.unwrap();
    assert_eq!(h.ended(next).await.status, RunStatus::Finished);
}

/// A provider failure mid-run comes back as an error item, and fails the run.
#[tokio::test]
async fn a_stream_error_fails_the_run() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Scripted(Vec::new())],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Failed);
    let events = h.journal(&SessionId::new("s1").unwrap()).await;
    assert_eq!(kinds(&events).first(), Some(&"started"));
    match &events.last().unwrap().payload {
        SessionEventPayload::Lifecycle(LifecycleEvent::Failed { error, .. }) => {
            assert!(error.contains("script exhausted"), "{error}");
        }
        other => panic!("not Failed: {other:?}"),
    }
}

/// A session runs one run at a time, whichever agent a second start names;
/// once the run ends, the session can run another agent.
#[tokio::test]
async fn a_session_runs_one_run_at_a_time() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a", "b"],
        vec![
            Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))]),
            Prepared::Scripted(vec![ScriptedTurn::text("from b")]),
        ],
    ));

    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let again = h.runtime.start(on_session("s1", "q")).await;
    assert!(
        matches!(again, Err(StartError::RunInProgress(live)) if live == first),
        "{again:?}"
    );
    let other_agent = h
        .runtime
        .start(StartRun {
            agent: Some("b".to_owned()),
            ..on_session("s1", "q")
        })
        .await;
    assert!(
        matches!(other_agent, Err(StartError::RunInProgress(live)) if live == first),
        "{other_agent:?}"
    );
    gate.notify_one();
    h.ended(first).await;
    let second = h
        .runtime
        .start(StartRun {
            agent: Some("b".to_owned()),
            ..on_session("s1", "q")
        })
        .await
        .unwrap();
    assert_eq!(h.ended(second).await.agent, "b");
    assert_eq!(*h.preparer.prepared.lock().unwrap(), vec!["a", "b"]);
}

/// A session's runs share its sequence: the next run's `Started` follows the
/// last run's ending.
#[tokio::test]
async fn a_sessions_runs_continue_one_sequence() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Scripted(vec![ScriptedTurn::text("one")]),
            Prepared::Scripted(vec![ScriptedTurn::text("two")]),
        ],
    ));

    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let first = h.ended(first).await;
    let second = h
        .runtime
        .start(StartRun {
            continues: Some(first.run_id),
            ..on_session("s1", "q")
        })
        .await
        .unwrap();
    let second = h.ended(second).await;

    assert_eq!(
        second.first_seq,
        first.latest_seq.map(SequenceNumber::next),
        "the second run's Started follows the first run's ending"
    );
    assert_eq!(second.continues, Some(first.run_id));
    let events = h.journal(&SessionId::new("s1").unwrap()).await;
    let started: Vec<_> = events
        .iter()
        .filter(|event| kind(event) == "started")
        .map(|event| event.run_id)
        .collect();
    assert_eq!(started, vec![Some(first.run_id), Some(second.run_id)]);
}

/// A run that parks ends `Parked`, pointing at the checkpoint its approvals
/// are kept under and naming the approvals it waits on; the session is then
/// parked, and a new start on it is accepted.
#[tokio::test]
async fn a_run_that_parks_ends_parked() {
    let decision = DecisionId::generate();
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fake(vec![Step::Emit(Box::new(AgentEventPayload::RunParked {
                run_id: "ignored".to_owned(),
                decision_ids: vec![decision.to_string()],
                expires_at: "2026-10-21T00:00:00Z".to_owned(),
                iteration: 1,
            }))]),
            Prepared::Fake(Vec::new()),
        ],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Parked);
    let checkpoint = format!("run:{run}");
    assert_eq!(
        facts.checkpoint.as_ref().map(|c| c.as_str()),
        Some(checkpoint.as_str())
    );
    assert_eq!(facts.pending_approvals, vec![decision]);
    match &h
        .journal(&SessionId::new("s1").unwrap())
        .await
        .last()
        .unwrap()
        .payload
    {
        SessionEventPayload::Lifecycle(LifecycleEvent::Parked { checkpoint: at, .. }) => {
            assert_eq!(at.as_str(), checkpoint);
        }
        other => panic!("not Parked: {other:?}"),
    }
    assert_eq!(
        h.runtime.session_state(&SessionId::new("s1").unwrap()),
        Some(AgentState::Parked)
    );

    let next = h.runtime.start(on_session("s1", "resume")).await.unwrap();
    assert_eq!(h.ended(next).await.status, RunStatus::Finished);
}

/// A run waiting on an approval blocks its session and lists as waiting,
/// until the approval completes.
#[tokio::test]
async fn an_open_approval_blocks_the_session_until_it_completes() {
    let decision = DecisionId::generate();
    let raised = Arc::new(Notify::new());
    let decided = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![
            Step::Emit(Box::new(requested(decision))),
            Step::Wait(Arc::clone(&raised)),
            Step::Emit(Box::new(completed(decision))),
            Step::Wait(Arc::clone(&decided)),
        ])],
    ));
    let session = SessionId::new("s1").unwrap();
    let blocked = AgentState::Blocked {
        on: BlockReason::Approval {
            decisions: vec![decision],
        },
    };

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the session is blocked", || {
        h.runtime.session_state(&session) == Some(blocked.clone())
    })
    .await;
    assert_eq!(
        h.runtime.facts(&run).unwrap().pending_approvals,
        vec![decision]
    );
    let waiting = RunFilter {
        pending_approval: Some(true),
        ..RunFilter::default()
    };
    let listed = h.runtime.list(&waiting).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].run_id, run);

    raised.notify_one();
    eventually("the session is running again", || {
        h.runtime.session_state(&session) == Some(AgentState::Running)
    })
    .await;
    assert!(h.runtime.facts(&run).unwrap().pending_approvals.is_empty());
    assert!(h.runtime.list(&waiting).await.unwrap().is_empty());

    decided.notify_one();
    assert_eq!(h.ended(run).await.status, RunStatus::Finished);
}

#[tokio::test]
async fn a_liveness_policy_the_agent_cannot_honour_is_refused() {
    use aura_events::run::LivenessPolicy;

    let h = harness(TestPreparer::new(&["a"], Vec::new()));
    let park = StartRun {
        liveness: Liveness {
            policy: LivenessPolicy::Park,
            grace: Duration::ZERO,
        },
        ..on_session("s1", "q")
    };

    let refused = h.runtime.start(park).await;

    assert!(
        matches!(
            &refused,
            Err(StartError::PolicyUnsupported { policy: LivenessPolicy::Park, agent }) if agent == "a"
        ),
        "{refused:?}"
    );
    assert_eq!(
        h.runtime.session_state(&SessionId::new("s1").unwrap()),
        None
    );
}

#[test]
fn only_an_orchestrated_agent_with_park_mode_can_park() {
    use aura_events::run::LivenessPolicy;

    let parking = |policy| Liveness {
        policy,
        grace: Duration::ZERO,
    };
    let mut orchestrated = config("a");
    orchestrated.orchestration = Some(Default::default());
    orchestrated.orchestration.as_mut().unwrap().enabled = true;
    let mut parks = orchestrated.clone();
    parks.hitl = Some(aura_config::HitlConfig {
        require_approval: Vec::new(),
        route: aura_config::DecisionRouteConfig::Conversational { timeout_secs: 60 },
        park: aura_config::ParkConfig { enabled: true },
    });

    assert!(honours(&config("a"), parking(LivenessPolicy::Cancel)));
    assert!(honours(&config("a"), parking(LivenessPolicy::Continue)));
    assert!(!honours(&config("a"), parking(LivenessPolicy::Park)));
    assert!(!honours(&orchestrated, parking(LivenessPolicy::Park)));
    assert!(honours(&parks, parking(LivenessPolicy::Park)));
    assert!(!honours(&parks, parking(LivenessPolicy::Unknown)));
}

#[tokio::test]
async fn an_unknown_agent_is_refused() {
    let h = harness(TestPreparer::new(&["a"], Vec::new()));

    let refused = h
        .runtime
        .start(StartRun {
            agent: Some("nope".to_owned()),
            ..on_session("s1", "q")
        })
        .await;

    assert!(
        matches!(&refused, Err(StartError::NoSuchAgent(name)) if name == "nope"),
        "{refused:?}"
    );
}

/// The production preparer resolves agents as a request's `model` does.
#[test]
fn the_preparer_resolves_agents_as_a_request_does() {
    let preparer = |names: &[&str], default: Option<&str>| {
        RigBuilderPreparer::new(
            Arc::new(names.iter().map(|name| config(name)).collect()),
            PendingApprovals::new(),
        )
        .with_default_agent(default.map(str::to_owned))
    };
    let resolved = |preparer: &RigBuilderPreparer, agent: Option<&str>| {
        preparer
            .config(agent)
            .map(|config| config.agent_id().to_owned())
    };

    let one = preparer(&["a"], None);
    assert_eq!(resolved(&one, Some("anything")).unwrap(), "a");
    assert_eq!(resolved(&one, None).unwrap(), "a");

    let several = preparer(&["a", "b"], Some("b"));
    assert_eq!(resolved(&several, Some("a")).unwrap(), "a");
    assert_eq!(resolved(&several, None).unwrap(), "b");
    assert!(matches!(
        resolved(&several, Some("c")),
        Err(StartError::NoSuchAgent(name)) if name == "c"
    ));

    let no_default = preparer(&["a", "b"], None);
    assert!(matches!(
        resolved(&no_default, None),
        Err(StartError::AgentRequired)
    ));
}

/// Listing reads the run store, so it filters by session and status there.
#[tokio::test]
async fn list_reads_runs_through_the_store() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Scripted(vec![ScriptedTurn::text("one")]),
            Prepared::Fail("broken".to_owned()),
            Prepared::Scripted(vec![ScriptedTurn::text("three")]),
        ],
    ));
    let mut runs = Vec::new();
    for session in ["s1", "s2", "s1"] {
        let run = h.runtime.start(on_session(session, "q")).await.unwrap();
        h.ended(run).await;
        runs.push(run);
    }
    let listed = |filter: RunFilter| {
        let runtime = &h.runtime;
        async move {
            runtime
                .list(&filter)
                .await
                .unwrap()
                .into_iter()
                .map(|facts| facts.run_id)
                .collect::<Vec<_>>()
        }
    };

    assert_eq!(listed(RunFilter::default()).await, runs);
    assert_eq!(
        listed(RunFilter {
            session: Some(SessionId::new("s1").unwrap()),
            ..RunFilter::default()
        })
        .await,
        vec![runs[0], runs[2]]
    );
    assert_eq!(
        listed(RunFilter {
            status: Some(RunStatus::Failed),
            ..RunFilter::default()
        })
        .await,
        vec![runs[1]]
    );
}

// ---------------------------------------------------------------------------
// Cancelling, timing out, and draining
// ---------------------------------------------------------------------------

fn last_lifecycle(events: &[Arc<SessionEvent>]) -> &LifecycleEvent {
    match &events.last().expect("the journal has events").payload {
        SessionEventPayload::Lifecycle(lifecycle) => lifecycle,
        other => panic!("not a lifecycle event: {other:?}"),
    }
}

/// A cancelled run ends `Cancelled` with the reason and the caller's words,
/// its token is cancelled, and its MCP calls are cancelled on their servers.
/// Cancelling a run that has ended does nothing.
#[tokio::test]
async fn a_cancelled_run_ends_with_the_reason_it_was_given() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&run).unwrap().status == RunStatus::Running
    })
    .await;

    assert!(h.runtime.cancel(
        &run,
        RunCancelReason::External,
        Some("the user pressed stop".to_owned())
    ));
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Cancelled);
    match last_lifecycle(&h.journal(&SessionId::new("s1").unwrap()).await) {
        LifecycleEvent::Cancelled {
            reason, message, ..
        } => {
            assert_eq!(*reason, RunCancelReason::External);
            assert_eq!(message.as_deref(), Some("the user pressed stop"));
        }
        other => panic!("not Cancelled: {other:?}"),
    }
    assert!(h.runtime.shared.run(&run).unwrap().cancel.is_cancelled());
    assert_eq!(
        *h.preparer.closed.lock().unwrap(),
        vec![(run.to_string(), "run cancelled".to_owned())]
    );
    assert!(
        !h.runtime.cancel(&run, RunCancelReason::External, None),
        "an ended run cannot be cancelled"
    );
    assert!(
        !h.runtime
            .cancel(&RunId::mint(), RunCancelReason::External, None)
    );
}

/// A run stopped while a tool call waits on its MCP server ends: stopping it
/// cancels its MCP calls, which is what returns the call.
#[tokio::test]
async fn a_run_stopped_mid_tool_call_ends_once_its_mcp_calls_are_cancelled() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::McpCall])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&run).unwrap().status == RunStatus::Running
    })
    .await;

    assert!(h.runtime.cancel(&run, RunCancelReason::External, None));

    assert_eq!(h.ended(run).await.status, RunStatus::Cancelled);
    assert_eq!(h.preparer.closed.lock().unwrap().len(), 1);
}

/// A run cancelled while its agent is being prepared ends without starting:
/// the journal holds only its cancellation.
#[tokio::test]
async fn a_run_cancelled_while_preparing_ends_without_starting() {
    let h = harness(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]).gated(Arc::new(Notify::new())),
    );
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();

    assert!(h.runtime.cancel(&run, RunCancelReason::External, None));
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Cancelled);
    assert_eq!(facts.first_seq, None);
    assert_eq!(
        kinds(&h.journal(&SessionId::new("s1").unwrap()).await),
        vec!["cancelled"]
    );
    assert_eq!(
        h.runtime.session_state(&SessionId::new("s1").unwrap()),
        Some(AgentState::Done)
    );
}

/// A run still going when its timeout runs out is cancelled for the deadline.
#[tokio::test(start_paused = true)]
async fn a_run_past_its_timeout_is_cancelled_for_the_deadline() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))])],
    ));
    let after = Duration::from_secs(5);
    let run = h
        .runtime
        .start(StartRun {
            timeout: Some(after),
            ..on_session("s1", "q")
        })
        .await
        .unwrap();

    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Cancelled);
    let events = h.journal(&SessionId::new("s1").unwrap()).await;
    match last_lifecycle(&events) {
        LifecycleEvent::Cancelled { reason, .. } => {
            assert_eq!(*reason, RunCancelReason::Deadline { after });
        }
        other => panic!("not Cancelled: {other:?}"),
    }
    match &events[0].payload {
        SessionEventPayload::Lifecycle(LifecycleEvent::Started { timeout, .. }) => {
            assert_eq!(*timeout, Some(after));
        }
        other => panic!("not Started: {other:?}"),
    }
    assert_eq!(
        *h.preparer.closed.lock().unwrap(),
        vec![(run.to_string(), "run timed out".to_owned())]
    );
}

/// Draining stops every live run for shutdown, refuses later starts, and
/// lets go of every run and session it held.
#[tokio::test]
async fn drain_cancels_live_runs_for_shutdown_and_refuses_new_starts() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))]),
            Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))]),
        ],
    ));
    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let second = h.runtime.start(on_session("s2", "q")).await.unwrap();

    h.runtime.drain(PATIENCE).await;

    for (run, session) in [(first, "s1"), (second, "s2")] {
        let stored = h.runs.get(&run).await.unwrap().unwrap();
        assert_eq!(stored.status, RunStatus::Cancelled);
        match last_lifecycle(&h.journal(&SessionId::new(session).unwrap()).await) {
            LifecycleEvent::Cancelled { reason, .. } => {
                assert_eq!(*reason, RunCancelReason::Shutdown);
            }
            other => panic!("not Cancelled: {other:?}"),
        }
        assert_eq!(
            h.runtime.facts(&run),
            None,
            "the drained runtime holds no run"
        );
        assert_eq!(
            h.runtime.session_state(&SessionId::new(session).unwrap()),
            None,
            "the drained runtime holds no session"
        );
    }
    let refused = h.runtime.start(on_session("s3", "q")).await;
    assert!(
        matches!(refused, Err(StartError::ShuttingDown)),
        "{refused:?}"
    );
}

/// A run that does not stop within the grace period is abandoned: its task
/// is aborted, its ending is written for it, and the drain returns.
#[tokio::test(start_paused = true)]
async fn drain_abandons_a_run_that_does_not_stop() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Stall])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&run).unwrap().status == RunStatus::Running
    })
    .await;

    h.runtime.drain(Duration::from_secs(1)).await;

    let facts = h.runs.get(&run).await.unwrap().unwrap();
    assert_eq!(facts.status, RunStatus::Cancelled);
    assert!(facts.ended_at.is_some());
    let events = h.journal(&SessionId::new("s1").unwrap()).await;
    match last_lifecycle(&events) {
        LifecycleEvent::Cancelled {
            reason, message, ..
        } => {
            assert_eq!(*reason, RunCancelReason::Shutdown);
            assert!(
                message.as_deref().is_some_and(|m| m.contains("grace")),
                "{message:?}"
            );
        }
        other => panic!("not Cancelled: {other:?}"),
    }
    assert_eq!(
        events.iter().filter(|event| event.ends_run()).count(),
        1,
        "the run has one ending"
    );
}

/// While one start waits on a stalled journal store, a start on another
/// session and a drain both still finish. The stalled start is refused once
/// the store answers, because the drain has begun by then.
#[tokio::test(start_paused = true)]
async fn a_stalled_journal_store_does_not_block_other_starts_or_a_drain() {
    let gate = Arc::new(Notify::new());
    let journals = Arc::new(StallingJournalStore {
        inner: Arc::new(InMemoryJournalStore::new()),
        session: SessionId::new("slow").unwrap(),
        gate: Arc::clone(&gate),
        stalled: std::sync::atomic::AtomicUsize::new(0),
    });
    let runs = Arc::new(InMemoryRunStore::new());
    let runtime = Arc::new(AgentRuntime::with_preparer(
        Arc::new(TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())])),
        RuntimeStores {
            runs: Arc::clone(&runs) as Arc<dyn RunStore>,
            journals: Arc::clone(&journals) as Arc<dyn JournalStore>,
            skills: Arc::new(InMemorySkillInvocationStore::new()),
        },
        PendingApprovals::new(),
        DEFAULT_RETENTION,
    ));

    let slow = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        async move { runtime.start(on_session("slow", "q")).await }
    });
    eventually("the slow session's journal is opening", || {
        journals.stalled.load(Ordering::SeqCst) == 1
    })
    .await;

    let other = tokio::time::timeout(PATIENCE, runtime.start(on_session("s2", "q")))
        .await
        .expect("a start on another session is not held up by the stalled store")
        .unwrap();
    tokio::time::timeout(PATIENCE, runtime.drain(Duration::from_secs(1)))
        .await
        .expect("the drain is not held up by the stalled store");
    assert!(
        runs.get(&other)
            .await
            .unwrap()
            .is_some_and(|facts| facts.status.is_terminal()),
        "the other session's run ended"
    );

    gate.notify_one();
    let refused = slow.await.unwrap();
    assert!(
        matches!(refused, Err(StartError::ShuttingDown)),
        "{refused:?}"
    );
}

/// When a caller stops a run before a drain does, the run keeps the caller's
/// reason and message, even if the drain has to abandon it.
#[tokio::test(start_paused = true)]
async fn an_abandoned_run_keeps_the_reason_it_was_first_stopped_for() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Stall])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&run).unwrap().status == RunStatus::Running
    })
    .await;

    assert!(h.runtime.cancel(
        &run,
        RunCancelReason::External,
        Some("the user pressed stop".to_owned())
    ));
    h.runtime.drain(STOP_GRACE / 5).await;

    let events = h.journal(&SessionId::new("s1").unwrap()).await;
    match last_lifecycle(&events) {
        LifecycleEvent::Cancelled {
            reason, message, ..
        } => {
            assert_eq!(*reason, RunCancelReason::External);
            assert_eq!(message.as_deref(), Some("the user pressed stop"));
        }
        other => panic!("not Cancelled: {other:?}"),
    }
    assert_eq!(
        events.iter().filter(|event| event.ends_run()).count(),
        1,
        "the run has one ending"
    );
}

/// A run caught writing its own ending when the grace period runs out is
/// not abandoned: the drain waits for the ending, and the run keeps the one
/// it was writing.
#[tokio::test(start_paused = true)]
async fn drain_waits_for_a_run_already_writing_its_ending() {
    let gate = Arc::new(Notify::new());
    let runs = Arc::new(InMemoryRunStore::new());
    let store = Arc::new(GatedRunStore {
        inner: Arc::clone(&runs),
        gate: Arc::clone(&gate),
        finishing: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_over(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]),
        DEFAULT_RETENTION,
        Arc::clone(&store) as Arc<dyn RunStore>,
        runs,
    );
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is writing its ending", || {
        store.finishing.load(Ordering::SeqCst) == 1
    })
    .await;

    let grace = Duration::from_secs(1);
    let opener = tokio::spawn({
        let gate = Arc::clone(&gate);
        async move {
            tokio::time::sleep(grace + grace / 2).await;
            gate.notify_one();
        }
    });
    h.runtime.drain(grace).await;

    let facts = h.runs.get(&run).await.unwrap().unwrap();
    assert_eq!(
        facts.status,
        RunStatus::Finished,
        "the drain returned with the ending written"
    );
    let events = h.journal(&SessionId::new("s1").unwrap()).await;
    assert_eq!(kind(events.last().unwrap()), "finished");
    assert_eq!(
        events.iter().filter(|event| event.ends_run()).count(),
        1,
        "the run has one ending"
    );
    opener.await.unwrap();
}

/// An ended run stays held for the retention window, so a subscriber that
/// arrives after the end still reads it, and is let go after; its session
/// goes with it, and a later start on the session continues its sequence.
#[tokio::test(start_paused = true)]
async fn an_ended_run_is_held_for_the_retention_window() {
    use futures::StreamExt;

    let retention = Duration::from_secs(60);
    let h = harness_retaining(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Scripted(vec![ScriptedTurn::text("one")]),
                Prepared::Scripted(vec![ScriptedTurn::text("two")]),
            ],
        ),
        retention,
    );
    let session = SessionId::new("s1").unwrap();
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;

    let journal = Arc::clone(
        &h.runtime
            .shared
            .lock_sessions()
            .get(&session)
            .unwrap()
            .journal,
    );
    let mut late = journal.subscribe(crate::journal::Cursor::From(facts.first_seq.unwrap()));
    let mut seen = Vec::new();
    while let Some(crate::journal::JournalItem::Event(event)) = late.next().await {
        let ended = event.ends_run();
        seen.push(event);
        if ended {
            break;
        }
    }
    assert_eq!(kinds(&seen).first(), Some(&"started"));
    assert_eq!(kinds(&seen).last(), Some(&"finished"));
    assert_eq!(h.runtime.facts(&run), Some(facts.clone()));

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the run is let go", || h.runtime.facts(&run).is_none()).await;
    assert_eq!(h.runtime.session_state(&session), None);
    drop(journal);
    assert!(
        late.next().await.is_none(),
        "the session's journal is gone, so its subscription ends"
    );
    assert_eq!(
        h.runs.get(&run).await.unwrap(),
        Some(facts.clone()),
        "the store keeps the run's facts"
    );

    let next = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let next = h.ended(next).await;
    assert_eq!(next.first_seq, facts.latest_seq.map(SequenceNumber::next));
}

// ---------------------------------------------------------------------------
// Endings the stream decides
// ---------------------------------------------------------------------------

/// A run that calls a tool its client executes yields to the client: the
/// agent's hook ends the stream with an error, and the run finishes.
#[tokio::test]
async fn a_run_that_yields_to_a_client_tool_finishes() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::ClientToolCall])],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Finished);
    assert_eq!(
        kind(
            h.journal(&SessionId::new("s1").unwrap())
                .await
                .last()
                .unwrap()
        ),
        "finished"
    );
}

/// A run whose task panics fails like any other error, so the session is
/// freed for its next run.
#[tokio::test]
async fn a_panicking_run_fails_and_frees_its_session() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fake(vec![Step::Panic]),
            Prepared::Fake(Vec::new()),
        ],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Failed);
    match last_lifecycle(&h.journal(&SessionId::new("s1").unwrap()).await) {
        LifecycleEvent::Failed { error, .. } => {
            assert!(error.contains("the fake agent panicked"), "{error}");
        }
        other => panic!("not Failed: {other:?}"),
    }
    assert_eq!(
        h.runtime.session_state(&SessionId::new("s1").unwrap()),
        Some(AgentState::Done)
    );
    let next = h.runtime.start(on_session("s1", "again")).await.unwrap();
    assert_eq!(h.ended(next).await.status, RunStatus::Finished);
}

fn preparing(run_id: RunId) -> RunFacts {
    RunFacts {
        run_id,
        session_id: SessionId::new("s1").unwrap(),
        agent: "a".to_owned(),
        status: RunStatus::Preparing,
        liveness: Liveness::default(),
        started_at: aura_events::Timestamp::now(),
        ended_at: None,
        usage: TokenUsage::default(),
        first_seq: None,
        latest_seq: None,
        pending_approvals: Vec::new(),
        checkpoint: None,
        continues: None,
    }
}

/// The first stop's reason is the one kept. Once the run's stream has ended,
/// stops are refused, so the run keeps the ending it reached.
#[test]
fn a_stop_after_the_stream_ended_is_refused() {
    let record = RunRecord::new(preparing(RunId::mint()));
    assert!(record.stop(RunCancelReason::External, Some("first".to_owned())));
    assert!(
        record.stop(RunCancelReason::Shutdown, None),
        "a later stop still stops"
    );
    assert_eq!(
        record.stopped(),
        Some((RunCancelReason::External, Some("first".to_owned())))
    );
    assert!(record.cancel.is_cancelled());

    let record = RunRecord::new(preparing(RunId::mint()));
    record.close_to_stops();
    assert!(!record.stop(RunCancelReason::External, None));
    assert_eq!(record.stopped(), None);
    assert!(!record.cancel.is_cancelled());
}

/// A stopped run whose provider never answers cannot see the stop, so its
/// stream is dropped after the stop grace and the run ends cancelled.
#[tokio::test(start_paused = true)]
async fn a_stopped_run_whose_stream_does_not_end_is_dropped_after_the_grace() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Stall])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&run).unwrap().status == RunStatus::Running
    })
    .await;
    let stopped_at = tokio::time::Instant::now();

    assert!(h.runtime.cancel(&run, RunCancelReason::External, None));
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Cancelled);
    assert!(
        stopped_at.elapsed() >= STOP_GRACE,
        "the stream was given the grace: {:?}",
        stopped_at.elapsed()
    );
    assert_eq!(h.preparer.closed.lock().unwrap().len(), 1);
    assert_eq!(
        h.runtime.session_state(&SessionId::new("s1").unwrap()),
        Some(AgentState::Done)
    );
}

/// A session whose journal another writer fenced is reopened for its next
/// run, continuing from what the other writer wrote.
#[tokio::test]
async fn a_fenced_journal_is_reopened_for_the_next_run() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Scripted(vec![ScriptedTurn::text("one")]),
            Prepared::Scripted(vec![ScriptedTurn::text("two")]),
            Prepared::Scripted(vec![ScriptedTurn::text("three")]),
        ],
    ));
    let session = SessionId::new("s1").unwrap();
    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.ended(first).await;

    let elsewhere = h
        .journals
        .append(
            &session,
            h.journals.latest(&session).await.unwrap(),
            None,
            aura_events::Timestamp::now(),
            SessionEventPayload::Agent(AgentEvent::single_agent(AgentEventPayload::TextDelta {
                content: "from another writer".to_owned(),
            })),
        )
        .await
        .unwrap();

    let second = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let second = h.ended(second).await;
    assert_eq!(second.status, RunStatus::Failed);
    assert_eq!(h.runtime.session_state(&session), Some(AgentState::Done));

    let third = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let third = h.ended(third).await;
    assert_eq!(third.status, RunStatus::Finished);
    assert_eq!(
        third.first_seq,
        Some(elsewhere.seq.next()),
        "the reopened journal continues after the other writer"
    );
    let events = h.journal(&session).await;
    assert_eq!(kinds(&events).last(), Some(&"finished"));
}

/// Two drains at once write one ending: the second to abandon a run finds
/// its ending taken and waits for it.
#[tokio::test(start_paused = true)]
async fn concurrent_drains_write_one_ending() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Stall])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&run).unwrap().status == RunStatus::Running
    })
    .await;

    let grace = Duration::from_secs(1);
    tokio::join!(h.runtime.drain(grace), h.runtime.drain(grace));

    let events = h.journal(&SessionId::new("s1").unwrap()).await;
    assert_eq!(
        events.iter().filter(|event| event.ends_run()).count(),
        1,
        "the run has one ending: {:?}",
        kinds(&events)
    );
    assert_eq!(
        h.runs.get(&run).await.unwrap().unwrap().status,
        RunStatus::Cancelled
    );
}

/// A drain whose store never finishes an abandoned run's ending still
/// returns once the grace period has run out again.
#[tokio::test(start_paused = true)]
async fn drain_gives_up_on_an_ending_the_store_never_finishes() {
    let runs = Arc::new(InMemoryRunStore::new());
    let store = Arc::new(GatedRunStore {
        inner: Arc::clone(&runs),
        gate: Arc::new(Notify::new()),
        finishing: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_over(
        TestPreparer::new(&["a"], vec![Prepared::Fake(vec![Step::Stall])]),
        DEFAULT_RETENTION,
        Arc::clone(&store) as Arc<dyn RunStore>,
        runs,
    );
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&run).unwrap().status == RunStatus::Running
    })
    .await;

    let grace = Duration::from_secs(1);
    tokio::time::timeout(PATIENCE, h.runtime.drain(grace))
        .await
        .expect("the drain returns although the store never finishes");

    assert_eq!(store.finishing.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.runtime.facts(&run),
        None,
        "the drained runtime holds no run"
    );
    assert_eq!(
        kinds(&h.journal(&SessionId::new("s1").unwrap()).await).last(),
        Some(&"cancelled"),
        "the ending reached the journal before the store stalled"
    );
}

/// A run whose ending panics is marked failed in this process and retired
/// like any ended run, so its session does not stay held.
#[tokio::test(start_paused = true)]
async fn a_run_whose_ending_panics_is_still_retired() {
    let retention = Duration::from_secs(60);
    let runs = Arc::new(InMemoryRunStore::new());
    let h = harness_over(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]),
        retention,
        Arc::new(PanickingRunStore {
            inner: Arc::clone(&runs),
        }) as Arc<dyn RunStore>,
        runs,
    );
    let session = SessionId::new("s1").unwrap();

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;

    assert_eq!(facts.status, RunStatus::Failed);
    assert_eq!(h.runtime.session_state(&session), Some(AgentState::Done));

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the run is let go", || h.runtime.facts(&run).is_none()).await;
    assert_eq!(
        h.runtime.session_state(&session),
        None,
        "the session goes once it holds nothing"
    );
}
