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

use crate::mcp_streamable_http::McpClient;
use crate::request_cancellation::call_http_tool_cancellable;

// ---------------------------------------------------------------------------
// OTel recording helpers (keep tracing concerns out of business logic)
// ---------------------------------------------------------------------------

/// Record tool call input attributes on the current span.
///
/// Sets parameter count and (when content recording is enabled) the
/// serialised arguments. Called before tool execution.
fn record_tool_call_input(span: &tracing::Span, args: &Value) {
    if let Value::Object(map) = args {
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

/// Record the outbound header NAMES an approver override applies to this
/// call, sorted and comma-joined, never the values.
fn record_applied_headers(
    span: &tracing::Span,
    overrides: &crate::approver_headers::ApproverHeaders,
) {
    let mut names: Vec<&str> = overrides.captured_names().collect();
    names.sort_unstable();
    crate::logging::set_span_attribute(span, crate::logging::ATTR_APPLIED_HEADERS, names.join(","));
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

/// Execute an MCP tool with unified logging and error handling
///
/// Provides consistent behavior with:
/// 1. Structured logging (tool call start, arguments, completion)
/// 2. Response preview for large outputs
/// 3. Standardized error handling
/// 4. Per-request cancellation support (when executed within a cancellation context)
#[tracing::instrument(name = "mcp.tool_call", skip(client, args, approver_overrides), fields(tool.name = %tool_name, server.url = %client.server_url()))]
pub async fn execute_mcp_tool(
    client: &McpClient,
    tool_name: &str,
    args: Value,
    approver_overrides: Option<crate::approver_headers::ApproverHeaders>,
) -> Result<String, ToolError> {
    let span = tracing::Span::current();

    // OTel: record input attributes
    record_tool_call_input(&span, &args);
    if let Some(overrides) = approver_overrides.as_ref() {
        record_applied_headers(&span, overrides);
    }

    // Log tool call initiation
    info!(
        "Calling MCP tool '{}' on server '{}'",
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

    let result = call_http_tool_cancellable(client, tool_name, args_map, approver_overrides).await;

    // OTel: record result attributes
    record_tool_call_result(&span, &result);

    // Business logging
    match result {
        Ok(response) => {
            let response_preview = preview_response(&response, 200);
            info!("MCP tool '{}' completed: {}", tool_name, response_preview);
            Ok(response)
        }
        Err(e) => {
            let err_str = e.to_string();
            match bound_transport_error(&err_str) {
                None => {
                    // Cancellations must propagate unmodified — downstream
                    // lifecycle handling keys off the original error.
                    info!("MCP tool '{}' cancelled", tool_name);
                    Err(ToolError::ToolCallError(e.into()))
                }
                Some(bounded) => {
                    // Full detail is preserved in the log line; the agent only
                    // ever sees the bounded message.
                    error!("MCP tool '{}' failed: {}", tool_name, err_str);
                    Err(ToolError::ToolCallError(anyhow::anyhow!(bounded).into()))
                }
            }
        }
    }
}

/// Decide how a transport-level MCP error should be surfaced to the agent.
///
/// Returns `None` for cancellations — those must propagate unmodified so the
/// request lifecycle can react to the original error. Otherwise returns
/// `Some(bounded)`, the message bounded to [`MAX_TOOL_ERROR_BYTES`] so a
/// multi-KB transport/provider payload cannot flood a worker's context window.
///
/// [`MAX_TOOL_ERROR_BYTES`]: crate::mcp_response::MAX_TOOL_ERROR_BYTES
fn bound_transport_error(err_str: &str) -> Option<String> {
    if err_str.contains("Request cancelled") {
        None
    } else {
        Some(crate::mcp_response::bound_error_content(
            err_str.to_string(),
            crate::mcp_response::MAX_TOOL_ERROR_BYTES,
        ))
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
pub(crate) fn preview_response(response: &str, max_len: usize) -> String {
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

    #[test]
    fn test_bound_transport_error_cancellation_passthrough() {
        // Cancellations must not be bounded/rewrapped — `execute_mcp_tool`
        // returns the original error for these.
        assert_eq!(
            bound_transport_error("Request cancelled by client disconnect"),
            None
        );
    }

    #[test]
    fn test_bound_transport_error_small_passthrough() {
        // A normal-sized transport error is surfaced verbatim.
        let msg = "Tool execution failed: Connection refused (os error 61)";
        assert_eq!(bound_transport_error(msg), Some(msg.to_string()));
    }

    #[test]
    fn test_bound_transport_error_bounds_large() {
        let huge = format!("Tool execution failed: {}", "stack frame\n".repeat(8000));
        let bounded = bound_transport_error(&huge).expect("non-cancel error must be bounded");
        assert!(
            bounded.len() <= crate::mcp_response::MAX_TOOL_ERROR_BYTES + 128,
            "bounded transport error must stay near the budget; got {} bytes",
            bounded.len()
        );
        assert!(bounded.contains("[tool error truncated:"));
        // The leading context (where categorization keywords live) survives.
        assert!(bounded.starts_with("Tool execution failed:"));
    }

    /// Trace correlation: a gated call's `mcp.tool_call` span carries the
    /// captured override's header NAMES, never their values, and an
    /// ungated call's span carries neither.
    ///
    /// Gated on `otel`: without the feature there is no span data to
    /// assert against.
    #[cfg(feature = "otel")]
    mod applied_headers_span {

        use opentelemetry::trace::TracerProvider as _;

        use opentelemetry_sdk::trace::TracerProvider;
        use serde_json::json;
        use tracing_subscriber::layer::SubscriberExt;

        use super::*;
        use crate::logging::ATTR_APPLIED_HEADERS;
        use crate::mcp_streamable_http::tests::RecordingMcpServer;
        use crate::test_span_capture::CapturedSpans;

        /// Run `execute_mcp_tool` under a subscriber that exports to memory,
        /// returning the `applied_headers` attribute its `mcp.tool_call`
        /// span carries. `#[tracing::instrument]` on `execute_mcp_tool`
        /// opens the span itself, so no outer instrumentation is needed.
        async fn applied_headers_on_call(
            client: &McpClient,
            overrides: Option<crate::approver_headers::ApproverHeaders>,
        ) -> Option<String> {
            let captured = CapturedSpans::default();
            let provider = TracerProvider::builder()
                .with_simple_exporter(captured.clone())
                .build();
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry()
                    .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test"))),
            );

            execute_mcp_tool(client, "gated", json!({}), overrides)
                .await
                .expect("the call succeeds");

            assert!(
                captured.contains("mcp.tool_call"),
                "the mcp.tool_call span was never exported",
            );
            captured.attribute("mcp.tool_call", ATTR_APPLIED_HEADERS)
        }

        /// A gated call carrying an override stamps its captured names,
        /// sorted and comma-joined, on the execution span — never a value.
        /// Configured out of alphabetical order (`x-tenant` before
        /// `authorization`), so a joined-but-unsorted regression would
        /// produce `"x-tenant,authorization"` and this test would catch it.
        #[tokio::test]
        async fn gated_call_stamps_the_applied_header_names_never_values() {
            let server = RecordingMcpServer::start().await;
            let client = McpClient::new(server.url.clone(), &HashMap::new())
                .await
                .expect("the loopback server completes the handshake");

            let overrides = crate::approver_headers::tests::captured_overrides_multi(&[
                ("x-tenant", "acme"),
                ("authorization", "Bearer approver-secret"),
            ]);
            let attribute = applied_headers_on_call(&client, Some(overrides)).await;

            assert_eq!(
                attribute.as_deref(),
                Some("authorization,x-tenant"),
                "the span must name every applied header, sorted",
            );
            for value in ["acme", "Bearer approver-secret", "approver-secret"] {
                assert!(
                    !attribute.as_deref().unwrap().contains(value),
                    "the span must never carry a header value, got: {attribute:?}",
                );
            }
        }

        /// A call no approval gated carries no override, so its span records
        /// no `applied_headers` attribute at all.
        #[tokio::test]
        async fn ungated_call_records_no_applied_headers() {
            let server = RecordingMcpServer::start().await;
            let client = McpClient::new(server.url.clone(), &HashMap::new())
                .await
                .expect("the loopback server completes the handshake");

            let attribute = applied_headers_on_call(&client, None).await;

            assert_eq!(attribute, None);
        }
    }
}
