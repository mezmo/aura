//! Unified error taxonomy for all Aura runtime errors.
//!
//! Classifies errors into categories suitable for Prometheus metric labels,
//! structured API responses, and differentiated alerting.

use crate::tool_error_detection::DetectedToolError;

/// Unified error taxonomy for all Aura runtime errors.
/// Each variant maps to a Prometheus-label-safe string and a generic client message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    LlmTimeout,
    LlmRateLimit,
    LlmAuthError,
    LlmError,
    McpConnectionFailed,
    McpToolError,
    McpTimeout,
    ConfigValidation,
    RequestValidation,
    BudgetExceeded,
    ServiceUnavailable,
    Cancelled,
    Internal,
}

/// All ErrorCategory variants for exhaustive iteration in tests.
pub const ALL_CATEGORIES: &[ErrorCategory] = &[
    ErrorCategory::LlmTimeout,
    ErrorCategory::LlmRateLimit,
    ErrorCategory::LlmAuthError,
    ErrorCategory::LlmError,
    ErrorCategory::McpConnectionFailed,
    ErrorCategory::McpToolError,
    ErrorCategory::McpTimeout,
    ErrorCategory::ConfigValidation,
    ErrorCategory::RequestValidation,
    ErrorCategory::BudgetExceeded,
    ErrorCategory::ServiceUnavailable,
    ErrorCategory::Cancelled,
    ErrorCategory::Internal,
];

impl ErrorCategory {
    /// Returns a Prometheus-label-safe string (lowercase, underscores only).
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::LlmTimeout => "llm_timeout",
            Self::LlmRateLimit => "llm_rate_limit",
            Self::LlmAuthError => "llm_auth_error",
            Self::LlmError => "llm_error",
            Self::McpConnectionFailed => "mcp_connection_failed",
            Self::McpToolError => "mcp_tool_error",
            Self::McpTimeout => "mcp_timeout",
            Self::ConfigValidation => "config_validation",
            Self::RequestValidation => "request_validation",
            Self::BudgetExceeded => "budget_exceeded",
            Self::ServiceUnavailable => "service_unavailable",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
        }
    }

    /// Returns a safe, generic client-facing message.
    /// Internal details must NEVER appear in this output.
    pub fn client_message(&self) -> &'static str {
        match self {
            Self::LlmTimeout => "The language model did not respond in time",
            Self::LlmRateLimit => "The language model is temporarily rate limited",
            Self::LlmAuthError => "An authentication error occurred with an upstream provider",
            Self::LlmError => "An error occurred with the language model",
            Self::McpConnectionFailed => "A downstream service is temporarily unavailable",
            Self::McpToolError => "A tool execution error occurred",
            Self::McpTimeout => "A tool call did not respond in time",
            Self::ConfigValidation => "Server configuration error",
            Self::RequestValidation => "Invalid request",
            Self::BudgetExceeded => "Token budget exceeded for this request",
            Self::ServiceUnavailable => "Server is shutting down",
            Self::Cancelled => "Request was cancelled",
            Self::Internal => "An internal error occurred",
        }
    }
}

impl std::fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_label())
    }
}

impl From<&DetectedToolError> for ErrorCategory {
    fn from(err: &DetectedToolError) -> Self {
        match err {
            DetectedToolError::McpToolError(_) => ErrorCategory::McpToolError,
            DetectedToolError::ToolCallError(_) => ErrorCategory::McpToolError,
            DetectedToolError::JsonError(_) => ErrorCategory::Internal,
        }
    }
}

/// A classified runtime error with taxonomy category and internal message.
pub struct AuraError {
    pub category: ErrorCategory,
    /// Internal message for server-side logging. Never expose to clients.
    pub internal_message: String,
}

impl AuraError {
    pub fn new(category: ErrorCategory, internal_message: impl Into<String>) -> Self {
        Self {
            category,
            internal_message: internal_message.into(),
        }
    }

