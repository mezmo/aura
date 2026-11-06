//! Turns one run's events, read from its session's stream, into the SSE
//! frames of a chat completion: OpenAI chunks and `aura.*` events.

use aura::journal::JournalItem;
use aura::stream_events::{AuraStreamEvent, CorrelationContext, EmittingAgent};
use aura::{RunId, ToolCall, ToolResult, ToolResultKind};
use aura_events::TokenCount;
use aura_events::agent::{AgentEvent, AgentEventPayload, ResultFormat, ToolOutcome};
use aura_events::run::{RunCancelReason, RunEnding, SessionEventPayload};
use bytes::Bytes;

use super::handlers::{
    ToolDuration, build_final_chunk, build_text_chunk, handle_orchestrator_event, handle_reasoning,
    handle_stream_error, handle_text_delta, handle_timed_tool_result, handle_tool_call,
    run_event_sse,
};
use super::types::openai::UsageInfo;
use super::types::{StreamConfig, TurnContext, TurnState, format_sse_chunk};

/// The frame that ends an SSE response.
const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

/// The text the client gets in place of the rest of the answer when its
/// stream has lost events.
const GAP_MESSAGE: &str = "This response fell behind the agent and lost part of its output, \
    so it ends here. Please try again.";

/// Turns one run's events into SSE frames, and keeps the turn's state
/// (content, usage, tool calls) between events.
pub struct SseProjector {
    config: StreamConfig,
    ctx: TurnContext,
    state: TurnState,
    run: RunId,
    /// The frames of the run's closing usage readings.
    closing_readings: Vec<Bytes>,
}

/// What one journal item became on the wire.
pub enum Projected {
    /// The item has nothing for this run's response.
    Nothing,
    /// Frames for a run that is still going, and the pulse the body's
    /// timers act on.
    Frames { frames: Vec<Bytes>, pulse: Pulse },
    /// The run's last frames, and how its stream ended.
    End { frames: Vec<Bytes>, ending: Ending },
}

/// What an event says about its run, for the streaming body's timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pulse {
    /// One of the opening events: session info, rehydrated skills or MCP
    /// status.
    Opening,
    /// Any other event of a run that is still going.
    Activity,
    /// A tool call started.
    ToolStarted,
    /// A tool call finished.
    ToolFinished,
}

/// How a run's stream on the wire ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The run ended.
    Run(RunEnding),
    /// The subscription missed events the session's store no longer holds.
    Gap,
}

impl SseProjector {
    /// A projector for `run` that renders with `config` into the turn
    /// described by `ctx`. Callers build it after reading the run's
    /// `Started` event.
    pub fn new(config: StreamConfig, ctx: TurnContext, run: RunId) -> Self {
        Self {
            config,
            ctx,
            state: TurnState::new(),
            run,
            closing_readings: Vec::new(),
        }
    }

    pub fn run(&self) -> RunId {
        self.run
    }

    /// The turn's state so far: content, usage, tool calls.
    pub fn state(&self) -> &TurnState {
        &self.state
    }

    /// Projects one item of the session's stream. Items from other runs, the
    /// session's own events, and entries this binary cannot decode give
    /// [`Projected::Nothing`]. A gap ends the stream.
    pub fn project(&mut self, item: &JournalItem) -> Projected {
        match item {
            JournalItem::Event(event) if event.run_id == Some(self.run) => match &event.payload {
                SessionEventPayload::Agent(agent_event) => self.project_agent_event(agent_event),
                // `Started` was read before the projector was built. Observer
                // and liveness events concern the session, not this response.
                SessionEventPayload::Lifecycle(lifecycle) => match lifecycle.ending() {
                    Some(ending) => self.end(Ending::Run(ending)),
                    None => Projected::Nothing,
                },
                other => {
                    tracing::debug!(
                        payload = ?std::mem::discriminant(other),
                        "a session event this projection does not know reached no frame"
                    );
                    Projected::Nothing
                }
            },
            JournalItem::Event(_) => Projected::Nothing,
            JournalItem::Gap { resumed_at } => {
                tracing::warn!(
                    run_id = %self.run,
                    %resumed_at,
                    "the response fell behind the session's stream and missed events; ending it"
                );
                self.end(Ending::Gap)
            }
            JournalItem::Unknown { seq, .. } => {
                tracing::debug!(
                    run_id = %self.run,
                    %seq,
                    "an entry this binary cannot decode reached no frame"
                );
                Projected::Nothing
            }
        }
    }

    /// Projects one of the run's agent events. Every agent event shows the
    /// run is alive, so even an event with no frame returns a pulse for the
    /// timers. Otherwise turning custom events off would make a working run
    /// look stalled.
    fn project_agent_event(&mut self, event: &AgentEvent) -> Projected {
        use AgentEventPayload as Payload;

        let (frames, pulse) = match &event.payload {
            Payload::SessionInfo {
                model,
                model_context_limit,
            } => (
                self.custom_event(|correlation| {
                    AuraStreamEvent::session_info(
                        model.as_str(),
                        model_context_limit.map(TokenCount::get),
                        correlation,
                    )
                }),
                Pulse::Opening,
            ),
            Payload::SkillsRehydrated { skills } => (
                self.custom_event(|correlation| {
                    AuraStreamEvent::skills_rehydrated(skills.clone(), correlation)
                }),
                Pulse::Opening,
            ),
            Payload::McpStatus { servers } => (
                self.custom_event(|correlation| {
                    AuraStreamEvent::mcp_status(servers.clone(), correlation)
                }),
                Pulse::Opening,
            ),

            Payload::TextDelta { content } => (
                handle_text_delta(&self.ctx, &mut self.state, content.clone()),
                Pulse::Activity,
            ),
            Payload::Reasoning {
                content,
                task_id: None,
            } => (
                handle_reasoning(&self.config, &self.ctx, content.clone()),
                Pulse::Activity,
            ),
            // The run's final answer replaces the text deltas collected so
            // far. Its usage is what the final chunk reports.
            Payload::Completed { content, usage } => {
                self.state.accumulated_content.clone_from(content);
                self.state.usage_stats = Some(UsageInfo {
                    prompt_tokens: usage.prompt_tokens.get(),
                    completion_tokens: usage.completion_tokens.get(),
                    total_tokens: usage.total_tokens.get(),
                });
                (Vec::new(), Pulse::Activity)
            }

            // The call's chunk goes out before `aura.tool_requested`, because
            // rig yields the call before its hook emits that event. If the
            // arguments did not parse, the chunk carries them as the provider
            // sent them.
            Payload::ToolRequested {
                tool_call_id,
                tool_name,
                arguments,
                unparsed_arguments,
            } => {
                let call = ToolCall {
                    id: tool_call_id.to_string(),
                    name: tool_name.to_string(),
                    arguments: unparsed_arguments
                        .clone()
                        .unwrap_or_else(|| arguments.to_string()),
                };
                let mut frames = handle_tool_call(&self.config, &self.ctx, &mut self.state, &call);
                frames.extend(self.run_event(event));
                (frames, Pulse::ToolStarted)
            }
            Payload::ToolStart { task_id: None, .. } if event.agent.is_single_agent() => {
                (self.run_event(event), Pulse::Activity)
            }
            Payload::ToolComplete {
                tool_call_id,
                tool_name,
                duration_ms,
                outcome,
                task_id: None,
            } if event.agent.is_single_agent() => {
                let id = tool_call_id.to_string();
                // The hook emits no `ToolRequested` event for a scratchpad
                // call, so its name is registered here. The result handler
                // needs the name to recognise a scratchpad call.
                let index = self.state.tool_call_index;
                self.state
                    .tool_call_map
                    .entry(id.clone())
                    .or_insert_with(|| (tool_name.to_string(), index));
                // A failed call's text is its error as rig reports it. An
                // outcome this binary does not know is a failure with no
                // text, as the request-bound path reports one. A result
                // format it does not know is still the tool's output, so it
                // goes out as text.
                let (result, kind): (&str, ToolResultKind) = match outcome {
                    ToolOutcome::Success {
                        result,
                        format: ResultFormat::Text | ResultFormat::Unknown,
                    } => (result, ToolResultKind::Text),
                    ToolOutcome::Success {
                        result,
                        format: ResultFormat::Json,
                    } => (result, ToolResultKind::Json),
                    ToolOutcome::Failure { error } => (error, ToolResultKind::Error),
                    ToolOutcome::Unknown => ("", ToolResultKind::Error),
                };
                let tool_result = ToolResult {
                    id,
                    call_id: None,
                    result: result.to_owned(),
                    kind,
                };
                let frames = handle_timed_tool_result(
                    &self.config,
                    &self.ctx,
                    &mut self.state,
                    &tool_result,
                    ToolDuration::Reported(*duration_ms),
                );
                (frames, Pulse::ToolFinished)
            }
            Payload::ToolProgress { .. }
            | Payload::ToolUsage { .. }
            | Payload::ApprovalRequested(_)
            | Payload::ApprovalPending(_)
            | Payload::ApprovalCompleted(_) => (self.run_event(event), Pulse::Activity),

            // The run's billed usage and a single agent's context reading
            // belong at the end of the stream, so their frames are held until
            // the run ends.
            Payload::Usage {
                usage,
                cache_read_input_tokens,
                cache_creation_input_tokens,
            } => {
                let prompt = usage.prompt_tokens.get();
                if self.config.emit_custom_events && prompt > 0 {
                    let cache = cache_read_input_tokens
                        .zip(*cache_creation_input_tokens)
                        .map(|(read, creation)| (read.get(), creation.get()));
                    self.closing_readings.push(frame(AuraStreamEvent::usage(
                        prompt,
                        usage.completion_tokens.get(),
                        usage.total_tokens.get(),
                        cache,
                        self.ctx.correlation.clone(),
                    )));
                }
                (Vec::new(), Pulse::Activity)
            }
            // A single agent's context reading is custom telemetry, sent only
            // with custom events on. An orchestrated run's per-agent readings
            // go out as they arrive, whatever the setting.
            Payload::ContextUsage {
                context_tokens,
                response_tokens,
                context_window,
            } => {
                let single_agent = event.agent.is_single_agent();
                if single_agent && !self.config.emit_custom_events {
                    (Vec::new(), Pulse::Activity)
                } else {
                    let reading = frame(AuraStreamEvent::context_usage(
                        context_tokens.get(),
                        response_tokens.get(),
                        context_window.map(TokenCount::get),
                        by_id(&event.agent),
                        self.ctx.correlation.clone(),
                    ));
                    if single_agent {
                        self.closing_readings.push(reading);
                        (Vec::new(), Pulse::Activity)
                    } else {
                        (vec![reading], Pulse::Activity)
                    }
                }
            }
            Payload::ScratchpadUsage {
                tokens_intercepted,
                tokens_extracted,
            } => (
                vec![frame(AuraStreamEvent::scratchpad_usage(
                    tokens_intercepted.get() as usize,
                    tokens_extracted.get() as usize,
                    by_id(&event.agent),
                    self.ctx.correlation.clone(),
                ))],
                Pulse::Activity,
            ),
            Payload::WorkerPhase { .. } => (Vec::new(), Pulse::Activity),

            // Orchestration events, and the tool calls and reasoning of an
            // orchestrated run's agents: those with a task id, or from an
            // agent other than the single agent.
            Payload::Reasoning {
                task_id: Some(_), ..
            }
            | Payload::ToolStart { .. }
            | Payload::ToolComplete { .. }
            | Payload::PlanCreated { .. }
            | Payload::DirectAnswer { .. }
            | Payload::ClarificationNeeded { .. }
            | Payload::TaskStarted { .. }
            | Payload::TaskCompleted { .. }
            | Payload::TaskBlocked { .. }
            | Payload::RunParked { .. }
            | Payload::IterationComplete { .. }
            | Payload::ReplanStarted { .. }
            | Payload::Synthesizing { .. } => (
                handle_orchestrator_event(&self.config, &self.ctx, event),
                Pulse::Activity,
            ),

            // `AgentEventPayload` is `#[non_exhaustive]`. A variant this match
            // does not list lands here instead of failing to compile.
            unknown => {
                tracing::debug!(
                    payload = ?std::mem::discriminant(unknown),
                    "a run event this projection does not know reached no frame"
                );
                (Vec::new(), Pulse::Activity)
            }
        };
        Projected::Frames { frames, pulse }
    }

