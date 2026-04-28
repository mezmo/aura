# Architecture Spec: AURA-RM-008 Structured Error Taxonomy

**Status:** Draft
**Roadmap Item:** AURA-RM-008
**Product Spec:** [product-spec.md](product-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Summary

Introduce a unified `AuraError` enum in the `aura` crate that classifies all runtime errors into a fixed taxonomy. Map existing error sources (`BuilderError`, `DetectedToolError`, `StreamTermination`) into this taxonomy. Extend the API `ErrorDetail` struct with an `error_code` field. This is a refactoring — no new features, just structured classification of errors that already exist.

## Constitution Compliance Check

- [x] Article I: API responses gain an additive `code` field. Existing `message` and `type` fields unchanged. No breaking change.
- [x] Article II: `AuraError` lives in `aura` crate (runtime concern). `ErrorDetail` extension is in `aura-web-server` (HTTP concern). Config errors stay in `aura-config`.
- [x] Article V: No new integration tests needed (unit-testable).
- [x] Article VI: No secrets involved.
- [x] Article VII: No config changes.

## Technical Context

- **Language/Version:** Rust (edition 2024, stable 1.93.1)
- **Affected Crates:** `aura` (new error module), `aura-web-server` (ErrorDetail extension)
- **New Dependencies:** None
- **Performance Objectives:** Zero overhead on happy path (error classification only occurs on error)

## Design

### Crate Changes

#### `aura` (core)

**New: `crates/aura/src/error_taxonomy.rs`**

```rust
/// Unified error taxonomy for all Aura runtime errors.
/// Each variant maps to a Prometheus-label-safe string.
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
    Cancelled,
    Internal,
}

impl ErrorCategory {
    /// Returns a Prometheus-label-safe string.
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
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
        }
    }

    /// Returns the error code (e.g., AURA-E-LLM-001).
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::LlmTimeout => "AURA-E-LLM-001",
            Self::LlmRateLimit => "AURA-E-LLM-002",
            Self::LlmAuthError => "AURA-E-LLM-003",
            Self::LlmError => "AURA-E-LLM-099",
            Self::McpConnectionFailed => "AURA-E-MCP-001",
            Self::McpToolError => "AURA-E-MCP-002",
            Self::McpTimeout => "AURA-E-MCP-003",
            Self::ConfigValidation => "AURA-E-CFG-001",
            Self::RequestValidation => "AURA-E-REQ-001",
            Self::BudgetExceeded => "AURA-E-BDG-001",
            Self::Cancelled => "AURA-E-CXL-001",
            Self::Internal => "AURA-E-INT-001",
        }
    }
}
```

**New: `AuraError` wrapper struct**

```rust
/// A classified runtime error with taxonomy category, code, and message.
pub struct AuraError {
    pub category: ErrorCategory,
    pub message: String,
}
```

**Mapping from existing types:**

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

impl From<&StreamTermination> for ErrorCategory {
    fn from(term: &StreamTermination) -> Self {
        match term {
            StreamTermination::Complete => ErrorCategory::Internal, // shouldn't be called
            StreamTermination::StreamError(_) => ErrorCategory::LlmError,
            StreamTermination::Disconnected => ErrorCategory::Cancelled,
            StreamTermination::Timeout => ErrorCategory::LlmTimeout,
            StreamTermination::Shutdown => ErrorCategory::Cancelled,
        }
    }
}
```

**Modified: `crates/aura/src/lib.rs`**

Add `pub mod error_taxonomy;` and re-export `ErrorCategory` and `AuraError`.

#### `aura-web-server`

**Modified: `crates/aura-web-server/src/types.rs`**

```rust
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,  // NEW — additive, optional
}
```

**Modified: `crates/aura-web-server/src/handlers.rs`**

Where errors are constructed, include the error code:

```rust
// Before:
ErrorDetail { message: "...", error_type: "internal_error".to_string() }

// After:
ErrorDetail {
    message: "...",
    error_type: ErrorCategory::Internal.as_label().to_string(),
    code: Some(ErrorCategory::Internal.error_code().to_string()),
}
```

### Error Handling Flow

```
Tool execution fails
  → DetectedToolError::McpToolError("connection refused")
    → ErrorCategory::McpConnectionFailed (via From impl)
      → AuraError { category: McpConnectionFailed, message: "..." }
        → ErrorDetail { type: "mcp_connection_failed", code: "AURA-E-MCP-001", message: "..." }
          → HTTP 500 JSON response

Stream timeout
  → StreamTermination::Timeout
    → ErrorCategory::LlmTimeout (via From impl)
      → ErrorDetail { type: "llm_timeout", code: "AURA-E-LLM-001", message: "..." }

Invalid request
  → ErrorCategory::RequestValidation
    → ErrorDetail { type: "request_validation", code: "AURA-E-REQ-001", message: "..." }
```

### Configuration

No new configuration. The taxonomy is code-defined.

## Migration / Backward Compatibility

- `ErrorDetail.error_type` strings change from `"internal_error"` to taxonomy labels like `"llm_timeout"`. Clients parsing `error_type` may need updates. This is a **behavioral change** but the `type` field was never documented as stable.
- New `code` field is additive and `skip_serializing_if = "Option::is_none"`, so it won't appear when None.
- Existing `BuilderError` enum is unchanged — it covers initialization, not runtime.

## Alternatives Considered

| Approach | Pros | Cons | Why Not |
|----------|------|------|---------|
| Extend BuilderError with runtime variants | Single enum | Mixes init and runtime concerns; BuilderError is already used in return types | Violates separation of concerns |
| Use error codes only (no enum) | Simple strings | No type safety, easy to misspell, no exhaustive matching | Defeats purpose of structured taxonomy |
| Use thiserror derive macro | Less boilerplate | Adds dependency, not needed for simple enum | Over-engineering for this scope |

## Risks

- **Behavioral change in error_type strings**: Clients matching on `"internal_error"` will need updates. Mitigated by documenting the change and keeping it within a major version.
- **Incomplete mapping**: Some error paths may not be covered initially. Mitigated by having an `Internal` catch-all and logging when it's used.

## Implementation Order

1. Create `error_taxonomy.rs` with `ErrorCategory` enum and methods — Satisfies: AC-008.1.1, AC-008.3.1
2. Add `From` impls for `DetectedToolError` and `StreamTermination` — Satisfies: AC-008.1.2, AC-008.1.3
3. Add `code` field to `ErrorDetail` — Satisfies: AC-008.2.1, AC-008.2.2
4. Update handler error construction to use taxonomy — Satisfies: AC-008.2.1
5. Update existing tests — Satisfies: AC-008.2.2 (backward compat verification)
