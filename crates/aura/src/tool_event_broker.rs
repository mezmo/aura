//! Request-scoped tool event broker for aura.tool_start events.
//!
//! Routes tool start events from MCP execution to specific HTTP requests only
//! (no cross-customer leakage). Also manages tool_call_id correlation between
//! hook (where LLM decides to call) and execution (where MCP actually runs).
//!
//! ## Event Flow
//!
//! 1. Hook `on_tool_call`: Push tool_call_id to queue, emit `aura.tool_requested`
//! 2. Execution: Peek tool_call_id from queue, emit `aura.tool_start` with progress_token
//! 3. MCP progress: Emit `aura.progress` with progress_token
//! 4. Hook `on_tool_result`: Pop tool_call_id from queue, add to pending_tool_ids
//! 5. Hook `on_stream_completion_response_finish`: Emit `aura.tool_usage` with pending tools
//! 6. Completion: Emit `aura.tool_complete` with tool_call_id
//!
//! ## Sequential Execution Guarantee
//!
//! This design relies on Rig's streaming mode executing tools sequentially.
//! See `docs/rig-tool-execution-order.md` for analysis.
//! The FIFO queue is safe because: hook fires → tool executes → hook fires → next tool.

use crate::request_cancellation::RequestId;
use rmcp::model::ProgressToken;
use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use tokio::sync::{RwLock, mpsc};
use tracing::debug;

/// Channel capacity for tool events per request
const EVENT_CHANNEL_CAPACITY: usize = 32;

// Type aliases for tool identifiers - upgrade to newtypes later
/// The unique ID assigned by the LLM to a specific tool call
pub type ToolCallId = String;
/// The name of a tool (e.g., "search", "list_files")
pub type ToolName = String;

/// Tool lifecycle events routed through the broker.
///
/// Two distinct events in the tool lifecycle:
/// - `Requested`: LLM decided to call (immediate UI feedback, has arguments)
/// - `Start`: MCP execution actually began (has progress_token for correlation)
#[derive(Clone, Debug)]
pub enum ToolLifecycleEvent {
    /// Emitted from hook when LLM decides to call a tool.
    /// Provides immediate UI feedback with the tool arguments.
    Requested {
        tool_id: ToolCallId,
        tool_name: ToolName,
        /// The tool arguments as JSON
        arguments: serde_json::Value,
    },
    /// Emitted from execution context when MCP actually begins.
    /// Provides the progress_token for correlating with aura.progress events.
    Start {
        tool_id: ToolCallId,
        tool_name: ToolName,
        progress_token: Option<ProgressToken>,
    },
}

impl ToolLifecycleEvent {
    /// Get the tool_id regardless of event variant
    pub fn tool_id(&self) -> &str {
        match self {
            ToolLifecycleEvent::Requested { tool_id, .. } => tool_id,
            ToolLifecycleEvent::Start { tool_id, .. } => tool_id,
        }
    }

    /// Get the tool_name regardless of event variant
    pub fn tool_name(&self) -> &str {
        match self {
            ToolLifecycleEvent::Requested { tool_name, .. } => tool_name,
            ToolLifecycleEvent::Start { tool_name, .. } => tool_name,
        }
    }
}

/// Tool usage event emitted when usage information becomes available.
///
/// This associates tool calls with their usage snapshot. When
/// `on_stream_completion_response_finish` fires, all tools that completed
/// since the last usage event are grouped together.
#[derive(Clone, Debug)]
pub struct ToolUsageEvent {
    /// Tool IDs that share this usage snapshot
    pub tool_ids: Vec<String>,
    /// Prompt tokens (context size at this point)
    pub prompt_tokens: u64,
    /// Completion tokens generated
    pub completion_tokens: u64,
    /// Total tokens used
    pub total_tokens: u64,
}

/// Request-scoped tool event broker that routes MCP tool events
/// to specific HTTP requests only.
///
/// Also manages tool_call_id correlation between hook and execution contexts
/// using a simple FIFO queue per request (safe due to sequential execution).
///
/// ## Invariants
///
/// The FIFO queue relies on these invariants for correctness:
/// - For each `push_tool_call_id`, exactly one `pop_tool_call_id` must follow
/// - `peek_tool_call_id` may be called zero or more times between push and pop
/// - Push/pop pairing is maintained by calling push in `on_tool_call` and pop in `on_tool_result`
/// - This works because Rig's streaming mode executes tools sequentially (see module docs)
pub struct ToolEventBroker {
    /// Map of request_id -> event channel sender
    senders: RwLock<HashMap<String, mpsc::Sender<ToolLifecycleEvent>>>,

