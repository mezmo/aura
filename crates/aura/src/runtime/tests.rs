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
use super::task::{MCP_CLOSE_TIMEOUT, STOP_GRACE};
use super::*;
use crate::builder::test_support::prepared_agent;
use crate::builder::{ClientTool, no_run_tools};
use crate::config::SkillConfig;
use crate::error::BuilderError;
use crate::forwarded_headers::ForwardedHeaders;
use crate::hitl::DecisionId;
use crate::orchestration::{ScriptedCompletionModel, ScriptedTurn};
use crate::passthrough_tool::PASSTHROUGH_MARKER;
use crate::provider_agent::{
    ProviderAgent, StreamError, StreamItem, StreamedUserContent, ToolResult, ToolResultKind,
};
use crate::run_context::{Attendance, RunContext};
use crate::session_store::JournalEntry;
use crate::streaming::{AgentRun, RunOptions, StreamingAgent};
use crate::streaming_request_hook::UsageState;
use aura_events::McpServerStatus;

mod observe;

// ---------------------------------------------------------------------------
// Test agents
// ---------------------------------------------------------------------------

/// One step of a [`FakeAgent`]'s run.
#[derive(Clone)]
enum Step {
    /// Emits an event on the run's channel.
    Emit(Box<AgentEventPayload>),
    /// Sends an event as an item on the run's stream, the way the
    /// orchestrator sends its own events.
    EmitItem(Box<AgentEventPayload>),
    /// Waits for the gate, or for the run to be cancelled.
    Wait(Arc<Notify>),
    /// Waits forever, whatever happens to the run.
    Stall,
    /// Makes the agent's `cancel_and_close_mcp` never return.
    CloseHangs,
    /// Makes an MCP call, which returns only once the run's MCP calls are
    /// cancelled.
    McpCall,
    /// Calls a tool the client executes.
    ClientToolCall,
    /// Yields an item.
    Item(Box<StreamItem>),
    /// Records one provider turn on the run's usage counters.
    Bill { prompt: u64, completion: u64 },
    /// Panics.
    Panic,
}

/// A streaming agent that follows a script of steps.
struct FakeAgent {
    run_id: RunId,
    steps: Vec<Step>,
    skills: Vec<SkillConfig>,
    servers: Vec<McpServerStatus>,
    context_window: Option<u64>,
    /// Every `cancel_and_close_mcp` call: the request id and the reason.
    closed: Arc<Mutex<Vec<(String, String)>>>,
    /// Woken by `cancel_and_close_mcp`.
    mcp_cancelled: Arc<Notify>,
    /// The run's attendance as each `stream` call saw it.
    attended: Arc<Mutex<Vec<Attendance>>>,
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
        let attendance = options.attendance();
        let (_, cancel) = options.into_parts();
        let cancel = cancel.unwrap_or_default();
        let (run, events) = RunContext::channel_on(self.run_id, cancel.clone());
        run.attend(attendance);
        self.attended.lock().unwrap().push(run.attendance());
        let context = Arc::clone(&run);
        let steps = self.steps.clone();
        let token = cancel.clone();
        let mcp_cancelled = Arc::clone(&self.mcp_cancelled);
        let usage = UsageState::new();
        let counters = usage.clone();
        let items = async_stream::stream! {
            for step in steps {
                match step {
                    Step::Emit(payload) => {
                        run.emit(AgentEvent::single_agent(*payload)).await;
                    }
                    Step::EmitItem(payload) => {
                        yield Ok(StreamItem::AgentEvent(Box::new(AgentEvent::new(
                            aura_events::EmittingAgent::coordinator(),
                            *payload,
                        ))));
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
                    Step::CloseHangs => {}
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
                                kind: ToolResultKind::Text,
                            },
                        )));
                        let error: StreamError = "prompt cancelled".into();
                        yield Err(error);
                        return;
                    }
                    Step::Item(item) => yield Ok(*item),
                    Step::Bill { prompt, completion } => {
                        counters.store_usage(prompt, completion, prompt + completion, false);
                    }
                    Step::Panic => panic!("the fake agent panicked"),
                }
            }
        };
        let items: futures::stream::BoxStream<'static, Result<StreamItem, StreamError>> =
            Box::pin(items);
        AgentRun::new(items, cancel, usage)
            .observed_by(events)
            .within(context)
    }

    fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    fn mcp_server_status(&self) -> Vec<McpServerStatus> {
        self.servers.clone()
    }

    fn skills(&self) -> &[SkillConfig] {
        &self.skills
    }

    async fn cancel_and_close_mcp(&self, request_id: &str, reason: &str) -> usize {
        self.closed
            .lock()
            .unwrap()
            .push((request_id.to_owned(), reason.to_owned()));
        self.mcp_cancelled.notify_one();
        if self
            .steps
            .iter()
            .any(|step| matches!(step, Step::CloseHangs))
        {
            std::future::pending::<()>().await;
        }
        0
    }
}

/// What one `prepare` call hands back.
enum Prepared {
    /// A prepared agent over a model playing these turns.
    Scripted(Vec<ScriptedTurn>),
    /// A fake agent following these steps, attributed to the coordinator.
    Fake(Vec<Step>),
    /// A fake agent built from a `Fake`.
    Described(Fake),
    /// A failure to prepare.
    Fail(String),
}

/// A fake agent's steps, the agent its events are attributed to, and what it
/// reports about itself: its skills, MCP servers and context window.
struct Fake {
    steps: Vec<Step>,
    attribution: aura_events::EmittingAgent,
    skills: Vec<SkillConfig>,
    servers: Vec<McpServerStatus>,
    context_window: Option<u64>,
}

impl Fake {
    /// A fake standing in for a single agent.
    fn single(steps: Vec<Step>) -> Self {
        Self {
            steps,
            attribution: aura_events::EmittingAgent::single_agent(),
            skills: Vec::new(),
            servers: Vec::new(),
            context_window: None,
        }
    }

