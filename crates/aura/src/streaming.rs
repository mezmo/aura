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
//! use aura::{StreamingAgent, StreamItem, StreamError};
//! use tokio_util::sync::CancellationToken;
//!
//! async fn handle_request(agent: impl StreamingAgent, query: &str) {
//!     let cancel_token = CancellationToken::new();
//!     let stream = agent.stream(query, vec![], cancel_token, "req_123").await?;
//!
//!     // Process stream items (convert to SSE, etc.)
//!     while let Some(item) = stream.next().await {
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
use tokio_util::sync::CancellationToken;

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

    /// Stream a completion response.
    ///
    /// Returns a stream of `StreamItem`s. The caller is responsible for:
    /// - Converting items to SSE bytes (via handlers)
    /// - Sending to the client
    /// - Handling cancellation on disconnect
    ///
    /// # Arguments
    ///
    /// * `query` - The user's query/message
    /// * `chat_history` - Previous messages in the conversation
    /// * `cancel_token` - Token for cancellation (e.g., on client disconnect)
    /// * `request_id` - HTTP request ID for MCP progress routing and tool correlation
    ///
    /// # Returns
    ///
    /// A boxed stream of `StreamItem` results, or an error if streaming cannot start.
    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<Message>,
        cancel_token: CancellationToken,
        request_id: &str,
    ) -> Result<BoxStream<'static, Result<StreamItem, StreamError>>, StreamError>;

    /// Stream with timeout support.
    ///
    /// This is the primary entry point for production use. It wraps the stream
    /// with timeout handling and integrates with the cancellation hook.
    ///
    /// # Arguments
    ///
    /// * `query` - The user's query/message
    /// * `chat_history` - Previous messages in the conversation
    /// * `timeout` - Maximum duration for the entire stream
    /// * `request_id` - Request ID for MCP cancellation correlation
    ///
    /// # Returns
    ///
    /// A tuple of (stream, cancel_sender, usage_state) where cancel_sender can
    /// be used to signal cancellation to the underlying provider and usage_state
    /// tracks token consumption via Rig hooks.
    async fn stream_with_timeout(
        &self,
        query: &str,
        chat_history: Vec<Message>,
        timeout: Duration,
        request_id: &str,
    ) -> (
        BoxStream<'static, Result<StreamItem, StreamError>>,
        tokio::sync::watch::Sender<bool>,
        UsageState,
    );

    /// Cancel in-flight MCP requests and close connections.
    ///
    /// Called on client disconnect or timeout to propagate `notifications/cancelled`
    /// to MCP servers. Returns the number of cancelled requests.
    async fn cancel_and_close_mcp(&self, request_id: &str, reason: &str) -> usize;

    /// Return the configured context window size in tokens (from TOML config).
    /// Returns `None` if not configured (e.g., Orchestrator).
    fn context_window(&self) -> Option<u64> {
        None
    }

    /// Whether this agent is an orchestrator (emits per-phase LLM spans).
    ///
    /// Returned `true` by the multi-agent `OrchestratorFactory`. The web-server
    /// streaming handler uses this to suppress the total token write on the
    /// root `agent.stream` span, because each phase (`orchestration.planning`,
    /// `orchestration.worker`, `orchestration.synthesis`,
    /// `orchestration.evaluation`) already carries its own `set_token_usage`
    /// attributes. Letting Phoenix roll those descendants up is the accurate
    /// aggregate; also recording the total on the parent double-counts.
    fn is_orchestration(&self) -> bool {
        false
    }
}
