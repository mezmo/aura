//! Streaming agent trait for unified streaming interface.
//!
//! This module provides a trait abstraction over streaming agents, allowing
//! both single-agent and orchestrated multi-agent modes to be used
//! interchangeably by consumers.
//!
//! # Design Philosophy
//!
//! The trait returns a `Stream` of `StreamItem`s, NOT SSE bytes. This keeps
//! SSE formatting in the web server layer where it belongs, making agents
//! easier to test and allowing orchestrators to emit custom event types.
//!
//! # Usage
//!
//! ```ignore
//! use aura::streaming::{RunOptions, StreamingAgent};
//! use aura::{RunId, StreamError, StreamItem};
//! use futures::StreamExt;
//!
//! // `agent` was built for `run_id` (`AgentBuildContext::run_id`), the one
//! // id it streams under.
//! async fn handle_request(agent: impl StreamingAgent, run_id: RunId, query: &str) {
//!     // The default leaves the run unbounded and lets it mint its own token.
//!     let run = agent
//!         .stream(query, vec![], RunOptions::default(), &run_id.to_string())
//!         .await;
//!     let mut items = run.into_events();
//!
//!     // Process stream items (convert to SSE, etc.)
//!     while let Some(item) = items.next().await {
//!         match item {
//!             Ok(StreamItem::StreamAssistantItem(content)) => { /* ... */ }
//!             Ok(StreamItem::StreamUserItem(content)) => { /* ... */ }
//!             // ...
//!         }
//!     }
//! }
//! ```

use crate::provider_agent::{
    StreamError, StreamItem, StreamedAssistantContent, StreamedUserContent,
};
use crate::run_context::{Attendance, RunContext};
use crate::streaming_request_hook::UsageState;
use async_trait::async_trait;
use aura_events::RunId;
use futures::stream::BoxStream;
use rig::completion::Message;
use std::time::Duration;
use tokio_util::sync::{CancellationToken, DropGuard};

/// How a run is bounded and cancelled, and its session's attendance when it
/// starts.
#[derive(Default)]
pub struct RunOptions {
    pub timeout: Option<Duration>,
    cancel: Option<CancellationToken>,
    attendance: Attendance,
}

impl RunOptions {
    /// The bound and the token, for an implementation building a run.
    #[must_use]
    pub fn into_parts(self) -> (Option<Duration>, Option<CancellationToken>) {
        (self.timeout, self.cancel)
    }

    #[must_use]
    pub fn bounded(timeout: Option<Duration>) -> Self {
        Self {
            timeout,
            cancel: None,
            attendance: Attendance::default(),
        }
    }

    /// Sets the session's attendance as the run starts. A `StreamingAgent`
    /// writes it onto the run's context before the run does any work.
    #[must_use]
    pub fn attended(mut self, attendance: Attendance) -> Self {
        self.attendance = attendance;
        self
    }

    pub fn attendance(&self) -> Attendance {
        self.attendance
    }

    /// The run stops when `parent` does, and stopping the run leaves `parent`
    /// alone — a caller's token is often shared, so the run takes a child of it.
    #[must_use]
    pub fn cancelled_by(mut self, parent: &CancellationToken) -> Self {
        self.cancel = Some(parent.child_token());
        self
    }

    /// The run stops on exactly `cancel`, for a caller that took the token out
    /// of these options to build the run around it.
    #[must_use]
    pub fn on_token(timeout: Option<Duration>, cancel: CancellationToken) -> Self {
        Self {
            timeout,
            cancel: Some(cancel),
            attendance: Attendance::default(),
        }
    }
}

/// Why a run would not stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamRefused {
    /// The caller named a different run.
    #[error("run `{run}` cannot stream under `{requested}`; pass the agent's `run_id()`")]
    OtherId { run: RunId, requested: String },
    /// The run has streamed already.
    #[error("run `{run}` has streamed already; begin a new run to stream again")]
    AlreadyStreamed { run: RunId },
}

/// The claim on a run's one stream.
#[derive(Debug, Default)]
pub struct StreamClaim {
    taken: std::sync::atomic::AtomicBool,
}

impl StreamClaim {
    /// Claims the stream of run `run` for a caller naming `request_id`.
    ///
    /// The id is checked before the claim is taken, so a call naming the wrong
    /// run leaves the stream to the caller that names it. The id must be the
    /// run's own string form, the key every request-keyed registry holds, not
    /// merely the same UUID spelled differently.
    pub fn claim(&self, run: RunId, request_id: &str) -> Result<(), StreamRefused> {
        if run.to_string() != request_id {
            return Err(StreamRefused::OtherId {
                run,
                requested: request_id.to_owned(),
            });
        }
        if self.taken.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err(StreamRefused::AlreadyStreamed { run });
        }
        Ok(())
    }
}