    /// A fake standing in for an orchestrated run.
    fn coordinator(steps: Vec<Step>) -> Self {
        Self {
            attribution: aura_events::EmittingAgent::coordinator(),
            ..Self::single(steps)
        }
    }
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
    /// The attendance every fake agent's `stream` saw.
    attended: Arc<Mutex<Vec<Attendance>>>,
}

impl TestPreparer {
    fn new(agents: &[&str], queued: Vec<Prepared>) -> Self {
        Self {
            configs: agents.iter().map(|name| config(name)).collect(),
            queued: Mutex::new(queued.into()),
            gate: None,
            prepared: Mutex::new(Vec::new()),
            closed: Arc::default(),
            attended: Arc::default(),
        }
    }

    fn gated(mut self, gate: Arc<Notify>) -> Self {
        self.gate = Some(gate);
        self
    }

    fn fake(&self, run_id: RunId, fake: Fake) -> PreparedForRun {
        let Fake {
            steps,
            attribution,
            skills,
            servers,
            context_window,
        } = fake;
        PreparedForRun::Built {
            agent: Arc::new(FakeAgent {
                run_id,
                steps,
                skills,
                servers,
                context_window,
                closed: Arc::clone(&self.closed),
                mcp_cancelled: Arc::default(),
                attended: Arc::clone(&self.attended),
            }),
            attribution,
        }
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

    /// Opens and closes a `test.prepare` span, so a test can check which span
    /// the agent is prepared under. Then waits for the gate, if there is one,
    /// and hands back the next queued agent.
    async fn prepare(
        &self,
        config: &Config,
        request: PrepareRequest<'_>,
    ) -> Result<PreparedForRun, BuilderError> {
        drop(tracing::info_span!("test.prepare"));
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
            Prepared::Fake(steps) => Ok(self.fake(request.run_id, Fake::coordinator(steps))),
            Prepared::Described(fake) => Ok(self.fake(request.run_id, fake)),
            Prepared::Fail(error) => Err(BuilderError::AgentError(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    runtime: Arc<AgentRuntime>,
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
    let journals = Arc::new(InMemoryJournalStore::new());
    harness_with(
        preparer,
        retention,
        store,
        runs,
        Arc::clone(&journals) as Arc<dyn JournalStore>,
        journals,
    )
}

/// A harness whose runtime writes runs to `store` and journals to
/// `journal_store`. `runs` and `journals` are the in-memory stores behind
/// them, for the test to read.
fn harness_with(
    preparer: TestPreparer,
    retention: Duration,
    store: Arc<dyn RunStore>,
    runs: Arc<InMemoryRunStore>,
    journal_store: Arc<dyn JournalStore>,
    journals: Arc<InMemoryJournalStore>,
) -> Harness {
    let preparer = Arc::new(preparer);
    let runtime = Arc::new(AgentRuntime::with_preparer(
        Arc::clone(&preparer) as Arc<dyn AgentPreparer>,
        RuntimeStores {
            runs: store,
            journals: journal_store,
            skills: Arc::new(InMemorySkillInvocationStore::new()),
        },
        PendingApprovals::new(),
        retention,
    ));
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

/// A faulty journal store.
struct PanickingJournalStore {
    inner: Arc<InMemoryJournalStore>,
}

#[async_trait]
impl JournalStore for PanickingJournalStore {
    /// Panics instead of appending.
    async fn append(
        &self,
        _: &SessionId,
        _: Option<SequenceNumber>,
        _: Option<RunId>,
        _: aura_events::Timestamp,
        _: SessionEventPayload,
    ) -> Result<Arc<SessionEvent>, SessionStoreError> {
        panic!("the journal store panicked while appending")
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

    async fn latest(
        &self,
        session: &SessionId,
    ) -> Result<Option<SequenceNumber>, SessionStoreError> {
        self.inner.latest(session).await
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

/// A harness whose journal store stalls reading `session`'s newest number
/// until the returned gate is notified.
fn harness_stalling_on(
    session: &str,
    queued: Vec<Prepared>,
) -> (Harness, Arc<StallingJournalStore>, Arc<Notify>) {
    let gate = Arc::new(Notify::new());
    let inner = Arc::new(InMemoryJournalStore::new());
    let journals = Arc::new(StallingJournalStore {
        inner: Arc::clone(&inner),
        session: SessionId::new(session).unwrap(),
        gate: Arc::clone(&gate),
        stalled: std::sync::atomic::AtomicUsize::new(0),
    });
    let runs = Arc::new(InMemoryRunStore::new());
    let h = harness_with(
        TestPreparer::new(&["a"], queued),
        DEFAULT_RETENTION,
        Arc::clone(&runs) as Arc<dyn RunStore>,
        runs,
        Arc::clone(&journals) as Arc<dyn JournalStore>,
        inner,
    );
    (h, journals, gate)
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
            LifecycleEvent::ObserverAttached { .. } => "observer_attached",
            LifecycleEvent::ObserverDetached { .. } => "observer_detached",
            LifecycleEvent::ClaimsExhausted => "claims_exhausted",
            _ => "lifecycle",
        },
        SessionEventPayload::Agent(agent) => match &agent.payload {
            AgentEventPayload::SessionInfo { .. } => "session_info",
            AgentEventPayload::SkillsRehydrated { .. } => "skills_rehydrated",
            AgentEventPayload::McpStatus { .. } => "mcp_status",
            AgentEventPayload::TextDelta { .. } => "text_delta",
            AgentEventPayload::ToolRequested { .. } => "tool_requested",
            AgentEventPayload::ToolComplete { .. } => "tool_complete",
            AgentEventPayload::Completed { .. } => "completed",
            AgentEventPayload::Usage { .. } => "usage",
            AgentEventPayload::ContextUsage { .. } => "context_usage",
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
    assert_eq!(
        kinds[kinds.len() - 3..],
        ["usage", "context_usage", "finished"],
        "{kinds:?}"
    );
    assert!(kinds.contains(&"text_delta"), "{kinds:?}");
    assert!(kinds.contains(&"completed"), "{kinds:?}");
    assert!(
        !kinds.contains(&"skills_rehydrated"),
        "nothing was rehydrated, so nothing says so: {kinds:?}"
    );
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

/// The orchestrator reports a park as an item on the run's stream, not on
/// the run's channel. A run it parks still ends `Parked`, naming the
/// approvals it waits on.
#[tokio::test]
async fn a_run_the_orchestrator_parks_ends_parked() {
    let decision = DecisionId::generate();
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![
            Step::Emit(Box::new(requested(decision))),
            Step::EmitItem(Box::new(AgentEventPayload::RunParked {
                run_id: "ignored".to_owned(),
                decision_ids: vec![decision.to_string()],
                expires_at: "2026-10-21T00:00:00Z".to_owned(),
                iteration: 1,
            })),
        ])],
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
    assert_eq!(
        h.runtime.session_state(&SessionId::new("s1").unwrap()),
        Some(AgentState::Parked)
    );
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

/// The production preparer gives a single agent the client's tools only when
/// its config opts in, and only those `client_tool_filter` lets through.
#[tokio::test]
async fn the_preparer_gives_client_tools_only_to_an_agent_that_opts_in() {
    async fn client_tools_given(opted_in: bool) -> Vec<String> {
        let config = aura_config::load_config_from_str(&format!(
            r#"
[agent]
name = "a"
system_prompt = "Test"
enable_client_tools = {opted_in}
client_tool_filter = ["Read"]

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
"#
        ))
        .unwrap();
        let tool = |name: &str| ClientTool {
            name: name.to_owned(),
            description: String::new(),
            parameters: serde_json::json!({ "type": "object" }),
        };
        let preparer =
            RigBuilderPreparer::new(Arc::new(vec![config.clone()]), PendingApprovals::new());
        let prepared = preparer
            .prepare(
                &config,
                PrepareRequest {
                    session: &SessionId::new("s1").unwrap(),
                    run_id: RunId::mint(),
                    req_headers: None,
                    client_tools: Some(vec![tool("Read"), tool("Shell")]),
                    run_tools: no_run_tools(),
                    skill_recorder: None,
                },
            )
            .await
            .unwrap();
        let PreparedForRun::Prepared(prepared) = prepared else {
            panic!("a single agent is prepared, not built");
        };
        let mut names: Vec<_> = prepared.client_tool_names.iter().cloned().collect();
        names.sort();
        names
    }

    assert_eq!(client_tools_given(true).await, ["Read"]);
    assert!(
        client_tools_given(false).await.is_empty(),
        "an agent that did not opt in gets no client tools"
    );
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
/// lets go of every run and session it held. A run the drain stopped ends
/// with no message; one it had to abandon would say it did not stop in time.
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
            LifecycleEvent::Cancelled {
                reason, message, ..
            } => {
                assert_eq!(*reason, RunCancelReason::Shutdown);
                assert_eq!(*message, None, "the drain stopped the run");
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
    let (h, journals, gate) = harness_stalling_on("slow", vec![Prepared::Fake(Vec::new())]);

    let slow = tokio::spawn({
        let runtime = Arc::clone(&h.runtime);
        async move { runtime.start(on_session("slow", "q")).await }
    });
    eventually("the slow session's journal is opening", || {
        journals.stalled.load(Ordering::SeqCst) == 1
    })
    .await;

    let other = tokio::time::timeout(PATIENCE, h.runtime.start(on_session("s2", "q")))
        .await
        .expect("a start on another session is not held up by the stalled store")
        .unwrap();
    tokio::time::timeout(PATIENCE, h.runtime.drain(Duration::from_secs(1)))
        .await
        .expect("the drain is not held up by the stalled store");
    assert!(
        h.runs
            .get(&other)
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

/// Two starts racing to open the same new session admit one run between
/// them; the other is refused for that run.
#[tokio::test(start_paused = true)]
async fn two_starts_racing_on_a_new_session_admit_one_run() {
    let (h, journals, gate) = harness_stalling_on("new", vec![Prepared::Fake(Vec::new())]);
    let start = || {
        let runtime = Arc::clone(&h.runtime);
        tokio::spawn(async move { runtime.start(on_session("new", "q")).await })
    };
    let (first, second) = (start(), start());
    eventually("both starts are opening the session's journal", || {
        journals.stalled.load(Ordering::SeqCst) == 2
    })
    .await;

    gate.notify_waiters();
    let (first, second) = (first.await.unwrap(), second.await.unwrap());

    let (admitted, refused) = match (first, second) {
        (Ok(run), Err(refused)) | (Err(refused), Ok(run)) => (run, refused),
        other => panic!("one start is admitted and one refused, got {other:?}"),
    };
    assert!(
        matches!(refused, StartError::RunInProgress(live) if live == admitted),
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

/// A session is free for its next run as soon as the last run's ending is
/// journaled, while that run's facts are still being written to the store.
#[tokio::test]
async fn the_next_start_is_admitted_while_the_last_run_writes_its_facts() {
    let runs = Arc::new(InMemoryRunStore::new());
    let store = Arc::new(GatedRunStore {
        inner: Arc::clone(&runs),
        gate: Arc::new(Notify::new()),
        finishing: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(Vec::new()), Prepared::Fake(Vec::new())],
        ),
        DEFAULT_RETENTION,
        Arc::clone(&store) as Arc<dyn RunStore>,
        runs,
    );
    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the first run is writing its facts", || {
        store.finishing.load(Ordering::SeqCst) == 1
    })
    .await;

    assert_eq!(
        h.runtime.facts(&first).map(|facts| facts.status),
        Some(RunStatus::Finished)
    );
    let next = h.runtime.start(on_session("s1", "q")).await;
    assert!(next.is_ok(), "{next:?}");
}

/// A run that is still writing its own ending when the grace period runs
/// out is not abandoned. The drain waits for that ending, and the run keeps
/// it.
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

/// Letting an ended run go leaves its session alone while a later run on it
/// is still live.
#[tokio::test(start_paused = true)]
async fn retiring_a_run_keeps_a_session_whose_next_run_is_live() {
    let retention = Duration::from_secs(60);
    let gate = Arc::new(Notify::new());
    let h = harness_retaining(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Scripted(vec![ScriptedTurn::text("one")]),
                Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))]),
            ],
        ),
        retention,
    );
    let session = SessionId::new("s1").unwrap();
    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.ended(first).await;
    let second = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the second run is running", || {
        h.runtime.facts(&second).unwrap().status == RunStatus::Running
    })
    .await;

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the first run is let go", || {
        h.runtime.facts(&first).is_none()
    })
    .await;

