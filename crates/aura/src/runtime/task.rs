//! The task that executes one run: it prepares the session's agent, begins and
//! streams the run, appends what the run emits to the session's journal, and
//! records how the run ended.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use aura_config::Config;
use aura_events::agent::{AgentEvent, AgentEventPayload};
use aura_events::run::{
    CheckpointRef, LifecycleEvent, RunCancelReason, SessionEvent, SessionEventPayload,
};
use aura_events::{EmittingAgent, RunId, TokenCount, TokenUsage};
use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::hitl::DecisionId;
use crate::journal::SessionJournal;
use crate::passthrough_tool::PASSTHROUGH_MARKER;
use crate::provider_agent::{StreamError, StreamItem, StreamedUserContent};
use crate::session_store::{RunFacts, RunStatus, SessionStoreError, SkillLogKey};
use crate::skill_tool::SkillInvocationRecorder;
use crate::streaming::{RunOptions, StreamingAgent};

use super::Shared;
use super::StartRun;
use super::prepare::{PrepareRequest, PreparedForRun};
use super::record::{RunRecord, SessionRecord};

/// How long a stopped run's stream is given to end on its own.
pub(super) const STOP_GRACE: Duration = Duration::from_secs(5);

/// How a run's execution came out, before its terminal event is written.
#[derive(Default)]
struct Outcome {
    /// The agent that ran.
    agent: Option<Arc<dyn StreamingAgent>>,
    /// Why the run failed.
    error: Option<String>,
    /// The journal append that failed.
    journal_error: Option<SessionStoreError>,
    /// The approvals a parked run is waiting on.
    parked: Option<Vec<DecisionId>>,
    /// Whether the run yielded to a tool its client executes.
    yielded: bool,
}

impl Outcome {
    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }
}

/// Executes the run `record` on `session` and records how it ended.
///
/// A panic anywhere in the run fails it like any other error, so the session
/// is freed and the ending written. A panic while writing the ending frees
/// the session without one, and the run is retired like any ended run.
pub(super) async fn run(
    shared: Arc<Shared>,
    session: Arc<SessionRecord>,
    record: Arc<RunRecord>,
    config: Config,
    start: StartRun,
) {
    let outcome = match AssertUnwindSafe(execute(&shared, &session, &record, &config, start))
        .catch_unwind()
        .await
    {
        Ok(outcome) => outcome,
        Err(panic) => {
            let error = format!("the run's task panicked: {}", panic_message(&*panic));
            tracing::error!(run_id = %record.id, "{error}");
            Outcome::failed(error)
        }
    };
    match AssertUnwindSafe(end(&shared, &session, &record, outcome))
        .catch_unwind()
        .await
    {
        Ok(true) => shared.retire(&session, &record).await,
        Ok(false) => {}
        Err(panic) => {
            tracing::error!(run_id = %record.id, "ending the run panicked: {}", panic_message(&*panic));
            fail_without_ending(&session, &record);
            shared.retire(&session, &record).await;
        }
    }
}

/// What a panic was raised with, when that was a message.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

/// Frees the session from a run whose ending could not be written, and marks
/// the run failed in this process. Nothing reaches the journal or the store.
/// The run counts as retained, as a run whose ending was written does, so
/// `retire` lets it go the same way; a run `release` already freed is
/// counted once.
fn fail_without_ending(session: &SessionRecord, record: &RunRecord) {
    let mut state = session.lock();
    if state.live.as_ref().is_some_and(|live| live.id == record.id) {
        state.live = None;
        state.retained += 1;
    }
    state.last_run = Some((record.id, RunStatus::Failed));
    record.update(|facts| {
        if !facts.status.is_terminal() {
            facts.status = RunStatus::Failed;
            facts.ended_at = Some(aura_events::Timestamp::now());
        }
    });
}

/// When a run's bound runs out, and how long the bound was.
#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    after: Duration,
}

