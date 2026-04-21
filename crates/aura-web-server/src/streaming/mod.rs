//! SSE streaming module for OpenAI-compatible chat completions.
//!
//! This module handles Server-Sent Events (SSE) streaming for the `/v1/chat/completions`
//! endpoint when `stream: true` is requested.
//!
//! # Module Structure
//!
//! - `types`: SSE-specific types (chunks, deltas, config, state)
//! - `handlers`: Stream processing logic and item handlers
//!
//! # Usage
//!
//! ```ignore
//! use streaming::{process_sse_stream_full, StreamingCallbacks, StreamConfig, TurnContext};
//!
//! let callbacks = StreamingCallbacks {
//!     request_id: request_id.clone(),
//!     agent: agent.clone(),
//!     tool_event_rx,
//!     progress_rx,
//!     tool_usage_rx,
//!     usage_state,
//!     response_content,
//!     model_name: model_str.clone(),
//! };
//!
//! let termination = process_sse_stream_full(
//!     &config, &ctx, stream, tx, cancel_tx,
//!     timeout_duration, heartbeat_interval, callbacks
//! ).await;
//! ```

mod handlers;
mod otel;
mod types;

// Main streaming functions
pub use handlers::{
    StreamOutcome, StreamTermination, StreamingCallbacks, collect_stream_to_completion,
    process_sse_stream_full,
};

// OTel context for agent.stream span
pub use otel::StreamOtelContext;

// Streaming configuration and context
pub use types::{StreamConfig, ToolResultMode, TurnContext};

// Types for consumers of CollectedResult
pub use types::openai::UsageInfo;

// Types exported for tests
#[cfg(test)]
pub use types::{ChatCompletionChunkDelta, MessageRole, truncate_result};
