# Quality & Testing Spec: AURA-RM-001 Prometheus Metrics Endpoint

**Status:** In Review (Remediation Round 1)
**Roadmap Item:** AURA-RM-001
**Product Spec:** [product-spec.md](product-spec.md)
**Architecture Spec:** [architecture-spec.md](architecture-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Test Strategy

- **Unit Tests:** Metric recording functions, label generation, histogram bucket config, kill switch behavior
- **Integration Tests:** `/metrics` endpoint returns valid Prometheus format after real requests (feature flag: `integration-metrics`)
- **Performance Tests:** Metrics scrape completes in < 50ms with 500 time series

## Acceptance Criteria Traceability

| Acceptance Criterion | Test Type | Test Case ID | Test Location | Status |
|---------------------|-----------|-------------|---------------|--------|
| AC-001.1.1 (endpoint exists) | Integration | TC-001.1.1.1 | `crates/aura-web-server/tests/metrics_test.rs` | Pending |
| AC-001.1.2 (latency histogram) | Integration | TC-001.1.2.1 | `crates/aura-web-server/tests/metrics_test.rs` | Pending |
| AC-001.1.3 (kill switch) | Unit | TC-001.1.3.1 | `crates/aura-web-server/src/metrics.rs` inline tests | Pending |
| AC-001.2.1 (token counters) | Integration | TC-001.2.1.1 | `crates/aura-web-server/tests/metrics_test.rs` | Pending |
| AC-001.3.1 (tool duration) | Integration | TC-001.3.1.1 | `crates/aura-web-server/tests/metrics_test.rs` | Pending |
| AC-001.4.1 (error counters) | Unit | TC-001.4.1.1 | `crates/aura-web-server/src/metrics.rs` inline tests | Pending |
| AC-001.5.1 (in-flight gauge) | Unit | TC-001.5.1.1 | `crates/aura-web-server/src/metrics.rs` inline tests | Pending |
| AC-001.6.1 (MCP connection gauge) | Unit | TC-001.6.1.1 | `crates/aura-web-server/src/metrics.rs` inline tests | Pending |
| Performance (scrape < 50ms) | Performance | TC-001.P.1 | `crates/aura-web-server/tests/metrics_test.rs` | Pending |

## Test Cases

### TC-001.1.1.1: Metrics endpoint returns 200 with Prometheus format

**Satisfies:** AC-001.1.1
**Type:** Integration
**Feature Flag:** `integration-metrics`

**Setup:** Running Aura web server with test config (uses existing `aura-test-utils` infrastructure)

**Steps:**
1. Send `GET /metrics`

**Expected Result:**
- 200 OK
- Content-Type contains `text/plain`
- Body contains `# HELP` and `# TYPE` lines

### TC-001.1.2.1: Request duration histogram present after requests

**Satisfies:** AC-001.1.2
**Type:** Integration

**Steps:**
1. Send a `POST /v1/chat/completions` request
2. Send `GET /metrics`

**Expected Result:**
- Body contains `aura_http_request_duration_seconds_bucket`
- Labels include `method="POST"` and `status_code`
- At least one bucket has a count > 0

### TC-001.1.3.1: Kill switch disables metrics

**Satisfies:** AC-001.1.3
**Type:** Unit

**Steps:**
1. Set `AURA_METRICS_ENABLED=false` in env
2. Call `init_metrics()`

**Expected Result:**
- Returns `None`

### TC-001.2.1.1: Token counters present after requests

**Satisfies:** AC-001.2.1
**Type:** Integration

**Steps:**
1. Send a chat completion request
2. Send `GET /metrics`

**Expected Result:**
- Body contains `aura_llm_tokens_total`
- Labels include `type="prompt"` and `type="completion"`

### TC-001.3.1.1: Tool duration histogram present after tool calls

**Satisfies:** AC-001.3.1
**Type:** Integration

**Setup:** Running with mock MCP server (existing test infrastructure)

**Steps:**
1. Send a request that triggers a tool call (e.g., "Call mock_tool")
2. Send `GET /metrics`

**Expected Result:**
- Body contains `aura_mcp_tool_duration_seconds_bucket`
- Labels include `server`, `tool`, `status`

### TC-001.4.1.1: Error counter increments on error

**Satisfies:** AC-001.4.1
**Type:** Unit

**Steps:**
1. Call `record_error("mcp_tool_error")`
2. Call `record_error("mcp_tool_error")`
3. Render metrics

**Expected Result:**
- `aura_errors_total{error_type="mcp_tool_error"}` value is 2

### TC-001.5.1.1: In-flight gauge increments and decrements

**Satisfies:** AC-001.5.1
**Type:** Unit

**Steps:**
1. Call `increment_requests_in_flight()` twice
2. Call `decrement_requests_in_flight()` once
3. Render metrics

**Expected Result:**
- `aura_http_requests_in_flight` gauge value is 1

### TC-001.6.1.1: MCP connection gauge reflects state

**Satisfies:** AC-001.6.1
**Type:** Unit

**Steps:**
1. Call `set_mcp_server_connected("pagerduty", true)`
2. Call `set_mcp_server_connected("datadog", false)`
3. Render metrics

**Expected Result:**
- `aura_mcp_server_connected{server="pagerduty"}` is 1.0
- `aura_mcp_server_connected{server="datadog"}` is 0.0

### TC-001.P.1: Metrics scrape performance

**Satisfies:** Performance requirement
**Type:** Performance

**Steps:**
1. Record 500 unique time series (vary labels to create cardinality)
2. Time `PrometheusHandle::render()`

**Expected Result:**
- Render completes in < 50ms

## Test Infrastructure

### Required Fixtures
- Existing test config (`crates/aura-web-server/tests/test-config.toml`)
- Existing mock MCP server (aura-mock-mcp)

### Required Services (integration tests only)
- Mock MCP server on port 9999
- Aura web server with test config

### Environment Variables
- `AURA_METRICS_ENABLED` — tested in unit tests with env override

## Quality Gates

- [ ] All unit tests pass (`cargo test --workspace --lib`)
- [ ] Integration tests pass (`cargo test --package aura-web-server --features integration-metrics`)
- [ ] Zero compiler warnings
- [ ] Clippy clean (`cargo clippy --all-targets -- -D warnings`)
- [ ] Code formatted (`cargo fmt --check`)
- [ ] No regressions in existing test suite
- [ ] Every AC has at least one passing test (see traceability table)
- [ ] `/metrics` returns valid Prometheus format (validate with `promtool check metrics` if available in CI, otherwise manual verification)
- [ ] Performance: scrape < 50ms with 500 time series

## Coverage Notes

- Integration tests for token counters and tool duration require a running server with LLM/MCP. These run under the `integration-metrics` feature flag using existing Docker Compose infrastructure.
- Token counter accuracy depends on LLM provider returning usage data. Some providers (Ollama) may not report tokens — counters will show 0.
- Process-level metrics (CPU, memory, FDs) are out of scope. Operators should use container-level metrics or node exporter for these.
- Metrics scrape performance test (TC-001.P.1) runs as a unit test — does not require a running server.
