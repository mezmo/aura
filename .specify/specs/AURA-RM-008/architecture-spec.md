# Architecture Spec: AURA-RM-008 Structured Error Taxonomy

**Status:** In Review (Remediation Round 1)
**Roadmap Item:** AURA-RM-008
**Product Spec:** [product-spec.md](product-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Summary

Introduce a unified `ErrorCategory` enum in the `aura` crate that classifies all runtime errors into a fixed taxonomy. Add a sanitization layer that separates internal error messages (for logs) from generic client-facing messages (for API responses). Extend both `ErrorDetail` and `ChatCompletionErrorDetail` structs with a `code` field. Map existing error sources into the taxonomy at their crate boundaries — `DetectedToolError` mapping in `aura`, `StreamTermination` mapping in `aura-web-server`.

## Constitution Compliance Check

- [x] Article I: Existing `type` field values (`"internal_error"`, `"invalid_request_error"`, `"service_unavailable"`) are FROZEN. New `code` field is additive only. No breaking change.
- [x] Article II: `ErrorCategory` lives in `aura` crate. `StreamTermination` → `ErrorCategory` mapping lives in `aura-web-server` (where `StreamTermination` is defined). No circular dependencies.
- [x] Article IV: Implementation commits will follow conventional commit format.
- [x] Article V: Unit tests only — no new integration test feature flags needed.
- [x] Article VI: No secrets involved.
- [x] Article VII: No config changes.

## Technical Context

- **Affected Crates:** `aura` (new error_taxonomy module), `aura-web-server` (ErrorDetail extension, StreamTermination mapping)
- **New Dependencies:** None. (`thiserror` is already in workspace dependencies but not needed for this simple enum.)
- **Performance Objectives:** Zero overhead on happy path (error classification only occurs on error)

## Design

### Crate Changes

#### `aura` (core)

**New: `crates/aura/src/error_taxonomy.rs`**

```rust
/// Unified error taxonomy for all Aura runtime errors.
/// Each variant maps to a Prometheus-label-safe string and a generic client message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// All ErrorCategory variants, for exhaustive iteration in tests.
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

/// A classified runtime error with taxonomy category and internal message.
pub struct AuraError {
    pub category: ErrorCategory,
    /// Internal message for server-side logging. Never expose to clients.
    pub internal_message: String,
}

impl AuraError {
    pub fn new(category: ErrorCategory, internal_message: impl Into<String>) -> Self {
        Self { category, internal_message: internal_message.into() }
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
```

**Mapping from DetectedToolError (in `aura` crate — same crate boundary):**

```rust
impl From<&DetectedToolError> for ErrorCategory {
    fn from(err: &DetectedToolError) -> Self {
        match err {
            DetectedToolError::McpToolError(_) => ErrorCategory::McpToolError,
            DetectedToolError::ToolCallError(_) => ErrorCategory::McpToolError,
            DetectedToolError::JsonError(_) => ErrorCategory::Internal,
        }
    }
}
```

Note: `ToolCallError` maps to `McpToolError` because at the `DetectedToolError` level, the connection/timeout distinction is lost (the error is already an opaque string). Sub-classification of MCP errors into `McpConnectionFailed` vs `McpTimeout` will happen at the MCP execution layer in a future enhancement when richer error types are available from the transport layer.

**Modified: `crates/aura/src/lib.rs`**

Add `pub mod error_taxonomy;` and re-export `ErrorCategory`, `AuraError`, `ALL_CATEGORIES`.

#### `aura-web-server`

**StreamTermination → ErrorCategory mapping (lives HERE, not in `aura` crate):**

```rust
// In crates/aura-web-server/src/streaming/handlers.rs or a new mapping module
impl From<&StreamTermination> for ErrorCategory {
    fn from(term: &StreamTermination) -> Self {
        match term {
            StreamTermination::Complete => ErrorCategory::Internal, // should not be called on success
            StreamTermination::StreamError(_) => ErrorCategory::LlmError,
            StreamTermination::Disconnected => ErrorCategory::Cancelled,
            StreamTermination::Timeout => ErrorCategory::LlmTimeout,
            StreamTermination::Shutdown => ErrorCategory::ServiceUnavailable,
        }
    }
}
```

**Modified: `crates/aura-web-server/src/types.rs` — both error structs:**

```rust
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    /// Taxonomy error code from ErrorCategory. Additive field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

// Also update ChatCompletionErrorDetail to include `code` for consistency:
#[derive(Debug, Serialize)]
pub struct ChatCompletionErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}
```

**Modified: `crates/aura-web-server/src/handlers.rs` — error construction:**

Where errors are built, use `AuraError` for classification and sanitization:

```rust
// Before (current code, line 133):
ErrorDetail {
    message: format!("Failed to build agent: {e}"),
    error_type: "internal_error".to_string(),
}

// After:
let aura_err = AuraError::new(ErrorCategory::Internal, format!("Failed to build agent: {e}"));
tracing::warn!(internal_message = %aura_err.internal_message, "Request error");
ErrorDetail {
    message: aura_err.client_message(),            // generic, safe
    error_type: "internal_error".to_string(),       // FROZEN — not changed
    code: Some(aura_err.category.as_label().to_string()), // NEW taxonomy label
}
```

### Error Handling Flow

```
Tool execution fails
  → DetectedToolError::McpToolError("connection refused to pagerduty:8080")
    → ErrorCategory::McpToolError (via From impl in aura crate)
      → AuraError { category: McpToolError, internal_message: "connection refused..." }
        → LOGGED: warn!("MCP tool error: connection refused to pagerduty:8080")
        → API: ErrorDetail {
            message: "A tool execution error occurred",   // sanitized
            type: "internal_error",                        // FROZEN
            code: "mcp_tool_error"                         // NEW taxonomy label
          }

Stream timeout
  → StreamTermination::Timeout
    → ErrorCategory::LlmTimeout (via From impl in aura-web-server crate)
      → API: ErrorDetail { type: "internal_error", code: "llm_timeout", message: "..." }

Shutdown guard
  → ErrorCategory::ServiceUnavailable
    → API: ErrorDetail { type: "service_unavailable", code: "service_unavailable", message: "..." }

Invalid request
  → ErrorCategory::RequestValidation
    → API: ErrorDetail {
        type: "invalid_request_error",                     // FROZEN
        code: "request_validation",
        message: "Last message must be from user, got: system"  // pass-through (client input)
      }
```

### Configuration

No new configuration. The taxonomy is code-defined.

## Migration / Backward Compatibility

- **`error_type` values are FROZEN**: `"internal_error"`, `"invalid_request_error"`, `"service_unavailable"` remain unchanged. Clients matching on these values are NOT broken.
- **New `code` field is additive**: Uses `skip_serializing_if = "Option::is_none"` so it only appears when set.
- **Error messages change**: Client-facing messages become generic. This is a behavioral change but improves security. Clients that parsed error message strings for debugging will need to use server-side logs instead.
- **Both `ErrorDetail` and `ChatCompletionErrorDetail`** gain the `code` field for consistency.

## Alternatives Considered

| Approach | Pros | Cons | Why Not |
|----------|------|------|---------|
| Change `error_type` values to taxonomy labels | Simpler, one field | Breaks Article I, breaks existing clients | Violates constitution |
| Add external error code prefix (AURA-E-MCP-001) | Readable codes | Enables system fingerprinting, adds complexity | Security concern outweighs readability |
| Use thiserror derive | Available (already a workspace dep) | Over-engineering for a classification enum with no error chaining | YAGNI — simple match is sufficient |

## Risks

- **Generic messages reduce debuggability for operators**: Mitigated by logging the full internal message at WARN level. Operators check logs, not API responses.
- **`mcp_connection_failed` and `mcp_timeout` categories may not be emitted initially**: The `DetectedToolError` → `ErrorCategory` mapping loses this granularity. These categories exist for future use when MCP transport errors are richer. Document this as a known limitation.
- **Incomplete mapping**: Some error paths may fall through to `Internal`. Mitigated by logging when `Internal` is used so we can add specific categories over time.

## Implementation Order

1. Create `error_taxonomy.rs` with `ErrorCategory` enum, `ALL_CATEGORIES`, `client_message()`, `as_label()` — Satisfies: AC-008.1.1, AC-008.4.1
2. Create `AuraError` struct with `new()`, `client_message()` — Satisfies: AC-008.3.1, AC-008.3.2
3. Add `From<&DetectedToolError>` impl — Satisfies: AC-008.1.2
4. Add `code` field to both `ErrorDetail` and `ChatCompletionErrorDetail` — Satisfies: AC-008.2.1, AC-008.2.2
5. Add `From<&StreamTermination>` impl in `aura-web-server` — Satisfies: AC-008.1.3
6. Update handler error construction to use `AuraError` with sanitization — Satisfies: AC-008.3.1
7. Unit tests — Satisfies: all ACs via quality spec