/// Writes the run to the run store, prepares the agent, begins and streams
/// the run, and appends `Started`, the agent's session info and MCP status,
/// and every event the run emits.
async fn execute(
    shared: &Shared,
    session: &SessionRecord,
    record: &RunRecord,
    config: &Config,
    start: StartRun,
) -> Outcome {
    let StartRun {
        req_headers,
        client_tools,
        run_tools,
        continues,
        prompt,
        mut history,
        timeout,
        liveness,
        ..
    } = start;
    let agent_id = config.agent_id().to_owned();

    // The store has the run from the start, as `Preparing`.
    put_facts(shared, record).await;

    // Skill invocations record under the session and the agent that served
    // them, anchored after the history and this run's prompt, so a later run
    // of the same agent rehydrates them.
    let skill_log = SkillLogKey::new(session.id.clone(), agent_id.clone());
    let skill_recorder = (!config.agent.skills.local.is_empty()).then(|| {
        Arc::new(SkillInvocationRecorder::new(
            Arc::clone(&shared.skill_store),
            skill_log.clone(),
            history.len() as u32 + 1,
        ))
    });

    // A run stopped while its agent is prepared ends without starting.
    let prepared = tokio::select! {
        biased;
        () = record.cancel.cancelled() => return Outcome::default(),
        prepared = shared.preparer.prepare(
            config,
            PrepareRequest {
                session: &session.id,
                run_id: record.id,
                req_headers: req_headers.as_ref(),
                client_tools,
                run_tools,
                skill_recorder: skill_recorder.clone(),
            },
        ) => prepared,
    };

    let (agent, attribution): (Arc<dyn StreamingAgent>, EmittingAgent) = match prepared {
        Err(error) => {
            return Outcome::failed(format!("preparing agent `{agent_id}` failed: {error}"));
        }
        Ok(PreparedForRun::Prepared(prepared)) => {
            match prepared.begin_run(record.id, req_headers.as_ref(), skill_recorder.clone()) {
                Ok(agent) => (Arc::new(agent), EmittingAgent::single_agent()),
                Err(error) => return Outcome::failed(format!("beginning the run failed: {error}")),
            }
        }
        Ok(PreparedForRun::Built(agent)) => (agent, EmittingAgent::coordinator()),
    };

    // A store read failure costs only continuity; the run goes ahead.
    if skill_recorder.is_some() && !history.is_empty() {
        match shared.skill_store.list(&skill_log).await {
            Ok(records) => {
                let rehydrated = crate::skill_rehydration::rehydrate_chat_history(
                    &mut history,
                    records,
                    agent.skills(),
                )
                .await;
                if !rehydrated.is_empty() {
                    tracing::debug!(run_id = %record.id, skills = ?rehydrated, "rehydrated skill invocations");
                }
            }
            Err(error) => {
                tracing::warn!(run_id = %record.id, %error, "skipping skill rehydration: the skill store could not be read");
            }
        }
    }

    if record.cancel.is_cancelled() {
        return Outcome {
            agent: Some(agent),
            ..Outcome::default()
        };
    }

    // The bound starts here, so preparing the agent does not count against
    // it, and the run's own token stops when the record's does. The agent
    // gets the bound too, since an orchestrated run plans within it.
    let deadline = timeout.map(|after| Deadline {
        at: Instant::now() + after,
        after,
    });
    let mut run = agent
        .stream(
            &prompt,
            history,
            RunOptions::bounded(timeout).cancelled_by(&record.cancel),
            &record.id.to_string(),
        )
        .await;
    record.track_usage(run.usage().clone());
    let events = run.take_agent_events();
    let items = run.into_events();

    let journal = &session.journal;
    let started = LifecycleEvent::Started {
        agent: agent_id,
        prompt,
        timeout,
        liveness,
        continues,
    };
    let started = match journal
        .append(Some(record.id), SessionEventPayload::Lifecycle(started))
        .await
    {
        Ok(started) => started,
        Err(journal_error) => {
            return Outcome {
                agent: Some(agent),
                journal_error: Some(journal_error),
                ..Outcome::default()
            };
        }
    };
    record.update(|facts| {
        facts.status = RunStatus::Running;
        facts.first_seq = Some(started.seq);
        facts.latest_seq = Some(started.seq);
    });
    put_facts(shared, record).await;

    let (provider, model) = agent.get_provider_info();
    let mut opening = vec![AgentEventPayload::SessionInfo {
        model: format!("{provider}/{model}"),
        model_context_limit: agent.context_window().map(TokenCount::new),
    }];
    let servers = agent.mcp_server_status();
    if !servers.is_empty() {
        opening.push(AgentEventPayload::McpStatus { servers });
    }
    for payload in opening {
        let event = AgentEvent::new(attribution.clone(), payload);
        if let Err(journal_error) = append_agent_event(journal, record, event).await {
            return Outcome {
                agent: Some(agent),
                journal_error: Some(journal_error),
                ..Outcome::default()
            };
        }
    }

    let mut outcome = drive(
        shared,
        journal,
        record,
        agent.as_ref(),
        events,
        items,
        deadline,
    )
    .await;
    outcome.agent = Some(agent);
    outcome
}

