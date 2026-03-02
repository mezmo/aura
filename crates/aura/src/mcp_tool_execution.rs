/*!
 * Unified MCP Tool Execution
 *
 * This module provides shared execution logic for all MCP transports,
 * ensuring consistent behavior across HTTP and STDIO implementations:
 * - Structured logging (tool call start, arguments, completion)
 * - Response preview for large outputs
 * - Standardized error handling
 * - Per-request cancellation support (when executed within a cancellation context)
 */

use rig::tool::ToolError;
use serde_json::Value;
use std::collections::HashMap;
use tracing::{error, info};

use crate::mcp_streamable_http::StreamableHttpMcpClient;
use crate::request_cancellation::call_http_tool_cancellable;

// ---------------------------------------------------------------------------
// OTel recording helpers (keep tracing concerns out of business logic)
// ---------------------------------------------------------------------------

/// Record tool call input attributes on the current span.
///
/// Sets parameter count and (when content recording is enabled) the
/// serialised arguments. Called before tool execution.
fn record_tool_call_input(span: &tracing::Span, args: &Value) {
    if let Value::Object(ref map) = args {
        crate::logging::set_span_attribute(
            span,
            crate::logging::ATTR_TOOL_PARAMETERS_COUNT,
            map.len() as i64,
        );
    }
    if crate::logging::should_record_content() {
        let args_str = serde_json::to_string(args).unwrap_or_else(|_| "Invalid JSON".to_string());
        crate::logging::set_span_attribute(
            span,
            crate::logging::ATTR_TOOL_PARAMETERS,
            crate::logging::truncate_for_otel(&args_str),
        );
    }
}

/// Record tool call result attributes on the current span.
///
/// On success: result length, status OK, and (when content recording is
/// enabled) the truncated result body.
/// On cancellation: error status "cancelled" + `tool.cancelled = true`.
/// On other errors: error status with truncated message.
fn record_tool_call_result(span: &tracing::Span, result: &Result<String, anyhow::Error>) {
    match result {
        Ok(response) => {
            crate::logging::set_span_attribute(
                span,
                crate::logging::ATTR_TOOL_RESULT_LENGTH,
                response.len() as i64,
            );
            // MCP errors arrive as Ok("Tool returned an error: ...") —
            // detect these so the span shows ERROR in Phoenix / Jaeger.
            let status = crate::tool_error_detection::detect_tool_error(response);
            if let Some(err) = status.error() {
                crate::logging::set_span_error(
                    span,
                    crate::logging::truncate_for_otel(&err.full_message()),
                );
            } else {
                crate::logging::set_span_ok(span);
            }
            if crate::logging::should_record_content() {
                crate::logging::set_span_attribute(
                    span,
                    crate::logging::ATTR_TOOL_RESULT,
                    crate::logging::truncate_for_otel(response),
                );
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("Request cancelled") {
                crate::logging::set_span_error(span, "cancelled");
                crate::logging::set_span_attribute(span, crate::logging::ATTR_TOOL_CANCELLED, true);
            } else {
                crate::logging::set_span_error(span, crate::logging::truncate_for_otel(&err_str));
            }
        }
    }
}

/// Execute an HTTP MCP tool with unified logging and error handling
///
/// Provides consistent behavior with:
/// 1. Structured logging (tool call start, arguments, completion)
/// 2. Response preview for large outputs
/// 3. Standardized error handling
/// 4. Per-request cancellation support (when executed within a cancellation context)
#[tracing::instrument(name = "mcp.tool_call", skip(client, args), fields(tool.name = %tool_name, server.url = %client.server_url()))]
pub async fn execute_http_mcp_tool(
    client: &StreamableHttpMcpClient,
    tool_name: &str,
    args: Value,
) -> Result<String, ToolError> {
    let span = tracing::Span::current();

    // OTel: record input attributes
    record_tool_call_input(&span, &args);

    // Log tool call initiation
    info!(
        "Calling HTTP Streamable MCP tool '{}' on server '{}'",
        tool_name,
        client.server_url()
    );
    info!(
        "   Arguments: {}",
        serde_json::to_string(&args).unwrap_or_else(|_| "Invalid JSON".to_string())
    );

    // Note: aura.tool_start is now emitted from mcp_streamable_http.rs call_tool_tracked()
    // using Rig 0.28's id parameter for correct correlation via the FIFO queue.
    // This eliminates thread-local context dependency.

    // Convert Value to HashMap for HTTP client
    let args_map = match args {
        Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
        _ => HashMap::new(),
    };

    let result = call_http_tool_cancellable(client, tool_name, args_map).await;

    // OTel: record result attributes
    record_tool_call_result(&span, &result);

    // Business logging
    match result {
        Ok(response) => {
            let response_preview = preview_response(&response, 200);
            info!(
                "HTTP Streamable MCP tool '{}' completed: {}",
                tool_name, response_preview
            );
            Ok(response)
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("Request cancelled") {
                info!("HTTP Streamable MCP tool '{}' cancelled", tool_name);
            } else {
                error!(
                    "HTTP Streamable MCP tool '{}' failed: {}",
                    tool_name, err_str
                );
            }
            Err(ToolError::ToolCallError(e.into()))
        }
    }
}

/// Preview a response string for logging
///
/// Truncates long responses to approximately max_len bytes and appends "... (N chars)"
/// Short responses are returned unchanged. Respects UTF-8 character boundaries.
///
/// # Arguments
/// * `response` - The response string to preview
/// * `max_len` - Maximum byte length before truncation
///
/// # Returns
/// Preview string suitable for logging
fn preview_response(response: &str, max_len: usize) -> String {
    if response.len() > max_len {
        let truncate_at = response.floor_char_boundary(max_len);
        format!("{}... ({} chars)", &response[..truncate_at], response.len())
    } else {
        response.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preview_response_short() {
        let response = "Hello, world!";
        let preview = preview_response(response, 200);
        assert_eq!(preview, "Hello, world!");
    }

    #[test]
    fn test_preview_response_long() {
        let response = "a".repeat(300);
        let preview = preview_response(&response, 200);
        assert!(preview.starts_with(&"a".repeat(200)));
        assert!(preview.contains("(300 chars)"));
    }

    #[test]
    fn test_preview_response_exact_length() {
        let response = "a".repeat(200);
        let preview = preview_response(&response, 200);
        assert_eq!(preview.len(), 200);
        assert!(!preview.contains("chars"));
    }

    #[test]
    fn test_preview_response_multibyte_boundary() {
        // "Hello 🎉 World" - emoji is 4 bytes at positions 6-9
        let response = "Hello 🎉 World";
        assert_eq!(response.len(), 16);

        // Truncate at byte 8 (middle of emoji) should back up to byte 6
        let preview = preview_response(response, 8);
        assert!(preview.starts_with("Hello "));
        assert!(preview.contains("(16 chars)"));
        // Should not include partial emoji
        assert!(!preview.contains("🎉"));
    }
}