    /// Map of request_id -> FIFO queue of tool_call_ids
    /// Hook pushes when on_tool_call fires, execution peeks when tool starts,
    /// hook pops when on_tool_result fires.
    /// Sequential execution in streaming mode guarantees correct ordering.
    pending_tool_calls: RwLock<HashMap<String, VecDeque<String>>>,
}

impl ToolEventBroker {
    /// Create a new tool event broker
    pub fn new() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
            pending_tool_calls: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to tool events for a specific request.
    ///
    /// Returns a receiver that will only get tool events for this request.
    /// When the receiver is dropped, the request is automatically unsubscribed.
    pub async fn subscribe(&self, request_id: &str) -> mpsc::Receiver<ToolLifecycleEvent> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

        let mut senders = self.senders.write().await;
        senders.insert(request_id.to_string(), tx);

        debug!(
            "Tool event subscription created for request '{}' (total active: {})",
            request_id,
            senders.len()
        );

        rx
    }

    /// Unsubscribe a request from tool events and clean up pending tool_call_ids.
    pub async fn unsubscribe(&self, request_id: &str) {
        // Release senders lock before acquiring pending lock to reduce contention
        {
            let mut senders = self.senders.write().await;
            if senders.remove(request_id).is_some() {
                debug!(
                    "Tool event subscription removed for request '{}' (remaining: {})",
                    request_id,
                    senders.len()
                );
            }
        } // senders lock released here

        // Clean up pending tool_call_ids for this request
        let mut pending = self.pending_tool_calls.write().await;
        if pending.remove(request_id).is_some() {
            debug!("Pending tool call IDs removed for request '{}'", request_id);
        }
    }

    /// Push a tool_call_id from the hook context.
    ///
    /// Called when the hook's `on_tool_call` fires. The execution context
    /// will peek this ID when the tool actually starts executing.
    /// FIFO order is guaranteed by sequential tool execution in streaming mode.
    pub async fn push_tool_call_id(&self, request_id: &str, tool_call_id: impl Into<String>) {
        let mut pending = self.pending_tool_calls.write().await;
        let queue = pending.entry(request_id.to_string()).or_default();
        let tool_call_id = tool_call_id.into();

        debug!(
            "Pushed tool_call_id '{}' for request '{}' (queue_len: {})",
            tool_call_id,
            request_id,
            queue.len() + 1
        );

        queue.push_back(tool_call_id);
    }

    /// Peek at the next tool_call_id without removing it.
    ///
    /// Called when MCP execution begins to get the tool_call_id for correlation.
    /// The actual removal happens in `on_tool_result` via `pop_tool_call_id`.
    /// This ensures push/pop pairing regardless of tool type.
    ///
    /// Returns `None` if no pending tool_call_id exists.
    pub async fn peek_tool_call_id(&self, request_id: &str) -> Option<String> {
        let pending = self.pending_tool_calls.read().await;

        let queue = pending.get(request_id)?;
        let tool_call_id = queue.front().cloned();

        if let Some(id) = &tool_call_id {
            debug!(
                "Peeked tool_call_id '{}' for request '{}' (queue_len: {})",
                id,
                request_id,
                queue.len()
            );
        } else {
            debug!("No pending tool_call_id for request '{}'", request_id);
        }

        tool_call_id
    }

    /// Pop a tool_call_id from the queue.
    ///
    /// Called from `on_tool_result` after tool execution completes.
    /// This ensures push/pop pairing for ALL tools (MCP and non-MCP).
    ///
    /// Returns `None` if no pending tool_call_id exists.
    pub async fn pop_tool_call_id(&self, request_id: &str) -> Option<String> {
        let mut pending = self.pending_tool_calls.write().await;

        let queue = pending.get_mut(request_id)?;
        let tool_call_id = queue.pop_front();

        if let Some(id) = &tool_call_id {
            debug!(
                "Popped tool_call_id '{}' for request '{}' (remaining: {})",
                id,
                request_id,
                queue.len()
            );

            // Clean up empty queue
            if queue.is_empty() {
                pending.remove(request_id);
            }
        } else {
            debug!("No pending tool_call_id for request '{}'", request_id);
        }

        tool_call_id
    }

    /// Publish a tool event to a specific request.
    ///
    /// Returns `true` if the event was sent, `false` if no subscriber exists.
    pub async fn publish(&self, request_id: &str, event: ToolLifecycleEvent) -> bool {
        let sender = {
            let senders = self.senders.read().await;
            senders.get(request_id).cloned()
        };

        if let Some(sender) = sender {
            match sender.send(event).await {
                Ok(()) => {
                    debug!("Tool event sent to request '{}'", request_id);
                    true
                }
                Err(_) => {
                    debug!(
                        "Tool event receiver dropped for request '{}' (cleaned on unsubscribe)",
                        request_id
                    );
                    false
                }
            }
        } else {
            debug!(
                "No tool event subscriber for request '{}' (event dropped)",
                request_id
            );
            false
        }
    }

    /// Get the number of active subscriptions
    pub async fn active_subscriptions(&self) -> usize {
        self.senders.read().await.len()
    }
}

