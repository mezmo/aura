# Architecture Spec: AURA-RM-001 Prometheus Metrics Endpoint

**Status:** Implemented
**Roadmap Item:** AURA-RM-001
**Product Spec:** [product-spec.md](product-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Summary

Add a Prometheus-compatible `/metrics` endpoint to Aura's web server. Use the `metrics` crate (Rust metrics facade) with `metrics-exporter-prometheus` for exposition. All metric recording happens in `aura-web-server` (respecting Article II crate boundaries). The `aura` crate provides data via existing tracing spans and return values — no `metrics` dependency in the core crate.

## Constitution Compliance Check

- [x] Article I: `/metrics` is a new endpoint. No changes to `/v1/chat/completions` or `/v1/models`.
- [x] Article II: All metrics recording and exposition in `aura-web-server`. The `aura` crate has NO dependency on the `metrics` crate. Tool execution timing is captured from the handler layer using `Instant::now()` around tool calls, or extracted from existing OTel span data.
- [x] Article IV: Implementation commits will follow conventional commit format.
- [x] Article V: New integration tests use `integration-metrics` feature flag, added to the `integration` parent flag.
- [x] Article VI: No secrets involved.
- [x] Article VII: `AURA_METRICS_ENABLED` defaults to `true`. No TOML config changes required. (Separate metrics bind address is deferred to V2.)

## Technical Context

- **Affected Crates:** `aura-web-server` only (metrics recording + exposition)
- **New Dependencies (aura-web-server only):**
  - `metrics = "0.24"` — metrics facade (counters, gauges, histograms)
  - `metrics-exporter-prometheus = "0.16"` — Prometheus text format exporter
- **Performance Objectives:** < 1ms overhead per request for metric recording. Metrics scrape < 50ms for up to 500 unique time series.

## Design

### Crate Changes

#### `aura-web-server` (ONLY crate modified)

**New: `crates/aura-web-server/src/metrics.rs`**

```rust
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Initialize the metrics registry. Returns None if metrics are disabled.
pub fn init_metrics() -> Option<PrometheusHandle> {
    if std::env::var("AURA_METRICS_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        tracing::info!("Metrics disabled via AURA_METRICS_ENABLED=false");
        return None;
    }

    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("aura_http_request_duration_seconds".to_string()),
            &[0.025, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0],
        )
        .expect("valid bucket config")
        .set_buckets_for_metric(
            Matcher::Full("aura_mcp_tool_duration_seconds".to_string()),
            &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0],
        )
        .expect("valid bucket config")
        .install_recorder()
        .expect("metrics recorder installation");

    tracing::info!("Metrics enabled on /metrics endpoint");
    Some(handle)
}

pub fn record_request_duration(method: &str, status: u16, agent: &str, duration_secs: f64) {
    histogram!("aura_http_request_duration_seconds",
        "method" => method.to_string(),
        "status_code" => status.to_string(),
        "agent" => agent.to_string(),
    ).record(duration_secs);
}

pub fn record_tokens(token_type: &str, provider: &str, agent: &str, count: u64) {
    if count > 0 {
        counter!("aura_llm_tokens_total",
            "type" => token_type.to_string(),
            "provider" => provider.to_string(),
            "agent" => agent.to_string(),
        ).increment(count);
    }
}

/// Track unique tool names globally (across all MCP servers) to enforce cardinality cap.
static TOOL_NAMES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

const MAX_UNIQUE_TOOL_LABELS: usize = 100;

pub fn record_tool_duration(server: &str, tool: &str, status: &str, duration_secs: f64) {
    // Cardinality guard: cap unique tool labels at 100, length at 64 chars
    let tool_label = if tool.len() > 64 {
        "_other"
    } else {
        let mut names = TOOL_NAMES.lock().unwrap();
        if names.contains(tool) || names.len() < MAX_UNIQUE_TOOL_LABELS {
            names.insert(tool.to_string());
            tool
        } else {
            "_other"
        }
    };
    histogram!("aura_mcp_tool_duration_seconds",
        "server" => server.to_string(),
        "tool" => tool_label.to_string(),
        "status" => status.to_string(),
    ).record(duration_secs);
}

pub fn record_error(error_type: &str) {
    counter!("aura_errors_total", "error_type" => error_type.to_string()).increment(1);
}

pub fn increment_requests_in_flight() {
    gauge!("aura_http_requests_in_flight").increment(1.0);
}

pub fn decrement_requests_in_flight() {
    gauge!("aura_http_requests_in_flight").decrement(1.0);
}

pub fn set_mcp_server_connected(server: &str, connected: bool) {
    gauge!("aura_mcp_server_connected", "server" => server.to_string())
        .set(if connected { 1.0 } else { 0.0 });
}
```

**New: `/metrics` handler**

```rust
pub async fn metrics_handler(handle: web::Data<PrometheusHandle>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4; charset=utf-8")
        .body(handle.render())
}
```

**Modified: `main.rs`**

```rust
// In main():
let metrics_handle = metrics::init_metrics();

// Register /metrics route OUTSIDE shutdown_guard middleware
// so it remains available during graceful shutdown for final scrapes
if let Some(handle) = metrics_handle {
    app = app
        .app_data(web::Data::new(handle))
        .route("/metrics", web::get().to(metrics::metrics_handler));
}
```

**Modified: `handlers.rs`**

At request entry:
```rust
let start_time = std::time::Instant::now();
metrics::increment_requests_in_flight();
```

At request completion (both streaming and non-streaming, in a finally/drop guard):
```rust
metrics::decrement_requests_in_flight();
let duration = start_time.elapsed().as_secs_f64();
let (provider, _model) = agent.get_provider_info();
metrics::record_request_duration("POST", status_code, &agent_name, duration);

// Token recording from UsageState (actual API: get_final_usage() -> (u64, u64, u64))
let (prompt_tokens, completion_tokens, _total) = usage_state.get_final_usage();
metrics::record_tokens("prompt", provider, &agent_name, prompt_tokens);
metrics::record_tokens("completion", provider, &agent_name, completion_tokens);

// Error recording (if error occurred)
if let Some(category) = error_category {
    metrics::record_error(category.as_label());
}
```

**Modified: `types.rs` — ActiveRequestTracker**

The existing `ActiveRequestTracker` atomic counter is unchanged. Metrics gauge uses separate `increment(1.0)` / `decrement(1.0)` calls, avoiding the set-after-read race. The guard pattern in `ActiveRequestGuard` now also increments/decrements the metrics gauge:

```rust
impl ActiveRequestGuard {
    fn new(tracker: Arc<ActiveRequestTracker>) -> Self {
        tracker.increment(); // existing atomic
        crate::metrics::increment_requests_in_flight(); // new gauge
        Self { tracker }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.tracker.decrement(); // existing atomic
        crate::metrics::decrement_requests_in_flight(); // new gauge
    }
}
```

The existing `Ordering::Release` / `Ordering::AcqRel` on the atomic counter is NOT changed. The gauge operates independently.

**New feature flag in `aura-web-server/Cargo.toml`:**

```toml
[features]
integration = ["integration-streaming", "integration-header-forwarding", ..., "integration-metrics"]
integration-metrics = []
```

### Data Flow

```
Request arrives
  → ActiveRequestGuard::new() → atomic increment + gauge increment
  → start_time = Instant::now()
  → Handler processes request
    → Agent streams response
      → (MCP tool timing captured at handler layer via tool event broker)
      → Token usage accumulated in UsageState
    → Response complete
  → record_request_duration(method, status, agent, duration)
  → record_tokens(prompt/completion, provider, agent, count)
  → ActiveRequestGuard::drop() → atomic decrement + gauge decrement
  → (on error) record_error(category.as_label())

Prometheus scrapes GET /metrics
  → PrometheusHandle.render() → text exposition format
```

### Configuration

| Env Var | Default | Description |
|---------|---------|-------------|
| `AURA_METRICS_ENABLED` | `true` | Set to `false` to disable `/metrics` endpoint entirely |
| N/A | N/A | V1 metrics are served on the same bind address as the main server. A separate `AURA_METRICS_BIND_ADDRESS` is deferred to V2. Operators must use network-level controls (firewall, Kubernetes NetworkPolicy) to restrict `/metrics` access if the main server binds to `0.0.0.0`. |

### Tool Duration Recording Strategy

The `aura` crate does NOT gain a `metrics` dependency. Instead, tool duration is recorded at the `aura-web-server` handler layer by:
1. Tool completion with duration is handled at the handler level in `streaming/handlers.rs` via `AuraStreamEvent::tool_complete_success/failure`, which computes `duration_ms` from `state.tool_start_times`
2. The `ToolLifecycleEvent` enum in `tool_event_broker.rs` only has `Requested` and `Start` variants (no `Complete`)
3. Metrics recording hooks into the existing handler-level `tool_complete` event construction point, where `tool_name`, `duration_ms`, and `success` are already available

This preserves Article II: the `aura` crate only produces events, the web server records metrics from them.

## Migration / Backward Compatibility

- New `/metrics` route — no conflict with existing routes
- New dependencies only in `aura-web-server` — `aura` crate unchanged
- No config changes required (env vars have defaults)
- ActiveRequestTracker atomic behavior unchanged (gauge is a parallel side-channel)
- Metrics endpoint available during graceful shutdown (registered outside shutdown_guard)

## Alternatives Considered

| Approach | Pros | Cons | Why Not |
|----------|------|------|---------|
| Add `metrics` crate to `aura` core | Simpler instrumentation | Violates Article II | Boundary preservation is more important |
| OpenTelemetry metrics (OTel Meter) | Unified with tracing | Complex, OTLP metrics less widely scraped | Prometheus scrape is the standard |
| Separate metrics bind address | Better security isolation | More complex networking, port conflicts | Deferred to v2 (env var documented) |

## Risks

- **Cardinality from tool names**: MCP servers can expose many tools. Mitigated by 100-tool cap with `_other` aggregation, plus tool name length guard (>64 chars → `_other`).
- **Performance on scrape**: Rendering 500+ time series on every 15s scrape. Mitigated by the `metrics-exporter-prometheus` crate's efficient rendering. Add performance test (quality spec TC) to validate < 50ms.
- **Gauge leak on panic**: If handler panics between increment and drop, the Drop trait still runs (Rust guarantee during stack unwinding), so the gauge correctly decrements. No leak risk.

## Implementation Order

1. Add `metrics` and `metrics-exporter-prometheus` to `aura-web-server/Cargo.toml` — Infrastructure
2. Create `metrics.rs` with init, all record functions, and handler — Satisfies: AC-001.1.1, AC-001.1.3
3. Register `/metrics` route in main.rs outside shutdown_guard — Satisfies: AC-001.1.1
4. Add request duration recording in handlers.rs — Satisfies: AC-001.1.2
5. Add token recording from UsageState at request completion — Satisfies: AC-001.2.1
6. Add tool duration recording from ToolLifecycleEvent — Satisfies: AC-001.3.1
7. Add error recording using ErrorCategory.as_label() — Satisfies: AC-001.4.1
8. Add in-flight gauge to ActiveRequestGuard — Satisfies: AC-001.5.1
9. Add MCP server connection state gauge — Satisfies: AC-001.6.1
10. Add `integration-metrics` feature flag — Satisfies: Article V
