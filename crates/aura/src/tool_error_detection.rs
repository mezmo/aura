//! Tells a tool call's failure apart from its output, and what part of a
//! failure to show people.

use crate::mcp::MCP_ERROR_PREFIX;

/// Rig's `ToolServerError`, the text a tool's `Err` reaches the model as.
const TOOLSET_ERROR_PREFIX: &str = "Toolset error: ";

/// Rig's `ToolError` and `ToolSetError`, which can nest several deep.
const TOOL_CALL_ERROR_PREFIX: &str = "ToolCallError: ";

/// Whether a tool's output reports a failure: an MCP error result.
///
/// Only that reaches a call as output. Any other failure is the call's
/// `Err`, so an output that merely starts like one is still a success.
///
/// ```
/// use aura::tool_error_detection::is_reported_error;
///
/// assert!(is_reported_error("Tool returned an error: 401 Unauthorized"));
/// assert!(!is_reported_error("Tool execution failed: disk full on node-3"));
/// ```
pub fn is_reported_error(output: &str) -> bool {
    output.starts_with(MCP_ERROR_PREFIX)
}

/// A failed call's text without the layers rig wraps around a tool's `Err`,
/// for events, logs, and traces. The model still sees the full text.
///
/// ```
/// use aura::tool_error_detection::tool_error_message;
///
/// assert_eq!(
///     tool_error_message("Toolset error: ToolCallError: ToolCallError: connection refused"),
///     "connection refused",
/// );
/// ```
pub fn tool_error_message(error_text: &str) -> &str {
    let mut message = error_text
        .strip_prefix(TOOLSET_ERROR_PREFIX)
        .unwrap_or(error_text);
    while let Some(rest) = message.strip_prefix(TOOL_CALL_ERROR_PREFIX) {
        message = rest;
    }
    if message.trim().is_empty() {
        error_text
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_error_results_are_reported_errors() {
        let result = rmcp::model::CallToolResult::error(vec![rmcp::model::Content::text(
            "401 Unauthorized",
        )]);
        let text = crate::mcp::extract_tool_result(result, "auth_tool");
        assert!(is_reported_error(&text), "{text}");
    }

    #[test]
    fn output_that_only_reads_like_a_failure_is_not_a_reported_error() {
        for output in [
            "Tool execution failed: disk full on node-3",
            "Toolset error: from a log line",
            "Sending message was cancelled by the user",
            "Timeout: probe exceeded 5s",
            "HTTP error: 503 from upstream",
            "Found 3 error logs in the file",
            "{\"files\": [\"a.txt\", \"b.txt\"]}",
            "",
        ] {
            assert!(!is_reported_error(output), "{output}");
        }
    }

    /// A tool's `Err` reaches the model through rig's tool server, which
    /// wraps it in its own error prefixes.
    #[tokio::test]
    async fn rig_wrapped_tool_errors_lose_their_wrapping() {
        use rig::tool::server::ToolServer;
        use rig::tool::{Tool, ToolError};

        #[derive(Clone)]
        struct Failing;

        impl Tool for Failing {
            const NAME: &'static str = "failing";
            type Error = ToolError;
            type Args = serde_json::Value;
            type Output = String;

            async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
                rig::completion::ToolDefinition {
                    name: Self::NAME.into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }

            async fn call(&self, _args: Self::Args) -> Result<String, ToolError> {
                Err(ToolError::ToolCallError("Connection refused".into()))
            }
        }

        let handle = ToolServer::new().tool(Failing).run();

        let failed = handle.call_tool("failing", "{}").await.unwrap_err();
        assert_eq!(
            tool_error_message(&failed.to_string()),
            "Connection refused"
        );

        let missing = handle.call_tool("missing", "{}").await.unwrap_err();
        assert_eq!(
            tool_error_message(&missing.to_string()),
            "ToolNotFoundError: missing"
        );
    }

    #[test]
    fn error_messages_keep_the_kind_of_failure() {
        for text in [
            "Tool returned an error: 401 Unauthorized",
            "Tool execution failed: connection refused",
            "ToolNotFoundError: missing",
        ] {
            assert_eq!(tool_error_message(text), text);
        }
    }

    #[test]
    fn an_error_with_nothing_inside_its_wrapper_keeps_the_wrapper() {
        for text in ["Toolset error: ToolCallError: ", "ToolCallError:  "] {
            assert_eq!(tool_error_message(text), text);
        }
    }
}
