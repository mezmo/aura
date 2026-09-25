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
//! use aura::{StreamError, StreamItem};
//! use futures::StreamExt;
//!
//! async fn handle_request(agent: impl StreamingAgent, query: &str) {
//!     // The default leaves the run unbounded and lets it mint its own token.
//!     let run = agent.stream(query, vec![], RunOptions::default(), "req_123").await;
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

use crate::provider_agent::{StreamError, StreamItem};
use crate::streaming_request_hook::UsageState;
use async_trait::async_trait;
use futures::stream::BoxStream;
use rig::completion::Message;
use std::time::Duration;
use tokio_util::sync::{CancellationToken, DropGuard};

/// How a run is bounded and cancelled.
///
/// A caller that must be able to cancel before `stream` returns, or that wants
/// the run to stop when a token it already holds is cancelled, names that
/// token; otherwise the run mints one and hands it back on the handle.
#[derive(Default)]
pub struct RunOptions {
    pub timeout: Option<Duration>,
    /// Private because the run cancels this when its stream is dropped, which
    /// a token shared with anything else must not be subject to.
    /// [`RunOptions::cancelled_by`] is what makes it a child.
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
}

/// A started run: the events it produces, the token that cancels it, and the
/// usage it accumulates.
pub struct AgentRun {
    events: BoxStream<'static, Result<StreamItem, StreamError>>,
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
            cancel,
            usage,
        }
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