    assert_eq!(
        h.runtime.session_state(&session),
        Some(AgentState::Running),
        "the session stays while its second run is live"
    );
    gate.notify_one();
    assert_eq!(h.ended(second).await.status, RunStatus::Finished);
}

/// Letting an ended run go leaves the session's record alone once a later
/// start has reopened the session under a record of its own.
#[tokio::test(start_paused = true)]
async fn retiring_a_run_keeps_a_reopened_session_record() {
    let retention = Duration::from_secs(60);
    let gate = Arc::new(Notify::new());
    let h = harness_retaining(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Scripted(vec![ScriptedTurn::text("one")]),
                Prepared::Scripted(vec![ScriptedTurn::text("two")]),
                Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))]),
            ],
        ),
        retention,
    );
    let session = SessionId::new("s1").unwrap();
    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.ended(first).await;
    h.journals
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
    // The second run meets the other writer, which fences the journal.
    let second = h.runtime.start(on_session("s1", "q")).await.unwrap();
    assert_eq!(h.ended(second).await.status, RunStatus::Failed);
    // The third start reopens the session under a record of its own.
    let third = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the third run is running", || {
        h.runtime.facts(&third).unwrap().status == RunStatus::Running
    })
    .await;

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the first two runs are let go", || {
        h.runtime.facts(&first).is_none() && h.runtime.facts(&second).is_none()
    })
    .await;

    assert_eq!(
        h.runtime.session_state(&session),
        Some(AgentState::Running),
        "the reopened record stays while its run is live"
    );
    gate.notify_one();
    assert_eq!(h.ended(third).await.status, RunStatus::Finished);
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