impl Default for ToolEventBroker {
    fn default() -> Self {
        Self::new()
    }
}

/// Global tool event broker instance
static GLOBAL_BROKER: OnceLock<ToolEventBroker> = OnceLock::new();

/// Get the global tool event broker instance
pub fn global() -> &'static ToolEventBroker {
    GLOBAL_BROKER.get_or_init(ToolEventBroker::new)
}

/// Convenience function to subscribe to tool events for a request
pub async fn subscribe(request_id: &str) -> mpsc::Receiver<ToolLifecycleEvent> {
    global().subscribe(request_id).await
}

/// Convenience function to unsubscribe a request
pub async fn unsubscribe(request_id: &str) {
    global().unsubscribe(request_id).await
}

/// Convenience function to publish a tool event to a request
pub async fn publish(request_id: &str, event: ToolLifecycleEvent) -> bool {
    global().publish(request_id, event).await
}

/// Convenience function to publish a tool_requested event
pub async fn publish_tool_requested(
    request_id: &str,
    tool_id: ToolCallId,
    tool_name: ToolName,
    arguments: serde_json::Value,
) -> bool {
    publish(
        request_id,
        ToolLifecycleEvent::Requested {
            tool_id,
            tool_name,
            arguments,
        },
    )
    .await
}

/// Convenience function to publish a tool_start event
pub async fn publish_tool_start(
    request_id: &str,
    tool_id: ToolCallId,
    tool_name: ToolName,
    progress_token: Option<ProgressToken>,
) -> bool {
    publish(
        request_id,
        ToolLifecycleEvent::Start {
            tool_id,
            tool_name,
            progress_token,
        },
    )
    .await
}

/// Convenience function to push a tool_call_id from hook context.
pub async fn push_tool_call_id(request_id: &RequestId, tool_call_id: String) {
    global().push_tool_call_id(request_id, tool_call_id).await
}

/// Convenience function to peek at the next tool_call_id (for MCP execution).
pub async fn peek_tool_call_id(request_id: &RequestId) -> Option<String> {
    global().peek_tool_call_id(request_id).await
}

/// Convenience function to pop a tool_call_id (from on_tool_result).
pub async fn pop_tool_call_id(request_id: &RequestId) -> Option<String> {
    global().pop_tool_call_id(request_id).await
}

// ============================================================================
// Tool Usage Event Broker (for aura.tool_usage events)
// ============================================================================

/// Channel capacity for tool usage events per request
const USAGE_EVENT_CHANNEL_CAPACITY: usize = 16;

/// Global tool usage event broker instance
static TOOL_USAGE_BROKER: OnceLock<ToolUsageBroker> = OnceLock::new();

/// Broker for ToolUsageEvent routing (separate from ToolLifecycleEvent).
///
/// This handles the `aura.tool_usage` events that associate completed tools
/// with usage snapshots.
struct ToolUsageBroker {
    senders: RwLock<HashMap<String, mpsc::Sender<ToolUsageEvent>>>,
}

