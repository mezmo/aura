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
//! use aura::{RequestId, StreamError, StreamItem};
//! use futures::StreamExt;
//!
//! async fn handle_request(agent: impl StreamingAgent, query: &str) {
//!     // The default leaves the run unbounded and lets it mint its own token.
//!     let run = agent
//!         .stream(query, vec![], RunOptions::default(), &RequestId::generate())
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

use crate::provider_agent::{StreamError, StreamItem, StreamedAssistantContent};
use crate::run_context::RunContext;
use crate::streaming_request_hook::UsageState;
use async_trait::async_trait;
use futures::stream::BoxStream;
use rig::completion::Message;
use std::time::Duration;
use tokio_util::sync::{CancellationToken, DropGuard};

/// How a run is bounded and cancelled.
#[derive(Default)]
pub struct RunOptions {
    pub timeout: Option<Duration>,
    cancel: Option<CancellationToken>,
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
        }
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
        }
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
        }
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

/// Copies a run's content onto its event stream as the items pass, so an
/// observer of the run sees what an agent is saying and not only what it is
/// doing.
///
/// The `StreamItem` stream stays the content path every consumer reads; this
/// puts the same content where a consumer built on the run's events can reach
/// it, without changing what the existing ones see. The run is passed rather
/// than read from scope because this wraps the stream from outside it.
///
/// `agent` is whose content this stream carries. An orchestrated run's answer
/// is the coordinator's, not that of any worker that fed it.
pub fn tee_content<S>(
    run: std::sync::Arc<RunContext>,
    agent: aura_events::AgentContext,
    inner: S,
) -> impl futures::Stream<Item = Result<StreamItem, StreamError>>
where
    S: futures::Stream<Item = Result<StreamItem, StreamError>>,
{
    async_stream::stream! {
        for await item in inner {
            if let Ok(item) = &item
                && let Some(payload) = content_of(item)
            {
                run.emit(aura_events::agent::AgentEvent::new(agent.clone(), payload))
                    .await;
            }
            yield item;
        }
    }
}

/// The content an item carries, or `None` for an item that carries none. Tool
/// lifecycle and progress reach the run from their producers instead, so an
/// item that only marks them has nothing to copy.
fn content_of(item: &StreamItem) -> Option<aura_events::agent::AgentEventPayload> {
    use aura_events::agent::AgentEventPayload as Payload;

    match item {
        StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(content)) => {
            Some(Payload::TextDelta {
                content: content.clone(),
            })
        }
        StreamItem::StreamAssistantItem(StreamedAssistantContent::ReasoningDelta {
            delta, ..
        }) => Some(Payload::Reasoning {
            content: delta.clone(),
            task_id: None,
        }),
        StreamItem::Final(final_response) => Some(Payload::Completed {
            content: final_response.content.clone(),
            usage: aura_events::TokenUsage {
                prompt_tokens: aura_events::TokenCount::new(final_response.usage.input_tokens),
                completion_tokens: aura_events::TokenCount::new(final_response.usage.output_tokens),
                total_tokens: aura_events::TokenCount::new(final_response.usage.total_tokens),
            },
        }),
        _ => None,
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

    /// Start a run.
    ///
    /// `options` bounds the run and may hand it a token the caller already
    /// holds. The returned handle owns the events, the token that cancels them,
    /// and the usage they accumulate.
    ///
    /// `request_id` correlates MCP progress and tool events for this run.
    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<Message>,
        options: RunOptions,
        request_id: &crate::domain::RequestId,
    ) -> AgentRun;

    /// Cancel in-flight MCP requests and close connections.
    ///
    /// Called on client disconnect or timeout to propagate `notifications/cancelled`
    /// to MCP servers. Returns the number of cancelled requests.
    async fn cancel_and_close_mcp(
        &self,
        request_id: &crate::domain::RequestId,
        reason: &str,
    ) -> usize;

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

    mod content {
        use super::*;
        use crate::provider_agent::FinalResponseInfo;
        use aura_events::agent::AgentEventPayload as Payload;

        fn say(content: &str) -> Result<StreamItem, StreamError> {
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text(content.to_string()),
            ))
        }

        fn finish(content: &str) -> Result<StreamItem, StreamError> {
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

        async fn teed(items: Vec<Result<StreamItem, StreamError>>) -> (usize, Vec<Payload>) {
            let (_, passed, seen) = teed_as(aura_events::AgentContext::single_agent(), items).await;
            (passed, seen)
        }

        async fn teed_as(
            agent: aura_events::AgentContext,
            items: Vec<Result<StreamItem, StreamError>>,
        ) -> (Vec<aura_events::AgentContext>, usize, Vec<Payload>) {
            let (run, mut events) = RunContext::channel(crate::domain::RequestId::generate());
            let passed = tee_content(run, agent, futures::stream::iter(items))
                .collect::<Vec<_>>()
                .await
                .len();

            let mut agents = Vec::new();
            let mut payloads = Vec::new();
            while let Ok(event) = events.try_recv() {
                agents.push(event.agent);
                payloads.push(event.payload);
            }
            (agents, passed, payloads)
        }

        /// The items still reach the consumer that reads them today; the copy is
        /// additional, not a diversion.
        #[tokio::test]
        async fn every_item_still_passes_through() {
            let (passed, _) = teed(vec![say("a"), say("b"), finish("ab")]).await;
            assert_eq!(passed, 3);
        }

        #[tokio::test]
        async fn text_reaches_the_run_in_order() {
            let (_, seen) = teed(vec![say("Hello "), say("world")]).await;

            let deltas: Vec<_> = seen
                .iter()
                .filter_map(|payload| match payload {
                    Payload::TextDelta { content } => Some(content.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(deltas, vec!["Hello ", "world"]);
        }

        /// The final item carries the whole answer and its billed usage, which is
        /// what an observer needs to close out a run it only watched.
        #[tokio::test]
        async fn the_final_item_reaches_the_run_as_a_completed_run() {
            let (_, seen) = teed(vec![say("hi"), finish("hi there")]).await;

            let completed = seen
                .iter()
                .find_map(|payload| match payload {
                    Payload::Completed { content, usage } => {
                        Some((content.as_str(), usage.total_tokens.get()))
                    }
                    _ => None,
                })
                .expect("a completed run");
            assert_eq!(completed, ("hi there", 15));
        }

        /// An orchestrated run's answer is the coordinator's. A worker feeds it,
        /// so attributing the content to whoever produced the item would name the
        /// wrong agent to an observer deciding who said what.
        #[tokio::test]
        async fn orchestrated_content_is_the_coordinators() {
            let (agents, _, seen) = teed_as(
                aura_events::AgentContext::coordinator(),
                vec![say("the answer"), finish("the answer")],
            )
            .await;

            assert!(
                !seen.is_empty(),
                "the coordinator's content reaches the run"
            );
            for agent in &agents {
                assert_eq!(
                    agent,
                    &aura_events::AgentContext::coordinator(),
                    "every copied item is attributed to the coordinator"
                );
            }
        }

        /// A run nobody observes has dropped its receiver, so the copy fails and
        /// the items must still pass.
        #[tokio::test]
        async fn an_unobserved_run_still_streams_its_items() {
            let (run, events) = RunContext::channel(crate::domain::RequestId::generate());
            drop(events);

            let passed = tee_content(
                run,
                aura_events::AgentContext::single_agent(),
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