    /// Builds the run's last frames for how its stream ended.
    fn end(&mut self, ending: Ending) -> Projected {
        let frames = match &ending {
            // The run stopped because its last claimant left, so nobody is
            // reading. Nothing more goes out, not even the closing readings.
            Ending::Run(RunEnding::Cancelled {
                reason: RunCancelReason::Unclaimed,
                ..
            }) => Vec::new(),
            Ending::Run(RunEnding::Finished | RunEnding::Parked | RunEnding::Cancelled { .. }) => {
                self.closing(Vec::new())
            }
            Ending::Run(RunEnding::Failed { error }) => {
                let frames = handle_stream_error(&self.config, &self.ctx, &mut self.state, error);
                self.closing(frames)
            }
            Ending::Gap => {
                self.state.accumulated_content.push_str(GAP_MESSAGE);
                let chunk = build_text_chunk(&self.ctx, GAP_MESSAGE, self.state.is_first_chunk);
                let frames = format_sse_chunk(&chunk).into_iter().collect();
                self.closing(frames)
            }
        };
        Projected::End { frames, ending }
    }

    /// Returns `frames` followed by the run's closing usage readings, the
    /// final chunk and `[DONE]`. This is the order the request-bound loop
    /// uses to end a stream.
    fn closing(&mut self, mut frames: Vec<Bytes>) -> Vec<Bytes> {
        frames.append(&mut self.closing_readings);
        frames.extend(build_final_chunk(&self.ctx, &self.state));
        frames.push(Bytes::from_static(DONE_FRAME));
        frames
    }

    /// The frame `run_event_sse` renders for `event`, or none.
    fn run_event(&self, event: &AgentEvent) -> Vec<Bytes> {
        run_event_sse(
            event.clone(),
            self.ctx.correlation.clone(),
            self.config.emit_custom_events,
        )
        .map(frame)
        .into_iter()
        .collect()
    }

    /// Builds a custom event with `event` and returns its frame. Returns
    /// nothing when custom events are off.
    fn custom_event(
        &self,
        event: impl FnOnce(CorrelationContext) -> AuraStreamEvent,
    ) -> Vec<Bytes> {
        if self.config.emit_custom_events {
            vec![frame(event(self.ctx.correlation.clone()))]
        } else {
            Vec::new()
        }
    }
}

fn frame(event: AuraStreamEvent) -> Bytes {
    Bytes::from(event.format_sse())
}