/// A started run: the events it produces, the token that cancels it, and the
/// usage it accumulates.
pub struct AgentRun {
    events: BoxStream<'static, Result<StreamItem, StreamError>>,
    agent_events: Option<tokio::sync::mpsc::Receiver<aura_events::agent::AgentEvent>>,
    cancel: CancellationToken,
    usage: UsageState,
    guard: DropGuard,
    /// The run's context.
    run: Option<std::sync::Arc<RunContext>>,
}

impl AgentRun {
    pub fn new(
        events: BoxStream<'static, Result<StreamItem, StreamError>>,
        cancel: CancellationToken,
        usage: UsageState,
    ) -> Self {
        Self {
            // On the run's own token, because that is what its work watches.
            // `RunOptions::cancelled_by` is what keeps a caller's shared token
            // from being that token.
            guard: cancel.clone().drop_guard(),
            events,
            agent_events: None,
            cancel,
            usage,
            run: None,
        }
    }

    /// Gives the handle the run's context, which the runtime uses to keep
    /// the run's attendance current.
    #[must_use]
    pub fn within(mut self, run: std::sync::Arc<RunContext>) -> Self {
        self.run = Some(run);
        self
    }

    /// The run's context, if the agent gave one through [`within`](Self::within).
    pub fn run_context(&self) -> Option<&std::sync::Arc<RunContext>> {
        self.run.as_ref()
    }

    /// A run that does nothing, whose stream yields `refused` and ends.
    pub fn refused(refused: StreamRefused) -> Self {
        let error: StreamError = Box::new(refused);
        Self::new(
            Box::pin(futures::stream::once(async move { Err(error) })),
            CancellationToken::new(),
            UsageState::new(),
        )
    }

    /// Hands the run's events to its observer. A run whose producers emit
    /// through [`crate::run_context::RunContext::emit`] pairs the sender it
    /// scopes with the receiver named here.
    #[must_use]
    pub fn observed_by(
        mut self,
        receiver: tokio::sync::mpsc::Receiver<aura_events::agent::AgentEvent>,
    ) -> Self {
        self.agent_events = Some(receiver);
        self
    }

    /// Takes the run's events, which one observer reads.
    pub fn take_agent_events(
        &mut self,
    ) -> Option<tokio::sync::mpsc::Receiver<aura_events::agent::AgentEvent>> {
        self.agent_events.take()
    }

    /// Orchestration races this token, so cancelling it stops a run at once.
    /// A single agent reads it from the streaming hook's callbacks, so a run
    /// stalled with no provider output needs its MCP calls cancelled too.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn usage(&self) -> &UsageState {
        &self.usage
    }

    /// Wraps the run's stream, keeping the cancellation and usage that belong
    /// with it. A layer that decorates the stream has no reason to take the
    /// handle apart and rebuild it.
    #[must_use]
    pub fn map_stream<F>(self, f: F) -> Self
    where
        F: FnOnce(
            BoxStream<'static, Result<StreamItem, StreamError>>,
        ) -> BoxStream<'static, Result<StreamItem, StreamError>>,
    {
        Self {
            events: f(self.events),
            ..self
        }
    }

    /// Dropping the returned stream cancels the run.
    ///
    /// A consumer that goes away without draining — an aborted task, a dropped
    /// stream — would otherwise leave the run spending provider turns nobody
    /// reads, and a run started with no timeout has nothing else to stop it.
    /// The guard moves with the stream, so the run outlives the handle for as
    /// long as something is reading it.
    pub fn into_events(self) -> BoxStream<'static, Result<StreamItem, StreamError>> {
        Box::pin(CancelOnDrop {
            _guard: self.guard,
            inner: self.events,
        })
    }
}

