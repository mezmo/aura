//! # Ollama Text-to-Tool Stream Wrapper
//!
//! Handles Ollama models that output tool calls as text instead of native `tool_call`
//! structures. This is a known issue with models like qwen3-coder.
//!
//! ## When This Activates
//!
//! Enabled via config (`fallback_tool_parsing = true` in `[llm]` for Ollama):
//!
//! ```toml
//! [llm]
//! provider = "ollama"
//! model = "qwen3-coder:30b"
//! fallback_tool_parsing = true
//! ```
//!
//! ## How It Works
//!
//! 1. `Agent::maybe_wrap_with_fallback()` wraps the Rig stream with `FallbackToolExecutor`
//! 2. Executor buffers all text content while streaming through to the client
//! 3. On `Final` marker, checks buffered text for tool call patterns
//! 4. If found (and no native tool_calls already handled), executes via `McpManager`
//! 5. Injects synthetic `ToolCall` and `ToolResult` events before the `Final` marker
//!
//! ## Supported Formats
//!
//! See `fallback_tool_parser` module for format details:
//! - JSON: `{"name": "...", "parameters": {...}}`
//! - Hermes XML: `<tool_call>{...}</tool_call>`
//! - Pythonic: `[tool_name(arg="value")]`
//! - Qwen XML: `<function=name><parameter=arg>value</parameter></function>`

use crate::fallback_tool_parser::parse_fallback_tool_calls;
use crate::mcp::McpManager;
use crate::provider_agent::{
    StreamError, StreamItem, StreamedAssistantContent, StreamedUserContent, ToolCall, ToolResult,
};
use crate::string_utils::truncate_for_log;
use futures::stream::{BoxStream, StreamExt};
use std::sync::Arc;
use tracing::{debug, info, warn};
use uuid::Uuid;

const LOG_TRUNCATE_LIMIT: usize = 200;

/// Tool call ID length (24 hex chars after `call_` prefix for log consistency)
const TOOL_CALL_ID_HEX_LEN: usize = 24;

fn generate_fallback_tool_id() -> String {
    let uuid = Uuid::new_v4().to_string().replace('-', "");
    format!("call_{}", &uuid[..TOOL_CALL_ID_HEX_LEN])
}

/// Executor for fallback tool calls detected in text content.
///
/// This is used by Ollama models that output tool calls as text (JSON, XML, etc.)
/// instead of using native tool calling APIs.
pub struct FallbackToolExecutor {
    mcp_manager: Arc<McpManager>,
    available_tools: Vec<String>,
}

impl FallbackToolExecutor {
    /// Create a new executor for text-to-tool parsing.
    ///
    /// # Arguments
    /// * `mcp_manager` - MCP manager for executing discovered tools
    /// * `available_tools` - Tool names to match against (only these will be executed)
    ///
    /// # Note
    /// If `available_tools` is empty, no tool calls will ever be detected.
    /// The caller (`Agent::maybe_wrap_with_fallback`) should skip wrapping in this case.
    pub fn new(mcp_manager: Arc<McpManager>, available_tools: Vec<String>) -> Self {
        debug!(
            tool_count = available_tools.len(),
            "Fallback tool executor created"
        );
        Self {
            mcp_manager,
            available_tools,
        }
    }

    /// Wrap a stream with fallback tool execution capability.
    ///
    /// This returns a new stream that:
    /// 1. Buffers text content
    /// 2. Detects tool call patterns in the text
    /// 3. Executes detected tool calls
    /// 4. Injects tool call and result events into the stream
    pub fn wrap_stream(
        self,
        stream: BoxStream<'static, Result<StreamItem, StreamError>>,
    ) -> BoxStream<'static, Result<StreamItem, StreamError>> {
        let executor = Arc::new(self);
        let state = Arc::new(tokio::sync::Mutex::new(StreamState::new()));

        // Use unfold to process items and potentially inject extras
        let state_clone = state.clone();
        let executor_clone = executor.clone();