/// A stopped run whose MCP close never returns still ends: the close gets
/// `MCP_CLOSE_TIMEOUT`, then the stream gets the stop grace.
#[tokio::test(start_paused = true)]
async fn a_stopped_run_whose_mcp_close_hangs_still_ends() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::CloseHangs, Step::Stall])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&run).unwrap().status == RunStatus::Running
    })
    .await;
    let stopped_at = tokio::time::Instant::now();

    assert!(h.runtime.cancel(&run, RunCancelReason::External, None));
    // Longer than `Harness::ended` waits, which is the close and the grace
    // together.
    let record = h
        .runtime
        .shared
        .run(&run)
        .expect("the runtime holds the run");
    let mut watch = record.watch();
    let facts = tokio::time::timeout(
        MCP_CLOSE_TIMEOUT + STOP_GRACE + PATIENCE,
        watch.wait_for(|facts| facts.status.is_terminal()),
    )
    .await
    .expect("the run ends in time")
    .expect("the run's record is alive")
    .clone();

    assert_eq!(facts.status, RunStatus::Cancelled);
    assert!(
        stopped_at.elapsed() >= MCP_CLOSE_TIMEOUT + STOP_GRACE,
        "the close and then the stream were each given their time: {:?}",
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

/// A run whose final store write panics keeps the ending it reached, since
/// its session is already free by then, and is retired like any ended run.
#[tokio::test(start_paused = true)]
async fn a_run_whose_final_store_write_panics_keeps_its_ending() {
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

    assert_eq!(facts.status, RunStatus::Finished);
    assert_eq!(kind(h.journal(&session).await.last().unwrap()), "finished");
    assert_eq!(h.runtime.session_state(&session), Some(AgentState::Done));

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the run is let go", || h.runtime.facts(&run).is_none()).await;
    assert_eq!(
        h.runtime.session_state(&session),
        None,
        "the session goes once it holds nothing"
    );
}

/// A run whose ending panics before its session is freed is marked failed
/// in this process and retired like any ended run, so its session does not
/// stay held.
#[tokio::test(start_paused = true)]
async fn a_run_whose_ending_panics_before_its_session_is_freed_fails() {
    let retention = Duration::from_secs(60);
    let runs = Arc::new(InMemoryRunStore::new());
    let journals = Arc::new(InMemoryJournalStore::new());
    let h = harness_with(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]),
        retention,
        Arc::clone(&runs) as Arc<dyn RunStore>,
        runs,
        Arc::new(PanickingJournalStore {
            inner: Arc::clone(&journals),
        }) as Arc<dyn JournalStore>,
        journals,
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

// ---------------------------------------------------------------------------
// What the chat stream reads off the journal
// ---------------------------------------------------------------------------

/// A skill named `name` in its own directory under `dir`.
fn skill(dir: &std::path::Path, name: &str) -> SkillConfig {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} skill\n---\nDeploy with care."),
    )
    .unwrap();
    SkillConfig {
        name: crate::config::SkillName::new(name).unwrap(),
        description: format!("{name} skill"),
        path: skill_dir,
    }
}

fn server() -> McpServerStatus {
    McpServerStatus {
        server_name: "files".to_owned(),
        transport: "stdio".to_owned(),
        status: "connected".to_owned(),
        tools_count: 3,
        reason: None,
    }
}

fn item(item: StreamItem) -> Step {
    Step::Item(Box::new(item))
}

fn said(content: &str) -> StreamItem {
    StreamItem::StreamAssistantItem(crate::provider_agent::StreamedAssistantContent::Text(
        content.to_owned(),
    ))
}

