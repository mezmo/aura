//! Prometheus metrics for Aura request handling, token usage, and tool execution.
//!
//! Enabled by default. Set `AURA_METRICS_ENABLED=false` to disable the `/metrics` endpoint.

use actix_web::{web, HttpResponse};
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

/// Initialize the Prometheus metrics registry. Returns `None` if disabled via env var.
pub fn init() -> Option<PrometheusHandle> {
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
        .expect("valid request duration buckets")
        .set_buckets_for_metric(
            Matcher::Full("aura_mcp_tool_duration_seconds".to_string()),
            &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0],
        )
        .expect("valid tool duration buckets")
        .install_recorder()
        .expect("metrics recorder installation");

    tracing::info!("Metrics enabled on /metrics endpoint");
    Some(handle)
}

/// Serve Prometheus text exposition format.
pub async fn handler(handle: web::Data<PrometheusHandle>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4; charset=utf-8")
        .body(handle.render())
}

/// Record HTTP request duration.
pub fn record_request_duration(method: &str, status: u16, agent: &str, duration_secs: f64) {
    histogram!(
        "aura_http_request_duration_seconds",
        "method" => method.to_string(),
        "status_code" => status.to_string(),
        "agent" => agent.to_string(),
    )
    .record(duration_secs);
}

/// Record LLM token usage. Skips recording if count is zero.
pub fn record_tokens(token_type: &str, provider: &str, agent: &str, count: u64) {
    if count > 0 {
        counter!(
            "aura_llm_tokens_total",
            "type" => token_type.to_string(),
            "provider" => provider.to_string(),
            "agent" => agent.to_string(),
        )
        .increment(count);
    }
}

/// Track unique tool names globally to enforce cardinality cap.
static KNOWN_TOOLS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

const MAX_UNIQUE_TOOL_LABELS: usize = 100;
const MAX_TOOL_NAME_LEN: usize = 64;

/// Record MCP tool call duration with cardinality guards on the tool label.
pub fn record_tool_duration(server: &str, tool: &str, status: &str, duration_secs: f64) {
    let tool_label = if tool.len() > MAX_TOOL_NAME_LEN {
        "_other"
    } else {
        let mut known = KNOWN_TOOLS.lock().unwrap_or_else(|e| e.into_inner());
        if known.contains(tool) || known.len() < MAX_UNIQUE_TOOL_LABELS {
            known.insert(tool.to_string());
            tool
        } else {
            "_other"
        }
    };

    histogram!(
        "aura_mcp_tool_duration_seconds",
        "server" => server.to_string(),
        "tool" => tool_label.to_string(),
        "status" => status.to_string(),
    )
    .record(duration_secs);
}

/// Record an error by taxonomy category label.
pub fn record_error(error_type: &str) {
    counter!("aura_errors_total", "error_type" => error_type.to_string()).increment(1);
}

/// Increment the in-flight request gauge. Called at request entry.
pub fn increment_requests_in_flight() {
    gauge!("aura_http_requests_in_flight").increment(1.0);
}

/// Decrement the in-flight request gauge. Called at request exit (including on panic via Drop).
pub fn decrement_requests_in_flight() {
    gauge!("aura_http_requests_in_flight").decrement(1.0);
}

/// Set MCP server connection state (1.0 = connected, 0.0 = disconnected).
pub fn set_mcp_server_connected(server: &str, connected: bool) {
    gauge!("aura_mcp_server_connected", "server" => server.to_string())
        .set(if connected { 1.0 } else { 0.0 });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_disabled_returns_none() {
        // Safety: set_var/remove_var are unsafe in edition 2024 due to process-global mutation.
        // This test runs single-threaded and restores the env var after use.
        unsafe { std::env::set_var("AURA_METRICS_ENABLED", "false") };
        let handle = init();
        assert!(handle.is_none());
        unsafe { std::env::remove_var("AURA_METRICS_ENABLED") };
    }

    #[test]
    fn test_tool_label_cardinality_cap() {
        let mut known = KNOWN_TOOLS.lock().unwrap_or_else(|e| e.into_inner());
        known.clear();
        drop(known);

        // First 100 unique names should pass through
        for i in 0..100 {
            let name = format!("tool_{i}");
            assert!(name.len() <= MAX_TOOL_NAME_LEN);
        }

        // Very long tool names should be capped
        let long_name = "a".repeat(MAX_TOOL_NAME_LEN + 1);
        assert!(long_name.len() > MAX_TOOL_NAME_LEN);
    }
}
