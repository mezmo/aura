# Tasks: AURA-RM-002 Deep Health and Readiness Checks

## Task 1: Create health types, probe functions, and HealthCheckService

**Status:** Complete
**File:** `crates/aura-web-server/src/health.rs`
**Satisfies:** AC-002.1.1, AC-002.1.2, AC-002.1.3, AC-002.2.1, AC-002.2.2, AC-002.3.1, AC-002.3.2, AC-002.3.3, AC-002.4.1, AC-002.4.2, AC-002.5.1, AC-002.5.2, AC-002.5.3
**Dependencies:** None
**Note:** All types + probes + cache + handlers in `aura-web-server` (pragmatic deviation from spec; `aura` crate doesn't depend on `aura-config`)

- [x] Create `HealthStatus` enum (`Healthy`, `Unhealthy`)
- [x] Create `SubsystemStatus` enum (`Ok`, `Error { message }`) with serde tag flattening
- [x] Create `LlmHealthResult`, `McpHealthResult`, `VectorStoreHealthResult` structs
- [x] Create `HealthChecks` struct with `BTreeMap` for mcp and vector_stores
- [x] Create `HealthCheckResult` struct
- [x] Implement `probe_llm()` for OpenAI, Anthropic, Bedrock, Gemini, Ollama with sanitized error messages
- [x] Implement `probe_mcp_server()` for HttpStreamable (GET without auth) and Stdio (always Ok)
- [x] Implement `probe_vector_store()` for Qdrant (healthz), BedrockKb (creds), InMemory (always Ok)
- [x] Implement `run_health_check()` with deduplication, concurrent probes, timeout wrapping, status aggregation
- [x] Create `HealthCheckService` with `RwLock<Option<CachedResult>>`, TTL, probe timeout
- [x] Implement `get_health()` with double-checked locking, cache TTL enforcement
- [x] Add `#[cfg(test)] set_cache_for_test()` helper
- [x] Implement `liveness()` handler -> 200 `{"status": "alive"}`
- [x] Implement `readiness()` handler -> 200/503 with HealthCheckResult JSON, record metrics
- [x] Shared `PROBE_CLIENT` via `LazyLock` for connection pool reuse
- [x] AWS credential resolution via `provide_credentials().await`
- [x] Inline unit tests: TC-002.1.1.1, TC-002.1.1.2, TC-002.1.2.1, TC-002.1.3.1, TC-002.1.E.1, TC-002.2.1.1, TC-002.2.2.1, TC-002.3.1.1, TC-002.3.2.1, TC-002.3.3.1, TC-002.4.1.1, TC-002.4.2.1, TC-002.4.2.2, TC-002.4.E.1, TC-002.5.1.1, TC-002.5.3.1, TC-002.7.1.1, TC-002.BC.1, TC-002.SER.1

## Task 2: Export health module from lib.rs

**Status:** Complete
**File:** `crates/aura-web-server/src/lib.rs`
**Satisfies:** N/A (infrastructure)
**Dependencies:** Task 1

- [x] Add `pub mod health;`
- [x] Add `pub mod types;` (needed for `AppState` access from health module)

## Task 3: AppState, routes, CLI args, shutdown guard

**Status:** Complete
**Files:** `crates/aura-web-server/src/main.rs`, `crates/aura-web-server/src/types.rs`
**Satisfies:** AC-002.5.2, AC-002.7.1
**Dependencies:** Task 1

- [x] Add `health_service: Arc<crate::health::HealthCheckService>` to `AppState`
- [x] Add `HEALTH_CHECK_CACHE_TTL_SECS` CLI arg (default: 10)
- [x] Add `HEALTH_CHECK_TIMEOUT_SECS` CLI arg (default: 5)
- [x] Add startup warning if timeout >= 10s
- [x] Register `/health/live` and `/health/ready` routes
- [x] Update shutdown guard: `path == "/health" || path.starts_with("/health/") || path == "/metrics"`
- [x] Construct `HealthCheckService` in `run()` and pass to `AppState`
- [x] Add `mod health;` declaration
- [x] Add `Default` impl for `ActiveRequestTracker` (clippy fix)

## Task 4: Prometheus health metrics

**Status:** Complete
**File:** `crates/aura-web-server/src/metrics.rs`
**Satisfies:** AC-002.6.1
**Dependencies:** Task 1

- [x] Add `record_health_check()` function
- [x] Emit `aura_health_ready` gauge (1.0 healthy, 0.0 unhealthy)
- [x] Emit `aura_health_check_duration_seconds` histogram
- [x] Emit `aura_health_subsystem_status` gauge with `subsystem` and `name` labels