/// Wraps an item stream and copies some of its items onto the run's event
/// channel as they pass. These are the events the session's journal needs
/// that nothing else sends. They let an observer of the run see what the
/// agent says, not only what it does. The copies are:
///
/// - text as `TextDelta`, reasoning deltas as `Reasoning`, and the final
///   response as `Completed`, all attributed to `agent`;
/// - each tool result as a `ToolComplete` attributed to `agent`, with the
///   name of the call it answers and the time since that call's item passed;
/// - a single agent's closing `ScratchpadUsage`, attributed to the agent id
///   in the item.
///
/// No other item is copied. A tool call already reaches the channel as the
/// streaming hook's `ToolRequested`. The orchestrator's emitter sends its own
/// events, readings and content to the channel itself. Every item still
/// reaches the stream's reader, unchanged and in the same order.
///
/// A tool result is a failure exactly when [`ToolResult::failure`] says so.
/// Either way the full text is copied, and each reader decides what to do
/// with it: truncate it, render it raw, or look for the passthrough marker
/// in it. A result whose call never passed is not copied, because there is
/// no call name to give it.
///
/// [`ToolResult::failure`]: crate::provider_agent::ToolResult::failure
pub fn tee_run_events<S>(
    run: std::sync::Arc<RunContext>,
    agent: aura_events::EmittingAgent,
    inner: S,
) -> BoxStream<'static, Result<StreamItem, StreamError>>
where
    S: futures::Stream<Item = Result<StreamItem, StreamError>> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut ledger = ToolCallLedger::default();
        for await item in inner {
            if let Ok(item) = &item
                && let Some(event) = ledger.copy_of(item, &agent)
            {
                run.emit(event).await;
            }
            yield item;
        }
    })
}

/// A tool call whose result has not passed through the stream yet.
#[derive(Debug)]
struct PendingToolCall {
    id: aura_events::ToolCallId,
    name: aura_events::ToolName,
    /// When the call's item passed through the stream.
    since: std::time::Instant,
}

/// The run's tool calls that are still waiting for their results, in the
/// order the calls were made.
#[derive(Debug, Default)]
struct ToolCallLedger {
    pending: Vec<PendingToolCall>,
}

impl ToolCallLedger {
    /// The event to copy onto the run's channel for `item`, if any. The event
    /// is attributed to `agent` unless the item names its own agent. A tool
    /// call copies nothing, but it is recorded here so that its result can
    /// take the call's name and duration.
    fn copy_of(
        &mut self,
        item: &StreamItem,
        agent: &aura_events::EmittingAgent,
    ) -> Option<aura_events::agent::AgentEvent> {
        use aura_events::agent::{AgentEvent, AgentEventPayload as Payload};

        let payload = match item {
            StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(content)) => {
                Payload::TextDelta {
                    content: content.clone(),
                }
            }
            StreamItem::StreamAssistantItem(StreamedAssistantContent::ReasoningDelta {
                delta,
                ..
            }) => Payload::Reasoning {
                content: delta.clone(),
                task_id: None,
            },
            StreamItem::Final(response) => Payload::Completed {
                content: response.content.clone(),
                usage: aura_events::TokenUsage {
                    prompt_tokens: aura_events::TokenCount::new(response.usage.input_tokens),
                    completion_tokens: aura_events::TokenCount::new(response.usage.output_tokens),
                    total_tokens: aura_events::TokenCount::new(response.usage.total_tokens),
                },
            },
            StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall(call)) => {
                self.pending.push(PendingToolCall {
                    id: aura_events::ToolCallId::new(&call.id),
                    name: aura_events::ToolName::new(&call.name),
                    since: std::time::Instant::now(),
                });
                return None;
            }
            StreamItem::StreamUserItem(StreamedUserContent::ToolResult(result)) => {
                self.complete(result)?
            }
            StreamItem::ScratchpadUsage {
                agent_id,
                tokens_intercepted,
                tokens_extracted,
            } => {
                return Some(AgentEvent::new(
                    aura_events::EmittingAgent {
                        agent_id: agent_id.clone(),
                        agent_name: None,
                        parent_agent_id: None,
                    },
                    Payload::ScratchpadUsage {
                        tokens_intercepted: aura_events::TokenCount::new(
                            *tokens_intercepted as u64,
                        ),
                        tokens_extracted: aura_events::TokenCount::new(*tokens_extracted as u64),
                    },
                ));
            }
            StreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. }
                | StreamedAssistantContent::Reasoning(_),
            )
            | StreamItem::FinalMarker
            | StreamItem::TurnUsage(..)
            | StreamItem::AgentEvent(_)
            | StreamItem::ContextUsage { .. }
            | StreamItem::McpStatus(_) => return None,
        };
        Some(AgentEvent::new(agent.clone(), payload))
    }

    /// Builds the `ToolComplete` for `result` and removes its call from the
    /// ledger. The call is matched on the result's `id` first, then on its
    /// `call_id`.
    fn complete(
        &mut self,
        result: &crate::provider_agent::ToolResult,
    ) -> Option<aura_events::agent::AgentEventPayload> {
        use aura_events::agent::{AgentEventPayload, ToolOutcome};

        let position = self
            .pending
            .iter()
            .position(|call| call.id.as_str() == result.id)
            .or_else(|| {
                let call_id = result.call_id.as_deref()?;
                self.pending
                    .iter()
                    .position(|call| call.id.as_str() == call_id)
            });
        let Some(position) = position else {
            tracing::debug!(
                tool_call_id = %result.id,
                "a tool result whose call never passed is not copied"
            );
            return None;
        };
        let call = self.pending.remove(position);
        let outcome = match result.failure() {
            Some(_) => ToolOutcome::Failure {
                error: result.result.clone(),
            },
            None => ToolOutcome::Success {
                result: result.result.clone(),
                format: result.format(),
            },
        };
        Some(AgentEventPayload::ToolComplete {
            tool_call_id: call.id,
            tool_name: call.name,
            duration_ms: u64::try_from(call.since.elapsed().as_millis()).unwrap_or(u64::MAX),
            outcome,
            task_id: None,
        })
    }
}