impl ToolUsageBroker {
    fn new() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
        }
    }

    async fn subscribe(&self, request_id: &str) -> mpsc::Receiver<ToolUsageEvent> {
        let (tx, rx) = mpsc::channel(USAGE_EVENT_CHANNEL_CAPACITY);
        let mut senders = self.senders.write().await;
        senders.insert(request_id.to_string(), tx);
        debug!(
            "Tool usage subscription created for request '{}' (total: {})",
            request_id,
            senders.len()
        );
        rx
    }

    async fn unsubscribe(&self, request_id: &str) {
        let mut senders = self.senders.write().await;
        if senders.remove(request_id).is_some() {
            debug!(
                "Tool usage subscription removed for request '{}' (remaining: {})",
                request_id,
                senders.len()
            );
        }
    }

    async fn publish(&self, request_id: &str, event: ToolUsageEvent) -> bool {
        let sender = {
            let senders = self.senders.read().await;
            senders.get(request_id).cloned()
        };

        if let Some(sender) = sender {
            match sender.send(event).await {
                Ok(()) => {
                    debug!("Tool usage event sent to request '{}'", request_id);
                    true
                }
                Err(_) => {
                    debug!(
                        "Tool usage receiver dropped for request '{}' (cleaned on unsubscribe)",
                        request_id
                    );
                    false
                }
            }
        } else {
            debug!(
                "No tool usage subscriber for request '{}' (event dropped)",
                request_id
            );
            false
        }
    }
}

/// Get the global tool usage broker instance
fn usage_broker_global() -> &'static ToolUsageBroker {
    TOOL_USAGE_BROKER.get_or_init(ToolUsageBroker::new)
}

/// Subscribe to tool usage events for a request.
///
/// Returns a receiver that will get `aura.tool_usage` events for this request.
pub async fn tool_usage_subscribe(request_id: &str) -> mpsc::Receiver<ToolUsageEvent> {
    usage_broker_global().subscribe(request_id).await
}

/// Unsubscribe from tool usage events.
pub async fn tool_usage_unsubscribe(request_id: &str) {
    usage_broker_global().unsubscribe(request_id).await
}

