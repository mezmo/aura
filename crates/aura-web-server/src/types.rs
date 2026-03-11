use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::streaming::ToolResultMode;

/// Tracks in-flight request count for graceful shutdown.
pub struct ActiveRequestTracker {
    count: AtomicUsize,
    drained: Notify,
}

impl ActiveRequestTracker {
    pub fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            drained: Notify::new(),
        }
    }

    pub fn increment(&self) {
        self.count.fetch_add(1, Ordering::Release);
    }

    pub fn decrement(&self) {
        if self.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drained.notify_waiters();
        }
    }

    /// Resolves when count reaches zero. If already zero, resolves immediately.
    pub async fn wait_for_drain(&self) {
        loop {
            // Register BEFORE checking count to close TOCTOU gap:
            // if decrement() fires between register and check, the stored
            // permit ensures notified().await returns immediately.
            let notified = self.drained.notified();
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// Application state
pub struct AppState {
    pub config: Arc<aura_config::Config>,
    pub tool_result_mode: ToolResultMode,
    /// Maximum length for tool results (0 = no truncation)
    pub tool_result_max_length: usize,
    pub streaming_buffer_size: usize,
    /// Enable Aura custom SSE events (aura.tool_requested, aura.tool_start, aura.tool_complete, etc.)
    pub aura_custom_events: bool,
    /// Enable reasoning event emission (only when aura_custom_events is true)
    pub aura_emit_reasoning: bool,
    /// SSE streaming request timeout in seconds (0 = no timeout)
    pub streaming_timeout_secs: u64,
    /// First chunk timeout in seconds (0 = disabled). Protects against hung provider connections.
    pub first_chunk_timeout_secs: u64,
    /// Base directory for resolving relative config paths (skill sources, etc.)
    pub config_dir: std::path::PathBuf,
    /// Shutdown gate — cancelled immediately on SIGTERM/SIGINT to reject new requests (503)
    pub shutdown_token: CancellationToken,
    /// Stream shutdown — cancelled after grace period to terminate in-flight streams
    pub stream_shutdown_token: CancellationToken,
    /// Tracks in-flight requests for early shutdown when all requests complete
    pub active_requests: Arc<ActiveRequestTracker>,
}

/// OpenAI-compatible chat message structure
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// OpenAI-compatible chat completions request
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ChatCompletionRequest {
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
    pub stream: Option<bool>,

    /// OpenAI-compatible metadata field (up to 16 key-value pairs)
    #[serde(default)]
    pub metadata: Option<HashMap<String, String>>,
}

/// OpenAI-compatible choice structure
#[derive(Debug, Serialize)]
pub struct ChatChoice {
    pub index: usize,
    pub message: ChatMessage,
    pub finish_reason: String,
}

/// Usage statistics
#[derive(Debug, Serialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// OpenAI-compatible chat completions response
#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Option<Usage>,

    /// OpenAI-compatible metadata field (return session info)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

/// Error response structure
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_active_request_tracker_immediate_drain() {
        let tracker = ActiveRequestTracker::new();
        // count=0 should resolve immediately
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            tracker.wait_for_drain(),
        )
        .await
        .expect("wait_for_drain should resolve immediately when count is 0");
    }

    #[tokio::test]
    async fn test_active_request_tracker_drain_after_decrement() {
        let tracker = Arc::new(ActiveRequestTracker::new());
        tracker.increment();

        let tracker_clone = tracker.clone();
        let handle = tokio::spawn(async move {
            tracker_clone.wait_for_drain().await;
        });

        // Give waiter time to register
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tracker.decrement();

        tokio::time::timeout(std::time::Duration::from_millis(200), handle)
            .await
            .expect("wait_for_drain should resolve after decrement")
            .expect("task should not panic");
    }

    #[tokio::test]
    async fn test_active_request_tracker_multiple_requests() {
        let tracker = Arc::new(ActiveRequestTracker::new());
        tracker.increment();
        tracker.increment();
        tracker.increment();

        let tracker_clone = tracker.clone();
        let handle = tokio::spawn(async move {
            tracker_clone.wait_for_drain().await;
        });

        // Give waiter time to register
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Decrement one by one - should not resolve until count hits 0
        tracker.decrement();
        tracker.decrement();

        // Still at count=1, should not resolve yet
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "Should not resolve with count > 0");

        tracker.decrement(); // Now count=0

        tokio::time::timeout(std::time::Duration::from_millis(200), handle)
            .await
            .expect("wait_for_drain should resolve when count reaches 0")
            .expect("task should not panic");
    }
}
