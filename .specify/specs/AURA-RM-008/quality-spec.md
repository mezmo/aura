# Quality & Testing Spec: AURA-RM-008 Structured Error Taxonomy

**Status:** Draft
**Roadmap Item:** AURA-RM-008
**Product Spec:** [product-spec.md](product-spec.md)
**Architecture Spec:** [architecture-spec.md](architecture-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Test Strategy

- **Unit Tests:** All ErrorCategory methods, From impls, label/code generation
- **Integration Tests:** Not needed (pure enum + mapping logic)
- **End-to-End Tests:** Not needed (consumed by AURA-RM-001)

## Acceptance Criteria Traceability

| Acceptance Criterion | Test Type | Test Case ID | Test Location | Status |
|---------------------|-----------|-------------|---------------|--------|
| AC-008.1.1 (categories cover all paths) | Unit | TC-008.1.1.1 | `crates/aura/src/error_taxonomy_tests.rs::test_all_categories_have_labels` | Pending |
| AC-008.1.1 (categories cover all paths) | Unit | TC-008.1.1.2 | `...::test_all_categories_have_error_codes` | Pending |
| AC-008.1.2 (DetectedToolError maps) | Unit | TC-008.1.2.1 | `...::test_detected_tool_error_to_category` | Pending |
| AC-008.1.3 (StreamTermination maps) | Unit | TC-008.1.3.1 | `...::test_stream_termination_to_category` | Pending |
| AC-008.2.1 (error code in response) | Unit | TC-008.2.1.1 | `crates/aura-web-server/src/types_tests.rs::test_error_detail_with_code` | Pending |
| AC-008.2.2 (backward compat) | Unit | TC-008.2.2.1 | `...::test_error_detail_without_code_serializes_clean` | Pending |
| AC-008.3.1 (label-safe strings) | Unit | TC-008.3.1.1 | `...::test_labels_are_prometheus_safe` | Pending |

## Test Cases

### TC-008.1.1.1: All categories produce non-empty labels

**Satisfies:** AC-008.1.1
**Type:** Unit
**Location:** `crates/aura/src/error_taxonomy_tests.rs::test_all_categories_have_labels`

**Steps:**
1. Iterate all `ErrorCategory` variants
2. Call `as_label()` on each

**Expected Result:**
- Every variant returns a non-empty `&'static str`
- Every label is lowercase and contains only `[a-z_]` characters

### TC-008.1.1.2: All categories produce non-empty error codes

**Satisfies:** AC-008.1.1
**Type:** Unit
**Location:** `crates/aura/src/error_taxonomy_tests.rs::test_all_categories_have_error_codes`

**Steps:**
1. Iterate all `ErrorCategory` variants
2. Call `error_code()` on each

**Expected Result:**
- Every variant returns a non-empty `&'static str`
- Every code matches pattern `AURA-E-[A-Z]{3}-[0-9]{3}`

### TC-008.1.2.1: DetectedToolError maps to correct category

**Satisfies:** AC-008.1.2
**Type:** Unit
**Location:** `crates/aura/src/error_taxonomy_tests.rs::test_detected_tool_error_to_category`

**Steps:**
1. Create each `DetectedToolError` variant
2. Convert to `ErrorCategory` via `From`

**Expected Result:**
- `McpToolError(_)` → `ErrorCategory::McpToolError`
- `ToolCallError(_)` → `ErrorCategory::McpToolError`
- `JsonError(_)` → `ErrorCategory::Internal`

### TC-008.1.3.1: StreamTermination maps to correct category

**Satisfies:** AC-008.1.3
**Type:** Unit
**Location:** `crates/aura/src/error_taxonomy_tests.rs::test_stream_termination_to_category`

**Steps:**
1. Create each `StreamTermination` variant
2. Convert to `ErrorCategory` via `From`

**Expected Result:**
- `StreamError(_)` → `ErrorCategory::LlmError`
- `Disconnected` → `ErrorCategory::Cancelled`
- `Timeout` → `ErrorCategory::LlmTimeout`
- `Shutdown` → `ErrorCategory::Cancelled`

### TC-008.2.1.1: ErrorDetail serializes with code field

**Satisfies:** AC-008.2.1
**Type:** Unit
**Location:** `crates/aura-web-server/src/types_tests.rs::test_error_detail_with_code`

**Steps:**
1. Create `ErrorDetail` with `code: Some("AURA-E-MCP-001".to_string())`
2. Serialize to JSON

**Expected Result:**
- JSON contains `"code": "AURA-E-MCP-001"`
- JSON also contains `"type"` and `"message"` fields

### TC-008.2.2.1: ErrorDetail without code serializes cleanly

**Satisfies:** AC-008.2.2
**Type:** Unit
**Location:** `crates/aura-web-server/src/types_tests.rs::test_error_detail_without_code`

**Steps:**
1. Create `ErrorDetail` with `code: None`
2. Serialize to JSON

**Expected Result:**
- JSON does NOT contain `"code"` key (skip_serializing_if works)
- JSON still contains `"type"` and `"message"`

### TC-008.3.1.1: All labels are Prometheus-safe

**Satisfies:** AC-008.3.1
**Type:** Unit
**Location:** `crates/aura/src/error_taxonomy_tests.rs::test_labels_are_prometheus_safe`

**Steps:**
1. Iterate all `ErrorCategory` variants
2. Check each label against regex `^[a-z][a-z0-9_]*$`

**Expected Result:**
- All labels match the Prometheus label name pattern

## Quality Gates

- [x] All unit tests pass (`cargo test --workspace --lib`)
- [ ] Zero compiler warnings
- [ ] Clippy clean
- [ ] Code formatted
- [ ] No regressions in existing test suite
- [ ] Every AC has at least one passing test (see traceability table)

## Coverage Notes

- LLM provider-specific error classification (rate limit vs auth vs other) depends on parsing provider error responses. Initial implementation may classify all LLM errors as `llm_error` and refine in a follow-up.
- `BudgetExceeded` category is defined but will not be exercised until AURA-RM-005 is implemented.
