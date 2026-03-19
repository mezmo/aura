//! SSE streaming types for OpenAI-compatible chat completions.
//!
//! Organized into submodules:
//! - `openai`: OpenAI-compatible chunk structures
//! - `config`: Server-level streaming configuration
//! - `context`: Per-request turn context and mutable state

pub mod openai {
    //! OpenAI-compatible chunk types for SSE streaming.
    //!
    //! See: <https://platform.openai.com/docs/api-reference/chat/streaming>

    use serde::Serialize;

    #[derive(Debug, Serialize)]
    pub struct FunctionCallChunk {
        pub name: String,
        pub arguments: String,
    }

    #[derive(Debug, Serialize)]
    pub struct ToolCallChunk {
        pub index: usize,
        pub id: String,
        #[serde(rename = "type")]
        pub call_type: String,
        pub function: FunctionCallChunk,
    }

    #[derive(Debug, Clone, Copy, Serialize)]
    #[serde(rename_all = "lowercase")]
    pub enum MessageRole {
        Assistant,
    }

    #[derive(Debug, Serialize)]
    pub struct ChatCompletionChunkDelta {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub role: Option<MessageRole>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool_calls: Option<Vec<ToolCallChunk>>,
    }

    #[derive(Debug, Serialize)]
    pub struct ChatCompletionChunkChoice {
        pub index: usize,
        pub delta: ChatCompletionChunkDelta,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub finish_reason: Option<String>,
    }

    #[derive(Debug, Serialize, Clone)]
    pub struct UsageInfo {
        pub prompt_tokens: u64,
        pub completion_tokens: u64,
        pub total_tokens: u64,
    }

    #[derive(Debug, Serialize)]
    pub struct ChatCompletionChunk {
        pub id: String,
        pub object: String,
        pub created: u64,
        pub model: String,
        pub choices: Vec<ChatCompletionChunkChoice>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub usage: Option<UsageInfo>,
    }
}

pub mod config {
    //! Server-level streaming configuration.

    /// How to handle tool results in SSE streaming.
    ///
    /// - `None`: Spec-compliant, tool results only used internally
    /// - `OpenWebUI`: Hack to make "View Results" work (accumulates arguments)
    /// - `Aura`: Results included in `aura.tool_complete` custom events
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
    pub enum ToolResultMode {
        #[default]
        None,
        #[clap(name = "open-web-ui")]
        OpenWebUI,
        Aura,
    }

    impl std::str::FromStr for ToolResultMode {
        type Err = std::convert::Infallible;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            Ok(match s.to_lowercase().as_str() {
                "open-web-ui" => Self::OpenWebUI,
                "aura" => Self::Aura,
                _ => Self::None,
            })
        }
    }

    /// Server-level streaming configuration (from env vars and CLI flags).
    #[derive(Debug, Clone)]
    pub struct StreamConfig {
        pub emit_custom_events: bool,
        pub emit_reasoning: bool,
        pub tool_result_mode: ToolResultMode,
        pub tool_result_max_length: usize,
        /// Enable fallback tool call parsing for Ollama models.
        /// When true, text content that looks like tool calls will be parsed and executed.
        pub fallback_tool_parsing: bool,
    }

    impl StreamConfig {
        pub fn new(
            emit_custom_events: bool,
            emit_reasoning: bool,
            tool_result_mode: ToolResultMode,
            tool_result_max_length: usize,
        ) -> Self {
            Self {
                emit_custom_events,
                emit_reasoning,
                tool_result_mode,
                tool_result_max_length,
                fallback_tool_parsing: false,
            }
        }

        /// Enable fallback tool call parsing (for Ollama models).
        pub fn with_fallback_tool_parsing(mut self, enabled: bool) -> Self {
            self.fallback_tool_parsing = enabled;
            self
        }
    }
}

pub mod context {
    //! Per-request turn context and mutable state.