/// Publish a tool usage event.
///
/// Called from `on_stream_completion_response_finish` when usage becomes available.
/// Associates the listed tool_ids with the usage snapshot.
pub async fn publish_tool_usage(
    request_id: &str,
    tool_ids: Vec<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
) -> bool {
    usage_broker_global()
        .publish(
            request_id,
            ToolUsageEvent {
                tool_ids,
                prompt_tokens,
                completion_tokens,
                total_tokens,
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::NumberOrString;
    use std::sync::Arc;

    fn numeric_token(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    fn string_token(s: &str) -> ProgressToken {
        ProgressToken(NumberOrString::String(Arc::from(s)))
    }

    #[tokio::test]
    async fn test_broker_creation() {
        let broker = ToolEventBroker::new();
        assert_eq!(broker.active_subscriptions().await, 0);
    }

    #[tokio::test]
    async fn test_subscribe_creates_channel() {
        let broker = ToolEventBroker::new();
        let _rx = broker.subscribe("req_123").await;
        assert_eq!(broker.active_subscriptions().await, 1);
    }

    #[tokio::test]
    async fn test_unsubscribe_removes_channel() {
        let broker = ToolEventBroker::new();
        let _rx = broker.subscribe("req_123").await;
        assert_eq!(broker.active_subscriptions().await, 1);

        broker.unsubscribe("req_123").await;
        assert_eq!(broker.active_subscriptions().await, 0);
    }

    #[tokio::test]
    async fn test_publish_start_event() {
        let broker = ToolEventBroker::new();
        let mut rx = broker.subscribe("req_123").await;

        let event = ToolLifecycleEvent::Start {
            tool_id: "call_abc".to_string(),
            tool_name: "list_pipelines".to_string(),
            progress_token: Some(numeric_token(42)),
        };

        let sent = broker.publish("req_123", event).await;
        assert!(sent);

        let received = rx.recv().await.unwrap();
        match received {
            ToolLifecycleEvent::Start {
                tool_id,
                tool_name,
                progress_token,
            } => {
                assert_eq!(tool_id, "call_abc");
                assert_eq!(tool_name, "list_pipelines");
                assert!(progress_token.is_some());
            }
            _ => panic!("Expected ToolLifecycleEvent::Start"),
        }
    }

    #[tokio::test]
    async fn test_publish_requested_event() {
        let broker = ToolEventBroker::new();
        let mut rx = broker.subscribe("req_123").await;

        let event = ToolLifecycleEvent::Requested {
            tool_id: "call_abc".to_string(),
            tool_name: "search".to_string(),
            arguments: serde_json::json!({"query": "test"}),
        };

        let sent = broker.publish("req_123", event).await;
        assert!(sent);

        let received = rx.recv().await.unwrap();
        match received {
            ToolLifecycleEvent::Requested {
                tool_id,
                tool_name,
                arguments,
            } => {
                assert_eq!(tool_id, "call_abc");
                assert_eq!(tool_name, "search");
                assert_eq!(arguments, serde_json::json!({"query": "test"}));
            }
            _ => panic!("Expected ToolLifecycleEvent::Requested"),
        }
    }

    #[tokio::test]
    async fn test_publish_to_unsubscribed_request_fails() {
        let broker = ToolEventBroker::new();

        let event = ToolLifecycleEvent::Start {
            tool_id: "call_abc".to_string(),
            tool_name: "list_pipelines".to_string(),
            progress_token: None,
        };

        let sent = broker.publish("req_nonexistent", event).await;
        assert!(!sent);
    }

    #[tokio::test]
    async fn test_requests_are_isolated() {
        let broker = ToolEventBroker::new();
        let mut rx1 = broker.subscribe("req_1").await;
        let mut rx2 = broker.subscribe("req_2").await;

        let event = ToolLifecycleEvent::Start {
            tool_id: "call_1".to_string(),
            tool_name: "tool_for_req_1".to_string(),
            progress_token: Some(string_token("token_1")),
        };
        broker.publish("req_1", event).await;

        // req_1 should receive it
        let received = rx1.recv().await.unwrap();
        assert_eq!(received.tool_name(), "tool_for_req_1");

        // req_2 should NOT receive anything
        assert!(rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_tool_start_without_progress_token() {
        let broker = ToolEventBroker::new();
        let mut rx = broker.subscribe("req_123").await;

        let event = ToolLifecycleEvent::Start {
            tool_id: "call_xyz".to_string(),
            tool_name: "some_tool".to_string(),
            progress_token: None,
        };

        broker.publish("req_123", event).await;

        let received = rx.recv().await.unwrap();
        match received {
            ToolLifecycleEvent::Start {
                tool_id,
                progress_token,
                ..
            } => {
                assert_eq!(tool_id, "call_xyz");
                assert!(progress_token.is_none());
            }
            _ => panic!("Expected ToolLifecycleEvent::Start"),
        }
    }

    // Tool call ID FIFO queue tests

    #[tokio::test]
    async fn test_push_and_pop_tool_call_id() {
        let broker = ToolEventBroker::new();

        broker.push_tool_call_id("req_1", "call_abc123").await;

        let result = broker.pop_tool_call_id("req_1").await;
        assert_eq!(result, Some("call_abc123".to_string()));

        // Second pop should return None (consumed)
        let result2 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(result2, None);
    }

    #[tokio::test]
    async fn test_pop_nonexistent_returns_none() {
        let broker = ToolEventBroker::new();

        let result = broker.pop_tool_call_id("req_nonexistent").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_fifo_order_preserved() {
        let broker = ToolEventBroker::new();

        // Push multiple tool calls (simulating sequential tool execution)
        broker.push_tool_call_id("req_1", "call_first").await;
        broker.push_tool_call_id("req_1", "call_second").await;
        broker.push_tool_call_id("req_1", "call_third").await;

        // Pops should return in FIFO order
        let first = broker.pop_tool_call_id("req_1").await;
        assert_eq!(first, Some("call_first".to_string()));

        let second = broker.pop_tool_call_id("req_1").await;
        assert_eq!(second, Some("call_second".to_string()));

        let third = broker.pop_tool_call_id("req_1").await;
        assert_eq!(third, Some("call_third".to_string()));

        // Fourth should be None
        let fourth = broker.pop_tool_call_id("req_1").await;
        assert_eq!(fourth, None);
    }

    #[tokio::test]
    async fn test_different_requests_isolated_fifo() {
        let broker = ToolEventBroker::new();

        broker.push_tool_call_id("req_1", "call_for_req_1").await;
        broker.push_tool_call_id("req_2", "call_for_req_2").await;

        // Each request gets its own tool_call_id
        let result1 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(result1, Some("call_for_req_1".to_string()));

        let result2 = broker.pop_tool_call_id("req_2").await;
        assert_eq!(result2, Some("call_for_req_2".to_string()));
    }

    #[tokio::test]
    async fn test_unsubscribe_cleans_up_pending_tool_calls() {
        let broker = ToolEventBroker::new();

        let _rx = broker.subscribe("req_1").await;
        broker.push_tool_call_id("req_1", "call_abc").await;

        // Unsubscribe should clean up both channel and pending tool_call_ids
        broker.unsubscribe("req_1").await;

        // Pop should now return None
        let result = broker.pop_tool_call_id("req_1").await;
        assert_eq!(result, None);
    }

    // Tests for peek_tool_call_id

    #[tokio::test]
    async fn test_peek_returns_front_without_removing() {
        let broker = ToolEventBroker::new();

        broker.push_tool_call_id("req_1", "call_abc").await;

        // Peek should return the item
        let result = broker.peek_tool_call_id("req_1").await;
        assert_eq!(result, Some("call_abc".to_string()));

        // Peek again should return the same item (not consumed)
        let result2 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(result2, Some("call_abc".to_string()));

        // Pop should also return the same item
        let result3 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(result3, Some("call_abc".to_string()));

        // Now it's consumed
        let result4 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(result4, None);
    }

    #[tokio::test]
    async fn test_peek_nonexistent_returns_none() {
        let broker = ToolEventBroker::new();

        let result = broker.peek_tool_call_id("req_nonexistent").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_peek_then_pop_fifo_order() {
        let broker = ToolEventBroker::new();

        // Push multiple items
        broker.push_tool_call_id("req_1", "call_first").await;
        broker.push_tool_call_id("req_1", "call_second").await;

        // Peek returns first item
        let peek1 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(peek1, Some("call_first".to_string()));

        // Pop removes first item
        let pop1 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(pop1, Some("call_first".to_string()));

        // Peek now returns second item
        let peek2 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(peek2, Some("call_second".to_string()));

        // Pop removes second item
        let pop2 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(pop2, Some("call_second".to_string()));

        // Both are now empty
        assert_eq!(broker.peek_tool_call_id("req_1").await, None);
        assert_eq!(broker.pop_tool_call_id("req_1").await, None);
    }

    #[tokio::test]
    async fn test_peek_and_pop_workflow_for_mcp_tools() {
        // This test simulates the actual workflow:
        // 1. on_tool_call pushes tool_call_id
        // 2. MCP execution peeks to get tool_call_id for tool_start event
        // 3. on_tool_result pops to maintain queue integrity
        let broker = ToolEventBroker::new();

        // Simulate on_tool_call
        broker.push_tool_call_id("req_1", "call_mcp_1").await;

        // Simulate MCP execution (peek)
        let for_tool_start = broker.peek_tool_call_id("req_1").await;
        assert_eq!(for_tool_start, Some("call_mcp_1".to_string()));

        // Simulate on_tool_result (pop)
        let cleanup = broker.pop_tool_call_id("req_1").await;
        assert_eq!(cleanup, Some("call_mcp_1".to_string()));

        // Queue is now empty
        assert_eq!(broker.peek_tool_call_id("req_1").await, None);
    }

    #[tokio::test]
    async fn test_non_mcp_tool_workflow() {
        // This test simulates the workflow for non-MCP tools (e.g., vector stores):
        // 1. on_tool_call pushes tool_call_id
        // 2. Non-MCP tool executes (no peek - doesn't go through MCP execution)
        // 3. on_tool_result pops to maintain queue integrity
        let broker = ToolEventBroker::new();

        // Simulate on_tool_call for vector store
        broker.push_tool_call_id("req_1", "call_vector_1").await;

        // No MCP execution (no peek)

        // Simulate on_tool_result (pop)
        let cleanup = broker.pop_tool_call_id("req_1").await;
        assert_eq!(cleanup, Some("call_vector_1".to_string()));

        // Queue is clean
        assert_eq!(broker.peek_tool_call_id("req_1").await, None);
    }

    #[tokio::test]
    async fn test_mixed_tool_workflow() {
        // This test verifies the critical bug fix:
        // Mix of MCP and non-MCP tools should maintain queue integrity
        let broker = ToolEventBroker::new();

        // Tool 1: Vector store (non-MCP)
        broker.push_tool_call_id("req_1", "call_vector").await;
        // Vector store executes (no peek)
        let pop1 = broker.pop_tool_call_id("req_1").await; // on_tool_result
        assert_eq!(pop1, Some("call_vector".to_string()));

        // Tool 2: MCP HTTP tool
        broker.push_tool_call_id("req_1", "call_mcp").await;
        let peek2 = broker.peek_tool_call_id("req_1").await; // MCP execution
        assert_eq!(peek2, Some("call_mcp".to_string()));
        let pop2 = broker.pop_tool_call_id("req_1").await; // on_tool_result
        assert_eq!(pop2, Some("call_mcp".to_string()));

        // Tool 3: Another vector store
        broker.push_tool_call_id("req_1", "call_vector_2").await;
        let pop3 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(pop3, Some("call_vector_2".to_string()));

        // Queue integrity maintained - all clean
        assert_eq!(broker.peek_tool_call_id("req_1").await, None);
    }

    #[tokio::test]
    async fn test_tool_failure_still_pops() {
        // Simulates: tool executes but returns an error
        // on_tool_result still fires (Rig converts errors to strings)
        // Queue must still be cleaned up
        let broker = ToolEventBroker::new();

        // on_tool_call pushes
        broker.push_tool_call_id("req_1", "call_failing_tool").await;

        // MCP execution peeks
        let peek = broker.peek_tool_call_id("req_1").await;
        assert_eq!(peek, Some("call_failing_tool".to_string()));

        // Tool execution fails... but on_tool_result still fires
        // (Rig converts Err(e) to e.to_string() and calls on_tool_result)

        // on_tool_result pops (even on failure)
        let pop = broker.pop_tool_call_id("req_1").await;
        assert_eq!(pop, Some("call_failing_tool".to_string()));

        // Queue is clean despite failure
        assert_eq!(broker.peek_tool_call_id("req_1").await, None);
    }

    #[tokio::test]
    async fn test_multiple_tools_one_fails_queue_stays_synced() {
        // Simulates: tool_1 succeeds, tool_2 fails, tool_3 succeeds
        // Queue must stay in sync throughout
        let broker = ToolEventBroker::new();

        // Tool 1: Success
        broker.push_tool_call_id("req_1", "call_1_success").await;
        let peek1 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(peek1, Some("call_1_success".to_string()));
        // Tool executes successfully
        let pop1 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(pop1, Some("call_1_success".to_string()));

        // Tool 2: Failure (but on_tool_result still fires)
        broker.push_tool_call_id("req_1", "call_2_failure").await;
        let peek2 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(peek2, Some("call_2_failure".to_string()));
        // Tool execution fails...
        let pop2 = broker.pop_tool_call_id("req_1").await; // on_tool_result still fires
        assert_eq!(pop2, Some("call_2_failure".to_string()));

        // Tool 3: Success - must get correct tool_call_id, not stale one
        broker.push_tool_call_id("req_1", "call_3_success").await;
        let peek3 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(peek3, Some("call_3_success".to_string())); // Not call_2!
        let pop3 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(pop3, Some("call_3_success".to_string()));

        // Queue integrity maintained
        assert_eq!(broker.peek_tool_call_id("req_1").await, None);
    }

    #[tokio::test]
    async fn test_non_mcp_tool_failure_queue_integrity() {
        // Simulates: non-MCP tool (vector store) fails
        // No peek happens (non-MCP), but on_tool_result still pops
        let broker = ToolEventBroker::new();

        // Vector store tool (non-MCP) - no peek
        broker.push_tool_call_id("req_1", "call_vector_fails").await;
        // Tool execution fails... no peek happened
        let pop1 = broker.pop_tool_call_id("req_1").await; // on_tool_result
        assert_eq!(pop1, Some("call_vector_fails".to_string()));

        // Next MCP tool should work correctly
        broker
            .push_tool_call_id("req_1", "call_mcp_after_failure")
            .await;
        let peek2 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(peek2, Some("call_mcp_after_failure".to_string()));
        let pop2 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(pop2, Some("call_mcp_after_failure".to_string()));

        assert_eq!(broker.peek_tool_call_id("req_1").await, None);
    }

    #[tokio::test]
    async fn test_interleaved_requests_with_failures() {
        // Simulates: two concurrent requests, one has failures
        // Verifies request isolation is maintained even during failures
        let broker = ToolEventBroker::new();

        // Request 1: Tool fails
        broker.push_tool_call_id("req_1", "call_1_fail").await;

        // Request 2: Tool succeeds (interleaved)
        broker.push_tool_call_id("req_2", "call_2_success").await;

        // Request 2 peeks and pops (success)
        let peek_r2 = broker.peek_tool_call_id("req_2").await;
        assert_eq!(peek_r2, Some("call_2_success".to_string()));
        let pop_r2 = broker.pop_tool_call_id("req_2").await;
        assert_eq!(pop_r2, Some("call_2_success".to_string()));

        // Request 1 still has its item (failure doesn't affect isolation)
        let peek_r1 = broker.peek_tool_call_id("req_1").await;
        assert_eq!(peek_r1, Some("call_1_fail".to_string()));
        let pop_r1 = broker.pop_tool_call_id("req_1").await;
        assert_eq!(pop_r1, Some("call_1_fail".to_string()));

        // Both queues clean
        assert_eq!(broker.peek_tool_call_id("req_1").await, None);
        assert_eq!(broker.peek_tool_call_id("req_2").await, None);
    }

    #[tokio::test]
    async fn test_rapid_push_pop_under_stress() {
        // Stress test: rapid sequential tool calls
        // Verifies queue doesn't get corrupted under load
        let broker = ToolEventBroker::new();

        for i in 0..100 {
            let tool_id = format!("call_{}", i);
            broker.push_tool_call_id("req_stress", &tool_id).await;

            // Simulate MCP tool (peek)
            if i % 2 == 0 {
                let peek = broker.peek_tool_call_id("req_stress").await;
                assert_eq!(peek, Some(tool_id.clone()));
            }

            // on_tool_result (pop) - always fires
            let pop = broker.pop_tool_call_id("req_stress").await;
            assert_eq!(pop, Some(tool_id));
        }

        // Queue should be empty after 100 push/pop cycles
        assert_eq!(broker.peek_tool_call_id("req_stress").await, None);
    }

    #[tokio::test]
    async fn test_peek_empty_queue_warning_case() {
        // This test documents the warning scenario:
        // MCP execution calls peek but hook never pushed a tool_call_id.
        let broker = ToolEventBroker::new();

        // Simulate MCP execution trying to peek without prior hook push
        let result = broker.peek_tool_call_id("req_no_hook").await;
        assert_eq!(result, None);

        // Verify queue was never created for this request
        let pending = broker.pending_tool_calls.read().await;
        assert!(!pending.contains_key("req_no_hook"));
    }

    #[tokio::test]
    async fn test_peek_after_all_consumed_warning_case() {
        // Another warning scenario: peek after all items consumed
        let broker = ToolEventBroker::new();

        // Normal flow: push, then pop
        broker.push_tool_call_id("req_1", "call_123").await;
        let _ = broker.pop_tool_call_id("req_1").await;

        // Now peek returns None (queue consumed)
        let result = broker.peek_tool_call_id("req_1").await;
        assert_eq!(result, None);

        // Queue entry was cleaned up (empty queue removed)
        let pending = broker.pending_tool_calls.read().await;
        assert!(!pending.contains_key("req_1"));
    }

    // ========================================================================
    // ToolUsageEvent broker tests
    // ========================================================================

    #[tokio::test]
    async fn test_tool_usage_subscribe_and_publish() {
        let broker = ToolUsageBroker::new();
        let mut rx = broker.subscribe("req_usage_1").await;

        let event = ToolUsageEvent {
            tool_ids: vec!["call_abc".to_string(), "call_def".to_string()],
            prompt_tokens: 18777,
            completion_tokens: 500,
            total_tokens: 19277,
        };

        let sent = broker.publish("req_usage_1", event).await;
        assert!(sent);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.tool_ids, vec!["call_abc", "call_def"]);
        assert_eq!(received.prompt_tokens, 18777);
        assert_eq!(received.completion_tokens, 500);
        assert_eq!(received.total_tokens, 19277);
    }

    #[tokio::test]
    async fn test_tool_usage_unsubscribe() {
        let broker = ToolUsageBroker::new();
        let _rx = broker.subscribe("req_usage_2").await;
        broker.unsubscribe("req_usage_2").await;

        // Publish should return false after unsubscribe
        let sent = broker
            .publish(
                "req_usage_2",
                ToolUsageEvent {
                    tool_ids: vec![],
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                },
            )
            .await;
        assert!(!sent);
    }

    #[tokio::test]
    async fn test_tool_usage_request_isolation() {
        let broker = ToolUsageBroker::new();
        let mut rx1 = broker.subscribe("req_usage_a").await;
        let mut rx2 = broker.subscribe("req_usage_b").await;

        // Publish to request A
        broker
            .publish(
                "req_usage_a",
                ToolUsageEvent {
                    tool_ids: vec!["tool_for_a".to_string()],
                    prompt_tokens: 1000,
                    completion_tokens: 100,
                    total_tokens: 1100,
                },
            )
            .await;

        // Request A receives it
        let received = rx1.recv().await.unwrap();
        assert_eq!(received.tool_ids, vec!["tool_for_a"]);

        // Request B should NOT receive anything
        assert!(rx2.try_recv().is_err());
    }
}
