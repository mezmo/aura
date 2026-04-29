# Architecture Spec: AURA-RM-002 Deep Health and Readiness Checks

**Status:** In Review
**Roadmap Item:** AURA-RM-002
**Product Spec:** [product-spec.md](product-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Summary

Add deep health and readiness probes to the Aura web server. Two new endpoints: `/health/live` (simple liveness) and `/health/ready` (per-subsystem connectivity verification with cached results). Probe logic for LLM providers, MCP servers, and vector stores lives in the `aura` crate. HTTP handlers and cache management live in `aura-web-server`. Individual probes use lightweight connectivity checks (model-listing API calls, HTTP pings, REST health checks) rather than building full agents. Each individual probe is wrapped in `tokio::time::timeout` to bound execution time.

## Constitution Compliance Check

- [x] Article I: No changes to OpenAI-compatible API endpoints. Health endpoints are new, additive paths.
- [x] Article II: Probe logic in `aura` crate (access to config types + provider clients). HTTP handlers + cache in `aura-web-server`. No circular dependencies.
- [x] Article III: Commits will be authored and signed off by human contributors.
- [x] Article IV: Implementation commits will follow conventional commit format.
- [x] Article V: Integration tests behind `integration-health` feature flag.
- [x] Article VI: No secrets in config. Probe functions use credentials already in agent configs (loaded via env var templates).
- [x] Article VII: No TOML config changes. Server-level settings use CLI args / env vars with defaults.

## Technical Context

- **Language/Version:** Rust (edition 2024, stable toolchain)
- **Affected Crates:** `aura` (new health module), `aura-web-server` (new health module, route registration, AppState, metrics)
- **New Dependencies:** None. Uses existing `reqwest` (already a dependency for MCP HTTP clients), `tokio` (async runtime), `serde` (serialization).
- **Performance Objectives:** Health check probes complete within 5s (configurable timeout). Cache ensures repeated probe requests within TTL add zero subsystem load. Liveness endpoint has zero external dependencies.

## Design

### Crate Changes

#### `aura` (core)

**New: `crates/aura/src/health.rs`**

Core types and probe functions for each subsystem. This module has no HTTP or Actix dependencies -- it operates on `aura` crate config types (`LlmConfig`, `McpServerConfig`, `VectorStoreConfig`) and returns result structs.

- `HealthStatus` enum: `Healthy`, `Unhealthy` (Note: `Degraded` variant reserved in Rust enum for forward compatibility but not documented as an observable status; will not be produced until circuit breaker integration in AURA-RM-003)
- `SubsystemStatus` enum: `Ok`, `Error { message }`
- `LlmHealthResult`, `McpHealthResult`, `VectorStoreHealthResult` structs
- `HealthCheckResult` aggregate struct
- `probe_llm()`, `probe_mcp_server()`, `probe_vector_store()` async functions (each wrapped in `tokio::time::timeout`)
- `run_health_check()` orchestrator that deduplicates and runs probes concurrently

**Modified: `crates/aura/src/lib.rs`**

Add `pub mod health;` and re-export key types.

#### `aura-web-server`

**New: `crates/aura-web-server/src/health.rs`**

HTTP handlers and cache management.

- `HealthCheckService` struct with `RwLock<Option<CachedResult>>` for TTL-based caching
- `liveness()` handler -- returns `{"status": "alive"}`
- `readiness()` handler -- delegates to `HealthCheckService`, returns 200 or 503, records Prometheus metrics

**Modified: `crates/aura-web-server/src/main.rs`**

- Register `/health/live` and `/health/ready` routes
- Add `HEALTH_CHECK_CACHE_TTL_SECS` and `HEALTH_CHECK_TIMEOUT_SECS` CLI args
- Construct `HealthCheckService` and add to `AppState`
- Update shutdown guard: `let is_exempt = path == "/health" || path.starts_with("/health/") || path == "/metrics";`

**Modified: `crates/aura-web-server/src/types.rs`**

- Add `health_service: Arc<crate::health::HealthCheckService>` to `AppState`

**Modified: `crates/aura-web-server/src/metrics.rs`**

- Add health check Prometheus metrics (see Prometheus Metrics section below)

### Data Flow

```
GET /health/live
  -> liveness() handler
    -> 200 {"status": "alive"}

GET /health/ready
  -> readiness() handler
    -> HealthCheckService::get_health()
      -> [cache fresh?] -> return cached result (with "cached": true)
      -> [cache stale?] -> run_health_check(configs, timeout)
        -> tokio::time::timeout(probe_timeout, probe_llm()) x N      |
        -> tokio::time::timeout(probe_timeout, probe_mcp_server()) x N |- concurrent via join_all
        -> tokio::time::timeout(probe_timeout, probe_vector_store()) x N |
        -> aggregate results
        -> update cache
        -> return fresh result
    -> 200 (healthy) or 503 (unhealthy) with JSON body
    -> record_health_check() (Prometheus metrics)
    -> log status transitions at INFO level
```

### Key Types / Interfaces

```rust
// === crates/aura/src/health.rs ===

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubsystemStatus {
    Ok,
    Error {
        /// Sanitized message safe for unauthenticated responses.
        /// Uses categories like "connection_refused", "auth_failed", "timeout", "dns_error".
        /// Raw internal details (IPs, hostnames, DNS names) are logged at WARN, never returned.
        message: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmHealthResult {
    pub provider: String,
    pub model: String,
    pub agent: String,
    #[serde(flatten)]
    pub status: SubsystemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpHealthResult {
    pub transport: String,
    #[serde(flatten)]
    pub status: SubsystemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VectorStoreHealthResult {
    #[serde(rename = "type")]
    pub store_type: String,
    #[serde(flatten)]
    pub status: SubsystemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthChecks {
    pub llm: Vec<LlmHealthResult>,
    /// BTreeMap for stable JSON key ordering.
    pub mcp: BTreeMap<String, McpHealthResult>,
    /// BTreeMap for stable JSON key ordering.
    pub vector_stores: BTreeMap<String, VectorStoreHealthResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthCheckResult {
    pub status: HealthStatus,
    pub checks: HealthChecks,
    pub check_duration_ms: u64,
    pub cached: bool,
}
```

```rust
// === Probe function signatures ===

/// Probe a single LLM provider. Wrapped in tokio::time::timeout by caller.
pub async fn probe_llm(
    config: &aura::LlmConfig,
    agent_name: &str,
    timeout: Duration,
) -> LlmHealthResult { ... }

/// Probe a single MCP server. Wrapped in tokio::time::timeout by caller.
pub async fn probe_mcp_server(
    server_name: &str,
    config: &aura::McpServerConfig,
    timeout: Duration,
) -> (String, McpHealthResult) { ... }

/// Probe a single vector store. Wrapped in tokio::time::timeout by caller.
pub async fn probe_vector_store(
    store_name: &str,
    config: &aura::VectorStoreConfig,
    timeout: Duration,
) -> (String, VectorStoreHealthResult) { ... }

/// Run all health probes across all agent configs with deduplication.
/// Accepts aura crate config types extracted by the web-server handler.
pub async fn run_health_check(
    configs: &[aura_config::Config],
    timeout: Duration,
) -> HealthCheckResult { ... }
```

```rust
// === crates/aura-web-server/src/health.rs ===

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

pub struct HealthCheckService {
    cache: RwLock<Option<CachedResult>>,
    configs: Arc<Vec<aura_config::Config>>,  // Same Arc as AppState.configs
    ttl: Duration,
    probe_timeout: Duration,
}

struct CachedResult {
    result: aura::health::HealthCheckResult,
    checked_at: Instant,
}

impl HealthCheckService {
    pub fn new(
        configs: Arc<Vec<aura_config::Config>>,
        ttl: Duration,
        probe_timeout: Duration,
    ) -> Self { ... }

    /// Returns health check result. Uses cache if fresh, otherwise runs probes.
    /// Double-checked locking: first caller on cache miss acquires write lock and
    /// runs probes (bounded by probe_timeout). Other concurrent callers wait on
    /// the write lock (max wait = probe_timeout + serialization overhead).
    /// This is acceptable for expected probe concurrency (1-3 concurrent K8s probes).
    pub async fn get_health(&self) -> aura::health::HealthCheckResult {
        // Fast path: read lock, check TTL
        {
            let cache = self.cache.read().await;
            if let Some(ref cached) = *cache {
                if cached.checked_at.elapsed() < self.ttl {
                    let mut result = cached.result.clone();
                    result.cached = true;
                    return result;
                }
            }
        }
        // Slow path: write lock, double-check, run probes
        let mut cache = self.cache.write().await;
        if let Some(ref cached) = *cache {
            if cached.checked_at.elapsed() < self.ttl {
                let mut result = cached.result.clone();
                result.cached = true;
                return result;
            }
        }
        let result = aura::health::run_health_check(
            &self.configs, self.probe_timeout
        ).await;
        *cache = Some(CachedResult {
            result: result.clone(),
            checked_at: Instant::now(),
        });
        result
    }

    /// Test helper: pre-populate cache for unit testing.
    #[cfg(test)]
    pub fn set_cache_for_test(&self, result: aura::health::HealthCheckResult) { ... }
}
```

### Probe Strategy

#### LLM Provider Probes

Each `LlmConfig` variant gets a minimal connectivity check. A successful HTTP response (200) = `Ok`. An auth failure (401/403) = `Error { message: "auth_failed" }`. A connection error = `Error { message: "connection_refused" }`. A timeout = `Error { message: "timeout" }`.

| Provider | Probe Method | Rationale |
|----------|-------------|-----------|
| OpenAI | `GET {base_url}/v1/models` with `Authorization: Bearer <key>` | Cheapest API call, no tokens consumed. 200=ok, 401=auth_failed |
| Anthropic | `GET {base_url}/v1/messages` with `x-api-key` header | Returns 405 (method not allowed) confirming reachability + auth. Any HTTP response=reachable, only connection error=down. 401=auth_failed |
| Bedrock | Load AWS credentials via `aws-config` | No cheap Bedrock ping; credential resolution catches expired STS tokens. Credential load failure=auth_failed |
| Ollama | `GET {base_url}/api/tags` | Ollama's model listing. Connection refused=down |

**Trust boundary:** Probe URLs come from the same config that controls request-time behavior. A malicious `base_url` could steal API keys, but this is equally true for normal request processing. Probes do not introduce a new attack surface beyond what already exists. HTTPS enforcement for non-localhost URLs is recommended but not enforced (matches existing behavior for request-time provider calls).

#### MCP Server Probes

| Transport | Probe Method | Rationale |
|-----------|-------------|-----------|
| HttpStreamable | `GET {server_url}` without auth headers | Connectivity check only. Any HTTP response (200, 404, 405) = reachable. Only connection error/timeout = down. Auth headers are NOT sent to avoid credential exposure on connectivity checks. |
| Stdio | Always report `Ok` | STDIO processes are spawned per-request; no persistent connection to probe |

#### Vector Store Probes

| Type | Probe Method | Rationale |
|------|-------------|-----------|
| Qdrant | `GET {url}/healthz` (Qdrant REST health) | Lightweight built-in health endpoint |
| BedrockKb | Load AWS credentials | Same as Bedrock LLM -- credential validation |
| InMemory | Always `Ok` | No external dependency |

#### Error Message Sanitization

Health response error messages use a fixed set of safe categories. Raw internal details are logged at WARN level only:

| Condition | Sanitized Message | Internal Log |
|-----------|------------------|-------------|
| Connection refused | `"connection_refused"` | Full address + error |
| DNS resolution failure | `"dns_error"` | Full hostname |
| Auth failure (401/403) | `"auth_failed"` | Provider + status code |
| Timeout | `"timeout"` | Duration + endpoint |
| Unexpected error | `"probe_error"` | Full error chain |

#### Deduplication

Probes are deduplicated across agent configs to avoid redundant checks:
- **LLM:** Key = `(provider_type, base_url_or_region)` -- same endpoint probed once, result shared across agents
- **MCP:** Key = `(transport, url_or_cmd+args)` -- same server probed once
- **Vector stores:** Key = `(type, url_or_kb_id)` -- same store probed once

When deduplicated, the result is cloned for each agent config that shares the subsystem, with the `agent` field set to the first agent name.

#### Status Aggregation

- All subsystems `Ok` or empty collections: `HealthStatus::Healthy`
- Any subsystem `Error`: `HealthStatus::Unhealthy`

#### Timeout Wrapping

Each individual probe call is wrapped in `tokio::time::timeout(probe_timeout, probe_fn())`. On timeout, the probe returns `SubsystemStatus::Error { message: "timeout" }`. This ensures a single hanging probe cannot block the entire health check. The `run_health_check()` function runs all timeout-wrapped probes concurrently via `futures::future::join_all`.

### Prometheus Metrics

```rust
// In crates/aura-web-server/src/metrics.rs

/// Record health check outcome as Prometheus metrics.
pub fn record_health_check(result: &aura::health::HealthCheckResult) {
    // Overall readiness gauge
    gauge!("aura_health_ready").set(match result.status {
        HealthStatus::Healthy => 1.0,
        _ => 0.0,
    });

    // Check duration histogram
    histogram!("aura_health_check_duration_seconds")
        .record(result.check_duration_ms as f64 / 1000.0);

    // Per-subsystem status gauges
    for llm in &result.checks.llm {
        let ok = matches!(llm.status, SubsystemStatus::Ok);
        gauge!("aura_health_subsystem_status",
            "subsystem" => "llm",
            "name" => llm.provider.clone(),
        ).set(if ok { 1.0 } else { 0.0 });
    }

    for (name, mcp) in &result.checks.mcp {
        let ok = matches!(mcp.status, SubsystemStatus::Ok);
        gauge!("aura_health_subsystem_status",
            "subsystem" => "mcp",
            "name" => name.clone(),
        ).set(if ok { 1.0 } else { 0.0 });
    }

    for (name, vs) in &result.checks.vector_stores {
        let ok = matches!(vs.status, SubsystemStatus::Ok);
        gauge!("aura_health_subsystem_status",
            "subsystem" => "vector_store",
            "name" => name.clone(),
        ).set(if ok { 1.0 } else { 0.0 });
    }
}
```

### Logging

- **INFO:** Status transitions (healthy -> unhealthy, unhealthy -> healthy) with previous and current status
- **WARN:** Individual probe failures with subsystem type, name, sanitized category, and full internal error detail
- **DEBUG:** Each probe execution with subsystem, name, and latency

### Error Handling

Health check probes catch all errors internally. A failing probe does NOT panic or propagate errors -- it produces a `SubsystemStatus::Error { message }` with a sanitized category string (see Error Message Sanitization above). Raw error details are logged at WARN level.

### Configuration

No TOML config changes. Server-level settings as CLI args / env vars (matching existing pattern):

```bash
# CLI args in main.rs
--health-check-cache-ttl-secs <N>    # env: HEALTH_CHECK_CACHE_TTL_SECS (default: 10)
--health-check-timeout-secs <N>      # env: HEALTH_CHECK_TIMEOUT_SECS (default: 5)
```

Startup warning is logged if `HEALTH_CHECK_TIMEOUT_SECS` >= 10 (approaching common K8s `timeoutSeconds` defaults).

## Migration / Backward Compatibility

- **Existing `/health` endpoint:** UNCHANGED. Returns `{"status": "healthy"}` as before. Docker Compose healthchecks and any existing monitoring that uses `/health` continues to work.
- **New endpoints are additive:** `/health/live` and `/health/ready` are new routes. No existing behavior modified.
- **No TOML changes:** New CLI args have defaults. Existing deployments without the new args work identically.
- **Shutdown guard update:** Exemption changes from `path == "/health" || path == "/metrics"` to `path == "/health" || path.starts_with("/health/") || path == "/metrics"`. The existing `/health` and `/metrics` exemptions are preserved.

## Alternatives Considered

| Approach | Pros | Cons | Why Not |
|----------|------|------|---------|
| Build full agent per health check | Tests actual initialization path | Expensive: spawns MCP STDIO processes, creates provider clients, discovers tools. Would take 5-30s per check. | Way too heavy for K8s probes firing every 5-15s |
| Background polling task | Non-blocking reads, always-fresh cache | Adds complexity (task lifecycle, shutdown coordination, error recovery) | Double-checked RwLock achieves same result with less code. Background task can be added later if needed. |
| Health config in TOML per-agent | More flexible per-agent health rules | Health checks are server-level, not per-agent. TOML configs are per-agent. Mismatch. | Server-level env vars match existing pattern (STREAMING_TIMEOUT_SECS, etc.) |
| gRPC health checking protocol | Standard K8s protocol | Aura is HTTP-only; adding gRPC infra for one endpoint is disproportionate | HTTP health checks are the default K8s pattern for HTTP services |
| Serve stale results while refreshing | Prevents write-lock blocking | More complex cache logic, stale data served during transition | Double-checked locking is simpler and write-lock duration is bounded by probe_timeout (5s). Acceptable for low probe concurrency (1-3 concurrent K8s probes). Can be revisited if needed. |

## Risks

- **LLM provider probe costs:** Model-listing API calls may be rate-limited by providers. Mitigated by caching (default 10s TTL means max 6 calls/minute per provider).
- **Probe timeout vs K8s timeout:** If probe timeout (5s) is close to K8s `timeoutSeconds`, probes may race. Mitigated by startup warning when timeout >= 10s, and documenting that K8s `timeoutSeconds` should be >= `HEALTH_CHECK_TIMEOUT_SECS` + 5s buffer.
- **Bedrock credential check is shallow:** Only verifies credentials load, not that they have Bedrock permissions. Mitigated by documenting this limitation.
- **STDIO MCP servers always report ok:** Cannot probe without spawning the process. Mitigated by documenting this limitation. STDIO failures will surface at request time.
- **Write-lock contention:** When cache expires, concurrent health requests queue behind the write lock for up to `probe_timeout` duration. Mitigated by keeping `probe_timeout` short (5s default) and the double-check pattern ensuring only one probe run per cache miss. K8s typically sends 1-3 concurrent probes, not hundreds.

## Implementation Order

1. Core health types (`HealthStatus`, `SubsystemStatus`, result structs, `HealthChecks`, `HealthCheckResult`) -- Satisfies: AC-002.4.1
2. LLM probe functions (`probe_llm()` per provider) with timeout wrapping -- Satisfies: AC-002.1.2, AC-002.1.3
3. MCP probe functions (`probe_mcp_server()`) -- Satisfies: AC-002.3.1, AC-002.3.2, AC-002.3.3
4. Vector store probe functions (`probe_vector_store()`) -- Satisfies: AC-002.4.2
5. Aggregation, deduplication, and status logic (`run_health_check()`) -- Satisfies: AC-002.4.1
6. Export from `aura/src/lib.rs`
7. `HealthCheckService` with cache in `aura-web-server` -- Satisfies: AC-002.5.1, AC-002.5.2, AC-002.5.3
8. HTTP handlers (`liveness()`, `readiness()`) -- Satisfies: AC-002.1.1, AC-002.2.1, AC-002.2.2
9. AppState, routes, CLI args in `main.rs`
10. Shutdown guard update for `/health/*` -- Satisfies: AC-002.7.1
11. Prometheus metrics in `metrics.rs` -- Satisfies: AC-002.6.1
12. Unit tests -- Satisfies: all ACs via quality spec