fn answered(content: &str, input: u64, output: u64, cache: Option<(u64, u64)>) -> StreamItem {
    StreamItem::Final(crate::provider_agent::FinalResponseInfo {
        content: content.to_owned(),
        usage: rig::completion::Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
        },
        cache_usage: cache.map(|(read, creation)| rig::completion::CacheUsage {
            cache_read_input_tokens: read,
            cache_creation_input_tokens: creation,
        }),
    })
}

fn tokens(prompt: u64, completion: u64) -> TokenUsage {
    TokenUsage {
        prompt_tokens: TokenCount::new(prompt),
        completion_tokens: TokenCount::new(completion),
        total_tokens: TokenCount::new(prompt + completion),
    }
}

/// The agent event inside `event`. Panics if `event` is not an agent event.
fn agent_event(event: &SessionEvent) -> &AgentEvent {
    match &event.payload {
        SessionEventPayload::Agent(agent) => agent,
        other => panic!("not an agent event: {other:?}"),
    }
}

/// The usage carried by `event`, which must be a terminal event.
fn ending_usage(event: &SessionEvent) -> TokenUsage {
    match &event.payload {
        SessionEventPayload::Lifecycle(
            LifecycleEvent::Finished { usage }
            | LifecycleEvent::Failed { usage, .. }
            | LifecycleEvent::Cancelled { usage, .. }
            | LifecycleEvent::Parked { usage, .. },
        ) => *usage,
        other => panic!("not an ending: {other:?}"),
    }
}

/// Waits until the run store holds `run` with a terminal status, and returns
/// its facts.
async fn stored(h: &Harness, run: RunId) -> RunFacts {
    tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(facts) = h.runs.get(&run).await.unwrap()
                && facts.status.is_terminal()
            {
                return facts;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the run store has the run's ending")
}

/// A run opens with its session info, then the skills put back into its
/// history, then its MCP servers' status. That is the order the
/// request-bound path opens a stream with.
#[tokio::test]
async fn a_run_opens_with_session_info_then_rehydrated_skills_then_mcp_status() {
    use crate::session_store::{
        SKILL_INVOCATION_RECORD_VERSION, SkillInvocation, SkillInvocationRecord, SkillLogKey,
    };

    let skills = tempfile::TempDir::new().unwrap();
    let mut preparer = TestPreparer::new(
        &["a"],
        vec![Prepared::Described(Fake {
            skills: vec![skill(skills.path(), "deploy")],
            servers: vec![server()],
            ..Fake::single(Vec::new())
        })],
    );
    preparer.configs[0]
        .agent
        .skills
        .local
        .push(aura_config::LocalSkillSource {
            source: skills.path().to_path_buf(),
        });
    let h = harness(preparer);
    let session = SessionId::new("s1").unwrap();
    h.runtime
        .shared
        .skill_store
        .record(
            &SkillLogKey::new(session.clone(), "a"),
            SkillInvocationRecord {
                version: SKILL_INVOCATION_RECORD_VERSION,
                invocation: SkillInvocation::LoadSkill {
                    name: "deploy".to_owned(),
                },
                tool_call_id: "call_skill".to_owned(),
                anchor: 1,
                seq: 0,
                invoked_at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();

    let run = h
        .runtime
        .start(StartRun {
            history: vec![Message::user("deploy it"), Message::assistant("deployed")],
            ..on_session("s1", "again")
        })
        .await
        .unwrap();
    h.ended(run).await;

    let events = h.journal(&session).await;
    assert_eq!(
        kinds(&events)[..4],
        ["started", "session_info", "skills_rehydrated", "mcp_status"]
    );
    assert!(matches!(
        &agent_event(&events[2]).payload,
        AgentEventPayload::SkillsRehydrated { skills } if skills == &["deploy"]
    ));
}

/// Before its ending, a single agent's run appends a `Usage` event for what
/// it billed, then a `ContextUsage` event for how full its context window
/// was on its last turn. The billed usage comes from the final response,
/// which counts every turn, while the usage counters here missed a turn.
/// The ending and the run's facts carry the same billed usage.
#[tokio::test]
async fn a_single_agent_closes_with_what_it_billed_then_its_context_occupancy() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Described(Fake {
            context_window: Some(128_000),
            ..Fake::single(vec![
                item(said("Done.")),
                Step::Bill {
                    prompt: 7_000,
                    completion: 200,
                },
                item(answered("Done.", 12_000, 250, Some((9_000, 2_000)))),
            ])
        })],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;
    let events = h.journal(&SessionId::new("s1").unwrap()).await;

    assert_eq!(
        kinds(&events),
        [
            "started",
            "session_info",
            "text_delta",
            "completed",
            "usage",
            "context_usage",
            "finished"
        ]
    );
    let billed = tokens(12_000, 250);
    let usage = agent_event(&events[4]);
    assert_eq!(usage.agent, aura_events::EmittingAgent::single_agent());
    assert!(
        matches!(
            usage.payload,
            AgentEventPayload::Usage {
                usage,
                cache_read_input_tokens: Some(read),
                cache_creation_input_tokens: Some(creation),
            } if usage == billed && read.get() == 9_000 && creation.get() == 2_000
        ),
        "{:?}",
        usage.payload
    );
    let occupancy = agent_event(&events[5]);
    assert_eq!(occupancy.agent, aura_events::EmittingAgent::single_agent());
    assert!(
        matches!(
            occupancy.payload,
            AgentEventPayload::ContextUsage {
                context_tokens,
                response_tokens,
                context_window: Some(window),
            } if context_tokens.get() == 7_000
                && response_tokens.get() == 200
                && window.get() == 128_000
        ),
        "{:?}",
        occupancy.payload
    );
    assert_eq!(ending_usage(&events[6]), billed);
    assert_eq!(facts.usage, billed);
    assert_eq!(stored(&h, run).await.usage, billed);
}

/// A prepared agent's tool turn produces no text, so the usage counters
/// never see it. The final response does count it, so the run is billed for
/// both turns. The streaming hook sends the tool call's `ToolRequested`, and
/// the copy of the run's items sends its `ToolComplete`.
#[tokio::test]
async fn a_prepared_agent_is_billed_for_a_tool_turn_its_counters_missed() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Scripted(vec![
            ScriptedTurn::tool_calls(vec![crate::orchestration::ScriptedToolCall::new(
                "call_0",
                "lookup",
                serde_json::json!({ "q": "x" }),
            )]),
            ScriptedTurn::text("found it"),
        ])],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;
    let events = h.journal(&SessionId::new("s1").unwrap()).await;

    let kinds = kinds(&events);
    let requested = kinds.iter().position(|kind| *kind == "tool_requested");
    let completed = kinds.iter().position(|kind| *kind == "tool_complete");
    assert!(
        requested.is_some() && completed > requested,
        "the call is requested, then completed: {kinds:?}"
    );
    assert_eq!(
        kinds[kinds.len() - 3..],
        ["usage", "context_usage", "finished"]
    );
    let billed = tokens(20, 10);
    assert!(matches!(
        agent_event(&events[events.len() - 3]).payload,
        AgentEventPayload::Usage { usage, .. } if usage == billed
    ));
    assert!(matches!(
        agent_event(&events[events.len() - 2]).payload,
        AgentEventPayload::ContextUsage { context_tokens, response_tokens, context_window: None }
            if context_tokens.get() == 10 && response_tokens.get() == 5
    ));
    assert_eq!(ending_usage(events.last().unwrap()), billed);
    assert_eq!(facts.usage, billed);
}

