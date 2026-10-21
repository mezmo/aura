//! Decides whether a tool result's text reports a failure, and what part of
//! it to show people. Tool failures reach the model as result text, not as
//! an `Err`.

use crate::hitl::DENIAL_PREFIX;
use crate::mcp::MCP_ERROR_PREFIX;

/// Rig's `ToolServerError`, the text a tool's `Err` reaches the model as.
const TOOLSET_ERROR_PREFIX: &str = "Toolset error: ";

/// Rig's `ToolError` and `ToolSetError`, which can nest several deep.
const TOOL_CALL_ERROR_PREFIX: &str = "ToolCallError: ";

const ERROR_PREFIXES: &[&str] = &[
    TOOLSET_ERROR_PREFIX,
    "Sending message was cancelled",
    "Error while sending message: ",
    "An invalid message type was returned",
    MCP_ERROR_PREFIX,
    DENIAL_PREFIX,
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

/// Whether `result_text` is a HITL approver's denial, a failure a person
/// chose rather than a fault.
pub fn is_tool_denial(result_text: &str) -> bool {
    result_text.starts_with(DENIAL_PREFIX)
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
    if message.is_empty() {
        error_text
    } else {
        message
    }
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
        assert_eq!(
            tool_error_message(&failed.to_string()),
            "Connection refused"
        );

        let missing = handle.call_tool("missing", "{}").await.unwrap_err();
        assert!(is_tool_error(&missing.to_string()), "{missing}");
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
        let denial = format!("{DENIAL_PREFIX}not today");
        assert_eq!(tool_error_message(&denial), denial);
        assert_eq!(
            tool_error_message("Toolset error: ToolNotFoundError: missing"),
            "ToolNotFoundError: missing"
        );
    }

    #[test]
    fn an_error_with_nothing_inside_its_wrapper_keeps_the_wrapper() {
        assert_eq!(
            tool_error_message("Toolset error: ToolCallError: "),
            "Toolset error: ToolCallError: "
        );
    }

    #[test]
    fn only_a_denial_is_a_denial() {
        let denial = format!("{DENIAL_PREFIX}not today");
        assert!(is_tool_denial(&denial));
        assert!(is_tool_error(&denial));
        assert!(!is_tool_denial("Tool returned an error: 401 Unauthorized"));
        assert!(!is_tool_denial("denied by policy"));
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
