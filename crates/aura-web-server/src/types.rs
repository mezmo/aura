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
    pub configs: Arc<Vec<aura_config::Config>>,
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
    /// Shutdown gate — cancelled immediately on SIGTERM/SIGINT to reject new requests (503)
    pub shutdown_token: CancellationToken,
    /// Stream shutdown — cancelled after grace period to terminate in-flight streams
    pub stream_shutdown_token: CancellationToken,
    /// Tracks in-flight requests for early shutdown when all requests complete
    pub active_requests: Arc<ActiveRequestTracker>,
    /// Default agent name or alias, used when `model` is omitted from the request
    pub default_agent: Option<String>,
}

/// OpenAI-compatible message role
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Catch-all for roles we don't handle (e.g. "tool", "function")
    #[serde(other, rename = "unknown")]
    Unknown,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::System => write!(f, "system"),
            Role::User => write!(f, "user"),
            Role::Assistant => write!(f, "assistant"),
            Role::Unknown => write!(f, "unknown"),
        }
    }
}

/// OpenAI-compatible chat message structure
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
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

/// OpenAI-compatible error responses for chat completion requests.
pub enum ChatCompletionErrorResponse {
    /// Model parameter was not provided (HTTP 400).
    ModelNotProvided,
    /// Model was specified but does not match any configured agent (HTTP 404).
    ModelNotFound(String),
}

#[derive(Serialize)]
struct ChatCompletionErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
    param: String,
    code: String,
}

#[derive(Serialize)]
struct ChatCompletionErrorEnvelope {
    error: ChatCompletionErrorDetail,
}

impl Serialize for ChatCompletionErrorResponse {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (message, code) = match self {
            ChatCompletionErrorResponse::ModelNotProvided => (
                "you must provide a model parameter".to_string(),
                "missing_required_parameter".to_string(),
            ),
            ChatCompletionErrorResponse::ModelNotFound(model_name) => (
                format!(
                    "The model `{}` does not exist or you do not have access to it.",
                    model_name
                ),
                "model_not_found".to_string(),
            ),
        };

        let envelope = ChatCompletionErrorEnvelope {
            error: ChatCompletionErrorDetail {
                message,
                error_type: "invalid_request_error".to_string(),
                param: "model".to_string(),
                code,
            },
        };

        envelope.serialize(serializer)
    }
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
    /// Error taxonomy label from ErrorCategory. Additive field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ErrorDetail {
    /// Create an error with taxonomy classification and sanitized client message.
    ///
    /// Encapsulates three concerns: (1) classifying the error via ErrorCategory,
    /// (2) logging the internal message at WARN level for debugging, and
    /// (3) replacing the client-facing message with a safe generic string.
    /// This prevents internal details (hostnames, ports, library errors) from
    /// reaching API consumers while preserving the existing `error_type` values.
    pub fn classified(
        error_type: &str,
        category: aura::ErrorCategory,
        internal_message: &str,
    ) -> Self {
        let aura_err = aura::AuraError::new(category, internal_message);
        tracing::warn!(
            error_category = category.as_label(),
            internal_message = internal_message,
            "Request error"
        );
        Self {
            message: aura_err.client_message(),
            error_type: error_type.to_string(),
            code: Some(category.as_label().to_string()),
        }
    }

    /// Create a request validation error that passes the message through directly.
    ///
    /// Unlike `classified()`, this does not sanitize the message because request
    /// validation errors contain client-supplied input (e.g., "Last message must
    /// be from user, got: system") which is safe to return.
    pub fn validation(message: &str) -> Self {
        Self {
            message: message.to_string(),
            error_type: "invalid_request_error".to_string(),
            code: Some(aura::ErrorCategory::RequestValidation.as_label().to_string()),
        }
    }
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

    #[test]
    fn test_model_not_provided_serialization() {
        let error = ChatCompletionErrorResponse::ModelNotProvided;
        let json: serde_json::Value = serde_json::to_value(&error).unwrap();

        assert_eq!(
            json["error"]["message"],
            "you must provide a model parameter"
        );
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["param"], "model");
        assert_eq!(json["error"]["code"], "missing_required_parameter");
    }

    #[test]
    fn test_model_not_found_serialization() {
        let error = ChatCompletionErrorResponse::ModelNotFound("gpt-5".to_string());
        let json: serde_json::Value = serde_json::to_value(&error).unwrap();

        assert_eq!(
            json["error"]["message"],
            "The model `gpt-5` does not exist or you do not have access to it."
        );
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["param"], "model");
        assert_eq!(json["error"]["code"], "model_not_found");
    }

    #[test]
    fn test_error_detail_with_code_serializes_code_field() {
        let detail = ErrorDetail {
            message: "test".to_string(),
            error_type: "internal_error".to_string(),
            code: Some("llm_timeout".to_string()),
        };
        let json = serde_json::to_value(&ErrorResponse { error: detail }).unwrap();
        assert_eq!(json["error"]["code"], "llm_timeout");
        assert_eq!(json["error"]["type"], "internal_error");
        assert_eq!(json["error"]["message"], "test");
    }

    #[test]
    fn test_error_detail_without_code_omits_code_field() {
        let detail = ErrorDetail {
            message: "test".to_string(),
            error_type: "internal_error".to_string(),
            code: None,
        };
        let json = serde_json::to_value(&ErrorResponse { error: detail }).unwrap();
        assert!(json["error"]["code"].is_null(), "code field should be absent when None");
        assert_eq!(json["error"]["type"], "internal_error");
    }

    #[test]
    fn test_error_detail_classified_uses_generic_message() {
        let detail = ErrorDetail::classified(
            "internal_error",
            aura::ErrorCategory::McpConnectionFailed,
            "MCP server 'pagerduty' at 10.0.1.5:8080 refused",
        );
        assert_eq!(detail.message, "A downstream service is temporarily unavailable");
        assert_eq!(detail.error_type, "internal_error");
        assert_eq!(detail.code, Some("mcp_connection_failed".to_string()));
        assert!(!detail.message.contains("pagerduty"));
        assert!(!detail.message.contains("10.0.1.5"));
    }

    #[test]
    fn test_error_detail_validation_passes_through_message() {
        let detail = ErrorDetail::validation("messages array is empty");
        assert_eq!(detail.message, "messages array is empty");
        assert_eq!(detail.error_type, "invalid_request_error");
        assert_eq!(detail.code, Some("request_validation".to_string()));
    }
}
