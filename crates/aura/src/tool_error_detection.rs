//! Detects tool errors from string results.
//!
//! Tool errors appear as prefixed strings from two sources:
//! - Rig's `ToolError` variants (`ToolCallError:`, `JsonError:`)
//! - Aura's MCP response handling (`Tool returned an error:` from mcp_response.rs)
//!
//! This module parses them back into structured types for `aura.tool_complete` events.

/// Detected tool error type from string-serialized tool results.
///
/// This enum represents the structured error types that can be detected
/// from prefixed error strings in tool results.
#[derive(Debug, Clone, PartialEq)]
pub enum DetectedToolError {
    /// `ToolCallError: {message}` - generic tool execution failure
    ///
    /// This covers most tool execution errors including:
    /// - Connection failures
    /// - Timeout errors
    /// - Tool-specific errors
    ToolCallError(String),

    /// `JsonError: {message}` - JSON serialization/deserialization failure
    ///
    /// Occurs when:
    /// - Tool arguments cannot be parsed
    /// - Tool output cannot be serialized
    JsonError(String),

    /// `Tool returned an error: {message}` - MCP tool error response
    ///
    /// Occurs when an MCP server returns an error response for a tool call,
    /// such as authentication failures or invalid parameters.
    McpToolError(String),
}

impl DetectedToolError {
    /// Extract the error message without the prefix.
    ///
    /// # Example
    /// ```
    /// use aura::tool_error_detection::DetectedToolError;
    ///
    /// let err = DetectedToolError::ToolCallError("Connection refused".to_string());
    /// assert_eq!(err.message(), "Connection refused");
    /// ```
    pub fn message(&self) -> &str {
        match self {
            Self::ToolCallError(msg) => msg,
            Self::JsonError(msg) => msg,
            Self::McpToolError(msg) => msg,
        }
    }

    /// Get the full error string including the original prefix.
    ///
    /// This reconstructs the exact format that Rig uses when converting
    /// ToolError to string.
    ///
    /// # Example
    /// ```
    /// use aura::tool_error_detection::DetectedToolError;
    ///
    /// let err = DetectedToolError::ToolCallError("Connection refused".to_string());
    /// assert_eq!(err.full_message(), "ToolCallError: Connection refused");
    /// ```
    pub fn full_message(&self) -> String {
        match self {
            Self::ToolCallError(msg) => format!("ToolCallError: {}", msg),
            Self::JsonError(msg) => format!("JsonError: {}", msg),
            Self::McpToolError(msg) => format!("Tool returned an error: {}", msg),
        }
    }

    /// Get a short error type name for logging/display.
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::ToolCallError(_) => "ToolCallError",
            Self::JsonError(_) => "JsonError",
            Self::McpToolError(_) => "McpToolError",
        }
    }
}

/// Result of analyzing a tool result for errors.
///
/// This enum is used to indicate whether a tool result represents
/// a successful execution or a detected error.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultStatus {
    /// Tool executed successfully - result is valid data
    Success,
    /// Tool failed with a detected error
    Error(DetectedToolError),
}

impl ToolResultStatus {
    /// Returns true if the tool executed successfully.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Returns true if the tool failed with an error.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    /// Returns the error if present, None otherwise.
    pub fn error(&self) -> Option<&DetectedToolError> {
        match self {
            Self::Error(e) => Some(e),
            Self::Success => None,
        }
    }

    /// Converts to Option<DetectedToolError>, consuming self.
    pub fn into_error(self) -> Option<DetectedToolError> {
        match self {
            Self::Error(e) => Some(e),
            Self::Success => None,
        }
    }
}

