# Product Spec: AURA-RM-008 Structured Error Taxonomy

**Status:** Implemented
**Roadmap Item:** AURA-RM-008
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Problem Statement

Aura's error handling is ad-hoc. `BuilderError` covers initialization only. Runtime errors (tool failures, LLM timeouts, MCP disconnects, cancellations) flow as unstructured strings. The `error_type` field in API responses uses freeform strings like `"internal_error"` and `"invalid_request_error"` with no consistency. Error messages contain internal implementation details (MCP server names, hostnames, library errors) that leak to API consumers. This makes it impossible to:
- Set differentiated alerts (LLM timeout vs MCP failure)
- Build programmatic error handling in clients
- Use error types as metric labels (needed by AURA-RM-001)
- Prevent information disclosure to untrusted callers

## Scope

### In Scope
- Unified error enum covering all Aura runtime error categories
- Structured error codes in API responses (`code` field — additive)
- Error classification for: LLM errors, MCP errors, config errors, request validation, budget, service unavailable, cancellation, internal
- Error message sanitization: separate internal messages (for logs) from client-facing messages (for API responses)
- Backward-compatible API responses: existing `type` field values PRESERVED, new `code` field is additive

### Out of Scope
- Retry logic (AURA-RM-003)
- Circuit breaker state (AURA-RM-003)
- Metrics recording of errors (AURA-RM-001 — consumes this taxonomy)
- Error UI/dashboard

## User Stories

### US-008.1: Unified Error Enum

**As a** developer working on Aura,
**I want** a single error taxonomy that classifies all runtime errors,
**So that** error handling is consistent and errors can be categorized for metrics and alerting.

**Priority:** P1
**Rationale:** Foundation for AURA-RM-001, AURA-RM-002, AURA-RM-003, AURA-RM-004, AURA-RM-007

#### Acceptance Criteria

**AC-008.1.1:** Error categories cover all runtime paths
- **Given** any error that can occur during request processing
- **When** it is propagated through the system
- **Then** it can be classified into exactly one of:
  - `llm_timeout` — LLM provider did not respond within timeout
  - `llm_rate_limit` — LLM provider returned 429
  - `llm_auth_error` — LLM provider rejected credentials
  - `llm_error` — other LLM provider error
  - `mcp_connection_failed` — MCP server unreachable
  - `mcp_tool_error` — MCP tool returned an error result
  - `mcp_timeout` — MCP tool call timed out
  - `config_validation` — invalid configuration
  - `request_validation` — invalid client request (bad JSON, empty messages, etc.)
  - `budget_exceeded` — token budget exhausted (AURA-RM-005)
  - `service_unavailable` — server shutting down or overloaded
  - `cancelled` — client disconnected or request cancelled
  - `internal` — unexpected internal error

**AC-008.1.2:** Existing DetectedToolError maps to taxonomy
- **Given** a tool result classified by `tool_error_detection.rs`
- **When** it is a `ToolCallError`, `JsonError`, or `McpToolError`
- **Then** each maps to the corresponding taxonomy category:
  - `McpToolError` → `mcp_tool_error`
  - `ToolCallError` → `mcp_tool_error` (generic tool execution failure)
  - `JsonError` → `internal`

**AC-008.1.3:** StreamTermination maps to taxonomy
- **Given** a streaming request terminates
- **When** the termination reason is `StreamError`, `Disconnected`, `Timeout`, or `Shutdown`
- **Then** each maps to the corresponding taxonomy category:
  - `StreamError` → `llm_error`
  - `Disconnected` → `cancelled`
  - `Timeout` → `llm_timeout`
  - `Shutdown` → `service_unavailable`

Note: The `StreamTermination` → `ErrorCategory` mapping lives in `aura-web-server` (not `aura`), because `StreamTermination` is defined in the web server crate.

### US-008.2: Error Codes in API Responses

**As an** API consumer,
**I want** error responses to include a structured `code` field alongside the existing fields,
**So that** I can build programmatic error handling without parsing error message strings.

**Priority:** P1

#### Acceptance Criteria