/// `agent` with only its id kept, the way the item stream's usage readings
/// name an agent.
fn by_id(agent: &EmittingAgent) -> EmittingAgent {
    EmittingAgent {
        agent_id: agent.agent_id.clone(),
        agent_name: None,
        parent_agent_id: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aura::journal::JournalItem;
    use aura_events::agent::{AgentEvent, AgentEventPayload, ResultFormat, ToolOutcome};
    use aura_events::event_names;
    use aura_events::run::{
        CheckpointRef, LifecycleEvent, RunCancelReason, RunEnding, SessionEvent,
        SessionEventPayload,
    };
    use aura_events::{
        EmittingAgent, RunId, SequenceNumber, SessionId, Timestamp, TokenCount, TokenUsage,
    };
    use aura_test_utils::sse::{SseEvent, events_by_type, parse_sse_stream};
    use bytes::Bytes;
    use serde_json::Value;

    use super::{Ending, Projected, Pulse, SseProjector};
    use crate::streaming::types::{StreamConfig, ToolResultMode, TurnContext};

    const SESSION_ID: &str = "cs-tool-events";

    /// A projector for `run` with `config` and `session_id`, over the same
    /// turn the request-bound test suites render into.
    fn projector_with(run: RunId, config: StreamConfig, session_id: &str) -> SseProjector {
        SseProjector::new(
            config,
            TurnContext::new(
                "chatcmpl-test".to_string(),
                "test/fake".to_string(),
                1_700_000_000,
                None,
                session_id,
            ),
            run,
        )
    }

    fn config(custom_events: bool) -> StreamConfig {
        StreamConfig::new(custom_events, false, ToolResultMode::Aura, 0)
    }

    /// A projector for a newly minted run.
    fn projector(custom_events: bool) -> SseProjector {
        projector_with(RunId::mint(), config(custom_events), SESSION_ID)
    }

    /// `payload` wrapped as a journal item, as the session's stream
    /// delivers it.
    fn item(run: Option<RunId>, payload: SessionEventPayload) -> JournalItem {
        JournalItem::Event(Arc::new(SessionEvent {
            session_id: SessionId::new(SESSION_ID).expect("a session id"),
            run_id: run,
            seq: SequenceNumber::try_from(1).expect("a sequence starts at 1"),
            at: Timestamp::from_unix_millis(1_700_000_000_000),
            payload,
        }))
    }

    fn agent_item(run: RunId, event: AgentEvent) -> JournalItem {
        item(Some(run), SessionEventPayload::Agent(event))
    }

    fn lifecycle_item(run: RunId, event: LifecycleEvent) -> JournalItem {
        item(Some(run), SessionEventPayload::Lifecycle(event))
    }

    fn single(payload: AgentEventPayload) -> AgentEvent {
        AgentEvent::single_agent(payload)
    }

    fn text(content: &str) -> AgentEvent {
        single(AgentEventPayload::TextDelta {
            content: content.to_string(),
        })
    }

    fn finished() -> LifecycleEvent {
        LifecycleEvent::Finished {
            usage: TokenUsage::default(),
        }
    }

    /// Projects `event` for the projector's run and returns its frames and
    /// pulse.
    fn frames_of(projector: &mut SseProjector, event: AgentEvent) -> (Vec<Bytes>, Pulse) {
        match projector.project(&agent_item(projector.run(), event)) {
            Projected::Frames { frames, pulse } => (frames, pulse),
            Projected::Nothing => panic!("a run's agent event reaches the timers"),
            Projected::End { .. } => panic!("an agent event does not end the run"),
        }
    }

    /// Projects the terminal lifecycle `event` for the projector's run and
    /// returns the last frames and the ending.
    fn end_of(projector: &mut SseProjector, event: LifecycleEvent) -> (Vec<Bytes>, Ending) {
        match projector.project(&lifecycle_item(projector.run(), event)) {
            Projected::End { frames, ending } => (frames, ending),
            Projected::Nothing | Projected::Frames { .. } => panic!("a terminal event ends"),
        }
    }

    fn body_of(frames: &[Bytes]) -> String {
        frames
            .iter()
            .map(|frame| std::str::from_utf8(frame).expect("UTF-8"))
            .collect()
    }

    /// Projects `events` and then `ending` for the projector's run, and
    /// returns the body as text.
    fn project_run(
        projector: &mut SseProjector,
        events: Vec<AgentEvent>,
        ending: LifecycleEvent,
    ) -> String {
        let mut frames = Vec::new();
        for event in events {
            frames.extend(frames_of(projector, event).0);
        }
        frames.extend(end_of(projector, ending).0);
        body_of(&frames)
    }

    /// Projects `events`, ends the run as finished, and parses the body into
    /// SSE events, the same shape the request-bound test harness returns.
    fn run_scoped(custom_events: bool, session_id: &str, events: Vec<AgentEvent>) -> Vec<SseEvent> {
        let mut projector = projector_with(RunId::mint(), config(custom_events), session_id);
        let body = project_run(&mut projector, events, finished());
        let (events, done) = parse_sse_stream(&body);
        assert!(done, "stream should terminate with [DONE]");
        events
    }

    fn run_with(events: Vec<AgentEvent>) -> Vec<SseEvent> {
        run_scoped(true, SESSION_ID, events)
    }

    fn payload(event: &SseEvent) -> Value {
        serde_json::from_str(&event.data).expect("event data should be JSON")
    }

    /// The OpenAI chunks among `events`.
    fn chunks(events: &[SseEvent]) -> Vec<Value> {
        events
            .iter()
            .filter(|e| e.event_type.is_none())
            .map(payload)
            .filter(|json| json["object"] == "chat.completion.chunk")
            .collect()
    }

    /// The chunk that carries `finish_reason`. OpenAI clients read usage
    /// from it.
    fn final_chunk(events: &[SseEvent]) -> Value {
        chunks(events)
            .into_iter()
            .find(|json| json["choices"][0]["finish_reason"].is_string())
            .expect("stream should end with a finish_reason chunk")
    }

    fn content_of(events: &[SseEvent]) -> String {
        chunks(events)
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect()
    }

    /// The request-bound `tool_events` suite's scripts, written as the run's
    /// events. Here the run reports each call's duration (`DURATION_MS`)
    /// itself.
    mod tool_events {
        use super::*;
        use aura::ToolName;
        use aura_events::{NumberOrString, Progress, ProgressToken, ToolCallId};
        use serde_json::json;

        const TOOL_ID: &str = "call_abc123";
        const TOOL_NAME: &str = "list_files";
        const DURATION_MS: u64 = 42;

        fn tool_requested() -> AgentEvent {
            single(AgentEventPayload::ToolRequested {
                tool_call_id: ToolCallId::new(TOOL_ID),
                tool_name: ToolName::new(TOOL_NAME),
                arguments: json!({ "path": "/mock" }),
                unparsed_arguments: None,
            })
        }

        fn tool_start() -> AgentEvent {
            single(AgentEventPayload::ToolStart {
                tool_call_id: ToolCallId::new(TOOL_ID),
                tool_name: ToolName::new(TOOL_NAME),
                progress_token: Some(ProgressToken(NumberOrString::Number(7))),
                arguments: None,
                task_id: None,
            })
        }

        fn progress(progress: Progress, message: Option<&str>) -> AgentEvent {
            single(AgentEventPayload::ToolProgress {
                progress_token: ProgressToken(NumberOrString::Number(7)),
                progress,
                message: message.map(str::to_string),
            })
        }

        fn tool_complete(outcome: ToolOutcome) -> AgentEvent {
            single(AgentEventPayload::ToolComplete {
                tool_call_id: ToolCallId::new(TOOL_ID),
                tool_name: ToolName::new(TOOL_NAME),
                duration_ms: DURATION_MS,
                outcome,
                task_id: None,
            })
        }

        fn success(result: &str) -> ToolOutcome {
            ToolOutcome::Success {
                result: result.to_string(),
                format: ResultFormat::Text,
            }
        }

        fn failure(error: &str) -> ToolOutcome {
            ToolOutcome::Failure {
                error: error.to_string(),
            }
        }

        fn tool_turn(outcome: ToolOutcome) -> Vec<AgentEvent> {
            vec![
                tool_requested(),
                tool_start(),
                tool_complete(outcome),
                text("Here are the files."),
            ]
        }

        fn run_successful_tool_call() -> Vec<SseEvent> {
            run_with(tool_turn(success("README.md\nsrc/")))
        }

        fn tool_ids(events: &[&SseEvent]) -> Vec<String> {
            events
                .iter()
                .map(|e| payload(e)["tool_id"].as_str().expect("tool_id").to_string())
                .collect()
        }

        #[test]
        fn tool_requested_carries_the_call_and_its_arguments() {
            let events = run_successful_tool_call();
            let requested = events_by_type(&events, event_names::TOOL_REQUESTED);

            assert_eq!(requested.len(), 1, "expected one aura.tool_requested");
            let json = payload(requested[0]);
            assert_eq!(json["tool_id"], TOOL_ID);
            assert_eq!(json["tool_name"], TOOL_NAME);
            assert_eq!(json["arguments"], json!({ "path": "/mock" }));
            assert!(json["agent_id"].is_string(), "missing agent_id");
            assert_eq!(json["session_id"], SESSION_ID);
        }

        #[test]
        fn tool_start_carries_the_progress_token_for_correlation() {
            let events = run_successful_tool_call();
            let start = events_by_type(&events, event_names::TOOL_START);

            assert_eq!(start.len(), 1, "expected one aura.tool_start");
            let json = payload(start[0]);
            assert_eq!(json["tool_id"], TOOL_ID);
            assert_eq!(json["tool_name"], TOOL_NAME);
            assert_eq!(json["progress_token"], 7);
            assert!(json["agent_id"].is_string(), "missing agent_id");
            assert_eq!(json["session_id"], SESSION_ID);
        }

        #[test]
        fn tool_complete_reports_success_with_duration_and_result() {
            let events = run_successful_tool_call();
            let complete = events_by_type(&events, event_names::TOOL_COMPLETE);

            assert_eq!(complete.len(), 1, "expected one aura.tool_complete");
            let json = payload(complete[0]);
            assert_eq!(json["tool_id"], TOOL_ID);
            assert_eq!(json["tool_name"], TOOL_NAME);
            assert_eq!(json["success"], true);
            assert_eq!(json["result"], "README.md\nsrc/");
            assert_eq!(
                json["duration_ms"], DURATION_MS,
                "the duration is the run's own measure"
            );
            assert!(
                json.get("error").is_none(),
                "successful tool_complete must not carry an error"
            );
        }

        #[test]
        fn tool_complete_reports_failure_with_the_error_message() {
            let events = run_with(tool_turn(failure(
                "Tool execution failed: Connection refused",
            )));

            let complete = events_by_type(&events, event_names::TOOL_COMPLETE);
            assert_eq!(complete.len(), 1, "expected one aura.tool_complete");
            let json = payload(complete[0]);
            assert_eq!(json["success"], false);
            assert_eq!(json["error"], "Tool execution failed: Connection refused");
            assert!(
                json.get("result").is_none(),
                "failed tool_complete must not carry a result"
            );
        }

        #[test]
        fn tool_complete_reports_a_rig_wrapped_tool_err_as_a_failure_without_the_wrapping() {
            let error = "Toolset error: ToolCallError: ToolCallError: Connection refused";
            let events = run_with(tool_turn(failure(error)));

            let complete = events_by_type(&events, event_names::TOOL_COMPLETE);
            assert_eq!(complete.len(), 1, "expected one aura.tool_complete");
            let json = payload(complete[0]);
            assert_eq!(json["success"], false);
            assert_eq!(json["error"], "Connection refused");
        }

        /// An outcome a newer writer journaled and this binary does not
        /// know reads as a failure with no text, as the request-bound path
        /// reports one.
        #[test]
        fn tool_complete_reports_an_unknown_outcome_as_a_failure() {
            let events = run_with(tool_turn(ToolOutcome::Unknown));

            let complete = events_by_type(&events, event_names::TOOL_COMPLETE);
            assert_eq!(complete.len(), 1, "expected one aura.tool_complete");
            let json = payload(complete[0]);
            assert_eq!(json["success"], false);
            assert!(
                json.get("result").is_none(),
                "an unknown outcome carries no result"
            );
        }

        /// A result format a newer writer journaled and this binary does
        /// not know is still the tool's output.
        #[test]
        fn tool_complete_reports_a_result_in_an_unknown_format_as_text() {
            let events = run_with(tool_turn(ToolOutcome::Success {
                result: "a: 1".to_string(),
                format: ResultFormat::Unknown,
            }));

            let complete = events_by_type(&events, event_names::TOOL_COMPLETE);
            assert_eq!(complete.len(), 1, "expected one aura.tool_complete");
            let json = payload(complete[0]);
            assert_eq!(json["success"], true);
            assert_eq!(json["result"], "a: 1");
        }

        #[test]
        fn tool_complete_reports_output_that_reads_like_a_failure_as_a_success() {
            let output = "Tool execution failed: disk full on node-3 (12:04:11)";
            let events = run_with(tool_turn(success(output)));

            let complete = events_by_type(&events, event_names::TOOL_COMPLETE);
            assert_eq!(complete.len(), 1, "expected one aura.tool_complete");
            let json = payload(complete[0]);
            assert_eq!(json["success"], true);
            assert_eq!(json["result"], output);
        }

        #[test]
        fn every_requested_tool_is_paired_with_a_start_and_a_complete() {
            let events = run_successful_tool_call();

            let requested = tool_ids(&events_by_type(&events, event_names::TOOL_REQUESTED));
            let start = tool_ids(&events_by_type(&events, event_names::TOOL_START));
            let complete = tool_ids(&events_by_type(&events, event_names::TOOL_COMPLETE));

            assert_eq!(requested, start, "tool_requested/tool_start must pair");
            assert_eq!(start, complete, "tool_start/tool_complete must pair");
        }

        #[test]
        fn the_lifecycle_is_ordered_requested_start_progress_complete() {
            let mut script = tool_turn(success("README.md"));
            // MCP progress arrives after tool_start and before the result.
            script.insert(2, progress(Progress::ratio(50.0, 100.0), Some("halfway")));
            let events = run_with(script);

            let position = |event_type: &str| {
                events
                    .iter()
                    .position(|e| e.event_type.as_deref() == Some(event_type))
                    .unwrap_or_else(|| panic!("no {event_type} event in stream"))
            };

            let requested = position(event_names::TOOL_REQUESTED);
            let start = position(event_names::TOOL_START);
            let progress_pos = position(event_names::PROGRESS);
            let complete = position(event_names::TOOL_COMPLETE);

            assert!(requested < start, "tool_requested must precede tool_start");
            assert!(start < progress_pos, "tool_start must precede progress");
            assert!(
                progress_pos < complete,
                "progress must precede tool_complete"
            );
            assert_eq!(
                payload(&events[progress_pos])["progress_token"],
                7,
                "progress must correlate with tool_start's token"
            );
        }

        /// A progress notification without a message gets one built from its
        /// counts, and that text goes out on the wire.
        #[test]
        fn progress_without_a_message_renders_its_counts() {
            let message_for = |progress_value: Progress| {
                let events = run_with(vec![progress(progress_value, None), text("done")]);
                let progress_events = events_by_type(&events, event_names::PROGRESS);
                payload(progress_events[0])["message"]
                    .as_str()
                    .expect("message")
                    .to_string()
            };

            assert_eq!(
                message_for(Progress::ratio(50.0, 100.0)),
                "Progress: 50/100"
            );
            assert_eq!(message_for(Progress::indeterminate(3.0)), "Progress: 3");
        }

        #[test]
        fn openai_chunks_are_emitted_alongside_the_custom_events() {
            let events = run_successful_tool_call();

            let chunks = chunks(&events);
            assert!(
                !chunks.is_empty(),
                "custom events must not displace the OpenAI chunks"
            );
            for chunk in &chunks {
                assert_eq!(chunk["id"], "chatcmpl-test");
                assert!(chunk["choices"].is_array(), "chunk missing choices");
            }

            // Text resuming after a tool result is separated from the prior turn.
            assert_eq!(content_of(&events), "\n\nHere are the files.");

            let call = chunks
                .iter()
                .find(|c| c["choices"][0]["delta"]["tool_calls"].is_array())
                .map(|c| c["choices"][0]["delta"]["tool_calls"][0].clone())
                .expect("tool call should surface as an OpenAI delta");
            assert_eq!(call["id"], TOOL_ID);
            assert_eq!(call["function"]["name"], TOOL_NAME);
        }

        #[test]
        fn every_aura_event_carries_the_requests_session_id() {
            let session_id = "correlation-test-session";
            let events = run_scoped(true, session_id, tool_turn(success("README.md")));

            let aura_events: Vec<&SseEvent> = events
                .iter()
                .filter(|e| {
                    e.event_type
                        .as_deref()
                        .is_some_and(|t| t.starts_with("aura."))
                })
                .collect();

            assert!(!aura_events.is_empty(), "expected aura.* events");
            for event in aura_events {
                assert_eq!(
                    payload(event)["session_id"],
                    session_id,
                    "session_id mismatch in {:?}",
                    event.event_type
                );
            }
        }

        #[test]
        fn custom_events_are_suppressed_when_the_flag_is_off() {
            let events = run_scoped(false, SESSION_ID, tool_turn(success("README.md")));

            assert!(
                events.iter().all(|e| e.event_type.is_none()),
                "no aura.* events should be emitted when custom events are off"
            );
            assert!(
                !events.is_empty(),
                "OpenAI chunks must still flow with custom events off"
            );
        }

        /// The hook emits no `ToolRequested` event for a scratchpad call, so
        /// its result arrives for a call the projection has not seen. The
        /// result stays off the wire, like the call, and the text after it
        /// gets no separator.
        #[test]
        fn a_scratchpad_result_stays_off_the_wire() {
            assert!(aura::scratchpad::is_scratchpad_tool("head"));
            let events = run_with(vec![
                single(AgentEventPayload::ToolComplete {
                    tool_call_id: ToolCallId::new("call_head"),
                    tool_name: ToolName::new("head"),
                    duration_ms: 3,
                    outcome: success("first lines"),
                    task_id: None,
                }),
                text("Done."),
            ]);

            assert!(events_by_type(&events, event_names::TOOL_COMPLETE).is_empty());
            assert_eq!(content_of(&events), "Done.");
        }

        /// When the provider cuts the arguments short, the call's chunk
        /// carries them as sent, and `aura.tool_requested` carries an empty
        /// object.
        #[test]
        fn unparsed_arguments_go_out_in_the_chunk_as_the_provider_sent_them() {
            let cut = r#"{"path":"/mo"#;
            let events = run_with(vec![single(AgentEventPayload::ToolRequested {
                tool_call_id: ToolCallId::new(TOOL_ID),
                tool_name: ToolName::new(TOOL_NAME),
                arguments: json!({}),
                unparsed_arguments: Some(cut.to_string()),
            })]);

            let call = chunks(&events)
                .into_iter()
                .find_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"].get(0).cloned())
                .expect("the call's chunk");
            assert_eq!(call["function"]["arguments"], cut);
            let requested = events_by_type(&events, event_names::TOOL_REQUESTED);
            assert_eq!(payload(requested[0])["arguments"], json!({}));
        }

        /// In OpenWebUI mode the result is appended to the call's arguments,
        /// which OpenWebUI parses as JSON. So JSON output goes out unchanged,
        /// and text goes out as a JSON string.
        #[test]
        fn openwebui_gets_json_output_raw_and_text_quoted() {
            let result_chunk = |outcome: ToolOutcome| {
                let mut projector = projector_with(
                    RunId::mint(),
                    StreamConfig::new(false, false, ToolResultMode::OpenWebUI, 0),
                    SESSION_ID,
                );
                let body = project_run(&mut projector, tool_turn(outcome), finished());
                let (events, _) = parse_sse_stream(&body);
                chunks(&events)
                    .into_iter()
                    .filter_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"].get(0).cloned())
                    .nth(1)
                    .expect("the result's chunk follows the call's")
            };

            let json_output = result_chunk(ToolOutcome::Success {
                result: r#"{"files":11}"#.to_string(),
                format: ResultFormat::Json,
            });
            assert_eq!(json_output["function"]["arguments"], r#"{"files":11}"#);

            let text_output = result_chunk(success("a.txt"));
            assert_eq!(text_output["function"]["arguments"], r#""a.txt""#);
        }

        /// A client tool's result is the passthrough marker, not real
        /// output. It stays off the wire, and the final chunk's
        /// `finish_reason` tells the client to run the tool.
        #[test]
        fn a_passthrough_result_ends_the_turn_with_tool_calls() {
            let marker = format!("{} client tool", aura::PASSTHROUGH_MARKER);
            let events = run_with(vec![tool_requested(), tool_complete(success(&marker))]);

            assert!(events_by_type(&events, event_names::TOOL_COMPLETE).is_empty());
            assert_eq!(
                final_chunk(&events)["choices"][0]["finish_reason"],
                "tool_calls"
            );
        }
    }

    /// The request-bound `stream_output` suite's scripts, written as the
    /// run's events. They check the final chunk's `usage` and
    /// `finish_reason`, and the `aura.usage` event, built from the run's
    /// `Completed` and `Usage` events.
    mod stream_output {
        use super::*;

        fn completed(content: &str, input: u64, output: u64) -> AgentEvent {
            single(AgentEventPayload::Completed {
                content: content.to_string(),
                usage: usage(input, output),
            })
        }

        fn usage(input: u64, output: u64) -> TokenUsage {
            TokenUsage {
                prompt_tokens: TokenCount::new(input),
                completion_tokens: TokenCount::new(output),
                total_tokens: TokenCount::new(input + output),
            }
        }

        fn billed(input: u64, output: u64) -> AgentEvent {
            single(AgentEventPayload::Usage {
                usage: usage(input, output),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            })
        }

        #[test]
        fn the_final_chunk_carries_usage_from_the_runs_completed_event() {
            let events = run_with(vec![text("Hello."), completed("Hello.", 12, 7)]);
            let chunk = final_chunk(&events);

            assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
            assert_eq!(chunk["usage"]["prompt_tokens"], 12);
            assert_eq!(chunk["usage"]["completion_tokens"], 7);
            assert_eq!(
                chunk["usage"]["total_tokens"].as_u64().expect("total"),
                chunk["usage"]["prompt_tokens"].as_u64().expect("prompt")
                    + chunk["usage"]["completion_tokens"]
                        .as_u64()
                        .expect("completion"),
                "total_tokens must equal prompt + completion"
            );
        }

        /// A turn that ends without a `Completed` event still ends cleanly.
        /// The final chunk leaves `usage` out rather than reporting zeros.
        #[test]
        fn the_final_chunk_omits_usage_when_the_run_carries_none() {
            let events = run_with(vec![text("Hello.")]);
            let chunk = final_chunk(&events);

            assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
            assert!(
                chunk.get("usage").is_none_or(Value::is_null),
                "usage must be absent, not zeroed: {chunk}"
            );
        }

        #[test]
        fn aura_usage_is_emitted_at_stream_end() {
            let events = run_with(vec![
                text("Hello."),
                completed("Hello.", 12, 7),
                billed(12, 7),
            ]);
            let usage = events_by_type(&events, event_names::USAGE);

            assert_eq!(usage.len(), 1, "expected one aura.usage");
            let json = payload(usage[0]);
            assert_eq!(json["prompt_tokens"], 12);
            assert_eq!(json["completion_tokens"], 7);
            assert_eq!(json["total_tokens"], 19);
        }

        #[test]
        fn a_text_turn_streams_content_chunks_in_order() {
            let events = run_scoped(
                false,
                "cs-basic",
                ["Count: ", "1", "2", "3"].into_iter().map(text).collect(),
            );

            let chunks: Vec<Value> = events.iter().map(payload).collect();
            assert!(
                chunks.len() > 1,
                "expected token-by-token delivery, got {}",
                chunks.len()
            );
            for chunk in &chunks {
                assert_eq!(chunk["object"], "chat.completion.chunk");
                assert_eq!(chunk["id"], "chatcmpl-test");
                assert!(chunk["choices"].is_array(), "chunk missing choices");
            }
            assert_eq!(content_of(&events), "Count: 123");
        }

        /// The answer in the run's `Completed` event replaces the collected
        /// deltas in the turn's content, which the non-streaming response
        /// returns.
        #[test]
        fn the_runs_answer_is_the_turns_content() {
            let mut projector = projector(true);
            project_run(
                &mut projector,
                vec![text("Hel"), text("lo"), completed("Hello.", 1, 1)],
                finished(),
            );
            assert_eq!(projector.state().accumulated_content, "Hello.");
        }
    }

    /// The request-bound `billed_usage` suite, written as the run's events.
    /// The run's billed usage reaches `aura.usage` as exactly the frame the
    /// request-bound path writes for it.
    mod billed_usage {
        use super::*;
        use aura::stream_events::AuraStreamEvent;

        fn billed(prompt: u64, completion: u64, cache: Option<(u64, u64)>) -> AgentEvent {
            single(AgentEventPayload::Usage {
                usage: TokenUsage {
                    prompt_tokens: TokenCount::new(prompt),
                    completion_tokens: TokenCount::new(completion),
                    total_tokens: TokenCount::new(prompt + completion),
                },
                cache_read_input_tokens: cache.map(|(read, _)| TokenCount::new(read)),
                cache_creation_input_tokens: cache.map(|(_, creation)| TokenCount::new(creation)),
            })
        }

        #[test]
        fn the_usage_frame_is_the_one_the_request_bound_path_writes() {
            let mut projector = projector(true);
            let (frames, pulse) = frames_of(&mut projector, billed(120, 30, Some((7, 3))));
            assert!(frames.is_empty(), "the reading goes out with the ending");
            assert_eq!(pulse, Pulse::Activity);

            let (frames, _) = end_of(&mut projector, finished());
            let expected = AuraStreamEvent::usage(
                120,
                30,
                150,
                Some((7, 3)),
                aura::stream_events::CorrelationContext::new(SESSION_ID, None),
            )
            .format_sse();
            assert_eq!(frames.len(), 3, "the reading, the final chunk, [DONE]");
            assert_eq!(body_of(&frames[..1]), expected);
        }

        /// No frame goes out when nothing was billed, or when custom events
        /// are off. The event still counts as activity.
        #[test]
        fn no_usage_frame_for_nothing_billed_or_with_custom_events_off() {
            for (custom_events, prompt) in [(true, 0), (false, 120)] {
                let mut projector = projector(custom_events);
                let (frames, pulse) = frames_of(&mut projector, billed(prompt, 30, None));
                assert!(frames.is_empty());
                assert_eq!(pulse, Pulse::Activity);
                let (frames, _) = end_of(&mut projector, finished());
                assert_eq!(frames.len(), 2, "the final chunk, then [DONE]");
            }
        }
    }

    /// The request-bound `run_events` suite, written as the run's events.
    /// On the runtime path the run's events are the projection's only
    /// source, and each arrives once.
    mod run_events {
        use super::*;
        use aura_events::{NumberOrString, Progress, ProgressToken};

        /// A worker's event keeps the worker's agent id. Under orchestration
        /// the turn's own agent is the coordinator. A worker id makes this
        /// test fail if the projection drops `agent` or replaces it with the
        /// turn's agent.
        #[test]
        fn a_carried_agent_reaches_the_frame() {
            let event = AgentEvent::new(
                EmittingAgent::worker("log_worker", None, "coordinator"),
                AgentEventPayload::ToolProgress {
                    progress_token: ProgressToken(NumberOrString::Number(7)),
                    progress: Progress::ratio(1.0, 4.0),
                    message: Some("half".to_string()),
                },
            );

            let frame = body_of(&frames_of(&mut projector(true), event).0);

            assert!(
                frame.contains(r#""agent_id":"log_worker""#),
                "the carried agent must reach the frame, got: {frame}"
            );
            assert!(
                !frame.contains(r#""agent_id":"coordinator""#),
                "nothing may override a carried agent, got: {frame}"
            );
        }

        #[test]
        fn progress_without_a_message_renders_its_counts() {
            let event = single(AgentEventPayload::ToolProgress {
                progress_token: ProgressToken(NumberOrString::Number(7)),
                progress: Progress::ratio(3.0, 4.0),
                message: None,
            });

            let frame = body_of(&frames_of(&mut projector(true), event).0);
            assert!(frame.contains("Progress: 3/4"), "got: {frame}");
        }

        fn approval() -> AgentEvent {
            single(AgentEventPayload::ApprovalRequested(
                aura_events::ApprovalRequested {
                    decision_id: "dec_1".to_string(),
                    tool_name: "kubectl_apply".to_string(),
                    tool_namespace: None,
                    origin: aura_events::ApprovalOriginWire::ConfigGate {
                        matched_pattern: "kubectl_*".to_string(),
                        agent_name: "ops".to_string(),
                    },
                    scope: aura_events::AgentScopeWire::Single { session_id: None },
                },
            ))
        }

        /// An approval is protocol rather than telemetry. A client that never
        /// sees one cannot answer it, so the run stalls waiting on a human who
        /// was never asked. It goes out whether or not custom events are on.
        #[test]
        fn an_approval_reaches_the_wire_with_custom_events_off() {
            let (frames, _) = frames_of(&mut projector(false), approval());
            assert_eq!(
                frames.len(),
                1,
                "an approval must not be gated by AURA_CUSTOM_EVENTS"
            );
        }

        /// Tool telemetry is opt-in, and the custom-events setting gates it.
        /// Approvals are not gated. The event still counts as activity.
        #[test]
        fn tool_telemetry_stays_off_with_custom_events_off() {
            let event = single(AgentEventPayload::ToolProgress {
                progress_token: ProgressToken(NumberOrString::Number(7)),
                progress: Progress::ratio(1.0, 2.0),
                message: None,
            });

            let (frames, pulse) = frames_of(&mut projector(false), event);
            assert!(frames.is_empty());
            assert_eq!(pulse, Pulse::Activity);
        }

        fn tool_start(agent: EmittingAgent, task_id: Option<usize>) -> AgentEvent {
            AgentEvent::new(
                agent,
                AgentEventPayload::ToolStart {
                    tool_call_id: "call_1".into(),
                    tool_name: "search_logs".into(),
                    progress_token: None,
                    arguments: Some(serde_json::json!({"query": "errors"})),
                    task_id,
                },
            )
        }

        /// The run's stream carries an orchestrated agent's tool start once.
        /// Its only frame is the orchestration frame that the request-bound
        /// path renders from the item stream. This covers workers and the
        /// coordinator's own calls, which carry the agent id `main` with the
        /// coordinator as parent. A single agent's tool start renders as
        /// `aura.tool_start`.
        #[test]
        fn an_orchestrated_agents_tool_start_is_one_orchestration_frame() {
            let worker = tool_start(EmittingAgent::orchestrated("log_worker"), Some(0));
            let coordinator_call = tool_start(
                EmittingAgent::worker(
                    aura_events::CONVERSATION_AGENT_ID,
                    None,
                    aura_events::COORDINATOR_AGENT_ID,
                ),
                None,
            );
            for event in [worker, coordinator_call] {
                let body = body_of(&frames_of(&mut projector(true), event).0);
                let (events, _) = parse_sse_stream(&body);
                let types: Vec<_> = events.iter().map(|e| e.event_type.as_deref()).collect();
                assert_eq!(
                    types,
                    [Some(
                        aura_events::orchestration::event_names::TOOL_CALL_STARTED
                    )],
                    "got: {body}"
                );
            }

            let single = tool_start(EmittingAgent::single_agent(), None);
            let frame = body_of(&frames_of(&mut projector(true), single).0);
            assert!(
                frame.contains(&format!("event: {}", event_names::TOOL_START)),
                "got: {frame}"
            );
            assert!(frame.contains(r#""tool_id":"call_1""#), "got: {frame}");
        }

        /// Content goes out as the OpenAI chunk only, with no `aura.*` frame
        /// beside it.
        #[test]
        fn content_has_no_side_channel_frame() {
            let body = body_of(&frames_of(&mut projector(true), text("hello")).0);
            let (events, _) = parse_sse_stream(&body);

            assert_eq!(events.len(), 1, "got: {body}");
            assert!(events[0].event_type.is_none(), "got: {body}");
            assert_eq!(content_of(&events), "hello");
        }
    }

    /// How the stream ends for each run ending, and for a gap.
    mod endings {
        use super::*;

        fn assert_closes_with_the_final_chunk(frames: &[Bytes]) {
            let body = body_of(frames);
            let (events, done) = parse_sse_stream(&body);
            assert!(done, "the stream must end with [DONE]: {body}");
            assert!(body.ends_with("data: [DONE]\n\n"), "got: {body}");
            assert_eq!(
                final_chunk(&events)["choices"][0]["finish_reason"],
                "stop",
                "got: {body}"
            );
        }

        #[test]
        fn a_finished_a_parked_and_a_cancelled_run_end_with_the_final_chunk() {
            let cancelled = |reason| LifecycleEvent::Cancelled {
                reason,
                message: None,
                usage: TokenUsage::default(),
            };
            let endings = [
                (finished(), RunEnding::Finished),
                (
                    LifecycleEvent::Parked {
                        checkpoint: CheckpointRef::new("run:parked").expect("a checkpoint"),
                        usage: TokenUsage::default(),
                    },
                    RunEnding::Parked,
                ),
                (
                    cancelled(RunCancelReason::Deadline {
                        after: std::time::Duration::from_secs(30),
                    }),
                    RunEnding::Cancelled {
                        reason: RunCancelReason::Deadline {
                            after: std::time::Duration::from_secs(30),
                        },
                        message: None,
                    },
                ),
                (
                    cancelled(RunCancelReason::External),
                    RunEnding::Cancelled {
                        reason: RunCancelReason::External,
                        message: None,
                    },
                ),
                (
                    cancelled(RunCancelReason::Shutdown),
                    RunEnding::Cancelled {
                        reason: RunCancelReason::Shutdown,
                        message: None,
                    },
                ),
                // A reason a newer writer journaled, which this binary does
                // not know.
                (
                    cancelled(RunCancelReason::Unknown),
                    RunEnding::Cancelled {
                        reason: RunCancelReason::Unknown,
                        message: None,
                    },
                ),
            ];
            for (event, expected) in endings {
                let (frames, ending) = end_of(&mut projector(true), event);
                assert_eq!(ending, Ending::Run(expected));
                assert_eq!(frames.len(), 2, "the final chunk, then [DONE]");
                assert_closes_with_the_final_chunk(&frames);
            }
        }

        /// The run stopped because its last claimant left, so nobody is left
        /// to read anything more.
        #[test]
        fn an_unclaimed_run_ends_with_nothing() {
            let (frames, ending) = end_of(
                &mut projector(true),
                LifecycleEvent::Cancelled {
                    reason: RunCancelReason::Unclaimed,
                    message: None,
                    usage: TokenUsage::default(),
                },
            );
            assert!(frames.is_empty());
            assert_eq!(
                ending,
                Ending::Run(RunEnding::Cancelled {
                    reason: RunCancelReason::Unclaimed,
                    message: None,
                })
            );
        }

        fn failed(error: &str) -> LifecycleEvent {
            LifecycleEvent::Failed {
                error: error.to_string(),
                usage: TokenUsage::default(),
            }
        }

        /// Matches the request-bound loop's error branch. The stream sends
        /// the provider's error, in full only with the debug flag on, then
        /// the final chunk.
        #[test]
        fn a_failed_run_ends_with_the_providers_error_then_the_final_chunk() {
            let error = "upstream exploded: 502 Bad Gateway";

            let mut debug = projector_with(
                RunId::mint(),
                config(true).with_debug_provider_errors(true),
                SESSION_ID,
            );
            let (frames, ending) = end_of(&mut debug, failed(error));
            assert_eq!(
                ending,
                Ending::Run(RunEnding::Failed {
                    error: error.to_string()
                })
            );
            assert_closes_with_the_final_chunk(&frames);
            let (events, _) = parse_sse_stream(&body_of(&frames));
            assert_eq!(
                content_of(&events),
                format!("The upstream model provider (test/fake) returned an error: {error}")
            );

            let (frames, _) = end_of(&mut projector(true), failed(error));
            let (events, _) = parse_sse_stream(&body_of(&frames));
            let content = content_of(&events);
            assert!(
                content
                    .starts_with("The upstream model provider (test/fake) returned an error and"),
                "got: {content}"
            );
            assert!(
                !content.contains("502"),
                "the detail stays hidden: {content}"
            );
        }

        #[test]
        fn a_context_overflow_ends_with_the_guidance() {
            let mut projector = projector(true);
            let (frames, _) = end_of(
                &mut projector,
                failed("This model's maximum context length is 128000 tokens"),
            );
            let (events, _) = parse_sse_stream(&body_of(&frames));
            assert!(
                content_of(&events).starts_with("My tools returned more data"),
                "got: {}",
                content_of(&events)
            );
            assert_closes_with_the_final_chunk(&frames);
        }

        /// The subscription lost events the store no longer holds. The
        /// client gets the fell-behind text, then the stream closes the way a
        /// finished one does.
        #[test]
        fn a_gap_ends_with_the_fell_behind_text_then_the_final_chunk() {
            let mut projector = projector(true);
            frames_of(&mut projector, text("Partial "));
            let gap = JournalItem::Gap {
                resumed_at: SequenceNumber::try_from(9).expect("a sequence number"),
            };

            let Projected::End { frames, ending } = projector.project(&gap) else {
                panic!("a gap ends the wire");
            };
            assert_eq!(ending, Ending::Gap);
            assert_eq!(frames.len(), 3, "the text, the final chunk, then [DONE]");
            assert_closes_with_the_final_chunk(&frames);
            let (events, _) = parse_sse_stream(&body_of(&frames));
            assert_eq!(content_of(&events), super::super::GAP_MESSAGE);
            assert_eq!(
                projector.state().accumulated_content,
                format!("Partial {}", super::super::GAP_MESSAGE)
            );
        }
    }

    /// Items that reach the projector but are not this run's to render.
    mod not_this_run {
        use super::*;
        use aura_events::run::{DetachCause, Liveness, Observer, ObserverId, ObserverKind};

        fn nothing(projector: &mut SseProjector, item: &JournalItem) {
            assert!(
                matches!(projector.project(item), Projected::Nothing),
                "expected nothing for {item:?}"
            );
        }

        #[test]
        fn another_runs_events_and_ending_are_nothing() {
            let mut projector = projector(true);
            let other = RunId::mint();
            nothing(&mut projector, &agent_item(other, text("not mine")));
            nothing(&mut projector, &lifecycle_item(other, finished()));
            assert!(projector.state().accumulated_content.is_empty());
        }

        #[test]
        fn the_sessions_own_events_are_nothing() {
            let mut projector = projector(true);
            let run = projector.run();
            let observer = Observer {
                id: ObserverId::new("obs_1").expect("an observer id"),
                kind: ObserverKind::Claiming,
                presence: true,
            };
            nothing(
                &mut projector,
                &item(None, SessionEventPayload::Agent(text("no run"))),
            );
            for event in [
                LifecycleEvent::Started {
                    agent: "a".to_string(),
                    prompt: "q".to_string(),
                    timeout: None,
                    liveness: Liveness::default(),
                    continues: None,
                },
                LifecycleEvent::ObserverAttached {
                    observer: observer.clone(),
                },
                LifecycleEvent::ObserverDetached {
                    observer,
                    because: DetachCause::Released,
                },
                LifecycleEvent::ClaimsExhausted,
            ] {
                nothing(&mut projector, &lifecycle_item(run, event));
            }
        }

        #[test]
        fn an_entry_this_binary_cannot_decode_is_nothing() {
            nothing(
                &mut projector(true),
                &JournalItem::Unknown {
                    seq: SequenceNumber::try_from(3).expect("a sequence number"),
                    raw: Bytes::from_static(b"{\"kind\":\"from_the_future\"}"),
                },
            );
        }
    }

    /// The pulse each event gives the body's timers.
    mod pulses {
        use super::*;
        use aura::ToolName;
        use aura_events::{McpServerStatus, ToolCallId};

        fn session_info() -> AgentEvent {
            single(AgentEventPayload::SessionInfo {
                model: "test/fake".to_string(),
                model_context_limit: Some(TokenCount::new(128_000)),
            })
        }

        fn mcp_status() -> AgentEvent {
            single(AgentEventPayload::McpStatus {
                servers: vec![McpServerStatus {
                    server_name: "logs".to_string(),
                    transport: "http_streamable".to_string(),
                    status: "connected".to_string(),
                    tools_count: 3,
                    reason: None,
                }],
            })
        }

        fn skills_rehydrated() -> AgentEvent {
            single(AgentEventPayload::SkillsRehydrated {
                skills: vec!["triage".to_string()],
            })
        }

        /// The runtime sends the opening events, not the provider, so they
        /// do not count as the first chunk. They are custom events, so they
        /// have no frame with custom events off.
        #[test]
        fn the_opening_events_are_custom_and_not_the_first_chunk() {
            let opening = [
                (session_info(), event_names::SESSION_INFO),
                (skills_rehydrated(), event_names::SKILLS_REHYDRATED),
                (mcp_status(), event_names::MCP_STATUS),
            ];
            for (event, name) in opening {
                let (frames, pulse) = frames_of(&mut projector(true), event.clone());
                assert_eq!(pulse, Pulse::Opening);
                let body = body_of(&frames);
                assert!(body.starts_with(&format!("event: {name}\n")), "got: {body}");

                let (frames, pulse) = frames_of(&mut projector(false), event);
                assert_eq!(pulse, Pulse::Opening);
                assert!(frames.is_empty());
            }
        }

        fn complete(agent: EmittingAgent, task_id: Option<usize>) -> AgentEvent {
            AgentEvent::new(
                agent,
                AgentEventPayload::ToolComplete {
                    tool_call_id: ToolCallId::new("call_1"),
                    tool_name: ToolName::new("search_logs"),
                    duration_ms: 5,
                    outcome: ToolOutcome::Success {
                        result: "ok".to_string(),
                        format: ResultFormat::Text,
                    },
                    task_id,
                },
            )
        }

        /// A single agent's tool call pauses the inactivity window from the
        /// request to the result. An orchestrated agent's calls count as
        /// plain activity, as their items do on the request-bound path.
        #[test]
        fn a_single_agents_tool_call_pauses_and_resumes_the_window() {
            let mut projector = projector(true);
            let requested = single(AgentEventPayload::ToolRequested {
                tool_call_id: ToolCallId::new("call_1"),
                tool_name: ToolName::new("search_logs"),
                arguments: serde_json::json!({}),
                unparsed_arguments: None,
            });
            assert_eq!(frames_of(&mut projector, requested).1, Pulse::ToolStarted);
            assert_eq!(
                frames_of(
                    &mut projector,
                    complete(EmittingAgent::single_agent(), None)
                )
                .1,
                Pulse::ToolFinished
            );

            let worker = EmittingAgent::worker("log_worker", None, "coordinator");
            let (frames, pulse) = frames_of(&mut projector, complete(worker, Some(0)));
            assert_eq!(pulse, Pulse::Activity);
            let (events, _) = parse_sse_stream(&body_of(&frames));
            assert_eq!(
                events[0].event_type.as_deref(),
                Some(aura_events::orchestration::event_names::TOOL_CALL_COMPLETED)
            );
        }

        /// Every other event of the run is activity, whether or not it has
        /// a frame.
        #[test]
        fn the_runs_other_events_are_activity_with_or_without_a_frame() {
            let events = [
                text("hi"),
                single(AgentEventPayload::WorkerPhase {
                    phase: aura_events::WorkerPhase::Planning,
                    task_id: None,
                }),
                single(AgentEventPayload::Completed {
                    content: "hi".to_string(),
                    usage: TokenUsage::default(),
                }),
            ];
            for custom_events in [true, false] {
                let mut projector = projector(custom_events);
                for event in events.clone() {
                    assert_eq!(frames_of(&mut projector, event).1, Pulse::Activity);
                }
            }
        }
    }

    /// Context and scratchpad readings, sent or held back by the
    /// custom-events setting just as the request-bound path does.
    mod usage_readings {
        use super::*;
        use aura::stream_events::{AuraStreamEvent, CorrelationContext};

        fn context_usage(agent: EmittingAgent) -> AgentEvent {
            AgentEvent::new(
                agent,
                AgentEventPayload::ContextUsage {
                    context_tokens: TokenCount::new(900),
                    response_tokens: TokenCount::new(40),
                    context_window: Some(TokenCount::new(128_000)),
                },
            )
        }

        fn by_id(agent_id: &str) -> EmittingAgent {
            EmittingAgent {
                agent_id: agent_id.to_string(),
                agent_name: None,
                parent_agent_id: None,
            }
        }

        #[test]
        fn a_single_agents_context_reading_is_custom_telemetry() {
            let mut off = projector(false);
            let (frames, _) = frames_of(&mut off, context_usage(EmittingAgent::single_agent()));
            assert!(frames.is_empty());
            let (frames, _) = end_of(&mut off, finished());
            assert_eq!(frames.len(), 2, "the final chunk, then [DONE]");

            let mut on = projector(true);
            let (frames, _) = frames_of(&mut on, context_usage(EmittingAgent::single_agent()));
            assert!(frames.is_empty(), "the reading goes out with the ending");
            let (frames, _) = end_of(&mut on, finished());
            assert_eq!(frames.len(), 3, "the reading, the final chunk, [DONE]");
            let expected = AuraStreamEvent::context_usage(
                900,
                40,
                Some(128_000),
                EmittingAgent::single_agent(),
                CorrelationContext::new(SESSION_ID, None),
            )
            .format_sse();
            assert_eq!(body_of(&frames[..1]), expected);
        }

        /// An orchestrated run's context readings go out whatever the
        /// setting. Each names its agent by id only, as the item stream does.
        #[test]
        fn an_orchestrated_runs_context_reading_goes_out_by_agent_id() {
            for agent_id in ["log_worker", aura_events::CONVERSATION_AGENT_ID] {
                let agent = EmittingAgent::worker(agent_id, None, "coordinator");
                let (frames, _) = frames_of(&mut projector(false), context_usage(agent));
                let expected = AuraStreamEvent::context_usage(
                    900,
                    40,
                    Some(128_000),
                    by_id(agent_id),
                    CorrelationContext::new(SESSION_ID, None),
                )
                .format_sse();
                assert_eq!(body_of(&frames), expected);
            }
        }

        #[test]
        fn a_scratchpad_reading_goes_out_by_agent_id_whatever_the_setting() {
            let event = AgentEvent::new(
                EmittingAgent::worker("log_worker", None, "coordinator"),
                AgentEventPayload::ScratchpadUsage {
                    tokens_intercepted: TokenCount::new(5_000),
                    tokens_extracted: TokenCount::new(300),
                },
            );
            let (frames, _) = frames_of(&mut projector(false), event);
            let expected = AuraStreamEvent::scratchpad_usage(
                5_000,
                300,
                by_id("log_worker"),
                CorrelationContext::new(SESSION_ID, None),
            )
            .format_sse();
            assert_eq!(body_of(&frames), expected);
        }
    }

    /// Where the run's closing usage readings go relative to its ending.
    /// Each test runs the same turn through the request-bound loop (as
    /// stream items plus hook usage) and through the projector (as run
    /// events), and compares the two.
    mod closing_order {
        use super::*;
        use crate::streaming::{StreamTermination, StreamingCallbacks, process_sse_stream_full};
        use aura::{ResponseContent, StreamError, StreamItem, StreamingAgent, UsageState};
        use aura_test_utils::mock_agent::{MockAgent, Step, items};
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        /// Runs `script` through the request-bound loop, with `usage` as the
        /// hook's counts, and returns the SSE events it writes and how it
        /// ended.
        async fn request_bound(
            script: Vec<Result<StreamItem, StreamError>>,
            usage: UsageState,
        ) -> (Vec<SseEvent>, StreamTermination) {
            let agent = MockAgent::scripted(script.into_iter().map(Step::item).collect());
            let run_id = agent.run_id().to_string();
            let stream = agent
                .stream("q", vec![], aura::streaming::RunOptions::default(), &run_id)
                .await
                .into_events();
            let callbacks = StreamingCallbacks {
                request_id: run_id,
                agent: Arc::new(MockAgent::pending()),
                agent_events: None,
                usage_state: usage,
                response_content: ResponseContent::new(),
                model_name: "test/fake".to_string(),
                stream_shutdown_token: CancellationToken::new(),
                rehydrated_skills: vec![],
            };
            let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(64);
            let collector = tokio::spawn(async move {
                let mut body = String::new();
                while let Some(chunk) = chunk_rx.recv().await {
                    let chunk: Bytes = chunk.expect("an SSE chunk");
                    body.push_str(std::str::from_utf8(&chunk).expect("UTF-8"));
                }
                body
            });
            let ctx = TurnContext::new(
                "chatcmpl-test".to_string(),
                "test/fake".to_string(),
                1_700_000_000,
                None,
                SESSION_ID,
            );
            let termination = process_sse_stream_full(
                &config(true),
                &ctx,
                stream,
                chunk_tx,
                CancellationToken::new(),
                None,
                Duration::from_secs(86_400),
                None,
                None,
                callbacks,
            )
            .await;
            let body = collector.await.expect("the collector");
            let (events, done) = parse_sse_stream(&body);
            assert!(done, "the request-bound stream ends with [DONE]: {body}");
            (events, termination)
        }

        /// The SSE events the projector writes for the run's `events` and
        /// `ending`.
        fn projected(events: Vec<AgentEvent>, ending: LifecycleEvent) -> Vec<SseEvent> {
            let body = project_run(&mut projector(true), events, ending);
            let (events, done) = parse_sse_stream(&body);
            assert!(done, "the projected stream ends with [DONE]: {body}");
            events
        }

        /// Each frame's event type, or what kind of OpenAI chunk it is.
        fn kinds(events: &[SseEvent]) -> Vec<String> {
            events
                .iter()
                .map(|event| match &event.event_type {
                    Some(event_type) => event_type.clone(),
                    None if payload(event)["choices"][0]["finish_reason"].is_string() => {
                        "final chunk".to_string()
                    }
                    None => "content chunk".to_string(),
                })
                .collect()
        }

        /// Each frame's event type and data, as the wire carries them.
        fn frames(events: &[SseEvent]) -> Vec<(Option<String>, String)> {
            events
                .iter()
                .map(|event| (event.event_type.clone(), event.data.clone()))
                .collect()
        }

        fn session_info() -> AgentEvent {
            single(AgentEventPayload::SessionInfo {
                model: "test/fake".to_string(),
                model_context_limit: None,
            })
        }

        fn tokens(prompt: u64, completion: u64) -> TokenUsage {
            TokenUsage {
                prompt_tokens: TokenCount::new(prompt),
                completion_tokens: TokenCount::new(completion),
                total_tokens: TokenCount::new(prompt + completion),
            }
        }

        /// The closing readings the run's task appends: the run's billed
        /// usage, then the single agent's context occupancy.
        fn closing_readings(cache: Option<(u64, u64)>) -> Vec<AgentEvent> {
            vec![
                single(AgentEventPayload::Usage {
                    usage: tokens(120, 30),
                    cache_read_input_tokens: cache.map(|(read, _)| TokenCount::new(read)),
                    cache_creation_input_tokens: cache
                        .map(|(_, creation)| TokenCount::new(creation)),
                }),
                single(AgentEventPayload::ContextUsage {
                    context_tokens: TokenCount::new(120),
                    response_tokens: TokenCount::new(30),
                    context_window: None,
                }),
            ]
        }

        /// The hook's counts for one turn of 120 input and 30 output tokens.
        fn hook_usage(cache: Option<(u64, u64)>) -> UsageState {
            let usage = UsageState::new();
            usage.store_usage(120, 30, 150, false);
            if let Some((read, creation)) = cache {
                usage.store_cache_usage(read, creation);
            }
            usage
        }

        #[tokio::test(start_paused = true)]
        async fn a_failed_run_sends_its_error_text_before_the_closing_readings() {
            let cache = Some((7, 3));
            let (original, termination) = request_bound(
                vec![items::text("Partial "), Err("upstream exploded".into())],
                hook_usage(cache),
            )
            .await;
            assert_eq!(
                termination,
                StreamTermination::StreamError("upstream exploded".to_string())
            );
            assert_eq!(
                kinds(&original),
                [
                    event_names::SESSION_INFO,
                    "content chunk",
                    "content chunk",
                    event_names::USAGE,
                    event_names::CONTEXT_USAGE,
                    "final chunk",
                ],
                "the request-bound loop's order"
            );

            let mut events = vec![session_info(), text("Partial ")];
            events.extend(closing_readings(cache));
            let projected = projected(
                events,
                LifecycleEvent::Failed {
                    error: "upstream exploded".to_string(),
                    usage: tokens(120, 30),
                },
            );

            assert_eq!(frames(&projected), frames(&original));
        }

        #[tokio::test(start_paused = true)]
        async fn a_finished_run_sends_the_closing_readings_before_the_final_chunk() {
            let final_item = Ok(StreamItem::Final(aura::FinalResponseInfo {
                content: "Hello.".to_string(),
                usage: rig::completion::Usage {
                    input_tokens: 120,
                    output_tokens: 30,
                    total_tokens: 150,
                },
                cache_usage: None,
            }));
            let (original, termination) =
                request_bound(vec![items::text("Hello."), final_item], hook_usage(None)).await;
            assert_eq!(termination, StreamTermination::Complete);
            assert_eq!(
                kinds(&original),
                [
                    event_names::SESSION_INFO,
                    "content chunk",
                    event_names::USAGE,
                    event_names::CONTEXT_USAGE,
                    "final chunk",
                ],
                "the request-bound loop's order"
            );

            let mut events = vec![
                session_info(),
                text("Hello."),
                single(AgentEventPayload::Completed {
                    content: "Hello.".to_string(),
                    usage: tokens(120, 30),
                }),
            ];
            events.extend(closing_readings(None));
            let projected = projected(events, finished());

            assert_eq!(frames(&projected), frames(&original));
        }
    }
}