/// Polls the run's stream to its end, which is what executes the run, and
/// appends every event the run emits, in the order emitted.
///
/// Content reaches the run's events as it streams, so an item is read only
/// for an error, and for the result of a tool the client executes: that
/// result is the marker the passthrough tool returns, and the agent's own
/// hook then ends the stream with an error so the client can run the tool,
/// which makes the run a yield rather than a failure. The run's channel
/// stays open for as long as its prepared agent holds the run, so the
/// receiver is closed once the stream ends and what it still buffers is
/// appended before this returns. Once the stream has ended, how the run ends
/// is decided, so the run is closed to stops.
///
/// At `deadline` the run is stopped for it. A single agent also checks the
/// bound itself, between turns, and ends its stream with an error; that
/// error at or past the deadline is the deadline too, so both kinds of agent
/// end the same way.
///
/// Once the run is stopped, its MCP calls in flight are cancelled on their
/// servers. A single agent waits on a tool call it is making, so this is
/// what lets a run stopped mid-call end. A single agent sees the stop only
/// when its provider sends something, so a stopped run whose stream has not
/// ended within [`STOP_GRACE`] has its stream dropped, which drops the
/// provider request with it.
async fn drive(
    shared: &Shared,
    journal: &SessionJournal,
    record: &RunRecord,
    agent: &dyn StreamingAgent,
    mut events: Option<mpsc::Receiver<AgentEvent>>,
    items: BoxStream<'static, Result<StreamItem, StreamError>>,
    deadline: Option<Deadline>,
) -> Outcome {
    let mut outcome = Outcome::default();
    let mut approvals = Approvals::default();
    let mut items = Some(items);
    let mut deadline_armed = deadline.is_some();
    let mut stop_seen = false;
    let expiry = async move {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline.at).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(expiry);
    let stop_grace = tokio::time::sleep(Duration::MAX);
    tokio::pin!(stop_grace);

    loop {
        tokio::select! {
            biased;
            event = next_event(&mut events), if events.is_some() => {
                let Some(event) = event else {
                    events = None;
                    continue;
                };
                if outcome.journal_error.is_some() {
                    continue;
                }
                let pending_changed = approvals.observe(record.id, &event.payload);
                if let Err(journal_error) = append_agent_event(journal, record, event).await {
                    // The run's events can no longer be kept, so it stops.
                    tracing::warn!(run_id = %record.id, %journal_error, "the session journal refused the run's event; stopping the run");
                    outcome.journal_error = Some(journal_error);
                    record.cancel.cancel();
                    close(&mut events);
                    continue;
                }
                if pending_changed {
                    let pending = approvals.pending.clone();
                    record.update(|facts| facts.pending_approvals = pending);
                    put_facts(shared, record).await;
                }
            }
            () = record.cancel.cancelled(), if !stop_seen && items.is_some() => {
                stop_seen = true;
                let label = record
                    .stopped()
                    .map_or("run cancelled", |(reason, _)| stop_label(&reason));
                agent
                    .cancel_and_close_mcp(&record.id.to_string(), label)
                    .await;
                stop_grace.as_mut().reset(Instant::now() + STOP_GRACE);
            }
            () = &mut stop_grace, if stop_seen && items.is_some() => {
                tracing::warn!(run_id = %record.id, grace = ?STOP_GRACE, "the stopped run's stream did not end within the grace; dropping it");
                items = None;
                close(&mut events);
            }
            () = &mut expiry, if deadline_armed && items.is_some() => {
                deadline_armed = false;
                if let Some(deadline) = deadline {
                    record.stop(RunCancelReason::Deadline { after: deadline.after }, None);
                }
            }
            item = next_item(&mut items), if items.is_some() => match item {
                None => {
                    items = None;
                    close(&mut events);
                    if let Some(deadline) = deadline
                        && outcome.error.is_some()
                        && Instant::now() >= deadline.at
                    {
                        record.stop(RunCancelReason::Deadline { after: deadline.after }, None);
                    }
                    record.close_to_stops();
                }
                // The first error is what the run failed with; later ones
                // follow from it.
                Some(Err(error)) => {
                    outcome.error.get_or_insert_with(|| error.to_string());
                }
                Some(Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(result))))
                    if result.result.contains(PASSTHROUGH_MARKER) =>
                {
                    outcome.yielded = true;
                }
                Some(Ok(_)) => {}
            },
            else => break,
        }
    }

    outcome.parked = approvals.parked;
    outcome
}