**AC-008.2.1:** Error code in response (additive only)
- **Given** an error occurs during request processing
- **When** the error response is returned to the client
- **Then** the response includes a new `code` field:
  ```json
  {
    "error": {
      "message": "A downstream service is temporarily unavailable",
      "type": "internal_error",
      "code": "mcp_connection_failed"
    }
  }
  ```
- **And** the existing `type` field retains its current values (`"internal_error"`, `"invalid_request_error"`, `"service_unavailable"`) — these are NOT changed to taxonomy labels (Article I compliance)
- **And** the new `code` field uses the taxonomy label (e.g., `mcp_connection_failed`, `llm_timeout`)

**AC-008.2.2:** Backward compatible
- **Given** an existing client that only reads `error.message` and `error.type`
- **When** an error response is returned with the new `error.code` field
- **Then** the existing client is not broken (new field is additive, existing values unchanged)

#### Edge Cases
- The `type` field values are FROZEN at their current values. Only the new `code` field uses taxonomy labels.
- `ErrorDetail` gains a new `code: Option<String>` field for the taxonomy label.
- `ChatCompletionErrorDetail` already has a `code: String` field used for OpenAI-compatible error codes (`"missing_required_parameter"`, `"model_not_found"`). This field is LEFT UNCHANGED. A separate `error_category: Option<String>` field is added for the taxonomy label to avoid collision.

### US-008.3: Error Message Sanitization

**As a** security-conscious operator,
**I want** client-facing error messages to be generic and not leak internal details,
**So that** API consumers cannot learn about internal MCP server names, hostnames, or library internals.

**Priority:** P1

#### Acceptance Criteria

**AC-008.3.1:** Client messages are generic
- **Given** an internal error like "MCP server 'pagerduty' at 10.0.1.5:8080 connection refused"
- **When** the error response is returned to the client
- **Then** the `message` field contains a safe, generic string like "A downstream service is temporarily unavailable"
- **And** the full internal message is logged server-side at WARN or ERROR level

**AC-008.3.2:** Each error category has a fixed client message
- **Given** the error taxonomy
- **When** any category is converted to a client-facing message
- **Then** it uses one of these fixed messages:

| Category | Client Message |
|----------|---------------|
| `llm_timeout` | "The language model did not respond in time" |
| `llm_rate_limit` | "The language model is temporarily rate limited" |
| `llm_auth_error` | "An authentication error occurred with an upstream provider" |
| `llm_error` | "An error occurred with the language model" |
| `mcp_connection_failed` | "A downstream service is temporarily unavailable" |
| `mcp_tool_error` | "A tool execution error occurred" |
| `mcp_timeout` | "A tool call did not respond in time" |
| `config_validation` | "Server configuration error" |
| `request_validation` | "Invalid request" (default), or pass-through of the user-input-derived message via `AuraError::client_message()`. SAFETY: only pass through messages derived from client input, never library error strings. |
| `budget_exceeded` | "Token budget exceeded for this request" |
| `service_unavailable` | "Server is shutting down" |
| `cancelled` | "Request was cancelled" |
| `internal` | "An internal error occurred" |

### US-008.4: Error Type as Metric Label

**As an** operator,
**I want** the error taxonomy to produce string labels suitable for Prometheus metrics,
**So that** AURA-RM-001 can use `error_type` as a metric label without transformation.

**Priority:** P1

#### Acceptance Criteria

**AC-008.4.1:** Label-safe error type strings
- **Given** the error taxonomy enum
- **When** converted to a string label
- **Then** the result is lowercase, underscore-separated, and Prometheus-label-safe (matches `^[a-z][a-z0-9_]*$`)

## Dependencies

- **Depends on:** Nothing (foundation item)
- **Depended on by:** AURA-RM-001 (Metrics), AURA-RM-002 (Health), AURA-RM-003 (Circuit Breaker), AURA-RM-004 (Auth), AURA-RM-007 (Incident Response)

## Success Criteria

- All runtime error paths produce a classified error with taxonomy category
- API error responses include the new `code` field while preserving existing `type` values
- Client-facing messages never contain internal hostnames, server names, or library errors
- Internal details are logged server-side for debugging
- Existing tests and API clients are not broken
- Error type strings are usable as Prometheus metric labels

## Open Questions

- None remaining after review remediation