/// An orchestrated run closes with one `Usage` event, attributed to the
/// coordinator. The usage comes from its usage counters, because its final
/// response reports none. There is no `ContextUsage` event, because its
/// agents report their own as they go. Its items are not copied onto the
/// channel either, because its emitter sends its content there itself.
#[tokio::test]
async fn an_orchestrated_run_closes_with_what_it_billed_alone() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![
            Step::Bill {
                prompt: 5_000,
                completion: 300,
            },
            item(said("The answer.")),
            item(answered("The answer.", 0, 0, None)),
        ])],
    ));

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;
    let events = h.journal(&SessionId::new("s1").unwrap()).await;

    assert_eq!(
        kinds(&events),
        ["started", "session_info", "usage", "finished"]
    );
    let billed = tokens(5_000, 300);
    let usage = agent_event(&events[2]);
    assert_eq!(usage.agent, aura_events::EmittingAgent::coordinator());
    assert!(matches!(
        usage.payload,
        AgentEventPayload::Usage { usage, cache_read_input_tokens: None, .. } if usage == billed
    ));
    assert_eq!(ending_usage(&events[3]), billed);
    assert_eq!(facts.usage, billed);
}

/// `idle` resolves at once when no run is live. Otherwise it resolves once
/// the last live run, on any session, has freed its session.
#[tokio::test]
async fn idle_resolves_once_the_last_live_run_ends() {
    use futures::FutureExt;

    let first_gate = Arc::new(Notify::new());
    let second_gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fake(vec![Step::Wait(Arc::clone(&first_gate))]),
            Prepared::Fake(vec![Step::Wait(Arc::clone(&second_gate))]),
        ],
    ));
    tokio::time::timeout(PATIENCE, h.runtime.idle())
        .await
        .expect("nothing is live");

    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the first run is running", || {
        h.runtime.facts(&first).unwrap().status == RunStatus::Running
    })
    .await;
    let second = h.runtime.start(on_session("s2", "q")).await.unwrap();
    let idle = h.runtime.idle();
    tokio::pin!(idle);
    assert!(idle.as_mut().now_or_never().is_none(), "two runs are live");

    first_gate.notify_one();
    h.ended(first).await;
    assert!(
        idle.as_mut().now_or_never().is_none(),
        "the second run is still live"
    );

    second_gate.notify_one();
    h.ended(second).await;
    tokio::time::timeout(PATIENCE, idle)
        .await
        .expect("idle once both runs have ended");
}

/// A run whose agent cannot be prepared frees its session like any other
/// run. A run that a drain abandons does too. Neither one keeps `idle`
/// waiting. After a drain `idle` resolves whether or not the runs were
/// freed, so the drain's half also checks that no run is counted live.
#[tokio::test(start_paused = true)]
async fn idle_resolves_after_a_failed_preparation_and_a_drain() {
    use futures::FutureExt;

    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fail("no provider".to_owned()),
            Prepared::Fake(vec![Step::Stall]),
        ],
    ));
    let failed = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.ended(failed).await;
    tokio::time::timeout(PATIENCE, h.runtime.idle())
        .await
        .expect("a run that failed to prepare is not live");

    let stalled = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is running", || {
        h.runtime.facts(&stalled).unwrap().status == RunStatus::Running
    })
    .await;
    assert!(h.runtime.idle().now_or_never().is_none(), "the run is live");
    h.runtime.drain(Duration::from_secs(1)).await;
    assert_eq!(
        *h.runtime.shared.live_runs.borrow(),
        0,
        "the abandoned run freed its session"
    );
    tokio::time::timeout(PATIENCE, h.runtime.idle())
        .await
        .expect("a drained runtime has no live run");
}

// ---------------------------------------------------------------------------
// The run's span
// ---------------------------------------------------------------------------

#[cfg(feature = "otel")]
mod spans {
    use opentelemetry::trace::{SpanId, Status};
    use opentelemetry_sdk::export::trace::SpanData;
    use tracing::Instrument;

    use super::*;
    use crate::test_span_capture::CapturedSpans;

    /// The span a caller hands a run. It is a root `agent.stream` span that
    /// declares the `run.id` field for the runtime to record.
    fn agent_stream() -> tracing::Span {
        tracing::info_span!(parent: None, "agent.stream", run.id = tracing::field::Empty)
    }

    /// Waits until the span named `name` is exported, and returns it.
    async fn exported(captured: &CapturedSpans, name: &str) -> SpanData {
        eventually(&format!("`{name}` is exported"), || captured.contains(name)).await;
        captured
            .spans()
            .iter()
            .find(|span| span.name == name)
            .cloned()
            .expect("the span was exported")
    }

