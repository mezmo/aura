//! The task that executes one run. It prepares the agent, streams the run,
//! appends everything the run emits to the session's journal, and records
//! how the run ended.

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
use tracing::Instrument;

use crate::hitl::DecisionId;
use crate::journal::SessionJournal;
use crate::passthrough_tool::PASSTHROUGH_MARKER;
use crate::provider_agent::{FinalResponseInfo, StreamError, StreamItem, StreamedUserContent};
use crate::session_store::{RunFacts, RunStatus, SessionStoreError, SkillLogKey};
use crate::skill_tool::SkillInvocationRecorder;
use crate::streaming::{RunOptions, StreamingAgent, tee_run_events};
use crate::streaming_request_hook::{BilledUsage, UsageState, resolve_billed_usage};

use super::otel::{RunMode, RunSpan};
use super::prepare::{PrepareRequest, PreparedForRun};
use super::record::{RunRecord, SessionRecord};
use super::{Caller, Shared, StartRun};

/// How long a stopped run's stream is given to end on its own.
pub(super) const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long a stopped run waits for its MCP calls to be cancelled and its
/// connections closed.
pub(super) const MCP_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How a run's execution came out, before its terminal event is written.
#[derive(Default)]
struct Outcome {
    /// The agent that ran.
    agent: Option<Arc<dyn StreamingAgent>>,
    /// The agent the run's events are attributed to.
    started: Option<EmittingAgent>,
    /// The error the run's stream failed with.
    error: Option<String>,
    /// The journal append that failed.
    journal_error: Option<SessionStoreError>,
    /// The approvals a parked run is waiting on.
    parked: Option<Vec<DecisionId>>,
    /// Whether the run yielded to a tool its client executes.
    yielded: bool,
    /// The run's final response.
    response: Option<FinalResponseInfo>,
}

impl Outcome {
    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }
}

/// Executes the run `record` on `session` (see [`execute`] for how the agent
/// is prepared and streamed under its spans) and records how it ended.
///
/// When the agent was prepared, the ending is written inside the run's span,
/// `start.span`, and recorded on it. Otherwise nothing is recorded there.
/// After the run's span closes, the tracer is flushed, and only then is the
/// run held for the retention window.
///
/// If executing the run panics, the run fails as it would on any other
/// error: its ending is written and the session freed. If writing the ending
/// panics before the session is freed, it is freed without an ending, and
/// the run is retired like any other ended run.
pub(super) async fn run(
    shared: Arc<Shared>,
    session: Arc<SessionRecord>,
    record: Arc<RunRecord>,
    config: Config,
    start: StartRun,
    preparing: tracing::Span,
) {
    let span = start.span.clone();
    let mut traced = None;
    let execution = execute(
        &shared,
        &session,
        &record,
        &config,
        start,
        preparing,
        &mut traced,
    );
    let outcome = match AssertUnwindSafe(execution).catch_unwind().await {
        Ok(outcome) => outcome,
        Err(panic) => {
            let error = format!("the run's task panicked: {}", panic_message(&*panic));
            tracing::error!(run_id = %record.id, "{error}");
            Outcome::failed(error)
        }
    };
    let traced = traced.as_ref().map(|attributes| (attributes, &span));
    let ending = AssertUnwindSafe(end(&shared, &session, &record, outcome, traced)).catch_unwind();
    let ended = match traced {
        Some(_) => ending.instrument(span.clone()).await,
        None => ending.await,
    };
    // Close the span here, not after the retention window, so the flush
    // below exports it.
    drop(span);
    crate::logging::flush_tracer().await;
    match ended {
        Ok(true) => shared.retire(&session, &record).await,
        Ok(false) => {}
        Err(panic) => {
            tracing::error!(run_id = %record.id, "ending the run panicked: {}", panic_message(&*panic));
            fail_without_ending(&shared, &session, &record);
            record.settle();
            shared.retire(&session, &record).await;
        }
    }
}

/// The panic's message, or "no message" when its payload is not a string.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

