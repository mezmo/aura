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

use crate::fallback_tool_parser::{ParsedToolCall, parse_fallback_tool_calls};
use crate::mcp::McpManager;
use crate::provider_agent::{
    StreamError, StreamItem, StreamedAssistantContent, StreamedUserContent, ToolCall, ToolResult,
    ToolResultKind,
};
use crate::streaming_request_hook::{emit_from_stream, requested_arguments};
use crate::string_utils::truncate_for_log;
use aura_events::agent::{AgentEvent, AgentEventPayload};
use aura_events::{ToolCallId, ToolName};
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
    /// The same effective `mcp_filter` used to register tools, re-applied at
    /// execution time so a fallback-parsed call resolves to the identical
    /// server normal registration would have picked. See
    /// `McpManager::resolve_winning_tools`.
    mcp_filter: Option<Vec<aura_config::GlobPattern>>,
    /// The id of the stream this executor wraps.
    stream_id: Option<String>,
}

impl FallbackToolExecutor {
    /// Create a new executor for text-to-tool parsing.
    ///
    /// # Arguments
    /// * `mcp_manager` - MCP manager for executing discovered tools
    /// * `available_tools` - Tool names to match against (only these will be executed)
    /// * `mcp_filter` - The agent's effective `mcp_filter`, exactly as passed
    ///   to registration; execution re-applies it so a fallback call can't
    ///   reach a server the filter excluded
    ///
    /// # Note
    /// If `available_tools` is empty, no tool calls will ever be detected.
    /// The caller (`Agent::maybe_wrap_with_fallback`) should skip wrapping in this case.
    pub fn new(
        mcp_manager: Arc<McpManager>,
        available_tools: Vec<String>,
        mcp_filter: Option<Vec<aura_config::GlobPattern>>,
    ) -> Self {
        debug!(
            tool_count = available_tools.len(),
            "Fallback tool executor created"
        );
        Self {
            mcp_manager,
            available_tools,
            mcp_filter,
            stream_id: None,
        }
    }