        Box::pin(async_stream::stream! {
            let mut stream = stream;

            while let Some(item) = stream.next().await {
                match &item {
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                        // Buffer text content
                        {
                            let mut state = state_clone.lock().await;
                            state.text_buffer.push_str(text);
                        }

                        // Forward the original text
                        yield item;
                    }
                    Ok(StreamItem::Final(_)) | Ok(StreamItem::FinalMarker) => {
                        // Stream is ending - check if buffered text contains a tool call
                        // BUT only if no proper tool_calls were already executed
                        let (buffered_text, has_native_tool_calls) = {
                            let state = state_clone.lock().await;
                            (state.text_buffer.clone(), state.has_native_tool_calls)
                        };

                        // Skip fallback if proper tool_calls already executed
                        if has_native_tool_calls {
                            debug!("Skipping fallback parsing - native tool_calls already handled");
                            yield item;
                            continue;
                        }

                        if let Some(tool_calls) = parse_fallback_tool_calls(&buffered_text, &executor_clone.available_tools) {
                            let tool_names: Vec<_> = tool_calls.iter().map(|c| c.name.as_str()).collect();
                            info!(
                                count = tool_calls.len(),
                                tools = ?tool_names,
                                "Fallback tool parsing detected tool calls in text"
                            );
                            let (truncated_text, was_truncated) = truncate_for_log(&buffered_text, LOG_TRUNCATE_LIMIT);
                            debug!(
                                text = %truncated_text,
                                truncated = was_truncated,
                                "Buffered text content"
                            );

                            // Clear buffer and prevent re-parsing if stream emits multiple Final markers
                            {
                                let mut state = state_clone.lock().await;
                                state.text_buffer.clear();
                                state.has_native_tool_calls = true;
                            }

                            for parsed_call in tool_calls {
                                let tool_call_id = generate_fallback_tool_id();

                                yield Ok(StreamItem::StreamAssistantItem(
                                    StreamedAssistantContent::ToolCall(ToolCall {
                                        id: tool_call_id.clone(),
                                        name: parsed_call.name.clone(),
                                        arguments: parsed_call.arguments.clone(),
                                    })
                                ));

                                let tool_result = executor_clone.mcp_manager
                                    .execute_fallback_tool(&parsed_call.name, &parsed_call.arguments)
                                    .await;

                                match tool_result {
                                    Ok(result) => {
                                        debug!(tool = %parsed_call.name, "Fallback tool executed");
                                        yield Ok(StreamItem::StreamUserItem(
                                            StreamedUserContent::ToolResult(ToolResult {
                                                id: tool_call_id.clone(),
                                                call_id: Some(tool_call_id),
                                                result,
                                            })
                                        ));
                                    }
                                    Err(e) => {
                                        warn!(tool = %parsed_call.name, error = %e, "Fallback tool execution failed");

                                        // Emit error as tool result
                                        yield Ok(StreamItem::StreamUserItem(
                                            StreamedUserContent::ToolResult(ToolResult {
                                                id: tool_call_id.clone(),
                                                call_id: Some(tool_call_id),
                                                result: format!("Error: {}", e),
                                            })
                                        ));
                                    }
                                }
                            }
                        }

                        // Forward the final marker
                        yield item;
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall(_)))
                    | Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCallDelta { .. })) => {
                        // Real tool call received - mark as having real tool calls
                        {
                            let mut state = state_clone.lock().await;
                            if !state.has_native_tool_calls {
                                debug!("Native tool_call detected, disabling fallback parsing");
                            }
                            state.has_native_tool_calls = true;
                        }
                        yield item;
                    }
                    _ => {
                        // Pass through other items unchanged
                        yield item;
                    }
                }
            }
        })
    }

    /// Check if text content might contain a tool call.
    ///
    /// This is a quick heuristic check before doing full parsing.
    pub fn might_contain_tool_call(text: &str) -> bool {
        let trimmed = text.trim();

        // Check for JSON-like patterns
        if trimmed.starts_with('{') && trimmed.contains("\"name\"") {
            return true;
        }

        // Check for Hermes XML pattern
        if trimmed.contains("<tool_call>") {
            return true;
        }

        // Check for Pythonic pattern
        if trimmed.starts_with('[') && trimmed.contains('(') && trimmed.ends_with(")]") {
            return true;
        }

        // Check for Qwen XML pattern
        if trimmed.contains("<function=") && trimmed.contains("</function>") {
            return true;
        }

        false
    }
}

struct StreamState {
    text_buffer: String,
    /// Set when native tool_call events are seen; skips fallback parsing
    has_native_tool_calls: bool,
}

impl StreamState {
    fn new() -> Self {
        Self {
            text_buffer: String::new(),
            has_native_tool_calls: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_might_contain_tool_call_json() {
        assert!(FallbackToolExecutor::might_contain_tool_call(
            r#"{"name": "test", "parameters": {}}"#
        ));
    }

    #[test]
    fn test_might_contain_tool_call_hermes() {
        assert!(FallbackToolExecutor::might_contain_tool_call(
            r#"<tool_call>{"name": "test"}</tool_call>"#
        ));
    }

    #[test]
    fn test_might_contain_tool_call_pythonic() {
        assert!(FallbackToolExecutor::might_contain_tool_call(
            r#"[test_tool(arg="value")]"#
        ));
    }

    #[test]
    fn test_might_contain_tool_call_plain_text() {
        assert!(!FallbackToolExecutor::might_contain_tool_call(
            "Hello, how can I help you today?"
        ));
    }

    #[test]
    fn test_might_contain_tool_call_partial_json() {
        // JSON without "name" field should not match
        assert!(!FallbackToolExecutor::might_contain_tool_call(
            r#"{"key": "value"}"#
        ));
    }

    #[test]
    fn test_might_contain_tool_call_qwen() {
        assert!(FallbackToolExecutor::might_contain_tool_call(
            r#"<function=test_tool><parameter=arg>value</parameter></function>"#
        ));
    }
}
