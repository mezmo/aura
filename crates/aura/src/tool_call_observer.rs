//! Tool call observer for orchestrator visibility into tool execution.
//!
//! This module provides a broadcast channel (Observer pattern) that captures tool calls
//! and results without interrupting Rig's natural iteration loop. The orchestrator can
//! subscribe to tool events to gain awareness of what workers are doing without modifying
//! the core tool execution flow.
//!
//! # Design Philosophy
//!
//! The observer pattern allows coordinators to observe tool execution while workers
//! iterate naturally through Rig's chat/stream loop. Events are emitted at tool
//! execution boundaries, providing:
//!
//! - Real-time visibility into tool calls (name, arguments)
//! - Error classification with retry hints
//! - Duration tracking for performance analysis
//!
//! # Current Status
//!
//! **Phase 1.5**: Observer is instantiated but not yet wired to SSE event streaming.
//! **Phase 2**: Will be connected to `OrchestratorEvent` for real-time tool call
//! visibility during parallel worker execution.
//!
//! # Usage
//!
//! ```ignore
//! use aura::tool_call_observer::{ToolCallObserver, ToolEvent};
//!
//! // Coordinator creates observer and subscribes
//! let (observer, mut rx) = ToolCallObserver::new(16);
//!
//! // Worker emits events during tool execution
//! observer.emit(ToolEvent::call_started("tool_id", "search", json!({"query": "test"})));
//!
//! // Coordinator receives events
//! while let Ok(event) = rx.recv().await {
//!     match event {
//!         ToolEvent::CallStarted { tool_name, .. } => println!("Tool started: {}", tool_name),
//!         ToolEvent::CallCompleted { result, .. } => println!("Tool completed: {:?}", result),
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio::sync::broadcast;

/// Events emitted during tool execution for orchestrator visibility.
#[derive(Debug, Clone)]
pub enum ToolEvent {
    /// A tool call has started execution.
    CallStarted {
        /// Unique identifier for this tool call (e.g., "call_abc123")
        tool_call_id: String,
        /// Name of the tool being called
        tool_name: String,
        /// Arguments passed to the tool (parsed JSON)
        arguments: serde_json::Value,
        /// The ID of the orchestrator or worker that initiated the tool call
        tool_initiator_id: String,
        /// Timestamp when the call started
        timestamp: Instant,
    },

    /// A tool call has completed (success or failure).
    CallCompleted {
        /// The tool call ID this result corresponds to
        tool_call_id: String,
        /// The outcome of the tool call
        result: ToolOutcome,
        /// How long the call took in milliseconds
        duration_ms: u64,
    },
}

impl ToolEvent {
    /// Create a new CallStarted event.
    pub fn call_started(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_initiator_id: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self::CallStarted {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            tool_initiator_id: tool_initiator_id.into(),
            timestamp: Instant::now(),
        }
    }

    /// Create a successful CallCompleted event.
    pub fn call_completed_success(
        tool_call_id: impl Into<String>,
        result: impl Into<String>,
        duration_ms: u64,
    ) -> Self {
        Self::CallCompleted {
            tool_call_id: tool_call_id.into(),
            result: ToolOutcome::Success(result.into()),
            duration_ms,
        }
    }

    /// Create a failed CallCompleted event.
    pub fn call_completed_error(
        tool_call_id: impl Into<String>,
        message: impl Into<String>,
        retry_hint: Option<RetryHint>,
        duration_ms: u64,
    ) -> Self {
        Self::CallCompleted {
            tool_call_id: tool_call_id.into(),
            result: ToolOutcome::Error {
                message: message.into(),
                retry_hint,
            },
            duration_ms,
        }
    }
}

/// Outcome of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutcome {
    /// Tool executed successfully with the given result.
    Success(String),
    /// Tool execution failed.
    Error {
        /// Error message describing what went wrong.
        message: String,
        /// Hint for whether/how to retry.
        retry_hint: Option<RetryHint>,
    },
}

impl ToolOutcome {
    /// Returns true if this outcome represents success.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }

    /// Returns the error message if this is an error outcome.
    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error { message, .. } => Some(message),
            Self::Success(_) => None,
        }
    }
}

/// Hint for whether and how to retry a failed tool call.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RetryHint {
    /// Retry immediately - transient error.
    Immediate,
    /// Retry after a backoff period.
    Backoff {
        /// Suggested wait time in milliseconds.
        suggested_ms: u64,
    },
    /// Do not retry - permanent error.
    NoRetry,
}

impl RetryHint {
    /// Classify an error message into a retry hint.
    ///
    /// This heuristic examines error messages to determine if the error
    /// is transient (retry), rate-limited (backoff), or permanent (no retry).
    pub fn from_error_message(message: &str) -> Self {
        let lower = message.to_lowercase();

        // Rate limiting - suggest backoff
        if lower.contains("rate limit")
            || lower.contains("too many requests")
            || lower.contains("429")
        {
            return Self::Backoff { suggested_ms: 5000 };
        }

        // Transient network errors - retry immediately
        if lower.contains("timeout")
            || lower.contains("connection reset")
            || lower.contains("connection refused")
            || lower.contains("network")
            || lower.contains("temporary")
        {
            return Self::Immediate;
        }

        // Server errors (5xx) - retry with backoff
        if lower.contains("500")
            || lower.contains("502")
            || lower.contains("503")
            || lower.contains("504")
            || lower.contains("internal server error")
            || lower.contains("service unavailable")
        {
            return Self::Backoff { suggested_ms: 1000 };
        }

        // Client errors (4xx except 429) - don't retry
        if lower.contains("400")
            || lower.contains("401")
            || lower.contains("403")
            || lower.contains("404")
            || lower.contains("not found")
            || lower.contains("unauthorized")
            || lower.contains("forbidden")
            || lower.contains("invalid")
        {
            return Self::NoRetry;
        }

        // Default: suggest single retry with short backoff
        Self::Backoff { suggested_ms: 500 }
    }
}