    /// Wraps the stream with id `stream_id`, so each call it runs is
    /// announced to the run when that stream is the run's own. See
    /// [`announce`].
    pub(crate) fn for_stream(mut self, stream_id: &str) -> Self {
        self.stream_id = Some(stream_id.to_owned());
        self
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
                    Ok(StreamItem::Final(_)) | Ok(StreamItem::FinalMarker) | Ok(StreamItem::TurnUsage(..)) => {
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
                                if let Some(stream_id) = &executor_clone.stream_id {
                                    announce(stream_id, &tool_call_id, &parsed_call).await;
                                }

                                let tool_result = executor_clone.mcp_manager
                                    .execute_fallback_tool(
                                        &parsed_call.name,
                                        &parsed_call.arguments,
                                        executor_clone.mcp_filter.as_deref(),
                                    )
                                    .await;

                                match tool_result {
                                    Ok(result) => {
                                        debug!(tool = %parsed_call.name, "Fallback tool executed");
                                        yield Ok(StreamItem::StreamUserItem(
                                            StreamedUserContent::ToolResult(ToolResult {
                                                id: tool_call_id.clone(),
                                                call_id: Some(tool_call_id),
                                                kind: output_kind(&result),
                                                result,
                                            })
                                        ));
                                    }
                                    Err(e) => {
                                        warn!(tool = %parsed_call.name, error = %e, "Fallback tool execution failed");

                                        yield Ok(StreamItem::StreamUserItem(
                                            StreamedUserContent::ToolResult(ToolResult {
                                                id: tool_call_id.clone(),
                                                call_id: Some(tool_call_id),
                                                result: e,
                                                kind: ToolResultKind::Error,
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

/// Sends the call's `ToolRequested` event to the run, as the streaming hook
/// does for a native call. A fallback call never goes through rig's tool
/// loop, so the hook never sees it. The caller sends the event after the
/// call's item, which is the order rig gives a native call and its event.
/// Like the hook's, the event reaches the run only from the run's own
/// stream.
async fn announce(stream_id: &str, id: &str, call: &ParsedToolCall) {
    let (arguments, unparsed_arguments) = requested_arguments(call.arguments.clone());
    emit_from_stream(
        stream_id,
        AgentEvent::single_agent(AgentEventPayload::ToolRequested {
            tool_call_id: ToolCallId::new(id),
            tool_name: ToolName::new(&call.name),
            arguments,
            unparsed_arguments,
        }),
    )
    .await;
}

/// What a fallback call's output holds. The output is the MCP tool's own
/// text, not rig's encoding, so JSON other than a string stays JSON.
fn output_kind(output: &str) -> ToolResultKind {
    match serde_json::from_str::<serde_json::Value>(output) {
        Ok(serde_json::Value::String(_)) | Err(_) => ToolResultKind::Text,
        Ok(_) => ToolResultKind::Json,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_fallback_call_yields_a_tool_error() {
        let executor = FallbackToolExecutor::new(
            Arc::new(McpManager::new()),
            vec!["does_not_exist".to_string()],
            None,
        );
        let model_output = futures::stream::iter([
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text(
                    r#"{"name": "does_not_exist", "parameters": {}}"#.to_string(),
                ),
            )),
            Ok(StreamItem::FinalMarker),
        ])
        .boxed();

        let results: Vec<ToolResult> = executor
            .wrap_stream(model_output)
            .filter_map(|item| async move {
                match item {
                    Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(r))) => Some(r),
                    _ => None,
                }
            })
            .collect()
            .await;

        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].kind, ToolResultKind::Error, "{:?}", results[0]);
    }

    /// A model's text naming the `lookup` tool, then the end of its turn.
    fn calls_lookup() -> BoxStream<'static, Result<StreamItem, StreamError>> {
        futures::stream::iter([
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text(
                    r#"{"name": "lookup", "parameters": {"q": "x"}}"#.to_string(),
                ),
            )),
            Ok(StreamItem::FinalMarker),
        ])
        .boxed()
    }

    fn executor() -> FallbackToolExecutor {
        FallbackToolExecutor::new(
            Arc::new(McpManager::new()),
            vec!["lookup".to_string()],
            None,
        )
    }

    /// The run's own stream announces a fallback call with `ToolRequested`,
    /// after the call's item and before its result, as the streaming hook
    /// does for a native call.
    #[tokio::test]
    async fn the_run_s_own_stream_announces_a_call_after_its_item() {
        use crate::run_context::{RunContext, named_run_id, scope_stream};

        let run_id = named_run_id("req_fallback");
        let (run, mut events) = RunContext::channel(run_id);
        let mut stream = scope_stream(
            run,
            executor()
                .for_stream(&run_id.to_string())
                .wrap_stream(calls_lookup()),
        );

        let call = loop {
            match stream.next().await.expect("the stream yields the call") {
                Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall(call))) => {
                    break call;
                }
                _ => continue,
            }
        };
        assert!(
            events.try_recv().is_err(),
            "nothing is announced before the call's item"
        );

        let result = stream.next().await.expect("the stream yields the result");
        assert!(
            matches!(
                result,
                Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(
                    _
                )))
            ),
            "{result:?}"
        );
        let announced = events.try_recv().expect("the call is announced");
        let AgentEventPayload::ToolRequested {
            tool_call_id,
            tool_name,
            arguments,
            unparsed_arguments,
        } = announced.payload
        else {
            panic!("expected ToolRequested, got {:?}", announced.payload);
        };
        assert_eq!(tool_call_id.as_str(), call.id);
        assert_eq!(tool_name.as_str(), "lookup");
        assert_eq!(arguments, serde_json::json!({"q": "x"}));
        assert_eq!(unparsed_arguments, None);
        assert!(announced.agent.is_single_agent());
        assert!(events.try_recv().is_err(), "one call, one announcement");
    }

    /// A stream other than the run's own, such as an orchestration worker's,
    /// announces nothing to the run, as the streaming hook sends nothing
    /// from it.
    #[tokio::test]
    async fn another_stream_of_the_run_announces_nothing() {
        use crate::run_context::{RunContext, named_run_id, scope_stream};

        let (run, mut events) = RunContext::channel(named_run_id("req_fallback"));
        let items: Vec<_> = scope_stream(
            run,
            executor()
                .for_stream("worker_attempt_1")
                .wrap_stream(calls_lookup()),
        )
        .collect()
        .await;

        assert!(
            items.iter().any(|item| matches!(
                item,
                Ok(StreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ToolCall(_)
                ))
            )),
            "the call still runs: {items:?}"
        );
        assert!(events.try_recv().is_err(), "nothing reaches the run");
    }

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
