# Architecture Spec: AURA-RM-001 Prometheus Metrics Endpoint

**Status:** Draft
**Roadmap Item:** AURA-RM-001
**Product Spec:** [product-spec.md](product-spec.md)
**Author:** brandon.shelton
**Created:** 2026-04-28
**Last Updated:** 2026-04-28

---

## Summary

Add a Prometheus-compatible `/metrics` endpoint to Aura's web server. Use the `metrics` crate (Rust metrics facade) with `metrics-exporter-prometheus` for exposition. Instrument request handling, token tracking, MCP tool execution, and error classification using the taxonomy from AURA-RM-008.

## Constitution Compliance Check

- [x] Article I: `/metrics` is a new endpoint. No changes to `/v1/chat/completions` or `/v1/models`.
- [x] Article II: Metrics recording in `aura` crate (tool execution, token tracking). Metrics exposition in `aura-web-server` (HTTP endpoint). Clean separation.
- [x] Article V: No integration tests needed (metrics are unit-testable + manual verification).
- [x] Article VI: No secrets involved.
- [x] Article VII: No config changes required (metrics enabled by default, no opt-in needed).

## Technical Context

- **Affected Crates:** `aura-web-server` (new `/metrics` route, request middleware), `aura` (instrument tool execution, token recording)
- **New Dependencies:**
  - `metrics = "0.24"` — metrics facade (counters, gauges, histograms)
  - `metrics-exporter-prometheus = "0.16"` — Prometheus text format exporter
- **Performance Objectives:** < 1ms overhead per request for metric recording. Metrics scrape < 50ms.

## Design

### Crate Changes

#### `aura-web-server`

**New: `crates/aura-web-server/src/metrics.rs`**

```rust
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Initialize the metrics registry and return a handle for the /metrics endpoint.
pub fn init_metrics() -> PrometheusHandle {
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full("aura_http_request_duration_seconds".to_string()),
            &[0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0],
        )
        .unwrap()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full("aura_mcp_tool_duration_seconds".to_string()),
            &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0],
        )
        .unwrap()
        .install_recorder()
        .unwrap()
}

/// Record request completion metrics.
pub fn record_request(method: &str, status: u16, agent: &str, duration_secs: f64) {
    histogram!("aura_http_request_duration_seconds",
        "method" => method.to_string(),
        "status_code" => status.to_string(),
        "agent" => agent.to_string(),
    )
    .record(duration_secs);
}

/// Record token usage.
pub fn record_tokens(token_type: &str, provider: &str, agent: &str, count: u64) {
    counter!("aura_llm_tokens_total",
        "type" => token_type.to_string(),
        "provider" => provider.to_string(),
        "agent" => agent.to_string(),
    )
    .increment(count);
}

/// Record an error by taxonomy category.
pub fn record_error(error_type: &str) {
    counter!("aura_errors_total", "error_type" => error_type.to_string())
        .increment(1);
}

/// Update in-flight request gauge.
pub fn set_requests_in_flight(count: usize) {
    gauge!("aura_http_requests_in_flight").set(count as f64);
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

// Register route (before auth middleware, no auth required):
.route("/metrics", web::get().to(metrics::metrics_handler))

// Pass handle as app data:
.app_data(web::Data::new(metrics_handle))
```

**Modified: `handlers.rs`**

At request completion (both streaming and non-streaming paths):

```rust
// After response is built:
let duration = start_time.elapsed().as_secs_f64();
let (provider, model) = agent.get_provider_info();
metrics::record_request("POST", status_code, &agent_name, duration);
metrics::record_tokens("prompt", provider, &agent_name, usage.prompt_tokens);
metrics::record_tokens("completion", provider, &agent_name, usage.completion_tokens);

// On error:
metrics::record_error(error_category.as_label());
```

**Modified: `types.rs` — ActiveRequestTracker**

Add metrics recording to increment/decrement:

```rust
pub fn increment(&self) {
    let count = self.count.fetch_add(1, Ordering::SeqCst) + 1;
    crate::metrics::set_requests_in_flight(count);
}

pub fn decrement(&self) {
    let count = self.count.fetch_sub(1, Ordering::SeqCst) - 1;
    crate::metrics::set_requests_in_flight(count);
}
```

#### `aura` (core)

**Modified: `mcp_tool_execution.rs`**

After tool execution completes, record duration:

```rust
let start = std::time::Instant::now();
let result = call_http_tool_cancellable(...).await;
let duration = start.elapsed().as_secs_f64();

// Record metric (if metrics feature is available)
#[cfg(feature = "metrics")]
{
    metrics::histogram!("aura_mcp_tool_duration_seconds",
        "server" => server_name.to_string(),
        "tool" => tool_name.to_string(),
        "status" => if result.is_ok() { "ok" } else { "error" }.to_string(),
    )
    .record(duration);
}
```

### Data Flow

```
Request arrives
  → ActiveRequestTracker.increment() → gauge updated
  → start_time = Instant::now()
  → Handler processes request
    → Agent streams response
      → MCP tool called → tool duration histogram recorded
      → Token usage accumulated in UsageState
    → Response complete
  → record_request(method, status, agent, duration)
  → record_tokens(prompt/completion, provider, agent, count)
  → ActiveRequestTracker.decrement() → gauge updated
  → (on error) record_error(error_category.as_label())

Prometheus scrapes GET /metrics
  → PrometheusHandle.render() → text exposition format
```

### Configuration

No TOML config needed. Metrics are always enabled when the server starts. The `/metrics` endpoint is always available.

Future enhancement: configurable metric prefix, custom labels, or opt-out via env var.

## Migration / Backward Compatibility

- New `/metrics` route — no conflict with existing routes
- New dependencies (`metrics`, `metrics-exporter-prometheus`) — additive
- No config changes required
- ActiveRequestTracker gains a side-effect on increment/decrement — audit for correctness

## Alternatives Considered

| Approach | Pros | Cons | Why Not |
|----------|------|------|---------|
| `prometheus` crate directly | Widely used, direct Prometheus registry | Heavier API, requires manual registry management | `metrics` facade is more idiomatic in async Rust |
| OpenTelemetry metrics (OTel Meter) | Unified with existing OTel tracing | More complex setup, OTLP metrics not as widely scraped as /metrics | Prometheus scrape is the standard; can add OTel metrics later |
| `actix-web-prom` middleware | Drop-in request metrics | Only covers HTTP metrics, not token/tool metrics | Need custom metrics beyond HTTP |

## Risks

- **Cardinality explosion**: If agent names or tool names have high cardinality, Prometheus storage grows. Mitigated by using config-defined names (bounded set).
- **Performance**: Metrics recording adds microseconds per request. Mitigated by using atomic counters (metrics crate default).
- **Dependency size**: `metrics-exporter-prometheus` adds ~50KB. Acceptable.

## Implementation Order

1. Add `metrics` and `metrics-exporter-prometheus` to Cargo.toml — No AC (infrastructure)
2. Create `metrics.rs` with init, record functions, and `/metrics` handler — Satisfies: AC-001.1.1
3. Add request latency recording in handlers.rs — Satisfies: AC-001.1.2
4. Add token recording from UsageState at request completion — Satisfies: AC-001.2.1
5. Add MCP tool duration recording in mcp_tool_execution.rs — Satisfies: AC-001.3.1
6. Add error recording using ErrorCategory.as_label() — Satisfies: AC-001.4.1
7. Add in-flight gauge to ActiveRequestTracker — Satisfies: AC-001.5.1
8. Verify /metrics exempt from future auth (document for RM-004) — Satisfies: AC-001.1.3