/// The next event on the run's channel, for a `select!` branch that is
/// disabled once the channel is gone.
async fn next_event(events: &mut Option<mpsc::Receiver<AgentEvent>>) -> Option<AgentEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// The next item on the run's stream, for a `select!` branch that is
/// disabled once the stream is gone.
async fn next_item(
    items: &mut Option<BoxStream<'static, Result<StreamItem, StreamError>>>,
) -> Option<Result<StreamItem, StreamError>> {
    match items {
        Some(items) => items.next().await,
        None => std::future::pending().await,
    }
}

fn close(events: &mut Option<mpsc::Receiver<AgentEvent>>) {
    if let Some(events) = events {
        events.close();
    }
}

async fn append_agent_event(
    journal: &SessionJournal,
    record: &RunRecord,
    event: AgentEvent,
) -> Result<Arc<SessionEvent>, SessionStoreError> {
    let stamped = journal
        .append(Some(record.id), SessionEventPayload::Agent(event))
        .await?;
    record.update(|facts| facts.latest_seq = Some(stamped.seq));
    Ok(stamped)
}

/// Writes the run's facts to the run store. The record stays the source for
/// this process, so a store failure is logged and the run goes on.
async fn put_facts(shared: &Shared, record: &RunRecord) {
    if let Err(error) = shared.run_store.put(&record.facts()).await {
        tracing::warn!(run_id = %record.id, %error, "the run store refused the run's facts");
    }
}

/// The approvals a run is waiting on.
#[derive(Default)]
struct Approvals {
    pending: Vec<DecisionId>,
    parked: Option<Vec<DecisionId>>,
}

impl Approvals {
    /// Folds one event in, and says whether the pending set changed.
    fn observe(&mut self, run: RunId, payload: &AgentEventPayload) -> bool {
        match payload {
            AgentEventPayload::ApprovalRequested(requested) => {
                match decision_id(run, &requested.decision_id) {
                    Some(id) if !self.pending.contains(&id) => {
                        self.pending.push(id);
                        true
                    }
                    _ => false,
                }
            }
            AgentEventPayload::ApprovalCompleted(completed) => {
                let before = self.pending.len();
                if let Some(id) = decision_id(run, &completed.decision_id) {
                    self.pending.retain(|pending| *pending != id);
                }
                self.pending.len() != before
            }
            AgentEventPayload::RunParked { decision_ids, .. } => {
                self.parked = Some(
                    decision_ids
                        .iter()
                        .filter_map(|id| decision_id(run, id))
                        .collect(),
                );
                false
            }
            _ => false,
        }
    }
}

fn decision_id(run: RunId, raw: &str) -> Option<DecisionId> {
    DecisionId::parse(raw)
        .inspect_err(|error| {
            tracing::warn!(run_id = %run, decision_id = raw, %error, "an approval event carries a decision id that is not a UUID; ignoring it");
        })
        .ok()
}

/// Appends the run's terminal event, writes its final facts, releases what it
/// held, and frees the session for its next run. Returns `false` without
/// doing any of it when shutdown abandoned the run first, since the drain
/// writes that run's ending.
///
/// Precedence decides the ending: a journal that refused the run's events
/// fails it, then a stop cancels it with the stop's reason, then a yield to a
/// client tool finishes it, then an error from its stream fails it, then a
/// park, and otherwise it finished. A stop comes before the error because
/// the error a stopped run's stream ends with is the stop's doing, and the
/// yield comes before it for the same reason.
async fn end(
    shared: &Shared,
    session: &SessionRecord,
    record: &RunRecord,
    outcome: Outcome,
) -> bool {
    if !record.begin_ending() {
        return false;
    }
    record.close_to_stops();
    let Outcome {
        agent,
        error,
        journal_error,
        parked,
        yielded,
    } = outcome;
    let usage = record.usage();
    let run_key = record.id.to_string();

    let (terminal, status) = if let Some(journal_error) = &journal_error {
        failed(
            format!("the session journal refused the run's events: {journal_error}"),
            usage,
        )
    } else if let Some((reason, message)) = record.stopped() {
        cancelled(reason, message, usage)
    } else if yielded {
        (LifecycleEvent::Finished { usage }, RunStatus::Finished)
    } else if let Some(error) = error {
        failed(error, usage)
    } else if parked.is_some() {
        let checkpoint = CheckpointRef::new(crate::orchestration::run_owner_id(&run_key))
            .expect("a run's owner id starts with \"run:\", so it is not empty");
        (
            LifecycleEvent::Parked { checkpoint, usage },
            RunStatus::Parked,
        )
    } else {
        (LifecycleEvent::Finished { usage }, RunStatus::Finished)
    };
    let facts = write_ending(shared, session, record, terminal, status, parked, usage).await;
    release(shared, session, record, facts, true).await;
    // The agent goes with its run. For a single agent that is the prepared
    // agent too, and its MCP connections close with it.
    drop(agent);
    true
}

