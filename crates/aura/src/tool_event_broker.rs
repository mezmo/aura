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
//! See `docs/rig-fork-changes.md` for analysis.
//! The FIFO queue is safe because: hook fires → tool executes → hook fires → next tool.

use rmcp::model::ProgressToken;
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::{RwLock, mpsc};
use tracing::debug;

pub use aura_events::{AgentContext, TokenUsage, ToolCallId, ToolName};

/// Channel capacity for tool events per request
const EVENT_CHANNEL_CAPACITY: usize = 32;

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
        agent: Option<AgentContext>,
    },
    /// Emitted from execution context when MCP actually begins.
    /// Provides the progress_token for correlating with aura.progress events.
    Start {
        tool_id: ToolCallId,
        tool_name: ToolName,
        progress_token: Option<ProgressToken>,
        agent: Option<AgentContext>,
    },
}

impl ToolLifecycleEvent {
    /// Get the tool_name regardless of event variant
    pub fn tool_name(&self) -> &ToolName {
        match self {
            ToolLifecycleEvent::Requested { tool_name, .. } => tool_name,
            ToolLifecycleEvent::Start { tool_name, .. } => tool_name,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolUsageEvent {
    pub tool_ids: Vec<ToolCallId>,
    pub usage: TokenUsage,
    pub agent: Option<AgentContext>,
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
}

impl ToolEventBroker {
    /// Create a new tool event broker
    pub fn new() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
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

/// Superseded by [`crate::agent_events::emit`], which is the seam producers
/// publish through. Kept for the differential test that compares the two paths.
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
            agent: None,
        },
    )
    .await
}

/// Superseded by [`crate::agent_events::emit`], which is the seam producers
/// publish through. Kept for the differential test that compares the two paths.
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
            agent: None,
        },
    )
    .await
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

/// Publish a prebuilt usage event, so a caller that knows the agent can name it.
pub async fn publish_usage(request_id: &str, event: ToolUsageEvent) -> bool {
    usage_broker_global().publish(request_id, event).await
}

/// Superseded by [`crate::agent_events::emit`], which is the seam producers
/// publish through. Kept for the differential test that compares the two paths.
pub async fn publish_tool_usage(
    request_id: &str,
    tool_ids: Vec<ToolCallId>,
    usage: TokenUsage,
) -> bool {
    publish_usage(
        request_id,
        ToolUsageEvent {
            tool_ids,
            usage,
            agent: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_events::TokenCount;
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
            tool_id: ToolCallId::new("call_abc"),
            tool_name: ToolName::new("list_pipelines"),
            progress_token: Some(numeric_token(42)),
            agent: None,
        };

        let sent = broker.publish("req_123", event).await;
        assert!(sent);

        let received = rx.recv().await.unwrap();
        match received {
            ToolLifecycleEvent::Start {
                tool_id,
                tool_name,
                progress_token,
                ..
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
            tool_id: ToolCallId::new("call_abc"),
            tool_name: ToolName::new("search"),
            arguments: serde_json::json!({"query": "test"}),
            agent: None,
        };

        let sent = broker.publish("req_123", event).await;
        assert!(sent);

        let received = rx.recv().await.unwrap();
        match received {
            ToolLifecycleEvent::Requested {
                tool_id,
                tool_name,
                arguments,
                ..
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
            tool_id: ToolCallId::new("call_abc"),
            tool_name: ToolName::new("list_pipelines"),
            progress_token: None,
            agent: None,
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
            tool_id: ToolCallId::new("call_1"),
            tool_name: ToolName::new("tool_for_req_1"),
            progress_token: Some(string_token("token_1")),
            agent: None,
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
            tool_id: ToolCallId::new("call_xyz"),
            tool_name: ToolName::new("some_tool"),
            progress_token: None,
            agent: None,
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

    // ========================================================================
    // ToolUsageEvent broker tests
    // ========================================================================

    #[tokio::test]
    async fn test_tool_usage_subscribe_and_publish() {
        let broker = ToolUsageBroker::new();
        let mut rx = broker.subscribe("req_usage_1").await;

        let event = ToolUsageEvent {
            tool_ids: vec![ToolCallId::new("call_abc"), ToolCallId::new("call_def")],
            usage: TokenUsage {
                prompt_tokens: TokenCount::new(18777),
                completion_tokens: TokenCount::new(500),
                total_tokens: TokenCount::new(19277),
            },
            agent: None,
        };

        let sent = broker.publish("req_usage_1", event).await;
        assert!(sent);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.tool_ids, vec!["call_abc", "call_def"]);
        assert_eq!(received.usage.prompt_tokens.get(), 18777);
        assert_eq!(received.usage.completion_tokens.get(), 500);
        assert_eq!(received.usage.total_tokens.get(), 19277);
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
                    usage: TokenUsage {
                        prompt_tokens: TokenCount::new(0),
                        completion_tokens: TokenCount::new(0),
                        total_tokens: TokenCount::new(0),
                    },
                    agent: None,
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
                    tool_ids: vec![ToolCallId::new("tool_for_a")],
                    usage: TokenUsage {
                        prompt_tokens: TokenCount::new(1000),
                        completion_tokens: TokenCount::new(100),
                        total_tokens: TokenCount::new(1100),
                    },
                    agent: None,
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
