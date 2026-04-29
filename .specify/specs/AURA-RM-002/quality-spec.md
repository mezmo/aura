# Quality & Testing Spec: AURA-RM-002 Deep Health and Readiness Checks

**Status:** In Review
**Roadmap Item:** AURA-RM-002
**Product Spec:** [product-spec.md](product-spec.md)
**Architecture Spec:** [architecture-spec.md](architecture-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Test Strategy

- **Unit Tests:** All health types, serialization, status aggregation, probe result construction, cache TTL logic, Prometheus metric recording, shutdown guard exemption. Tests use `#[cfg(test)] mod tests` inline in source files (matching project convention). Cache tests use `#[cfg(test)] set_cache_for_test()` helper for injection. Probe timeout tests use short timeouts against non-routable addresses.
- **Integration Tests:** HTTP endpoint tests against a running server with mock MCP and real LLM config. Behind `integration-health` feature flag. Docker Compose infrastructure. (Deferred to when Docker Compose test harness is updated for health endpoint verification.)
- **End-to-End Tests:** Not needed separately -- integration tests cover the full HTTP path.
- **Performance Tests:** Not needed -- probes are bounded by `HEALTH_CHECK_TIMEOUT_SECS` and cached.

## Acceptance Criteria Traceability

| Acceptance Criterion | Test Type | Test Case ID | Test Location | Status |
|---------------------|-----------|-------------|---------------|--------|
| AC-002.1.1 (readiness endpoint - healthy) | Unit | TC-002.1.1.1 | `crates/aura-web-server/src/health.rs` | Pending |
| AC-002.1.1 (readiness endpoint - unhealthy) | Unit | TC-002.1.1.2 | `crates/aura-web-server/src/health.rs` | Pending |
| AC-002.1.2 (LLM provider check) | Unit | TC-002.1.2.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.1.3 (multiple LLM providers) | Unit | TC-002.1.3.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.1.edge (no LLM configs) | Unit | TC-002.1.E.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.2.1 (liveness endpoint) | Unit | TC-002.2.1.1 | `crates/aura-web-server/src/health.rs` | Pending |
| AC-002.2.2 (liveness no deps) | Unit | TC-002.2.2.1 | `crates/aura-web-server/src/health.rs` | Pending |
| AC-002.3.1 (MCP health check) | Unit | TC-002.3.1.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.3.2 (STDIO MCP ok) | Unit | TC-002.3.2.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.3.3 (no MCP configured) | Unit | TC-002.3.3.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.4.1 (subsystem breakdown) | Unit | TC-002.4.1.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.4.2 (vector store check - Qdrant) | Unit | TC-002.4.2.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.4.2 (vector store - InMemory) | Unit | TC-002.4.2.2 | `crates/aura/src/health.rs` | Pending |
| AC-002.4.edge (no vector stores) | Unit | TC-002.4.E.1 | `crates/aura/src/health.rs` | Pending |
| AC-002.5.1 (cache TTL) | Unit | TC-002.5.1.1 | `crates/aura-web-server/src/health.rs` | Pending |
| AC-002.5.2 (cache configurable) | Unit | TC-002.5.2.1 | `crates/aura-web-server/src/health.rs` | Pending |
| AC-002.5.3 (cache expires) | Unit | TC-002.5.3.1 | `crates/aura-web-server/src/health.rs` | Pending |
| AC-002.6.1 (Prometheus metrics) | Unit | TC-002.6.1.1 | `crates/aura-web-server/src/metrics.rs` | Pending |
| AC-002.7.1 (shutdown guard exemption) | Unit | TC-002.7.1.1 | `crates/aura-web-server/src/main.rs` or `health.rs` | Pending |
| Backward compat (/health unchanged) | Unit | TC-002.BC.1 | `crates/aura-web-server/src/health.rs` | Pending |
| HealthStatus serialization | Unit | TC-002.SER.1 | `crates/aura/src/health.rs` | Pending |

## Test Cases

### TC-002.1.1.1: Readiness returns 200 with healthy status

**Satisfies:** AC-002.1.1
**Type:** Unit

**Setup:**
- Create `HealthCheckResult` with all subsystems `Ok`, status `Healthy`

**Steps:**
1. Verify `HealthStatus::Healthy` maps to HTTP 200 status code
2. Serialize result to JSON

**Expected Result:**
- Status code mapping returns 200
- JSON contains `"status": "healthy"`, `"checks"`, `"check_duration_ms"`, `"cached"`

### TC-002.1.1.2: Readiness returns 503 with unhealthy status

**Satisfies:** AC-002.1.1
**Type:** Unit

**Setup:**
- Create `HealthCheckResult` with one LLM error, status `Unhealthy`

**Steps:**
1. Verify `HealthStatus::Unhealthy` maps to HTTP 503 status code
2. Serialize result to JSON

**Expected Result:**
- Status code mapping returns 503
- JSON contains `"status": "unhealthy"`
- `checks.llm` contains entry with `"status": "error"` and `"message"` field

### TC-002.1.2.1: LLM probe returns error for unreachable provider

**Satisfies:** AC-002.1.2
**Type:** Unit (with network -- uses non-routable address, completes via connection refused)

**Setup:**
- Create `LlmConfig::OpenAI` with `base_url` pointing to `http://127.0.0.1:1` (refused)

**Steps:**
1. Call `probe_llm()` with 1s timeout

**Expected Result:**
- Returns `LlmHealthResult` with `SubsystemStatus::Error`
- `provider` field is "openai"
- Error message is a sanitized category string (e.g., "connection_refused"), not a raw error

### TC-002.1.3.1: Multiple LLM providers deduplicated

**Satisfies:** AC-002.1.3
**Type:** Unit

**Setup:**
- Create 3 configs: 2 with same OpenAI base_url, 1 with Ollama

**Steps:**
1. Extract unique LLM probe targets via deduplication logic
2. Assert deduplication produced 2 unique targets (not 3)

**Expected Result:**
- 2 unique probe targets identified
- Each agent still has a result entry

### TC-002.1.E.1: No LLM configs produces empty array

**Satisfies:** AC-002.1 edge case
**Type:** Unit

**Setup:**
- Create configs with LLM section but ensure dedup produces no unique providers (or empty config set)

**Steps:**
1. Run health check aggregation with no LLM probes

**Expected Result:**
- `checks.llm` is an empty `Vec`
- Overall status is `Healthy` (no failures)

### TC-002.2.1.1: Liveness endpoint returns alive

**Satisfies:** AC-002.2.1
**Type:** Unit

**Steps:**
1. Call liveness handler (or verify liveness response construction)

**Expected Result:**
- Response JSON is `{"status": "alive"}`
- HTTP status is 200

### TC-002.2.2.1: Liveness is independent of health service state

**Satisfies:** AC-002.2.2
**Type:** Unit

**Setup:**
- Construct a scenario where HealthCheckService would return `Unhealthy`

**Steps:**
1. Call liveness handler

**Expected Result:**
- Still returns 200 `{"status": "alive"}`
- Liveness handler does not depend on HealthCheckService

### TC-002.3.1.1: MCP HTTP Streamable probe detects unreachable server

**Satisfies:** AC-002.3.1
**Type:** Unit (with network)

**Setup:**
- Create `McpServerConfig` with `HttpStreamable` transport, URL `http://127.0.0.1:1`

**Steps:**
1. Call `probe_mcp_server()` with 1s timeout

**Expected Result:**
- Returns `McpHealthResult` with `SubsystemStatus::Error`
- `transport` is "http_streamable"

### TC-002.3.2.1: STDIO MCP server reports ok

**Satisfies:** AC-002.3.2
**Type:** Unit

**Setup:**
- Create `McpServerConfig` with `Stdio` transport

**Steps:**
1. Call `probe_mcp_server()` with 1s timeout

**Expected Result:**
- Returns `McpHealthResult` with `SubsystemStatus::Ok`
- `transport` is "stdio"
- `latency_ms` is `None`

### TC-002.3.3.1: Empty MCP config produces empty map

**Satisfies:** AC-002.3.3
**Type:** Unit

**Setup:**
- Create configs with no MCP servers

**Steps:**
1. Run health check aggregation

**Expected Result:**
- `checks.mcp` is an empty `BTreeMap`

### TC-002.4.1.1: Health check result serializes with all subsystem sections

**Satisfies:** AC-002.4.1
**Type:** Unit

**Setup:**
- Create `HealthCheckResult` with mixed subsystem results (some ok, some error)

**Steps:**
1. Serialize to JSON via `serde_json::to_value()`

**Expected Result:**
- JSON has `"status"`, `"checks"`, `"check_duration_ms"`, `"cached"` fields
- `"checks"` has `"llm"` (array), `"mcp"` (object), `"vector_stores"` (object)
- Error entries contain `"message"` field
- Ok entries do NOT contain `"message"` field (serde tag flattening)
- `latency_ms` absent for entries where it is `None`

### TC-002.4.2.1: Vector store probe returns error for unreachable Qdrant

**Satisfies:** AC-002.4.2
**Type:** Unit (with network)

**Setup:**
- Create `VectorStoreConfig` with Qdrant type, URL `http://127.0.0.1:1`

**Steps:**
1. Call `probe_vector_store()` with 1s timeout

**Expected Result:**
- Returns `VectorStoreHealthResult` with `SubsystemStatus::Error`
- `store_type` is "qdrant"

### TC-002.4.2.2: InMemory vector store always returns ok

**Satisfies:** AC-002.4.2
**Type:** Unit

**Setup:**
- Create `VectorStoreConfig` with InMemory type

**Steps:**
1. Call `probe_vector_store()` with 1s timeout

**Expected Result:**
- Returns `VectorStoreHealthResult` with `SubsystemStatus::Ok`
- `store_type` is "in_memory"
- `latency_ms` is `None`

### TC-002.4.E.1: No vector stores produces empty map

**Satisfies:** AC-002.4 edge case
**Type:** Unit

**Setup:**
- Create configs with no vector stores

**Steps:**
1. Run health check aggregation

**Expected Result:**
- `checks.vector_stores` is an empty `BTreeMap`

### TC-002.5.1.1: Cache returns cached result within TTL

**Satisfies:** AC-002.5.1
**Type:** Unit

**Setup:**
- Create `HealthCheckService` with 10s TTL
- Use `set_cache_for_test()` to pre-populate cache with a known healthy result

**Steps:**
1. Call `get_health()` immediately (within TTL)

**Expected Result:**
- Returns the pre-populated result with `cached: true`

### TC-002.5.2.1: Cache TTL is respected

**Satisfies:** AC-002.5.2
**Type:** Unit

**Setup:**
- Create `HealthCheckService` with short TTL (100ms)
- Pre-populate cache

**Steps:**
1. Call `get_health()` within TTL -- returns `cached: true`
2. Sleep 150ms (past TTL)
3. Call `get_health()` again -- returns `cached: false` (fresh)

**Expected Result:**
- First call: `cached: true`
- Second call: `cached: false`

**Note:** Uses real sleep with 100ms TTL. Generous tolerance accounts for timing jitter.

### TC-002.5.3.1: Expired cache triggers fresh probe

**Satisfies:** AC-002.5.3
**Type:** Unit

**Setup:**
- Create `HealthCheckService` with 0ms TTL (always expired)

**Steps:**
1. Call `get_health()` twice

**Expected Result:**
- Both calls return `cached: false`

### TC-002.6.1.1: Health check records Prometheus metrics

**Satisfies:** AC-002.6.1
**Type:** Unit

**Setup:**
- Create `HealthCheckResult` with known values

**Steps:**
1. Call `record_health_check()` with the result
2. Read back metric values from the metrics handle

**Expected Result:**
- `aura_health_ready` gauge reflects health status (1.0 for healthy, 0.0 for unhealthy)
- `aura_health_check_duration_seconds` histogram has a recorded value
- `aura_health_subsystem_status` gauges exist with correct `subsystem` and `name` labels

### TC-002.7.1.1: Shutdown guard exempts /health/* paths

**Satisfies:** AC-002.7.1
**Type:** Unit

**Steps:**
1. Verify path matching logic: `/health/live` and `/health/ready` match the exemption predicate
2. Verify `/metrics` still matches
3. Verify `/v1/chat/completions` does NOT match

**Expected Result:**
- `/health` -> exempt
- `/health/live` -> exempt
- `/health/ready` -> exempt
- `/metrics` -> exempt
- `/v1/chat/completions` -> NOT exempt

### TC-002.BC.1: Existing /health endpoint unchanged

**Satisfies:** Backward compatibility success criterion
**Type:** Unit

**Steps:**
1. Verify the `health()` handler returns `{"status": "healthy"}` with HTTP 200

**Expected Result:**
- JSON response is exactly `{"status": "healthy"}`
- No new fields, no changes from current behavior

### TC-002.SER.1: HealthStatus variants serialize correctly

**Satisfies:** Serialization correctness
**Type:** Unit

**Steps:**
1. Serialize `HealthStatus::Healthy` and `HealthStatus::Unhealthy`

**Expected Result:**
- `Healthy` serializes to `"healthy"`
- `Unhealthy` serializes to `"unhealthy"`

## Test Infrastructure

### Required Fixtures
- Test configs with various LLM provider types (via `aura_config::load_config_from_str()`)
- `HealthCheckService::set_cache_for_test()` for cache injection in unit tests

### Required Services (Integration Tests Only)
- Docker Compose `mock-mcp` service (already exists in `compose/base.yml`)
- Aura web server instance (already in Docker Compose)

### Environment Variables
- `AURA_SERVER_URL` -- integration test server URL (from `aura-test-utils`)
- `HEALTH_CHECK_CACHE_TTL_SECS` -- set to small value for integration test cache expiry
- `HEALTH_CHECK_TIMEOUT_SECS` -- set to match test infrastructure expectations

## Quality Gates

All must pass before the feature is considered complete:

- [ ] All unit tests pass (`cargo test --workspace --lib`)
- [ ] All integration tests pass (`make test-integration`)
- [ ] Zero compiler warnings
- [ ] Clippy clean (`cargo clippy --all-targets --all-features -- -D warnings`)
- [ ] Code formatted (`cargo fmt --check`)
- [ ] No regressions in existing test suite
- [ ] Acceptance criteria traceability: every AC has at least one passing test
- [ ] Health check timeout < K8s probe timeout (documented)

## Coverage Notes

- **LLM probe actual API calls:** Unit tests probe against non-routable addresses (127.0.0.1:1) to verify error handling. Integration tests with real providers are out of scope (credentials would be needed in CI).
- **Bedrock/BedrockKb credential loading:** Unit tests cannot validate real AWS credentials. The probe validates that `aws-config` loads without error. A separate test verifies the code path handles missing credentials gracefully (returns `SubsystemStatus::Error`, not panic).
- **STDIO MCP probes:** Always return `Ok` by design -- cannot verify STDIO binary availability without spawning it.
- **InMemory vector stores:** Always return `Ok` -- no external dependency to fail.
- **Qdrant health check:** Unit tests use non-routable address. Integration tests with real Qdrant are deferred to when Qdrant is added to Docker Compose.
- **Cache concurrency:** The double-checked RwLock pattern is well-understood. Testing concurrent access would require tokio::spawn + barriers and is deferred -- the pattern is validated by its widespread use in Rust async caching libraries.