/// Frees the session from a run whose ending panicked before `release` freed
/// it, and marks the run failed in memory only. Nothing is written to the
/// journal or the run store. The run counts as retained, like any other ended
/// run, so `retire` lets it go the same way.
///
/// A panic after `release` changes nothing: the run already has the ending
/// it reached, and `release` already counted it.
fn fail_without_ending(shared: &Shared, session: &SessionRecord, record: &RunRecord) {
    let mut state = session.lock();
    if !free(shared, &mut state, record) {
        return;
    }
    state.retained += 1;
    state.last_run = Some((record.id, RunStatus::Failed));
    record.update(|facts| {
        facts.status = RunStatus::Failed;
        facts.ended_at = Some(aura_events::Timestamp::now());
    });
    record.unbind_context();
}

/// If `record` is the session's live run, clears it from the session and
/// takes one off the runtime's count of live runs. Returns whether `record`
/// was the live run.
fn free(shared: &Shared, state: &mut super::record::SessionState, record: &RunRecord) -> bool {
    if state.live.as_ref().is_none_or(|live| live.id != record.id) {
        return false;
    }
    state.live = None;
    shared
        .live_runs
        .send_modify(|live| *live = live.saturating_sub(1));
    true
}

/// When the run's timeout expires, and the timeout itself.
#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    after: Duration,
}