/// Analyze tool result text and detect if it contains a Rig error.
///
/// Uses strict prefix matching against Rig's thiserror-generated formats.
/// Only matches exact prefixes to avoid false positives on results that
/// happen to contain the word "error".
///
/// # Arguments
///
/// * `result_text` - The string content of a tool result
///
/// # Returns
///
/// * `ToolResultStatus::Success` - if no error prefix is detected
/// * `ToolResultStatus::Error(...)` - if an error prefix matches
///
/// # Example
///
/// ```
/// use aura::tool_error_detection::{detect_tool_error, ToolResultStatus, DetectedToolError};
///
/// // Detecting a tool call error
/// let status = detect_tool_error("ToolCallError: Connection refused");
/// assert!(matches!(status, ToolResultStatus::Error(DetectedToolError::ToolCallError(_))));
///
/// // Normal result (not an error)
/// let status = detect_tool_error("{\"files\": [\"a.txt\"]}");
/// assert!(status.is_success());
///
/// // Text containing "error" but not a prefix - still success
/// let status = detect_tool_error("Found 3 error logs in the file");
/// assert!(status.is_success());
/// ```
pub fn detect_tool_error(result_text: &str) -> ToolResultStatus {
    // Error prefixes from Rig (ToolCallError, JsonError) and Aura (MCP error)
    const TOOL_CALL_ERROR: &str = "ToolCallError: ";
    const JSON_ERROR: &str = "JsonError: ";
    const MCP_ERROR: &str = "Tool returned an error: "; // from aura/src/mcp_response.rs

    if let Some(msg) = result_text.strip_prefix(TOOL_CALL_ERROR) {
        ToolResultStatus::Error(DetectedToolError::ToolCallError(msg.to_string()))
    } else if let Some(msg) = result_text.strip_prefix(JSON_ERROR) {
        ToolResultStatus::Error(DetectedToolError::JsonError(msg.to_string()))
    } else if let Some(msg) = result_text.strip_prefix(MCP_ERROR) {
        ToolResultStatus::Error(DetectedToolError::McpToolError(msg.to_string()))
    } else {
        ToolResultStatus::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_tool_call_error() {
        let result = detect_tool_error("ToolCallError: Connection refused");
        assert!(matches!(
            result,
            ToolResultStatus::Error(DetectedToolError::ToolCallError(_))
        ));
        assert_eq!(result.error().unwrap().message(), "Connection refused");
        assert_eq!(
            result.error().unwrap().full_message(),
            "ToolCallError: Connection refused"
        );
    }

    #[test]
    fn test_detect_json_error() {
        let result = detect_tool_error("JsonError: expected value at line 1 column 1");
        assert!(matches!(
            result,
            ToolResultStatus::Error(DetectedToolError::JsonError(_))
        ));
        assert_eq!(
            result.error().unwrap().message(),
            "expected value at line 1 column 1"
        );
    }

    #[test]
    fn test_detect_mcp_error() {
        let result = detect_tool_error("Tool returned an error: 401 Unauthorized");
        assert!(matches!(
            result,
            ToolResultStatus::Error(DetectedToolError::McpToolError(_))
        ));
        assert_eq!(result.error().unwrap().message(), "401 Unauthorized");
    }

    #[test]
    fn test_success_normal_json_result() {
        let result = detect_tool_error("{\"files\": [\"a.txt\", \"b.txt\"]}");
        assert!(result.is_success());
        assert!(!result.is_error());
        assert!(result.error().is_none());
    }

    #[test]
    fn test_success_normal_text_result() {
        let result = detect_tool_error("Pipeline list_files executed successfully");
        assert!(result.is_success());
    }

    #[test]
    fn test_success_contains_error_word_not_prefix() {
        // Should NOT match - "error" is in the text but not a prefix
        let result = detect_tool_error("Found 3 error logs in the file");
        assert!(result.is_success());
    }

    #[test]
    fn test_success_contains_toolcallerror_not_prefix() {
        // Should NOT match - substring but not prefix
        let result = detect_tool_error("The ToolCallError: message was handled");
        assert!(result.is_success());
    }

    #[test]
    fn test_error_type_names() {
        assert_eq!(
            DetectedToolError::ToolCallError("test".into()).error_type(),
            "ToolCallError"
        );
        assert_eq!(
            DetectedToolError::JsonError("test".into()).error_type(),
            "JsonError"
        );
        assert_eq!(
            DetectedToolError::McpToolError("test".into()).error_type(),
            "McpToolError"
        );
    }

    #[test]
    fn test_into_error() {
        let status = detect_tool_error("ToolCallError: test");
        let error = status.into_error();
        assert!(error.is_some());
        assert_eq!(error.unwrap().message(), "test");

        let status = detect_tool_error("success");
        assert!(status.into_error().is_none());
    }
}