    use super::openai::UsageInfo;
    use aura::stream_events::{AgentContext, CorrelationContext};
    use std::collections::HashMap;
    use std::time::Instant;

    /// OpenAI chunk object type for streaming responses.
    pub const CHUNK_OBJECT: &str = "chat.completion.chunk";
    /// OpenAI function call type identifier.
    pub const FUNCTION_TYPE: &str = "function";
    /// OpenAI finish_reason: normal completion.
    /// Also used after server-side tool execution completes (Aura never sends finish_reason "tool_calls").
    pub const FINISH_REASON_STOP: &str = "stop";
    /// OpenAI finish_reason: hit max_tokens limit.
    pub const FINISH_REASON_LENGTH: &str = "length";

    /// Immutable context for a single streaming turn (created once per request).
    #[derive(Clone)]
    pub struct TurnContext {
        pub completion_id: String,
        pub model_str: String,
        pub created_timestamp: u64,
        pub max_tokens: Option<u32>,
        pub agent_context: AgentContext,
        pub correlation: CorrelationContext,
    }

    impl TurnContext {
        pub fn new(
            completion_id: String,
            model_str: String,
            created_timestamp: u64,
            max_tokens: Option<u32>,
            session_id: &str,
        ) -> Self {
            Self {
                completion_id,
                model_str,
                created_timestamp,
                max_tokens,
                agent_context: AgentContext::single_agent(),
                correlation: CorrelationContext::from_current_span(session_id),
            }
        }

        /// Set agent_context to "coordinator" for orchestration mode.
        pub fn with_orchestration(mut self) -> Self {
            self.agent_context = AgentContext {
                agent_id: "coordinator".to_string(),
                agent_name: None,
                parent_agent_id: None,
            };
            self
        }
    }

    /// Mutable state accumulated during a streaming turn.
    #[derive(Default)]
    pub struct TurnState {
        pub tool_call_index: usize,
        pub tool_call_map: HashMap<String, (String, usize)>,
        pub tool_start_times: HashMap<String, Instant>,
        pub has_tool_calls: bool,
        pub needs_separator: bool,
        pub is_first_chunk: bool,
        pub usage_stats: Option<UsageInfo>,
        /// Accumulated response content - Always populated regardless of streaming or not.
        pub accumulated_content: String,
        /// Stream error captured for OTel span recording.
        pub stream_error: Option<String>,
    }

    impl TurnState {
        pub fn new() -> Self {
            Self {
                is_first_chunk: true,
                ..Default::default()
            }
        }
    }
}

// Re-export for convenience
pub use config::{StreamConfig, ToolResultMode};
pub use context::{
    CHUNK_OBJECT, FINISH_REASON_LENGTH, FINISH_REASON_STOP, FUNCTION_TYPE, TurnContext, TurnState,
};
pub use openai::{
    ChatCompletionChunk, ChatCompletionChunkChoice, ChatCompletionChunkDelta, FunctionCallChunk,
    MessageRole, ToolCallChunk,
};

// Re-export UsageInfo for tests (construct StreamOutcome fixtures)
#[cfg(test)]
pub use openai::UsageInfo;

/// Error prefix patterns from Rig's tool error handling.
const ERROR_PREFIXES: &[(&str, &str)] = &[
    ("Tool returned error: ", "ToolError"),
    ("Tool execution failed: ", "ExecutionError"),
    ("Tool not found: ", "NotFoundError"),
    ("Invalid tool arguments: ", "ArgumentError"),
    ("MCP error: ", "McpError"),
    ("HTTP error: ", "HttpError"),
    ("JSON error: ", "JsonError"),
    ("Timeout: ", "TimeoutError"),
];

/// Truncate to approximately `max_length` bytes (0 = no truncation).
/// Respects UTF-8 character boundaries to avoid panics on multi-byte characters.
pub fn truncate_result(result: &str, max_length: usize) -> String {
    if max_length == 0 || result.len() <= max_length {
        result.to_string()
    } else {
        let truncate_at = result.floor_char_boundary(max_length);
        format!("{}... [truncated]", &result[..truncate_at])
    }
}