/// Writes the run to the run store, prepares the agent, and streams the run.
/// Appends `Started`, then the agent's session info, the skills rehydrated
/// into its history and its MCP status, then every event the run emits. For
/// a single agent, the item stream also passes through `tee_run_events`,
/// which copies the agent's content and tool results onto the run's event
/// channel. Returns how the run came out. [`end`] writes the ending.
///
/// Preparing the agent runs inside the `preparing` span, so what it logs
/// belongs to the caller's trace. Once the agent is prepared, the run's span
/// records the run's id and its input, and is entered for the rest of the
/// run. The caller has to declare the `run.id` field when it creates the
/// span, because a span records only the fields it declares. `traced` is
/// set to the attributes the span records.
async fn execute(
    shared: &Shared,
    session: &SessionRecord,
    record: &RunRecord,
    config: &Config,
    start: StartRun,
    preparing: tracing::Span,
    traced: &mut Option<RunSpan>,
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
        caller,
        span,
        ..
    } = start;
    let agent_id = config.agent_id().to_owned();
    // The agent, the skill log and the span see the chat id, not the lane's
    // session id, so all lanes of a conversation share them. The journal,
    // the run store and the claim stay on the lane's session.
    let landing = record.facts().landing;
    let chat = landing.lane().map_or_else(
        || session.id.clone(),
        |lane| lane.conversation().chat().clone(),
    );

    let ready: Result<_, Outcome> = async {
        // Write the run as `Preparing` before preparing the agent, so the
        // store lists it from the start.
        put_facts(shared, record).await;

        // A later run of the same agent rehydrates the skill invocations
        // logged here. Each one is anchored at this turn's position: after
        // the history and the prompt.
        let skill_log = SkillLogKey::new(chat.clone(), agent_id.clone());
        let skill_recorder = (!config.agent.skills.local.is_empty()).then(|| {
            Arc::new(SkillInvocationRecorder::new(
                Arc::clone(&shared.skill_store),
                skill_log.clone(),
                history.len() as u32 + 1,
            ))
        });

        // A run stopped while its agent is being prepared ends without a
        // `Started` event.
        let prepared = tokio::select! {
            biased;
            () = record.cancel.cancelled() => return Err(Outcome::default()),
            prepared = shared.preparer.prepare(
                config,
                PrepareRequest {
                    session: &chat,
                    run_id: record.id,
                    req_headers: req_headers.as_ref(),
                    client_tools,
                    run_tools,
                    skill_recorder: skill_recorder.clone(),
                },
            ) => prepared,
        };

        // Only a prepared single agent has MCP tool schemas for the span.
        let (agent, attribution, tools_json): (Arc<dyn StreamingAgent>, EmittingAgent, _) =
            match prepared {
                Err(error) => {
                    return Err(Outcome::failed(format!(
                        "preparing agent `{agent_id}` failed: {error}"
                    )));
                }
                Ok(PreparedForRun::Prepared(prepared)) => {
                    match prepared.begin_run(
                        record.id,
                        req_headers.as_ref(),
                        skill_recorder.clone(),
                    ) {
                        Ok(agent) => (
                            Arc::new(agent),
                            EmittingAgent::single_agent(),
                            prepared.otel_llm_tools(),
                        ),
                        Err(error) => {
                            return Err(Outcome::failed(format!(
                                "beginning the run failed: {error}"
                            )));
                        }
                    }
                }
                Ok(PreparedForRun::Built { agent, attribution }) => {
                    (agent, attribution, Vec::new())
                }
            };

        // If the skill store cannot be read, the run goes ahead without the
        // earlier skill invocations.
        let mut rehydrated = Vec::new();
        if skill_recorder.is_some() && !history.is_empty() {
            match shared.skill_store.list(&skill_log).await {
                Ok(records) => {
                    rehydrated = crate::skill_rehydration::rehydrate_chat_history(
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
        Ok((agent, attribution, tools_json, rehydrated))
    }
    .instrument(preparing)
    .await;
    let (agent, attribution, tools_json, rehydrated) = match ready {
        Ok(ready) => ready,
        Err(outcome) => return outcome,
    };

    // An empty `user` or `metadata` is left off the span, as the
    // request-bound path leaves it off. The span's mode comes from the
    // config, as it does on that path.
    let Caller { user, metadata } = caller.unwrap_or_default();
    let (provider, model) = agent.get_provider_info();
    let attributes = RunSpan {
        provider: provider.to_owned(),
        model: model.to_owned(),
        run: record.id,
        session: chat,
        lane: landing.lane().map(|lane| lane.number()),
        prompt: prompt.clone(),
        user: user.filter(|user| !user.is_empty()),
        metadata_json: metadata
            .filter(|metadata| !metadata.is_empty())
            .and_then(|metadata| serde_json::to_string(&metadata).ok()),
        invocation_parameters: crate::logging::llm_invocation_parameters(&config.agent.llm),
        history_len: history.len(),
        system_prompt: agent.system_prompt().map(str::to_owned),
        mode: if config.orchestration_enabled() {
            RunMode::Orchestration
        } else {
            RunMode::SingleAgent { tools_json }
        },
    };
    span.record("run.id", tracing::field::display(record.id));
    attributes.record_input(&span);
    *traced = Some(attributes);

    async {
        // An observer attach or detach still being written goes on the
        // stream first, so `Started` follows it.
        session.observer_writes_done().await;
        // Stopped after the agent was prepared, or while it waited for that
        // write: end without a `Started` event and without opening the
        // stream. `Registered`'s `Drop` relies on this when a claimed start's
        // attach fails.
        if record.cancel.is_cancelled() {
            return Outcome {
                agent: Some(agent),
                ..Outcome::default()
            };
        }

        // The timeout starts now, so preparing the agent does not count
        // against it. The agent is given the timeout too, because an
        // orchestrator plans within it. Its token is a child of the
        // record's, so stopping the record stops the run.
        let deadline = timeout.map(|after| Deadline {
            at: Instant::now() + after,
            after,
        });
        // The run starts with the session's current attendance. Once the
        // agent hands over its context, attaches and detaches keep it
        // current (see `RunRecord::mirror`).
        let attendance = session.lock().observers.attendance();
        let mut run = agent
            .stream(
                &prompt,
                history,
                RunOptions::bounded(timeout)
                    .cancelled_by(&record.cancel)
                    .attended(attendance),
                &record.id.to_string(),
            )
            .await;
        record.track_usage(run.usage().clone());
        let context = run.run_context().cloned();
        if let Some(context) = &context {
            // Under the session lock, so no attach or detach lands between
            // reading the attendance and binding the context.
            let state = session.lock();
            record.bind_context(Arc::clone(context), state.observers.attendance());
        }
        let events = run.take_agent_events();
        let items = run.into_events();
        // A single agent's content, tool results and closing scratchpad
        // reading only reach the run's event channel if they are copied from
        // its items. An orchestrated run's emitter sends all of that to the
        // channel itself, so copying its items as well would journal its
        // content twice.
        let items = match context {
            Some(context) if attribution.is_single_agent() => {
                tee_run_events(context, attribution.clone(), items)
            }
            _ => items,
        };

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

        // These go in the order the request-bound path's stream opens with.
        let (provider, model) = agent.get_provider_info();
        let mut opening = vec![AgentEventPayload::SessionInfo {
            model: format!("{provider}/{model}"),
            model_context_limit: agent.context_window().map(TokenCount::new),
        }];
        if !rehydrated.is_empty() {
            opening.push(AgentEventPayload::SkillsRehydrated { skills: rehydrated });
        }
        let servers = agent.mcp_server_status();
        if !servers.is_empty() {
            opening.push(AgentEventPayload::McpStatus { servers });
        }
        for payload in opening {
            let event = AgentEvent::new(attribution.clone(), payload);
            if let Err(journal_error) = append_agent_event(journal, record, event).await {
                return Outcome {
                    agent: Some(agent),
                    started: Some(attribution),
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
        outcome.started = Some(attribution);
        outcome
    }
    .instrument(span)
    .await
}

/// Polls the run's item stream to its end, and appends every event the run
/// emits to the journal, in the order emitted. Polling the stream is what
/// makes the agent do its work.
///
/// Content reaches the event channel as it streams, so items are checked
/// for only four things:
///
/// - An error. The first one is kept as what the run failed with.
/// - A tool result holding the passthrough tool's marker. That means the run
///   called a tool its client executes. The agent's hook then ends the
///   stream with an error so the client can run the tool, and the run counts
///   as a yield, not a failure.
/// - A `RunParked` event. A park can arrive as an item rather than on the
///   run's channel, and it decides how the run ends. It is observed, not
///   appended: the run's `Parked` ending records it.
/// - The final response. The run's span records its content as the run's
///   answer. Its usage, if it reports any, is what the run billed.
///
/// The event channel never closes on its own, because the prepared agent
/// keeps a sender. So when the item stream ends, the receiver is closed and
/// whatever it still buffers is appended before this returns. At that point
/// the run's ending is decided, so later stops are refused.
///
/// At `deadline` the run is stopped for [`RunCancelReason::Deadline`]. A
/// single agent also checks the timeout itself between turns, and ends its
/// stream with an error. An error that ends the stream at or after the
/// deadline counts as the deadline, so both kinds of agent end the same way.
///
/// Once the run is stopped, its MCP calls in flight are cancelled on their
/// servers. A single agent waits on a tool call it is making, so without
/// this a run stopped mid-call would never end. A single agent also notices
/// a stop only when its provider sends something. So if the stream has not
/// ended [`STOP_GRACE`] after the stop, the stream is dropped, and the
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
    // Reset to expire `STOP_GRACE` after the run is stopped.
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
                    // The run's events can no longer be kept, so stop the
                    // run. `end` fails it with the journal error.
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
            // The run was stopped: cancel its MCP calls and start the grace.
            // An MCP server that never answers the cancel would otherwise
            // keep the run live, and its session busy, for as long as it
            // hangs, so the close gets `MCP_CLOSE_TIMEOUT`. The grace starts
            // only once the close is done, so the stream is dropped at most
            // `MCP_CLOSE_TIMEOUT` plus `STOP_GRACE` after the stop.
            () = record.cancel.cancelled(), if !stop_seen && items.is_some() => {
                stop_seen = true;
                let label = record
                    .stopped()
                    .map_or("run cancelled", |(reason, _)| stop_label(&reason));
                let request_id = record.id.to_string();
                let close = agent.cancel_and_close_mcp(&request_id, label);
                if tokio::time::timeout(MCP_CLOSE_TIMEOUT, close).await.is_err() {
                    tracing::warn!(run_id = %record.id, timeout = ?MCP_CLOSE_TIMEOUT, "closing the stopped run's MCP calls timed out");
                }
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
            item = next_item(&mut items), if items.is_some() => {
                if item.as_ref().is_some_and(shows_activity) {
                    record.note_activity();
                }
                match item {
                    None => {
                        items = None;
                        close(&mut events);
                        // A single agent that reaches the timeout ends its own
                        // stream with an error. Count that as the deadline.
                        if let Some(deadline) = deadline
                            && outcome.error.is_some()
                            && Instant::now() >= deadline.at
                        {
                            record.stop(RunCancelReason::Deadline { after: deadline.after }, None);
                        }
                        record.close_to_stops();
                    }
                    // Keep the first error. Later ones follow from it.
                    Some(Err(error)) => {
                        outcome.error.get_or_insert_with(|| error.to_string());
                    }
                    Some(Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(result))))
                        if result.result.contains(PASSTHROUGH_MARKER) =>
                    {
                        outcome.yielded = true;
                    }
                    Some(Ok(StreamItem::AgentEvent(event)))
                        if matches!(event.payload, AgentEventPayload::RunParked { .. }) =>
                    {
                        approvals.observe(record.id, &event.payload);
                    }
                    Some(Ok(StreamItem::Final(response))) => {
                        outcome.response = Some(response);
                    }
                    Some(Ok(_)) => {}
                }
            }
            else => break,
        }
    }

    outcome.parked = approvals.parked;
    outcome
}

/// Whether `item` shows the run is still going, for whoever follows its
/// [`RunActivity`](super::RunActivity). Every item does, including those that
/// never become an event, such as a tool call's argument deltas, except an MCP
/// status reading. An orchestrated run puts that reading on its stream before
/// its provider has answered.
fn shows_activity(item: &Result<StreamItem, StreamError>) -> bool {
    !matches!(item, Ok(StreamItem::McpStatus(_)))
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

/// Closes the event receiver. `recv` then returns what is still buffered,
/// and `None` after that.
fn close(events: &mut Option<mpsc::Receiver<AgentEvent>>) {
    if let Some(events) = events {
        events.close();
    }
}

/// Appends one agent event and records its sequence number as the run's
/// latest.
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

/// Writes the run's facts to the run store. This process reads the facts
/// from the in-memory record, so a store failure is only logged.
async fn put_facts(shared: &Shared, record: &RunRecord) {
    if let Err(error) = shared.run_store.put(&record.facts()).await {
        tracing::warn!(run_id = %record.id, %error, "the run store refused the run's facts");
    }
}

/// The approvals a run is waiting on.
#[derive(Default)]
struct Approvals {
    /// Requested and not yet completed.
    pending: Vec<DecisionId>,
    /// The decisions the run's `RunParked` event named.
    parked: Option<Vec<DecisionId>>,
}

impl Approvals {
    /// Updates the approvals from one event, and returns whether `pending`
    /// changed.
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

/// Parses a decision id from an approval event. One that is not a UUID is
/// logged and skipped.
fn decision_id(run: RunId, raw: &str) -> Option<DecisionId> {
    DecisionId::parse(raw)
        .inspect_err(|error| {
            tracing::warn!(run_id = %run, decision_id = raw, %error, "an approval event carries a decision id that is not a UUID; ignoring it");
        })
        .ok()
}

/// Writes the run's ending, in three steps:
///
/// - Appends the run's closing readings, then its terminal event.
/// - Publishes the run's final facts and frees the session for its next run.
/// - Writes the facts to the run store and cancels the run's leftover
///   approvals.
///
/// Returns `true` when it wrote the ending. Returns `false` and does nothing
/// if a drain abandoned the run first, because in that case the drain writes
/// the ending.
///
/// What the run billed is worked out once, from its final response and its
/// usage counters. The `Usage` reading, the terminal event and the facts all
/// use that result, so they carry the same numbers.
///
/// The first of these that applies decides the ending:
///
/// 1. The journal refused one of the run's events, including its closing
///    readings: `Failed`.
/// 2. The run was stopped: `Cancelled`, with the stop's reason.
/// 3. The run yielded to a client tool: `Finished`.
/// 4. The run's stream returned an error: `Failed`.
/// 5. The run parked: `Parked`.
/// 6. Otherwise: `Finished`.
///
/// A stopped run's stream ends with an error, and so does a yield's. That is
/// why both come before the stream error.
///
/// `traced` holds the run's span and the attributes it records. Once the
/// ending is decided, the ending and the run's answer are recorded on the
/// span.
async fn end(
    shared: &Shared,
    session: &SessionRecord,
    record: &RunRecord,
    outcome: Outcome,
    traced: Option<(&RunSpan, &tracing::Span)>,
) -> bool {
    if !record.begin_ending() {
        return false;
    }
    record.close_to_stops();
    let Outcome {
        agent,
        started,
        error,
        mut journal_error,
        parked,
        yielded,
        response,
    } = outcome;
    let no_counters = UsageState::new();
    let billed = resolve_billed_usage(response.as_ref(), record.counters().unwrap_or(&no_counters));
    // Skip the closing readings if the run never started, since there is no
    // stream for them to close. Skip them too if the journal already refused
    // an event, because it would refuse these as well.
    if let Some(attribution) = &started
        && journal_error.is_none()
        && let Err(refused) =
            append_closing_readings(session, record, attribution, agent.as_deref(), &billed).await
    {
        tracing::warn!(run_id = %record.id, %refused, "the session journal refused the run's closing readings");
        journal_error = Some(refused);
    }
    let usage = billed.usage;
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
    if let Some((attributes, span)) = traced
        && let Some(ending) = terminal.ending()
    {
        let answer = response.as_ref().map(|response| response.content.as_str());
        attributes.record_output(span, &ending, answer);
    }
    let (stamped, facts) = journal_ending(session, record, terminal, status, parked, usage).await;
    release(shared, session, record, stamped, facts, true).await;
    // Drop the agent now that its run has ended. For a single agent this
    // also drops the prepared agent and closes its MCP connections.
    drop(agent);
    true
}

/// Appends the readings that close a started run, just before its ending.
/// They go in the order the request-bound path sends them after its stream
/// has ended:
///
/// - What the run billed, as a `Usage` event attributed like the run's other
///   events.
/// - For a single agent, how full its context window was on its last turn,
///   as a `ContextUsage` event. It is left out if the provider reported no
///   context tokens. An orchestrated run's agents report their own context
///   usage as they go.
///
/// Any observer attach or detach already being written goes on the stream
/// first. One that starts after that can still land between the readings
/// and the ending.
async fn append_closing_readings(
    session: &SessionRecord,
    record: &RunRecord,
    attribution: &EmittingAgent,
    agent: Option<&dyn StreamingAgent>,
    billed: &BilledUsage,
) -> Result<(), SessionStoreError> {
    session.observer_writes_done().await;
    let billed = AgentEventPayload::Usage {
        usage: billed.usage,
        cache_read_input_tokens: billed.cache.map(|cache| cache.read),
        cache_creation_input_tokens: billed.cache.map(|cache| cache.creation),
    };
    append_agent_event(
        &session.journal,
        record,
        AgentEvent::new(attribution.clone(), billed),
    )
    .await?;

    let (context_tokens, response_tokens) = record
        .counters()
        .map_or((0, 0), UsageState::get_context_usage);
    if !attribution.is_single_agent() || context_tokens == 0 {
        return Ok(());
    }
    let occupancy = AgentEventPayload::ContextUsage {
        context_tokens: TokenCount::new(context_tokens),
        response_tokens: TokenCount::new(response_tokens),
        context_window: agent
            .and_then(|agent| agent.context_window())
            .map(TokenCount::new),
    };
    append_agent_event(
        &session.journal,
        record,
        AgentEvent::new(EmittingAgent::single_agent(), occupancy),
    )
    .await?;
    Ok(())
}

/// Writes the ending for a run after a drain aborted its task. The ending is
/// `Cancelled` with the reason the run was first stopped for, which is what
/// the run would have written itself if it had stopped in time. If that stop
/// came without a message, the message says the run did not stop within the
/// grace period.
pub(super) async fn end_abandoned(shared: &Shared, session: &SessionRecord, record: &RunRecord) {
    record.close_to_stops();
    let usage = record.usage();
    let (reason, message) = record
        .stopped()
        .unwrap_or((RunCancelReason::Shutdown, None));
    let message = message
        .unwrap_or_else(|| "the run did not stop within the shutdown grace period".to_owned());
    let (terminal, status) = cancelled(reason, Some(message), usage);
    let (stamped, facts) = journal_ending(session, record, terminal, status, None, usage).await;
    release(shared, session, record, stamped, facts, false).await;
}

/// Appends `terminal` to the session's journal, and returns the appended
/// event, or `None` if the journal refused it, with the run's final facts.
/// `waiting` is the approvals a parked run waits on. `usage` is the run's
/// total cost, the same value `terminal` carries.
async fn journal_ending(
    session: &SessionRecord,
    record: &RunRecord,
    terminal: LifecycleEvent,
    status: RunStatus,
    waiting: Option<Vec<DecisionId>>,
    usage: TokenUsage,
) -> (Option<Arc<SessionEvent>>, RunFacts) {
    let checkpoint = match &terminal {
        LifecycleEvent::Parked { checkpoint, .. } => Some(checkpoint.clone()),
        _ => None,
    };
    // An observer detach still being written goes on the stream first, so a
    // claim that went while the run was live shows before the run's ending.
    session.observer_writes_done().await;
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
    (stamped, facts)
}

/// Frees the session and records the run as its last run. Then writes the
/// run's final `facts` to the run store, with `finish` when the journal took
/// the ending (`stamped`) and `put` when it did not, cancels any approvals
/// still waiting under the run's id, as the server does when a request ends,
/// and marks the run's ending settled.
///
/// Callers journal the ending first, so the next run's `Started` cannot land
/// ahead of it. The session is freed before the store write and the
/// approvals, which are keyed by this run's id, so a client that starts its
/// next turn as soon as it reads the ending is not refused while they
/// finish.
///
/// The final facts are published while the session lock is held, so a
/// reader never sees the session free while the run still looks live, or
/// the reverse. `retained` says whether the run is held for the retention
/// window.
async fn release(
    shared: &Shared,
    session: &SessionRecord,
    record: &RunRecord,
    stamped: Option<Arc<SessionEvent>>,
    facts: RunFacts,
    retained: bool,
) {
    {
        let mut state = session.lock();
        free(shared, &mut state, record);
        state.last_run = Some((record.id, facts.status));
        if retained {
            state.retained += 1;
        }
        // The record keeps its own copy; the store write below reads these too.
        record.update(|current| *current = facts.clone());
        record.unbind_context();
    }

    let written = match &stamped {
        Some(stamped) => shared.run_store.finish(stamped, &facts).await,
        None => shared.run_store.put(&facts).await,
    };
    if let Err(error) = written {
        tracing::warn!(run_id = %record.id, %error, "the run store refused the run's final facts");
    }
    shared
        .pending_approvals
        .cancel_request(&record.id.to_string())
        .await;
    record.settle();
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

/// The reason sent to MCP servers when a stopped run's calls are cancelled.
fn stop_label(reason: &RunCancelReason) -> &'static str {
    match reason {
        RunCancelReason::Deadline { .. } => "run timed out",
        RunCancelReason::Shutdown => "server shutting down",
        RunCancelReason::Unclaimed => "run unclaimed",
        _ => "run cancelled",
    }
}