    /// The spans in `root`'s trace, as sorted pairs of span name and parent
    /// name.
    fn tree(captured: &CapturedSpans, root: &SpanData) -> Vec<(String, Option<String>)> {
        let spans = captured.spans();
        let names: HashMap<SpanId, &str> = spans
            .iter()
            .map(|span| (span.span_context.span_id(), span.name.as_ref()))
            .collect();
        let mut tree: Vec<_> = spans
            .iter()
            .filter(|span| span.span_context.trace_id() == root.span_context.trace_id())
            .map(|span| {
                (
                    span.name.to_string(),
                    names
                        .get(&span.parent_span_id)
                        .map(|parent| (*parent).to_owned()),
                )
            })
            .collect();
        tree.sort();
        tree
    }

    /// Asserts that the run's `agent.stream` span has the run's id, has
    /// `termination` as its `stream.termination` attribute, and ends with
    /// `status`.
    async fn assert_ends(
        captured: &CapturedSpans,
        run: RunId,
        termination: Option<&str>,
        status: Status,
    ) {
        let span = exported(captured, "agent.stream").await;
        assert_eq!(
            captured.attribute("agent.stream", "run.id"),
            Some(run.to_string())
        );
        assert_eq!(
            captured
                .attribute("agent.stream", "stream.termination")
                .as_deref(),
            termination
        );
        assert_eq!(span.status, status);
    }

