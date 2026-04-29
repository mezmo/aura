# Tasks: AURA-RM-001 Prometheus Metrics Endpoint

## Task 1: Add metrics dependencies

**Status:** Complete
**File:** `crates/aura-web-server/Cargo.toml`
**Satisfies:** N/A (infrastructure)
**Dependencies:** None

- [x] Add `metrics = "0.24"` to dependencies
- [x] Add `metrics-exporter-prometheus = "0.16"` to dependencies
- [x] Add `integration-metrics = []` to `[features]`
- [x] Add `"integration-metrics"` to the `integration` parent feature list
- [x] Verify `cargo check` passes

## Task 2: Create metrics module with init and recording functions

**Status:** Complete
**File:** `crates/aura-web-server/src/metrics.rs`
**Satisfies:** AC-001.1.1, AC-001.1.3
**Dependencies:** Task 1

- [x] `init() -> Option<PrometheusHandle>` with `AURA_METRICS_ENABLED` kill switch
- [x] `record_request_duration(method, status, agent, duration_secs)` with buckets [0.025..300]
- [x] `record_tokens(type, provider, agent, count)` — skip if count == 0
- [x] `record_tool_duration(server, tool, status, duration_secs)` with 100-tool cardinality cap + 64-char guard
- [x] `record_error(error_type)` using ErrorCategory label
- [x] `increment_requests_in_flight()` / `decrement_requests_in_flight()`
- [x] `set_mcp_server_connected(server, connected)`
- [x] `handler(handle) -> HttpResponse` returning Prometheus text format
- [x] Inline unit tests for kill switch behavior and cardinality cap

## Task 3: Register /metrics route in main.rs

**Status:** Complete
**File:** `crates/aura-web-server/src/main.rs`
**Satisfies:** AC-001.1.1
**Dependencies:** Task 2

- [x] Call `metrics::init()` in main
- [x] Register `/metrics` route conditionally (only when metrics enabled)
- [x] Pass `PrometheusHandle` as `web::Data`
- [x] Add `pub mod metrics;` to lib.rs
- [x] Exempt `/health` and `/metrics` from shutdown_guard

## Task 4: Instrument request duration and in-flight gauge

**Status:** Complete
**File:** `crates/aura-web-server/src/handlers.rs`
**Satisfies:** AC-001.1.2, AC-001.5.1
**Dependencies:** Task 2, Task 3

- [x] Add `Instant::now()` at request entry (before validation)
- [x] Update `ActiveRequestGuard::new()` to call `metrics::increment_requests_in_flight()`
- [x] Update `ActiveRequestGuard::drop()` to call `metrics::decrement_requests_in_flight()`
- [x] Record `record_request_duration()` at request completion (both streaming and non-streaming paths)
- [x] Record `record_request_duration()` for validation errors (400s)
- [x] Extract agent name and provider from agent for labels

## Task 5: Instrument token recording

**Status:** Complete
**File:** `crates/aura-web-server/src/handlers.rs`
**Satisfies:** AC-001.2.1
**Dependencies:** Task 2, Task 4

- [x] At request completion in `execute_completion`, call `usage_state.get_final_usage()`
- [x] Call `record_tokens("prompt", provider, agent, prompt_tokens)`
- [x] Call `record_tokens("completion", provider, agent, completion_tokens)`

## Task 6: Instrument tool duration

**Status:** Complete
**File:** `crates/aura-web-server/src/streaming/handlers.rs`
**Satisfies:** AC-001.3.1
**Dependencies:** Task 2

- [x] Record tool_start_times unconditionally (not gated behind emit_custom_events)
- [x] Parse result text once, shared between metrics and custom events
- [x] Call `record_tool_duration(server, tool, status, duration_secs)` for every tool completion
- [x] Detect tool errors via `detect_tool_error()` for status label

## Task 7: Instrument error recording

**Status:** Complete
**File:** `crates/aura-web-server/src/handlers.rs`, `crates/aura-web-server/src/streaming/handlers.rs`
**Satisfies:** AC-001.4.1
**Dependencies:** AURA-RM-008 (ErrorCategory), Task 2

- [x] Record `aura_errors_total` for stream-level errors (LLM timeout, disconnect, shutdown)
- [x] Record `aura_errors_total` for tool-level errors (MCP tool failures)
- [x] Record `aura_errors_total` for validation errors (400 responses)

## Task 8: MCP server connection state gauge

**Status:** Complete
**File:** `crates/aura-web-server/src/handlers.rs`
**Satisfies:** AC-001.6.1
**Dependencies:** Task 2

- [x] After agent build success, set `mcp_server_connected(name, true)` for all configured servers
- [x] On agent build failure, set `mcp_server_connected(name, false)` for all configured servers

## Task 9: Integration tests

**Status:** Complete
**File:** `crates/aura-web-server/tests/metrics_test.rs`
**Satisfies:** AC-001.1.1, AC-001.1.2, AC-001.2.1, AC-001.3.1, AC-001.4.1, AC-001.5.1, AC-001.6.1
**Dependencies:** All previous tasks

- [x] `#![cfg(feature = "integration-metrics")]`
- [x] TC-001.1.1.1: GET /metrics returns 200 with Prometheus format
- [x] TC-001.1.2.1: Request duration histogram present after POST
- [x] TC-001.1.3.1: 400 status code recorded for validation errors
- [x] TC-001.2.1.1: Token counters present after request
- [x] TC-001.3.1.1: Tool duration present after tool call
- [x] TC-001.4.1.1: Error counter increments on validation error
- [x] TC-001.5.1.1: In-flight gauge present after request
- [x] TC-001.6.1.1: MCP connection gauge present
- [x] TC-001.P.1: Scrape performance < 50ms
