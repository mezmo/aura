# Quality & Testing Spec: AURA-RM-008 Structured Error Taxonomy

**Status:** In Review (Remediation Round 1)
**Roadmap Item:** AURA-RM-008
**Product Spec:** [product-spec.md](product-spec.md)
**Architecture Spec:** [architecture-spec.md](architecture-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Test Strategy

- **Unit Tests:** All ErrorCategory methods, From impls, label/message generation, AuraError sanitization. Tests use `#[cfg(test)] mod tests` inline in the source files (matching project convention).
- **Integration Tests:** Not needed (pure enum + mapping logic, no I/O)
- **End-to-End Tests:** Not needed (consumed by AURA-RM-001)

## Acceptance Criteria Traceability

| Acceptance Criterion | Test Type | Test Case ID | Test Location | Status |
|---------------------|-----------|-------------|---------------|--------|
| AC-008.1.1 (categories cover all paths) | Unit | TC-008.1.1.1 | `crates/aura/src/error_taxonomy.rs` inline tests | Pending |
| AC-008.1.1 (categories cover all paths) | Unit | TC-008.1.1.2 | `crates/aura/src/error_taxonomy.rs` inline tests | Pending |
| AC-008.1.2 (DetectedToolError maps) | Unit | TC-008.1.2.1 | `crates/aura/src/error_taxonomy.rs` inline tests | Pending |
| AC-008.1.3 (StreamTermination maps) | Unit | TC-008.1.3.1 | `crates/aura-web-server/src/streaming/handlers.rs` inline tests | Pending |
| AC-008.2.1 (code in response) | Unit | TC-008.2.1.1 | `crates/aura-web-server/src/types.rs` inline tests | Pending |
| AC-008.2.2 (backward compat) | Unit | TC-008.2.2.1 | `crates/aura-web-server/src/types.rs` inline tests | Pending |
| AC-008.3.1 (client messages generic) | Unit | TC-008.3.1.1 | `crates/aura/src/error_taxonomy.rs` inline tests | Pending |
| AC-008.3.2 (fixed client messages) | Unit | TC-008.3.2.1 | `crates/aura/src/error_taxonomy.rs` inline tests | Pending |
| AC-008.4.1 (label-safe strings) | Unit | TC-008.4.1.1 | `crates/aura/src/error_taxonomy.rs` inline tests | Pending |

## Test Cases

### TC-008.1.1.1: All categories produce non-empty labels

**Satisfies:** AC-008.1.1
**Type:** Unit

**Steps:**
1. Iterate all variants via `ALL_CATEGORIES` constant (avoids need for proc macro / strum)
2. Call `as_label()` on each

**Expected Result:**
- Every variant returns a non-empty `&'static str`
- Every label matches regex `^[a-z][a-z0-9_]*$`
- No two variants share the same label

### TC-008.1.1.2: All categories produce non-empty client messages

**Satisfies:** AC-008.1.1
**Type:** Unit

**Steps:**
1. Iterate all variants via `ALL_CATEGORIES`
2. Call `client_message()` on each

**Expected Result:**
- Every variant returns a non-empty `&'static str`
- No message contains common internal patterns: IP addresses, port numbers, hostnames, file paths, stack traces

### TC-008.1.2.1: DetectedToolError maps to correct category

**Satisfies:** AC-008.1.2
**Type:** Unit

**Steps:**
1. Create each `DetectedToolError` variant
2. Convert to `ErrorCategory` via `From`

**Expected Result:**
- `McpToolError("any message")` → `ErrorCategory::McpToolError`
- `ToolCallError("any message")` → `ErrorCategory::McpToolError`
- `JsonError("any message")` → `ErrorCategory::Internal`

### TC-008.1.3.1: StreamTermination maps to correct category

**Satisfies:** AC-008.1.3
**Type:** Unit
**Location:** `crates/aura-web-server` (StreamTermination is defined here)

**Steps:**
1. Create each `StreamTermination` variant
2. Convert to `ErrorCategory` via `From`

**Expected Result:**
- `Complete` → `ErrorCategory::Internal` (defensive case — should not be called on success path)
- `StreamError("err")` → `ErrorCategory::LlmError`
- `Disconnected` → `ErrorCategory::Cancelled`
- `Timeout` → `ErrorCategory::LlmTimeout`
- `Shutdown` → `ErrorCategory::ServiceUnavailable`

### TC-008.2.1.1: ErrorDetail serializes with code field

**Satisfies:** AC-008.2.1
**Type:** Unit

**Steps:**
1. Create `ErrorDetail` with `code: Some("mcp_tool_error".to_string())`
2. Serialize to JSON

**Expected Result:**
- JSON contains `"code": "mcp_tool_error"`
- JSON also contains `"type"` and `"message"` fields

### TC-008.2.2.1: ErrorDetail without code serializes cleanly

**Satisfies:** AC-008.2.2
**Type:** Unit

**Steps:**
1. Create `ErrorDetail` with `code: None`
2. Serialize to JSON

**Expected Result:**
- JSON does NOT contain `"code"` key (`skip_serializing_if` works)
- JSON still contains `"type"` and `"message"`

### TC-008.3.1.1: AuraError client_message never contains internal details

**Satisfies:** AC-008.3.1
**Type:** Unit

**Steps:**
1. Create `AuraError` with internal_message containing hostnames and IPs:
   `AuraError::new(McpConnectionFailed, "MCP server 'pagerduty' at 10.0.1.5:8080 connection refused")`
2. Call `client_message()`

**Expected Result:**
- Returns "A downstream service is temporarily unavailable"
- Does NOT contain "pagerduty", "10.0.1.5", "8080", or "connection refused"

### TC-008.3.2.1: RequestValidation passes through client message

**Satisfies:** AC-008.3.2
**Type:** Unit

**Steps:**
1. Create `AuraError::new(RequestValidation, "Last message must be from user, got: system")`
2. Call `client_message()`

**Expected Result:**
- Returns "Last message must be from user, got: system" (pass-through, since this is client input)

### TC-008.4.1.1: All labels are Prometheus-safe

**Satisfies:** AC-008.4.1
**Type:** Unit

**Steps:**
1. Iterate all variants via `ALL_CATEGORIES`
2. Check each label against regex `^[a-z][a-z0-9_]*$`

**Expected Result:**
- All labels match the Prometheus label name pattern

## Quality Gates

- [ ] All unit tests pass (`cargo test --workspace --lib`)
- [ ] Zero compiler warnings
- [ ] Clippy clean (`cargo clippy --all-targets -- -D warnings`)
- [ ] Code formatted (`cargo fmt --check`)
- [ ] No regressions in existing test suite
- [ ] Every AC has at least one passing test (see traceability table)

## Coverage Notes

- `mcp_connection_failed` and `mcp_timeout` categories are defined but will not be emitted by `From<&DetectedToolError>` — they exist for future MCP transport layer classification. Tests verify the enum variant exists and has correct label/message, but no From impl produces them yet.
- `BudgetExceeded` category is defined but will not be exercised until AURA-RM-005.
- `LlmRateLimit` and `LlmAuthError` require parsing provider-specific error responses — initial implementation may classify all LLM errors as `llm_error` and refine later.