/// Broadcast observer for tool execution visibility.
///
/// The observer allows coordinators to observe tool execution events
/// without modifying the core execution flow. Multiple subscribers
/// can receive events concurrently.
///
/// # Phase 2 Integration
///
/// This will be wired to emit `OrchestratorEvent::ToolCallStarted` and
/// `OrchestratorEvent::ToolCallCompleted` events for real-time SSE streaming
/// during parallel worker execution.
pub struct ToolCallObserver {
    /// Sender side of the broadcast channel.
    event_tx: broadcast::Sender<ToolEvent>,
}

impl ToolCallObserver {
    /// Create a new tool call observer with the given channel capacity.
    ///
    /// Returns the observer and an initial receiver. Additional receivers
    /// can be created via [`subscribe`](Self::subscribe).
    ///
    /// # Arguments
    ///
    /// * `capacity` - Buffer size for the broadcast channel. Recommended: 16-64
    ///   for typical workloads. If receivers lag behind, oldest events are dropped.
    pub fn new(capacity: usize) -> (Self, broadcast::Receiver<ToolEvent>) {
        let (event_tx, rx) = broadcast::channel(capacity);
        (Self { event_tx }, rx)
    }

    /// Create a new subscriber for tool events.
    ///
    /// Each subscriber receives all events emitted after subscribing.
    /// If a subscriber falls too far behind, it will receive a `Lagged` error.
    pub fn subscribe(&self) -> broadcast::Receiver<ToolEvent> {
        self.event_tx.subscribe()
    }

    /// Emit a tool event to all subscribers.
    ///
    /// If there are no active subscribers, the event is silently dropped.
    /// This is by design - the observer is passive and doesn't require subscribers.
    pub fn emit(&self, event: ToolEvent) {
        // Ignore send errors - no subscribers is fine
        let _ = self.event_tx.send(event);
    }

    /// Returns the number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.event_tx.receiver_count()
    }
}

impl Clone for ToolCallObserver {
    fn clone(&self) -> Self {
        Self {
            event_tx: self.event_tx.clone(),
        }
    }
}

impl Default for ToolCallObserver {
    fn default() -> Self {
        Self::new(32).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_outcome_success() {
        let outcome = ToolOutcome::Success("result".to_string());
        assert!(outcome.is_success());
        assert!(outcome.error_message().is_none());
    }

    #[test]
    fn test_tool_outcome_error() {
        let outcome = ToolOutcome::Error {
            message: "failed".to_string(),
            retry_hint: Some(RetryHint::Immediate),
        };
        assert!(!outcome.is_success());
        assert_eq!(outcome.error_message(), Some("failed"));
    }

    #[test]
    fn test_retry_hint_rate_limit() {
        let hint = RetryHint::from_error_message("Error: rate limit exceeded");
        assert!(matches!(hint, RetryHint::Backoff { suggested_ms: 5000 }));
    }

    #[test]
    fn test_retry_hint_timeout() {
        let hint = RetryHint::from_error_message("Connection timeout");
        assert_eq!(hint, RetryHint::Immediate);
    }

    #[test]
    fn test_retry_hint_not_found() {
        let hint = RetryHint::from_error_message("Tool not found: invalid_tool");
        assert_eq!(hint, RetryHint::NoRetry);
    }

    #[test]
    fn test_retry_hint_server_error() {
        let hint = RetryHint::from_error_message("Internal server error (500)");
        assert!(matches!(hint, RetryHint::Backoff { suggested_ms: 1000 }));
    }

    #[tokio::test]
    async fn test_observer_emit_and_receive() {
        let (observer, mut rx) = ToolCallObserver::new(8);

        observer.emit(ToolEvent::call_started(
            "call_1",
            "search",
            "worker",
            serde_json::json!({"query": "test"}),
        ));

        let event = rx.recv().await.unwrap();
        match event {
            ToolEvent::CallStarted {
                tool_call_id,
                tool_name,
                tool_initiator_id,
                ..
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(tool_name, "search");
                assert_eq!(tool_initiator_id, "worker");
            }
            _ => panic!("Expected CallStarted event"),
        }
    }

    #[tokio::test]
    async fn test_observer_multiple_subscribers() {
        let (observer, mut rx1) = ToolCallObserver::new(8);
        let mut rx2 = observer.subscribe();

        assert_eq!(observer.subscriber_count(), 2);

        observer.emit(ToolEvent::call_completed_success("call_1", "result", 100));

        let event1 = rx1.recv().await.unwrap();
        let event2 = rx2.recv().await.unwrap();

        // Both subscribers should receive the same event
        match (event1, event2) {
            (
                ToolEvent::CallCompleted {
                    tool_call_id: id1, ..
                },
                ToolEvent::CallCompleted {
                    tool_call_id: id2, ..
                },
            ) => {
                assert_eq!(id1, "call_1");
                assert_eq!(id2, "call_1");
            }
            _ => panic!("Expected CallCompleted events"),
        }
    }

    #[test]
    fn test_observer_no_subscribers() {
        let (observer, _) = ToolCallObserver::new(8);
        drop(observer.subscribe()); // Drop the receiver

        // Should not panic even with no subscribers
        observer.emit(ToolEvent::call_started(
            "call_1",
            "test",
            "worker",
            serde_json::Value::Null,
        ));
    }
}
