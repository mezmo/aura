# Tasks: AURA-RM-001 Prometheus Metrics Endpoint

## Task 1: Add metrics dependencies

**Status:** Pending
**File:** `crates/aura-web-server/Cargo.toml`
**Satisfies:** N/A (infrastructure)
**Dependencies:** None

- [ ] Add `metrics = "0.24"` to dependencies
- [ ] Add `metrics-exporter-prometheus = "0.16"` to dependencies
- [ ] Add `integration-metrics = []` to `[features]`
- [ ] Add `"integration-metrics"` to the `integration` parent feature list
- [ ] Verify `cargo check` passes

## Task 2: Create metrics module with init and recording functions

**Status:** Pending
**File:** `crates/aura-web-server/src/metrics.rs`
**Satisfies:** AC-001.1.1, AC-001.1.3
**Dependencies:** Task 1

- [ ] `init_metrics() -> Option<PrometheusHandle>` with `AURA_METRICS_ENABLED` kill switch
- [ ] `record_request_duration(method, status, agent, duration_secs)` with buckets [0.025..300]
- [ ] `record_tokens(type, provider, agent, count)` — skip if count == 0
- [ ] `record_tool_duration(server, tool, status, duration_secs)` with 100-tool cardinality cap + 64-char guard
- [ ] `record_error(error_type)` using ErrorCategory label
- [ ] `increment_requests_in_flight()` / `decrement_requests_in_flight()`
- [ ] `set_mcp_server_connected(server, connected)`
- [ ] `metrics_handler(handle) -> HttpResponse` returning Prometheus text format
- [ ] Inline unit tests for kill switch behavior

## Task 3: Register /metrics route in main.rs

**Status:** Pending
**File:** `crates/aura-web-server/src/main.rs`
**Satisfies:** AC-001.1.1
**Dependencies:** Task 2

- [ ] Call `metrics::init_metrics()` in main
- [ ] Register `/metrics` route OUTSIDE shutdown_guard middleware (available during graceful shutdown)
- [ ] Pass `PrometheusHandle` as `web::Data`
- [ ] Add `pub mod metrics;` to lib.rs

## Task 4: Instrument request duration and in-flight gauge

**Status:** Pending
**File:** `crates/aura-web-server/src/handlers.rs`
**Satisfies:** AC-001.1.2, AC-001.5.1
**Dependencies:** Task 2, Task 3

- [ ] Add `Instant::now()` at request entry
- [ ] Update `ActiveRequestGuard::new()` to call `metrics::increment_requests_in_flight()`
- [ ] Update `ActiveRequestGuard::drop()` to call `metrics::decrement_requests_in_flight()`
- [ ] Record `record_request_duration()` at request completion (both streaming and non-streaming paths)
- [ ] Extract agent name and provider from agent for labels

## Task 5: Instrument token recording

**Status:** Pending
**File:** `crates/aura-web-server/src/handlers.rs`
**Satisfies:** AC-001.2.1
**Dependencies:** Task 2, Task 4

- [ ] At request completion, call `usage_state.get_final_usage()` → `(prompt, completion, total)`
- [ ] Call `record_tokens("prompt", provider, agent, prompt_tokens)`
- [ ] Call `record_tokens("completion", provider, agent, completion_tokens)`

## Task 6: Instrument tool duration

**Status:** Pending
**File:** `crates/aura-web-server/src/streaming/handlers.rs`
**Satisfies:** AC-001.3.1
**Dependencies:** Task 2

- [ ] At the point where `AuraStreamEvent::tool_complete_success/failure` is constructed (where `duration_ms` and `tool_name` are available)
- [ ] Call `record_tool_duration(server_name, tool_name, status, duration_secs)`
- [ ] Server name comes from the tool event's server context

## Task 7: Instrument error recording

**Status:** Pending
**File:** `crates/aura-web-server/src/handlers.rs`
**Satisfies:** AC-001.4.1
**Dependencies:** AURA-RM-008 (ErrorCategory), Task 2

- [ ] Where `AuraError` is constructed (Task 6 of RM-008), also call `record_error(category.as_label())`
- [ ] Covers all 6 ErrorDetail construction sites

## Task 8: MCP server connection state gauge

**Status:** Pending
**File:** `crates/aura-web-server/src/handlers.rs` (or main.rs at agent initialization)
**Satisfies:** AC-001.6.1
**Dependencies:** Task 2

- [ ] After MCP manager initialization, iterate connected servers and call `set_mcp_server_connected(name, true)`
- [ ] On MCP connection failure, call `set_mcp_server_connected(name, false)`

## Task 9: Integration tests

**Status:** Pending
**File:** `crates/aura-web-server/tests/metrics_test.rs`
**Satisfies:** AC-001.1.1, AC-001.1.2, AC-001.2.1, AC-001.3.1
**Dependencies:** All previous tasks

- [ ] `#![cfg(feature = "integration-metrics")]`
- [ ] TC-001.1.1.1: GET /metrics returns 200 with Prometheus format
- [ ] TC-001.1.2.1: Request duration histogram present after POST
- [ ] TC-001.2.1.1: Token counters present after request
- [ ] TC-001.3.1.1: Tool duration present after tool call
- [ ] TC-001.P.1: Scrape performance < 50ms with 500 time series
