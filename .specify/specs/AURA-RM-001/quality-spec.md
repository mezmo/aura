# Quality & Testing Spec: AURA-RM-001 Prometheus Metrics Endpoint

**Status:** Draft
**Roadmap Item:** AURA-RM-001
**Product Spec:** [product-spec.md](product-spec.md)
**Architecture Spec:** [architecture-spec.md](architecture-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Test Strategy

- **Unit Tests:** Metric recording functions, label generation, histogram bucket config
- **Integration Tests:** `/metrics` endpoint returns valid Prometheus format after requests
- **End-to-End Tests:** Prometheus scrape → verify expected metric families present

## Acceptance Criteria Traceability

| Acceptance Criterion | Test Type | Test Case ID | Test Location | Status |
|---------------------|-----------|-------------|---------------|--------|
| AC-001.1.1 (endpoint exists) | Integration | TC-001.1.1.1 | `crates/aura-web-server/tests/metrics_test.rs::test_metrics_endpoint_returns_200` | Pending |
| AC-001.1.2 (latency histogram) | Integration | TC-001.1.2.1 | `...::test_metrics_contains_request_duration_histogram` | Pending |
| AC-001.1.3 (no auth required) | Integration | TC-001.1.3.1 | `...::test_metrics_endpoint_no_auth` | Pending |
| AC-001.2.1 (token counters) | Integration | TC-001.2.1.1 | `...::test_metrics_contains_token_counters` | Pending |
| AC-001.3.1 (tool duration) | Integration | TC-001.3.1.1 | `...::test_metrics_contains_tool_duration` | Pending |
| AC-001.4.1 (error counters) | Unit | TC-001.4.1.1 | `crates/aura-web-server/src/metrics_tests.rs::test_record_error_increments_counter` | Pending |
| AC-001.5.1 (in-flight gauge) | Unit | TC-001.5.1.1 | `...::test_in_flight_gauge_tracks_active_requests` | Pending |

## Test Cases

### TC-001.1.1.1: Metrics endpoint returns 200 with Prometheus format

**Satisfies:** AC-001.1.1
**Type:** Integration
**Feature Flag:** `integration-metrics`

**Setup:**
- Running Aura web server with test config

**Steps:**
1. Send `GET /metrics`

**Expected Result:**
- 200 OK
- Content-Type: `text/plain; version=0.0.4; charset=utf-8`
- Body contains `# HELP` and `# TYPE` lines

### TC-001.1.2.1: Request duration histogram present after requests

**Satisfies:** AC-001.1.2
**Type:** Integration

**Setup:**
- Running Aura web server
- Send at least one `POST /v1/chat/completions`

**Steps:**
1. Send a chat completion request
2. Send `GET /metrics`

**Expected Result:**
- Body contains `aura_http_request_duration_seconds_bucket`
- Labels include `method="POST"`, `status_code="200"`, `agent="..."`
- Buckets include the configured boundaries

### TC-001.2.1.1: Token counters present after requests

**Satisfies:** AC-001.2.1
**Type:** Integration

**Setup:**
- Running Aura web server with LLM provider configured

**Steps:**
1. Send a chat completion request that completes successfully
2. Send `GET /metrics`

**Expected Result:**
- Body contains `aura_llm_tokens_total`
- Labels include `type="prompt"` and `type="completion"`
- Values are > 0

### TC-001.3.1.1: Tool duration histogram present after tool calls

**Satisfies:** AC-001.3.1
**Type:** Integration

**Setup:**
- Running Aura web server with mock MCP server

**Steps:**
1. Send a request that triggers a tool call
2. Send `GET /metrics`

**Expected Result:**
- Body contains `aura_mcp_tool_duration_seconds_bucket`
- Labels include `server`, `tool`, `status`

### TC-001.4.1.1: Error counter increments on error

**Satisfies:** AC-001.4.1
**Type:** Unit

**Steps:**
1. Call `record_error("mcp_tool_error")`
2. Render metrics

**Expected Result:**
- `aura_errors_total{error_type="mcp_tool_error"}` shows count >= 1

### TC-001.5.1.1: In-flight gauge tracks active requests

**Satisfies:** AC-001.5.1
**Type:** Unit

**Steps:**
1. Call `set_requests_in_flight(3)`
2. Render metrics

**Expected Result:**
- `aura_http_requests_in_flight` gauge shows value 3

## Quality Gates

- [ ] All unit tests pass (`cargo test --workspace --lib`)
- [ ] Integration tests pass (`cargo test --package aura-web-server --features integration-metrics`)
- [ ] Zero compiler warnings
- [ ] Clippy clean
- [ ] Code formatted
- [ ] No regressions in existing test suite (all 743+ existing tests still pass)
- [ ] Every AC has at least one passing test
- [ ] `/metrics` endpoint returns valid Prometheus text format (verified with `promtool check metrics`)

## Coverage Notes

- Integration tests for AC-001.1.2, AC-001.2.1, and AC-001.3.1 require a running server with LLM/MCP — these run under the `integration-metrics` feature flag
- Token counter accuracy depends on LLM provider returning usage data — some providers (Ollama) may not report tokens
- Tool duration tests require mock MCP server from existing test infrastructure