/// Cancels its run when dropped, by holding the guard for as long as the stream
/// it wraps.
struct CancelOnDrop<S> {
    inner: S,
    _guard: DropGuard,
}

impl<S: futures::Stream + Unpin> futures::Stream for CancelOnDrop<S> {
    type Item = S::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }
}

/// Trait for agents that produce streaming completions.
///
/// This trait abstracts the streaming iteration loop so that both
/// single-agent and orchestrated multi-agent modes can be used
/// interchangeably by the web server.
///
/// # Implementors
///
/// - `Agent` - Single-agent streaming (default implementation)
/// - `OrchestratorFactory` - Multi-agent orchestration mode
///
/// # Design Notes
///
/// - Returns a `Stream`, not bytes - SSE formatting stays in web server
/// - Clean separation: agent produces semantic items, handlers format them
/// - Easier to test (inspect stream items without parsing SSE)
/// - Orchestrator can emit custom `StreamItem` variants for deep-agent events
#[async_trait]
pub trait StreamingAgent: Send + Sync {
    /// Return the LLM provider name and model identifier.
    ///
    /// Used for OTel attributes and response metadata so the handler never
    /// needs to know the concrete agent type.
    fn get_provider_info(&self) -> (&str, &str);

    /// The run this agent streams.
    fn run_id(&self) -> RunId;