    /// Safe message for API responses. Uses fixed generic text per category.
    /// For RequestValidation, passes through the internal message (client input is safe).
    pub fn client_message(&self) -> String {
        match self.category {
            ErrorCategory::RequestValidation => self.internal_message.clone(),
            _ => self.category.client_message().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_categories_have_non_empty_labels() {
        for category in ALL_CATEGORIES {
            let label = category.as_label();
            assert!(!label.is_empty(), "empty label for {:?}", category);
        }
    }

    #[test]
    fn test_all_labels_are_prometheus_safe() {
        let pattern = regex::Regex::new(r"^[a-z][a-z0-9_]*$").unwrap();
        for category in ALL_CATEGORIES {
            let label = category.as_label();
            assert!(
                pattern.is_match(label),
                "label {:?} does not match Prometheus pattern",
                label
            );
        }
    }

    #[test]
    fn test_all_labels_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for category in ALL_CATEGORIES {
            let label = category.as_label();
            assert!(seen.insert(label), "duplicate label: {}", label);
        }
    }

    #[test]
    fn test_all_categories_count() {
        assert_eq!(ALL_CATEGORIES.len(), 13);
    }

    #[test]
    fn test_all_categories_have_non_empty_client_messages() {
        for category in ALL_CATEGORIES {
            let msg = category.client_message();
            assert!(!msg.is_empty(), "empty client message for {:?}", category);
        }
    }

    #[test]
    fn test_client_messages_do_not_contain_internal_details() {
        let suspicious_patterns = ["10.0.", "127.0.", "localhost", "::1", ".rs:", "panicked"];
        for category in ALL_CATEGORIES {
            let msg = category.client_message();
            for pattern in &suspicious_patterns {
                assert!(
                    !msg.contains(pattern),
                    "client message for {:?} contains suspicious pattern {:?}: {}",
                    category,
                    pattern,
                    msg
                );
            }
        }
    }

    #[test]
    fn test_detected_tool_error_mcp_maps_to_mcp_tool_error() {
        let err = DetectedToolError::McpToolError("connection refused".to_string());
        assert_eq!(ErrorCategory::from(&err), ErrorCategory::McpToolError);
    }

    #[test]
    fn test_detected_tool_error_tool_call_maps_to_mcp_tool_error() {
        let err = DetectedToolError::ToolCallError("timeout".to_string());
        assert_eq!(ErrorCategory::from(&err), ErrorCategory::McpToolError);
    }

    #[test]
    fn test_detected_tool_error_json_maps_to_internal() {
        let err = DetectedToolError::JsonError("invalid json".to_string());
        assert_eq!(ErrorCategory::from(&err), ErrorCategory::Internal);
    }

    #[test]
    fn test_aura_error_client_message_sanitizes_internal_details() {
        let err = AuraError::new(
            ErrorCategory::McpConnectionFailed,
            "MCP server 'pagerduty' at 10.0.1.5:8080 connection refused",
        );
        let msg = err.client_message();
        assert_eq!(msg, "A downstream service is temporarily unavailable");
        assert!(!msg.contains("pagerduty"));
        assert!(!msg.contains("10.0.1.5"));
        assert!(!msg.contains("8080"));
    }

    #[test]
    fn test_aura_error_request_validation_passes_through() {
        let err = AuraError::new(
            ErrorCategory::RequestValidation,
            "Last message must be from user, got: system",
        );
        assert_eq!(
            err.client_message(),
            "Last message must be from user, got: system"
        );
    }

    #[test]
    fn test_aura_error_non_validation_uses_generic_message() {
        for category in ALL_CATEGORIES {
            if *category == ErrorCategory::RequestValidation {
                continue;
            }
            let err = AuraError::new(*category, "SECRET internal detail 10.0.1.5:8080");
            let msg = err.client_message();
            assert!(
                !msg.contains("SECRET"),
                "category {:?} leaked internal message: {}",
                category,
                msg
            );
        }
    }

    #[test]
    fn test_display_uses_label() {
        assert_eq!(format!("{}", ErrorCategory::LlmTimeout), "llm_timeout");
        assert_eq!(
            format!("{}", ErrorCategory::McpConnectionFailed),
            "mcp_connection_failed"
        );
    }
}