/// Format as SSE message: `data: {json}\n\n`
pub fn format_sse_chunk<T: serde::Serialize>(
    value: &T,
) -> Result<actix_web::web::Bytes, serde_json::Error> {
    let json = serde_json::to_string(value)?;
    let mut buffer = String::with_capacity(json.len() + 10);
    buffer.push_str("data: ");
    buffer.push_str(&json);
    buffer.push_str("\n\n");
    Ok(actix_web::web::Bytes::from(buffer))
}

#[derive(Debug)]
pub enum ToolResultStatus {
    Success,
    Error(ToolError),
}

#[derive(Debug)]
pub struct ToolError {
    error_type: String,
    message: String,
}

impl ToolError {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error_type: error_type.into(),
            message: message.into(),
        }
    }

    pub fn error_type(&self) -> &str {
        &self.error_type
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn full_message(&self) -> String {
        format!("{}: {}", self.error_type, self.message)
    }
}

/// Detect if a tool result is an error based on Rig's error prefix patterns.
pub fn detect_tool_error(result_text: &str) -> ToolResultStatus {
    for (prefix, error_type) in ERROR_PREFIXES {
        if result_text.starts_with(prefix) {
            let message = result_text.strip_prefix(prefix).unwrap_or(result_text);
            return ToolResultStatus::Error(ToolError::new(*error_type, message));
        }
    }
    ToolResultStatus::Success
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_result_mode_default() {
        let mode = ToolResultMode::default();
        assert_eq!(mode, ToolResultMode::None);
    }

    #[test]
    fn test_tool_result_mode_from_str() {
        assert_eq!(
            "open-web-ui".parse::<ToolResultMode>().unwrap(),
            ToolResultMode::OpenWebUI
        );
        assert_eq!(
            "aura".parse::<ToolResultMode>().unwrap(),
            ToolResultMode::Aura
        );
        assert_eq!(
            "none".parse::<ToolResultMode>().unwrap(),
            ToolResultMode::None
        );
        assert_eq!(
            "invalid".parse::<ToolResultMode>().unwrap(),
            ToolResultMode::None
        );
    }

    #[test]
    fn test_truncate_result_no_truncation() {
        assert_eq!(truncate_result("short", 0), "short");
        assert_eq!(truncate_result("short", 100), "short");
    }

    #[test]
    fn test_truncate_result_with_truncation() {
        let result = truncate_result("this is a long result", 10);
        assert!(result.starts_with("this is a "));
        assert!(result.ends_with("... [truncated]"));
    }

    #[test]
    fn test_truncate_result_multibyte_boundary() {
        // "Hello 🎉 World" - emoji is 4 bytes at positions 6-9
        let input = "Hello 🎉 World";
        assert_eq!(input.len(), 16); // 6 + 4 + 6 bytes

        // Truncate at byte 8 (middle of emoji) should back up to byte 6
        let result = truncate_result(input, 8);
        assert_eq!(result, "Hello ... [truncated]");

        // Truncate at byte 10 (just after emoji) should include it
        let result = truncate_result(input, 10);
        assert_eq!(result, "Hello 🎉... [truncated]");
    }

    #[test]
    fn test_detect_tool_error_success() {
        let status = detect_tool_error("Normal result data");
        assert!(matches!(status, ToolResultStatus::Success));
    }

    #[test]
    fn test_detect_tool_error_failure() {
        let status = detect_tool_error("Tool returned error: Connection refused");
        match status {
            ToolResultStatus::Error(err) => {
                assert_eq!(err.error_type(), "ToolError");
                assert_eq!(err.message(), "Connection refused");
            }
            _ => panic!("Expected error status"),
        }
    }
}