    /// Start a run.
    ///
    /// `options` bounds the run and may hand it a token the caller already
    /// holds. The returned handle owns the events, the token that cancels them,
    /// and the usage they accumulate.
    ///
    /// `request_id` names the run: [`run_id`](Self::run_id), as a string. A
    /// run streams once, and only under that id; any other call returns a run
    /// whose stream yields one [`StreamRefused`] and does nothing.
    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<Message>,
        options: RunOptions,
        request_id: &str,
    ) -> AgentRun;

    /// Cancel in-flight MCP requests and close connections.
    ///
    /// Called on client disconnect or timeout to propagate `notifications/cancelled`
    /// to MCP servers. Returns the number of cancelled requests.
    async fn cancel_and_close_mcp(&self, request_id: &str, reason: &str) -> usize;

    /// The configured context window size in tokens, `None` when the config
    /// sets no window.
    fn context_window(&self) -> Option<u64> {
        None
    }

    /// Snapshot the connection status of every configured MCP server.
    ///
    /// Used by the streaming handler to emit an `aura.mcp_status` event at
    /// stream start so clients can distinguish degraded/unavailable/available servers.
    /// Defaults to empty (no MCP servers, or an implementor without MCP — e.g. the orchestrator,
    /// whose workers own their own managers).
    fn mcp_server_status(&self) -> Vec<aura_events::McpServerStatus> {
        Vec::new()
    }

    /// The discovered skills this agent (or, in orchestration mode, its
    /// coordinator) can load. Defaults to empty for implementors without
    /// skills.
    fn skills(&self) -> &[aura_config::SkillConfig] {
        &[]
    }

    /// The assembled system prompt sent to the provider, if this agent has a
    /// single static one.
    ///
    /// Defaults to `None` for implementors that have no single static prompt:
    /// the orchestrator builds a distinct preamble per coordinator/worker
    /// phase, so there is nothing to report at the agent level.
    fn system_prompt(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::Arc;

    fn run(name: &str) -> RunId {
        crate::run_context::named_run_id(name)
    }

    #[test]
    fn the_first_claim_naming_the_run_streams() {
        let claim = StreamClaim::default();
        assert_eq!(claim.claim(run("a"), &run("a").to_string()), Ok(()));
    }

    #[test]
    fn a_second_claim_is_refused() {
        let claim = StreamClaim::default();
        let id = run("a").to_string();
        claim.claim(run("a"), &id).unwrap();

        assert_eq!(
            claim.claim(run("a"), &id),
            Err(StreamRefused::AlreadyStreamed { run: run("a") })
        );
    }

    /// A call naming the wrong run is refused without taking the stream, so
    /// the caller that names the run still gets it.
    #[test]
    fn a_claim_naming_another_run_leaves_the_stream_untaken() {
        let claim = StreamClaim::default();

        assert_eq!(
            claim.claim(run("a"), "req_123"),
            Err(StreamRefused::OtherId {
                run: run("a"),
                requested: "req_123".to_owned(),
            })
        );
        assert_eq!(claim.claim(run("a"), &run("a").to_string()), Ok(()));
    }

    /// The same UUID spelled another way is not the key the run's registries
    /// hold, so it is refused too.
    #[test]
    fn the_run_id_must_be_spelled_as_the_run_spells_it() {
        let claim = StreamClaim::default();
        let shouted = run("a").to_string().to_uppercase();

        assert!(matches!(
            claim.claim(run("a"), &shouted),
            Err(StreamRefused::OtherId { .. })
        ));
    }

    /// A caller refused for naming the wrong run is told where the right one is.
    #[test]
    fn a_refusal_for_another_run_points_at_run_id() {
        let refused = StreamRefused::OtherId {
            run: run("a"),
            requested: "req_123".to_owned(),
        };
        assert!(refused.to_string().contains("run_id()"), "got: {refused}");
    }

    /// A refused run reports why and ends, and dropping it cancels nothing but
    /// its own token.
    #[tokio::test]
    async fn a_refused_run_yields_its_refusal_and_ends() {
        let refused = AgentRun::refused(StreamRefused::AlreadyStreamed { run: run("a") });
        let items: Vec<_> = refused.into_events().collect().await;

        let [Err(error)] = items.as_slice() else {
            panic!("expected one error, got {items:?}");
        };
        assert_eq!(
            error.downcast_ref::<StreamRefused>(),
            Some(&StreamRefused::AlreadyStreamed { run: run("a") })
        );
    }

    fn empty_run() -> (AgentRun, CancellationToken) {
        let cancel = CancellationToken::new();
        let run = AgentRun::new(
            Box::pin(futures::stream::empty()),
            cancel.clone(),
            UsageState::new(),
        );
        (run, cancel)
    }

    /// A caller that takes a run and drops it without consuming has still
    /// started it — for orchestration the work is already spawned, and with no
    /// timeout nothing else would stop it.
    #[test]
    fn dropping_the_handle_cancels_the_run() {
        let (run, cancel) = empty_run();
        assert!(!cancel.is_cancelled());

        drop(run);
        assert!(cancel.is_cancelled());
    }

    /// The guard moves to the stream, so the run outlives the handle for as
    /// long as someone is reading it.
    #[tokio::test]
    async fn the_run_survives_the_handle_while_its_stream_is_held() {
        let (run, cancel) = empty_run();
        let mut events = run.into_events();

        assert!(!cancel.is_cancelled(), "the stream still holds the run");
        assert!(events.next().await.is_none());
        assert!(!cancel.is_cancelled());

        drop(events);
        assert!(cancel.is_cancelled());
    }

    /// An orchestration run is a spawned task feeding a channel. Dropping the
    /// stream stops that task, so a consumer that goes away does not leave it
    /// spending provider turns nobody reads.
    #[tokio::test]
    async fn dropping_a_spawned_run_stops_its_task() {
        let cancel = CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(4);

        let token = cancel.clone();
        let worked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&worked);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = token.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if tx.send(Ok(StreamItem::FinalMarker)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let run = AgentRun::new(
            Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            })),
            cancel.clone(),
            UsageState::new(),
        );

        drop(run.into_events());
        // Asserted before awaiting, so this isolates the guard: the channel
        // closing would stop the task either way.
        assert!(cancel.is_cancelled(), "dropping the stream cancels the run");
        // Bounded, so a task that keeps running fails here rather than hanging.
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("the task ends rather than running on")
            .expect("the task does not panic");
    }

    /// A caller's token is often shared, so one run's stream going away must
    /// stop that run and nothing else.
    #[test]
    fn dropping_a_run_leaves_the_token_it_inherited_alone() {
        let shared = CancellationToken::new();
        let options = RunOptions::default().cancelled_by(&shared);
        let run = AgentRun::new(
            Box::pin(futures::stream::empty()),
            options.into_parts().1.expect("a child of the shared token"),
            UsageState::new(),
        );

        drop(run);
        assert!(
            !shared.is_cancelled(),
            "the caller's token outlives one run"
        );
    }

    /// Cancelling the caller's token still stops the run.
    #[test]
    fn cancelling_the_inherited_token_stops_the_run() {
        let shared = CancellationToken::new();
        let options = RunOptions::default().cancelled_by(&shared);
        let run_token = options.into_parts().1.expect("a child of the shared token");

        shared.cancel();
        assert!(run_token.is_cancelled());
    }

    mod copy {
        use super::*;
        use crate::passthrough_tool::PASSTHROUGH_MARKER;
        use crate::provider_agent::{FinalResponseInfo, ToolCall, ToolResult, ToolResultKind};
        use aura_events::EmittingAgent;
        use aura_events::agent::ToolOutcome;
        use aura_events::agent::{AgentEvent, AgentEventPayload as Payload, ResultFormat};

        type Item = Result<StreamItem, StreamError>;

        fn say(content: &str) -> Item {
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text(content.to_string()),
            ))
        }

        fn think(delta: &str) -> Item {
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::ReasoningDelta {
                    id: None,
                    delta: delta.to_string(),
                },
            ))
        }

        fn finish(content: &str) -> Item {
            Ok(StreamItem::Final(FinalResponseInfo {
                content: content.to_string(),
                usage: rig::completion::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                },
                cache_usage: None,
            }))
        }

        fn call(id: &str, name: &str) -> Item {
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCall(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: "{}".to_string(),
                }),
            ))
        }

        fn result(id: &str, text: &str, kind: ToolResultKind) -> Item {
            answered(id, None, text, kind)
        }

        fn answered(id: &str, call_id: Option<&str>, text: &str, kind: ToolResultKind) -> Item {
            Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(
                ToolResult {
                    id: id.to_string(),
                    call_id: call_id.map(str::to_string),
                    result: text.to_string(),
                    kind,
                },
            )))
        }

        /// Runs `items` through the copy as a single agent's run. Returns how
        /// many items passed and the events that reached the run's channel.
        async fn copied(items: Vec<Item>) -> (usize, Vec<AgentEvent>) {
            copied_as(EmittingAgent::single_agent(), futures::stream::iter(items)).await
        }

        async fn copied_as(
            agent: EmittingAgent,
            items: impl futures::Stream<Item = Item> + Send + 'static,
        ) -> (usize, Vec<AgentEvent>) {
            let (run, mut events) =
                RunContext::channel(crate::run_context::named_run_id("run_copy"));
            let passed = tee_run_events(run, agent, items)
                .collect::<Vec<_>>()
                .await
                .len();
            let seen = std::iter::from_fn(|| events.try_recv().ok()).collect();
            (passed, seen)
        }

        fn payloads(events: Vec<AgentEvent>) -> Vec<Payload> {
            events.into_iter().map(|event| event.payload).collect()
        }

        /// The name, duration and outcome of the only event on the channel,
        /// which must be a `ToolComplete`.
        fn completion(events: Vec<AgentEvent>) -> (String, u64, ToolOutcome) {
            let [
                AgentEvent {
                    payload:
                        Payload::ToolComplete {
                            tool_name,
                            duration_ms,
                            outcome,
                            task_id: None,
                            ..
                        },
                    ..
                },
            ] = events.as_slice()
            else {
                panic!("expected one ToolComplete, got {events:?}");
            };
            (tool_name.to_string(), *duration_ms, outcome.clone())
        }

        /// Every item still reaches the stream's reader. The copy adds events
        /// and takes no items away.
        #[tokio::test]
        async fn every_item_still_passes_through() {
            let (passed, _) = copied(vec![
                say("a"),
                call("call_1", "list_files"),
                result("call_1", "a.txt", ToolResultKind::Text),
                Ok(StreamItem::FinalMarker),
                finish("a"),
            ])
            .await;
            assert_eq!(passed, 5);
        }

        #[tokio::test]
        async fn content_reaches_the_run_in_order_as_the_agents() {
            let (_, events) = copied(vec![think("Hmm."), say("Hello "), say("world")]).await;

            for event in &events {
                assert_eq!(event.agent, EmittingAgent::single_agent());
            }
            assert!(
                matches!(
                    payloads(events).as_slice(),
                    [
                        Payload::Reasoning { content: reasoning, task_id: None },
                        Payload::TextDelta { content: first },
                        Payload::TextDelta { content: second },
                    ] if reasoning == "Hmm." && first == "Hello " && second == "world"
                ),
                "the deltas arrive in order"
            );
        }

        /// The final item carries the whole answer and its billed usage, which
        /// is what an observer needs to close out a run it only watched.
        #[tokio::test]
        async fn the_final_item_reaches_the_run_as_a_completed_run() {
            let (_, events) = copied(vec![say("hi"), finish("hi there")]).await;

            let completed = payloads(events)
                .into_iter()
                .find_map(|payload| match payload {
                    Payload::Completed { content, usage } => {
                        Some((content, usage.total_tokens.get()))
                    }
                    _ => None,
                })
                .expect("a completed run");
            assert_eq!(completed, ("hi there".to_string(), 15));
        }

        /// A tool result produces a `ToolComplete` for its call. The event
        /// takes the call's name, and its duration is the time since the
        /// call's item passed.
        #[tokio::test]
        async fn a_result_completes_its_call_with_the_calls_name_and_time() {
            let items = async_stream::stream! {
                yield call("call_1", "list_files");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                yield result("call_1", "a.txt b.txt", ToolResultKind::Text);
            };
            let (_, events) = copied_as(EmittingAgent::single_agent(), items).await;

            assert_eq!(events[0].agent, EmittingAgent::single_agent());
            let (name, duration_ms, outcome) = completion(events);
            assert_eq!(name, "list_files");
            assert!(duration_ms >= 20, "measured {duration_ms} ms");
            assert!(
                matches!(
                    &outcome,
                    ToolOutcome::Success { result, format: ResultFormat::Text } if result == "a.txt b.txt"
                ),
                "{outcome:?}"
            );
        }

        /// A failure keeps its full text, including rig's prefixes. The reader
        /// decides how much of it to show.
        #[tokio::test]
        async fn an_error_completes_as_a_failure_with_its_whole_text() {
            let error = "Toolset error: ToolCallError: connection refused";
            let (_, events) = copied(vec![
                call("call_1", "read_file"),
                result("call_1", error, ToolResultKind::Error),
            ])
            .await;

            let (_, _, outcome) = completion(events);
            assert!(
                matches!(&outcome, ToolOutcome::Failure { error: text } if text == error),
                "{outcome:?}"
            );
        }

        /// A result of kind `Error` is a failure, whatever its text says.
        #[tokio::test]
        async fn an_error_without_a_known_prefix_is_still_a_failure() {
            let (_, events) = copied(vec![
                call("call_1", "read_file"),
                result("call_1", "boom", ToolResultKind::Error),
            ])
            .await;

            let (_, _, outcome) = completion(events);
            assert!(
                matches!(&outcome, ToolOutcome::Failure { error } if error == "boom"),
                "{outcome:?}"
            );
        }

        /// Output that starts with the MCP error prefix is a failure. Output
        /// that only looks like an error, such as `Tool execution failed:`, is
        /// a success.
        #[tokio::test]
        async fn output_is_a_failure_only_when_it_reports_one() {
            let (_, events) = copied(vec![
                call("call_1", "fetch"),
                result(
                    "call_1",
                    "Tool returned an error: 401 Unauthorized",
                    ToolResultKind::Text,
                ),
            ])
            .await;
            let (_, _, outcome) = completion(events);
            assert!(
                matches!(&outcome, ToolOutcome::Failure { error } if error == "Tool returned an error: 401 Unauthorized"),
                "{outcome:?}"
            );

            let (_, events) = copied(vec![
                call("call_1", "tail"),
                result(
                    "call_1",
                    "Tool execution failed: disk full on node-3",
                    ToolResultKind::Text,
                ),
            ])
            .await;
            let (_, _, outcome) = completion(events);
            assert!(
                matches!(outcome, ToolOutcome::Success { .. }),
                "{outcome:?}"
            );
        }

        #[tokio::test]
        async fn json_output_says_it_is_json() {
            let (_, events) = copied(vec![
                call("call_1", "stat"),
                result("call_1", r#"{"files":11}"#, ToolResultKind::Json),
            ])
            .await;

            let (_, _, outcome) = completion(events);
            assert!(
                matches!(
                    &outcome,
                    ToolOutcome::Success { result, format: ResultFormat::Json } if result == r#"{"files":11}"#
                ),
                "{outcome:?}"
            );
        }

        /// A reader spots a client tool's yield by the passthrough marker in
        /// its result, so the copy must keep the marker.
        #[tokio::test]
        async fn the_passthrough_marker_survives_the_copy() {
            let marked = format!("\"{PASSTHROUGH_MARKER}\"");
            let (_, events) = copied(vec![
                call("call_1", "get_weather"),
                result("call_1", &marked, ToolResultKind::Text),
            ])
            .await;

            let (_, _, outcome) = completion(events);
            assert!(
                matches!(&outcome, ToolOutcome::Success { result, .. } if result.contains(PASSTHROUGH_MARKER)),
                "{outcome:?}"
            );
        }

        /// Each result is matched to its own call, whatever order the results
        /// come back in. A result that matches only on its `call_id` still
        /// finds its call.
        #[tokio::test]
        async fn each_result_completes_the_call_it_answers() {
            let (_, events) = copied(vec![
                call("call_1", "list_files"),
                call("call_2", "read_file"),
                result("call_2", "contents", ToolResultKind::Text),
                answered("rig_1", Some("call_1"), "a.txt", ToolResultKind::Text),
            ])
            .await;

            let names: Vec<_> = payloads(events)
                .into_iter()
                .map(|payload| match payload {
                    Payload::ToolComplete {
                        tool_call_id,
                        tool_name,
                        ..
                    } => (tool_call_id.to_string(), tool_name.to_string()),
                    other => panic!("expected ToolComplete, got {other:?}"),
                })
                .collect();
            assert_eq!(
                names,
                vec![
                    ("call_2".to_string(), "read_file".to_string()),
                    ("call_1".to_string(), "list_files".to_string()),
                ]
            );
        }

        /// A result whose call never passed has no call name, so it is left
        /// out of the copy instead of being given a made-up name.
        #[tokio::test]
        async fn a_result_without_its_call_is_not_copied() {
            let (passed, events) =
                copied(vec![result("call_9", "orphan", ToolResultKind::Text)]).await;
            assert_eq!(passed, 1);
            assert!(events.is_empty(), "{events:?}");
        }

        /// A single agent's closing scratchpad reading is attributed to the
        /// agent id in the item.
        #[tokio::test]
        async fn a_scratchpad_reading_names_the_agent_it_names() {
            let (_, events) = copied(vec![Ok(StreamItem::ScratchpadUsage {
                agent_id: "main".to_string(),
                tokens_intercepted: 1_200,
                tokens_extracted: 300,
            })])
            .await;

            let [event] = events.as_slice() else {
                panic!("expected one reading, got {events:?}");
            };
            assert_eq!(event.agent, EmittingAgent::single_agent());
            assert!(matches!(
                &event.payload,
                Payload::ScratchpadUsage { tokens_intercepted, tokens_extracted }
                    if tokens_intercepted.get() == 1_200 && tokens_extracted.get() == 300
            ));
        }

        /// Agent events, context readings and MCP status are not copied,
        /// because the orchestrator's emitter puts those on the channel
        /// itself. The final marker, turn usage, tool-call deltas and a whole
        /// reasoning block have nothing to copy.
        #[tokio::test]
        async fn items_with_nothing_to_copy_stay_off_the_channel() {
            let (passed, events) = copied(vec![
                Ok(StreamItem::AgentEvent(Box::new(AgentEvent::new(
                    EmittingAgent::coordinator(),
                    Payload::Synthesizing { iteration: 1 },
                )))),
                Ok(StreamItem::ContextUsage {
                    agent_id: "main".to_string(),
                    context_tokens: 100,
                    response_tokens: 5,
                    context_window: None,
                }),
                Ok(StreamItem::McpStatus(Vec::new())),
                Ok(StreamItem::FinalMarker),
                Ok(StreamItem::TurnUsage(
                    rig::completion::Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 2,
                    },
                    None,
                )),
                Ok(StreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ToolCallDelta {
                        id: "call_1".to_string(),
                        name: None,
                        delta: Some("{".to_string()),
                    },
                )),
                Ok(StreamItem::StreamAssistantItem(
                    StreamedAssistantContent::Reasoning("whole".to_string()),
                )),
            ])
            .await;

            assert_eq!(passed, 7);
            assert!(events.is_empty(), "{events:?}");
        }

        /// When nobody observes the run, its channel's receiver is gone and
        /// every copy fails. The items must still pass through.
        #[tokio::test]
        async fn an_unobserved_run_still_streams_its_items() {
            let (run, events) =
                RunContext::channel(crate::run_context::named_run_id("run_unobserved"));
            drop(events);

            let passed = tee_run_events(
                run,
                EmittingAgent::single_agent(),
                futures::stream::iter(vec![say("a"), finish("a")]),
            )
            .collect::<Vec<_>>()
            .await
            .len();
            assert_eq!(passed, 2);
        }
    }

    /// Decorating the stream must not drop the guard along the way.
    #[test]
    fn mapping_the_stream_keeps_the_run_alive() {
        let (run, cancel) = empty_run();
        let mapped = run.map_stream(|stream| Box::pin(stream));

        assert!(!cancel.is_cancelled());
        drop(mapped);
        assert!(cancel.is_cancelled());
    }
}
