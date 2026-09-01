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

/// Cancelling reaches the provider through the same token the agent races
/// internally, so callers need no second mechanism.
pub struct AgentRun {
    events: BoxStream<'static, Result<StreamItem, StreamError>>,
    cancel: CancellationToken,
    usage: UsageState,
}

impl AgentRun {
    pub fn new(
        events: BoxStream<'static, Result<StreamItem, StreamError>>,
        cancel: CancellationToken,
        usage: UsageState,
    ) -> Self {
        Self {
            events,
            cancel,
            usage,
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn usage(&self) -> &UsageState {
        &self.usage
    }

    pub fn into_events(self) -> BoxStream<'static, Result<StreamItem, StreamError>> {
        self.events
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
    /// `timeout` bounds the whole run; `None` leaves it unbounded. The returned
    /// handle owns the events, the token that cancels them, and the usage they
    /// accumulate.
    ///
    /// `request_id` correlates MCP progress and tool events for this run.
    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<Message>,
        timeout: Option<Duration>,
        request_id: &str,
    ) -> AgentRun;

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
