# Tasks: AURA-RM-008 Structured Error Taxonomy

## Task 1: Create ErrorCategory enum and AuraError struct

**Status:** Pending
**File:** `crates/aura/src/error_taxonomy.rs`
**Satisfies:** AC-008.1.1, AC-008.3.1, AC-008.3.2, AC-008.4.1
**Dependencies:** None

- [ ] Create `ErrorCategory` enum with all 13 variants
- [ ] Implement `as_label()` returning Prometheus-safe strings
- [ ] Implement `client_message()` returning generic safe messages
- [ ] Create `ALL_CATEGORIES` const for test iteration
- [ ] Create `AuraError` struct with `category`, `internal_message`
- [ ] Implement `AuraError::new()` and `AuraError::client_message()` (pass-through for RequestValidation)
- [ ] Inline unit tests: labels non-empty, labels match regex, client messages don't contain IPs/hostnames, all categories unique

## Task 2: Add From<&DetectedToolError> impl

**Status:** Pending
**File:** `crates/aura/src/error_taxonomy.rs`
**Satisfies:** AC-008.1.2
**Dependencies:** Task 1

- [ ] Implement `From<&DetectedToolError> for ErrorCategory`
- [ ] McpToolError → McpToolError, ToolCallError → McpToolError, JsonError → Internal
- [ ] Inline unit tests verifying each mapping

## Task 3: Export from lib.rs

**Status:** Pending
**File:** `crates/aura/src/lib.rs`
**Satisfies:** N/A (infrastructure)
**Dependencies:** Task 1

- [ ] Add `pub mod error_taxonomy;`
- [ ] Add `pub use error_taxonomy::{ErrorCategory, AuraError, ALL_CATEGORIES};`

## Task 4: Add code field to ErrorDetail

**Status:** Pending
**File:** `crates/aura-web-server/src/types.rs`
**Satisfies:** AC-008.2.1, AC-008.2.2
**Dependencies:** Task 1

- [ ] Add `code: Option<String>` with `#[serde(skip_serializing_if = "Option::is_none")]` to `ErrorDetail`
- [ ] Do NOT modify `ChatCompletionErrorDetail` (existing `code` field is for OpenAI-compatible codes)
- [ ] Inline unit tests: serialization with code present, serialization with code None (field absent)

## Task 5: Add From<&StreamTermination> impl

**Status:** Pending
**File:** `crates/aura-web-server/src/streaming/handlers.rs` (or new mapping module)
**Satisfies:** AC-008.1.3
**Dependencies:** Task 1, Task 3

- [ ] Implement `From<&StreamTermination> for ErrorCategory`
- [ ] Complete→Internal, StreamError→LlmError, Disconnected→Cancelled, Timeout→LlmTimeout, Shutdown→ServiceUnavailable
- [ ] Inline unit tests verifying all 5 mappings

## Task 6: Update all ErrorDetail construction sites

**Status:** Pending
**Files:** `crates/aura-web-server/src/handlers.rs`, `crates/aura-web-server/src/main.rs`
**Satisfies:** AC-008.2.1, AC-008.3.1
**Dependencies:** Task 1, Task 3, Task 4

Six sites to update:

- [ ] handlers.rs:132 — `Internal` (build agent failure), log internal message at WARN
- [ ] handlers.rs:167 — `RequestValidation` (wrong last message role), pass-through message
- [ ] handlers.rs:175 — `RequestValidation` (empty messages), pass-through message
- [ ] handlers.rs:241 — `RequestValidation` (no messages provided), pass-through message
- [ ] handlers.rs:545 — `Internal` (completion error), log internal message at WARN
- [ ] main.rs:102 — `ServiceUnavailable` (shutdown guard)
- [ ] Verify no construction site is missed (`grep "ErrorDetail {" across both files`)
