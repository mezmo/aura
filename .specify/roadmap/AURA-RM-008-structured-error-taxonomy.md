# AURA-RM-008: Structured Error Taxonomy

**Priority:** P2 (Medium) — but executes FIRST due to dependencies
**Status:** Not Started
**Dependencies:** None (foundation item)
**Depended on by:** AURA-RM-001, AURA-RM-002, AURA-RM-003, AURA-RM-004, AURA-RM-007
**Affected Crates:** `aura`, `aura-web-server`, `aura-config`
**Complexity:** Medium

## Rationale

Error types are currently ad-hoc. `BuilderError` in `error.rs` has generic variants. `DetectedToolError` in `tool_error_detection.rs` parses error strings back into structured types. There is no unified taxonomy that distinguishes LLM timeout from MCP failure from validation error. This is the foundation for metrics labels (RM-001), health check results (RM-002), and circuit breaker classification (RM-003).

## User Stories

### US-008.1: Unified Error Enum

**As a** developer,
**I want** a unified error enum that categorizes all Aura errors,
**So that** error handling is consistent across the codebase.

#### Acceptance Criteria

**AC-008.1.1:** Error categories
- **Given** any error in the Aura system
- **When** it is propagated
- **Then** it can be classified into one of: `LlmTimeout`, `LlmRateLimit`, `LlmAuthError`, `McpConnectionFailed`, `McpToolError`, `McpTimeout`, `ConfigValidation`, `RequestValidation`, `VectorStoreError`, `BudgetExceeded`, `Internal`

### US-008.2: Error Codes in API Responses

**As an** operator,
**I want** error responses to include an error code (not just a message),
**So that** I can build programmatic error handling.

#### Acceptance Criteria

**AC-008.2.1:** Structured error response
- **Given** an error occurs during request processing
- **When** the error response is returned
- **Then** it includes `error_code` alongside `message`:
  ```json
  {
    "error": {
      "message": "MCP server 'pagerduty' is unreachable",
      "error_type": "mcp_connection_failed",
      "error_code": "AURA-E-MCP-001"
    }
  }
  ```

### US-008.3: Error Metrics Labels

**As an** operator,
**I want** error metrics categorized by this taxonomy,
**So that** I can set alerts on specific failure modes.

#### Acceptance Criteria

**AC-008.3.1:** Error type as metric label
- **Given** the error taxonomy and metrics endpoint (AURA-RM-001) exist
- **When** errors are recorded in metrics
- **Then** the `error_type` label matches the taxonomy categories

## Implementation Notes

- This is a refactoring effort — replacing ad-hoc error handling with structured types
- Must be backward-compatible: existing API error responses still work, new fields are additive
- The `error_type` field in `ErrorDetail` (handlers.rs) already exists — extend it with the taxonomy