    fn running(h: &Harness, run: RunId) -> impl FnMut() -> bool + '_ {
        move || h.runtime.facts(&run).unwrap().status == RunStatus::Running
    }

    /// The span handed to a run stays a root span. Once the agent is
    /// prepared, the span gets the run's id and input, and the agent's own
    /// spans nest under it. It ends with the answer and an ok status. The
    /// agent is prepared under the caller's span, and that span gets none of
    /// the run's attributes.
    #[tokio::test]
    async fn a_run_records_its_input_and_answer_on_the_span_it_was_handed() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Scripted(vec![ScriptedTurn::text("hello")])],
        ));

        let request = tracing::info_span!("chat_completions");
        let start = StartRun {
            history: vec![Message::user("earlier")],
            caller: Some(Caller {
                user: Some("u-1".to_owned()),
                metadata: Some(HashMap::from([("team".to_owned(), "sre".to_owned())])),
            }),
            span: agent_stream(),
            ..on_session("s1", "q")
        };
        let run = h
            .runtime
            .start(start)
            .instrument(request.clone())
            .await
            .unwrap();
        drop(request);
        assert_eq!(h.ended(run).await.status, RunStatus::Finished);
        let stream = exported(&captured, "agent.stream").await;
        let request = exported(&captured, "chat_completions").await;

        assert_eq!(
            stream.parent_span_id,
            SpanId::INVALID,
            "the span stays a root"
        );
        assert_eq!(
            tree(&captured, &stream),
            [
                ("agent.stream".to_owned(), None),
                ("agent.turn".to_owned(), Some("agent.stream".to_owned())),
                ("chat_streaming".to_owned(), Some("agent.turn".to_owned())),
            ]
        );
        let attribute = |key| captured.attribute("agent.stream", key);
        let run_id = Some(run.to_string());
        assert_eq!(attribute("run.id"), run_id);
        assert_eq!(attribute("http.request_id"), run_id);
        assert_eq!(attribute("session.id").as_deref(), Some("s1"));
        for key in ["llm.system", "llm.provider", "llm.model_name"] {
            assert_eq!(attribute(key).as_deref(), Some("scripted"), "{key}");
        }
        assert_eq!(
            attribute(crate::logging::ATTR_AURA_VERSION).as_deref(),
            Some(crate::logging::AURA_VERSION)
        );
        assert_eq!(attribute("aura.mode").as_deref(), Some("single-agent"));
        assert_eq!(attribute("user.id").as_deref(), Some("u-1"));
        assert_eq!(attribute("metadata").as_deref(), Some(r#"{"team":"sre"}"#));
        assert_eq!(
            attribute("llm.invocation_parameters"),
            crate::logging::llm_invocation_parameters(&config("a").agent.llm)
        );
        assert_eq!(attribute("message_count").as_deref(), Some("2"));
        assert_eq!(attribute("input.mime_type").as_deref(), Some("text/plain"));
        assert_eq!(attribute("input.length").as_deref(), Some("1"));
        assert_eq!(attribute("output.mime_type").as_deref(), Some("text/plain"));
        assert_eq!(attribute("output.length").as_deref(), Some("5"));
        // The input and output text is recorded only while content recording
        // is on. That is a process-wide switch, so the test accepts either
        // case, but any text that is recorded must be the run's.
        assert!(matches!(
            attribute("input.value").as_deref(),
            None | Some("q")
        ));
        assert!(matches!(
            attribute("output.value").as_deref(),
            None | Some("hello")
        ));
        assert_eq!(attribute("stream.termination"), None);
        assert_eq!(stream.status, Status::Ok);

        let prepare = exported(&captured, "test.prepare").await;
        assert_eq!(prepare.parent_span_id, request.span_context.span_id());
        assert_ne!(
            request.span_context.trace_id(),
            stream.span_context.trace_id()
        );
        for key in ["run.id", "http.request_id", "session.id", "aura.mode"] {
            assert_eq!(captured.attribute("chat_completions", key), None, "{key}");
        }
    }

    /// A run given `Span::none()` is untraced. The runtime opens no span of
    /// its own and records nothing on the caller's span.
    #[tokio::test]
    async fn an_untraced_run_opens_no_span_of_its_own() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]));

        let request = tracing::info_span!("chat_completions");
        let run = h
            .runtime
            .start(on_session("s1", "q"))
            .instrument(request.clone())
            .await
            .unwrap();
        drop(request);
        assert_eq!(h.ended(run).await.status, RunStatus::Finished);
        let request = exported(&captured, "chat_completions").await;
        let prepare = exported(&captured, "test.prepare").await;

        assert_eq!(prepare.parent_span_id, request.span_context.span_id());
        let mut names: Vec<_> = captured
            .spans()
            .iter()
            .map(|span| span.name.to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["chat_completions", "test.prepare"]);
        for key in ["run.id", "http.request_id", "session.id", "aura.mode"] {
            assert_eq!(captured.attribute("chat_completions", key), None, "{key}");
        }
    }

    /// If a run's agent is never prepared, nothing is recorded on the run's
    /// span: no id, no input and no status.
    #[tokio::test]
    async fn a_run_whose_agent_is_never_prepared_records_nothing_on_its_span() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Fail("no provider".to_owned())],
        ));

        let run = h
            .runtime
            .start(StartRun {
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        assert_eq!(h.ended(run).await.status, RunStatus::Failed);
        let stream = exported(&captured, "agent.stream").await;

        for key in [
            "run.id",
            "http.request_id",
            "aura.mode",
            "stream.termination",
        ] {
            assert_eq!(captured.attribute("agent.stream", key), None, "{key}");
        }
        assert_eq!(stream.status, Status::Unset);
    }

    /// An empty `user` or `metadata` is not recorded as an attribute.
    #[tokio::test]
    async fn an_empty_user_or_metadata_records_nothing() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]));

        let run = h
            .runtime
            .start(StartRun {
                caller: Some(Caller {
                    user: Some(String::new()),
                    metadata: Some(HashMap::new()),
                }),
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        h.ended(run).await;
        exported(&captured, "agent.stream").await;

        assert_eq!(captured.attribute("agent.stream", "user.id"), None);
        assert_eq!(captured.attribute("agent.stream", "metadata"), None);
        assert_eq!(
            captured.attribute("agent.stream", "aura.mode").as_deref(),
            Some("single-agent")
        );
    }

    /// A failed run's span ends with an error status holding the failure's
    /// text, and records no answer.
    #[tokio::test]
    async fn a_failed_run_records_its_error() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Scripted(Vec::new())],
        ));

        let run = h
            .runtime
            .start(StartRun {
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        assert_eq!(h.ended(run).await.status, RunStatus::Failed);
        let LifecycleEvent::Failed { error, .. } =
            last_lifecycle(&h.journal(&SessionId::new("s1").unwrap()).await).clone()
        else {
            panic!("the run failed");
        };

        assert_ends(
            &captured,
            run,
            None,
            Status::error(crate::logging::truncate_for_otel(&error)),
        )
        .await;
        assert_eq!(captured.attribute("agent.stream", "output.length"), None);
    }

    /// A run whose task panics still records its failure on its span.
    #[tokio::test]
    async fn a_panicking_run_records_its_failure() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(vec![Step::Panic])],
        ));

        let run = h
            .runtime
            .start(StartRun {
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        assert_eq!(h.ended(run).await.status, RunStatus::Failed);

        assert_ends(
            &captured,
            run,
            None,
            Status::error("the run's task panicked: the fake agent panicked"),
        )
        .await;
    }

    /// A run its caller cancels records `stream.termination` as `cancelled`,
    /// with no error status.
    #[tokio::test]
    async fn a_cancelled_run_records_cancelled() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))])],
        ));

        let run = h
            .runtime
            .start(StartRun {
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        eventually("the run is running", running(&h, run)).await;
        assert!(h.runtime.cancel(&run, RunCancelReason::External, None));
        assert_eq!(h.ended(run).await.status, RunStatus::Cancelled);

        assert_ends(&captured, run, Some("cancelled"), Status::Unset).await;
    }

    /// A run past its timeout ends with the `timeout` error status.
    #[tokio::test]
    async fn a_run_past_its_deadline_records_a_timeout() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))])],
        ));

        let run = h
            .runtime
            .start(StartRun {
                timeout: Some(Duration::from_millis(20)),
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        assert_eq!(h.ended(run).await.status, RunStatus::Cancelled);

        assert_ends(&captured, run, None, Status::error("timeout")).await;
    }

    /// A run a drain stops records `stream.termination` as `shutdown`, with
    /// no error status.
    #[tokio::test]
    async fn a_drained_run_records_shutdown() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))])],
        ));

        let run = h
            .runtime
            .start(StartRun {
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        eventually("the run is running", running(&h, run)).await;
        h.runtime.drain(PATIENCE).await;

        assert_ends(&captured, run, Some("shutdown"), Status::Unset).await;
    }

    /// A run whose last claimant leaves records `stream.termination` as
    /// `disconnected`, with no error status.
    #[tokio::test]
    async fn a_run_whose_claimant_leaves_records_disconnected() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))])],
        ));
        let session = SessionId::new("s1").unwrap();

        let claim = h
            .runtime
            .attach(&session, Attach::claiming())
            .await
            .unwrap();
        let run = h
            .runtime
            .start(StartRun {
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        eventually("the run is running", running(&h, run)).await;
        claim.detach().await;
        assert_eq!(h.ended(run).await.status, RunStatus::Cancelled);

        assert_ends(&captured, run, Some("disconnected"), Status::Unset).await;
    }

    /// A run that parks records `stream.termination` as `parked`, with no
    /// error status.
    #[tokio::test]
    async fn a_parked_run_records_parked() {
        let captured = CapturedSpans::default();
        let _capturing = captured.install();
        let h = harness(TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(vec![Step::Emit(Box::new(
                AgentEventPayload::RunParked {
                    run_id: "ignored".to_owned(),
                    decision_ids: vec![DecisionId::generate().to_string()],
                    expires_at: "2026-10-21T00:00:00Z".to_owned(),
                    iteration: 1,
                },
            ))])],
        ));

        let run = h
            .runtime
            .start(StartRun {
                span: agent_stream(),
                ..on_session("s1", "q")
            })
            .await
            .unwrap();
        assert_eq!(h.ended(run).await.status, RunStatus::Parked);

        assert_ends(&captured, run, Some("parked"), Status::Unset).await;
    }
}
