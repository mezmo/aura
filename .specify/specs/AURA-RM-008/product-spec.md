# Product Spec: AURA-RM-008 Structured Error Taxonomy

**Status:** Draft
**Roadmap Item:** AURA-RM-008
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Problem Statement

Aura's error handling is ad-hoc. `BuilderError` covers initialization only. Runtime errors (tool failures, LLM timeouts, MCP disconnects, cancellations) flow as unstructured strings. The `error_type` field in API responses uses freeform strings like `"internal_error"` and `"invalid_request_error"` with no consistency. This makes it impossible to:
- Set differentiated alerts (LLM timeout vs MCP failure)
- Build programmatic error handling in clients
- Use error types as metric labels (needed by AURA-RM-001)

## Scope

### In Scope
- Unified error enum covering all Aura runtime error categories
- Structured error codes in API responses (`error_code` field)
- Error classification for: LLM errors, MCP errors, config errors, request validation, budget, internal
- Backward-compatible API responses (existing fields preserved, new fields additive)

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
  - `cancelled` — client disconnected or request cancelled
  - `internal` — unexpected internal error

**AC-008.1.2:** Existing DetectedToolError maps to taxonomy
- **Given** a tool result classified by `tool_error_detection.rs`
- **When** it is a `ToolCallError`, `JsonError`, or `McpToolError`
- **Then** it maps to the corresponding taxonomy category (`mcp_tool_error` or `internal`)

**AC-008.1.3:** StreamTermination maps to taxonomy
- **Given** a streaming request terminates
- **When** the termination reason is `StreamError`, `Disconnected`, `Timeout`, or `Shutdown`
- **Then** each maps to the corresponding taxonomy category

### US-008.2: Error Codes in API Responses

**As an** API consumer,
**I want** error responses to include a structured `error_code` alongside the message,
**So that** I can build programmatic error handling without parsing error message strings.

**Priority:** P1

#### Acceptance Criteria

**AC-008.2.1:** Error code in response
- **Given** an error occurs during request processing
- **When** the error response is returned to the client
- **Then** the response includes `error_code`:
  ```json
  {
    "error": {
      "message": "MCP server 'pagerduty' connection refused",
      "type": "mcp_connection_failed",
      "code": "AURA-E-MCP-001"
    }
  }
  ```

**AC-008.2.2:** Backward compatible
- **Given** an existing client that only reads `error.message` and `error.type`
- **When** an error response is returned with the new `error.code` field
- **Then** the existing client is not broken (new field is additive)

#### Edge Cases
- The `type` field already exists as `error_type` (serialized as `type`). The new `code` field is additive.
- Error codes follow pattern: `AURA-E-<CATEGORY>-<NNN>`

### US-008.3: Error Type as Metric Label

**As an** operator,
**I want** the error taxonomy to produce string labels suitable for Prometheus metrics,
**So that** AURA-RM-001 can use `error_type` as a metric label without transformation.

**Priority:** P1

#### Acceptance Criteria

**AC-008.3.1:** Label-safe error type strings
- **Given** the error taxonomy enum
- **When** converted to a string label
- **Then** the result is lowercase, underscore-separated, and Prometheus-label-safe (e.g., `mcp_connection_failed`, `llm_timeout`)

## Dependencies

- **Depends on:** Nothing (foundation item)
- **Depended on by:** AURA-RM-001 (Metrics), AURA-RM-002 (Health), AURA-RM-003 (Circuit Breaker), AURA-RM-004 (Auth), AURA-RM-007 (Incident Response)

## Success Criteria

- All runtime error paths produce a classified error with taxonomy category
- API error responses include the new `code` field
- Existing tests and API clients are not broken
- Error type strings are usable as Prometheus metric labels
