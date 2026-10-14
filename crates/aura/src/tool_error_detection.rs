//! Decides whether a tool result's text reports a failure. Tool failures
//! reach the model as result text, not as an `Err`.

use crate::mcp::MCP_ERROR_PREFIX;

const ERROR_PREFIXES: &[&str] = &[
    // Rig's `ToolServerError`, the text a tool's `Err` reaches the model as.
    "Toolset error: ",
    "Sending message was cancelled",
    "Error while sending message: ",
    "An invalid message type was returned",
    MCP_ERROR_PREFIX,
    // `McpManager::execute_fallback_tool`
    "Tool execution failed: ",
    "Tool not found: ",
    "Invalid tool arguments: ",
];

/// Whether `result_text` is a tool failure rather than a result.
///
/// Matches whole prefixes only, so a result that merely mentions an error is
/// still a success.
///
/// ```
/// use aura::tool_error_detection::is_tool_error;
///
/// assert!(is_tool_error("Tool returned an error: 401 Unauthorized"));
/// assert!(!is_tool_error("Found 3 error logs in the file"));
/// ```
pub fn is_tool_error(result_text: &str) -> bool {
    ERROR_PREFIXES
        .iter()
        .any(|prefix| result_text.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_error_results_are_errors() {
        let result = rmcp::model::CallToolResult::error(vec![rmcp::model::Content::text(
            "401 Unauthorized",
        )]);
        let text = crate::mcp::extract_tool_result(result, "auth_tool");
        assert!(is_tool_error(&text), "{text}");
    }

    /// A tool's `Err` reaches the model through rig's tool server, which
    /// wraps it in its own error prefixes.
    #[tokio::test]
    async fn rig_wrapped_tool_errors_are_errors() {
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
        assert!(is_tool_error(&failed.to_string()), "{failed}");

        let missing = handle.call_tool("missing", "{}").await.unwrap_err();
        assert!(is_tool_error(&missing.to_string()), "{missing}");
    }

    #[test]
    fn results_that_mention_errors_are_successes() {
        assert!(!is_tool_error("{\"files\": [\"a.txt\", \"b.txt\"]}"));
        assert!(!is_tool_error("Found 3 error logs in the file"));
        assert!(!is_tool_error("Timeout: probe exceeded 5s"));
        assert!(!is_tool_error("HTTP error: 503 from upstream"));
        assert!(!is_tool_error(""));
    }
}