/// Ends a run whose task shutdown aborted before the run stopped: it is
/// cancelled for shutdown, as it would have been had it stopped in time.
pub(super) async fn end_abandoned(shared: &Shared, session: &SessionRecord, record: &RunRecord) {
    record.close_to_stops();
    let usage = record.usage();
    let (terminal, status) = cancelled(
        RunCancelReason::Shutdown,
        Some("the run did not stop within the shutdown grace period".to_owned()),
        usage,
    );
    let facts = write_ending(shared, session, record, terminal, status, None, usage).await;
    release(shared, session, record, facts, false).await;
}

/// Appends `terminal` to the session's journal and writes the run's final
/// facts to the run store, and returns those facts. `waiting` is what a
/// parked run waits on; `usage` is what the run cost, as `terminal` carries
/// it.
async fn write_ending(
    shared: &Shared,
    session: &SessionRecord,
    record: &RunRecord,
    terminal: LifecycleEvent,
    status: RunStatus,
    waiting: Option<Vec<DecisionId>>,
    usage: TokenUsage,
) -> RunFacts {
    let checkpoint = match &terminal {
        LifecycleEvent::Parked { checkpoint, .. } => Some(checkpoint.clone()),
        _ => None,
    };
    let stamped = session
        .journal
        .append(Some(record.id), SessionEventPayload::Lifecycle(terminal))
        .await
        .inspect_err(|error| {
            tracing::warn!(run_id = %record.id, %error, "the session journal refused the run's terminal event");
        })
        .ok();

    let mut facts = record.facts();
    facts.status = status;
    facts.ended_at = Some(aura_events::Timestamp::now());
    facts.usage = usage;
    facts.pending_approvals = waiting.unwrap_or_default();
    facts.checkpoint = checkpoint;
    if let Some(stamped) = &stamped {
        facts.latest_seq = Some(stamped.seq);
    }
    let written = match &stamped {
        Some(stamped) => shared.run_store.finish(stamped, &facts).await,
        None => shared.run_store.put(&facts).await,
    };
    if let Err(error) = written {
        tracing::warn!(run_id = %record.id, %error, "the run store refused the run's final facts");
    }
    facts
}

/// Cancels the approvals the run left parked under its id, as a request's
/// are when the request ends, and frees the session: the run is no longer
/// live and is the session's last run. The run's final `facts` are published
/// while the session is held, so a reader finds the session free and the
/// run ended together, never one without the other. `retained` counts the
/// run as held for the retention window.
async fn release(
    shared: &Shared,
    session: &SessionRecord,
    record: &RunRecord,
    facts: RunFacts,
    retained: bool,
) {
    shared
        .pending_approvals
        .cancel_request(&record.id.to_string())
        .await;

    let mut state = session.lock();
    if state.live.as_ref().is_some_and(|live| live.id == record.id) {
        state.live = None;
    }
    state.last_run = Some((record.id, facts.status));
    if retained {
        state.retained += 1;
    }
    record.update(|current| *current = facts);
}

fn failed(error: String, usage: TokenUsage) -> (LifecycleEvent, RunStatus) {
    (LifecycleEvent::Failed { error, usage }, RunStatus::Failed)
}

fn cancelled(
    reason: RunCancelReason,
    message: Option<String>,
    usage: TokenUsage,
) -> (LifecycleEvent, RunStatus) {
    (
        LifecycleEvent::Cancelled {
            reason,
            message,
            usage,
        },
        RunStatus::Cancelled,
    )
}

/// What MCP servers are told about a call a stopped run leaves in flight.
fn stop_label(reason: &RunCancelReason) -> &'static str {
    match reason {
        RunCancelReason::Deadline { .. } => "run timed out",
        RunCancelReason::Shutdown => "server shutting down",
        RunCancelReason::Unclaimed => "run unclaimed",
        _ => "run cancelled",
    }
}
