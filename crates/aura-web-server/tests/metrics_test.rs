#![cfg(feature = "integration-metrics")]

//! Integration tests for the Prometheus /metrics endpoint.
//!
//! Verifies that metrics are scraped in valid Prometheus format,
//! that request duration, token counters, tool duration, and error
//! counters are recorded correctly after real requests.

use aura_test_utils::server_urls::AURA_SERVER;
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

async fn send_chat_request(
    client: &reqwest::Client,
    messages: Vec<Value>,
) -> reqwest::Response {
    client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "Test Assistant",
            "messages": messages,
            "stream": false,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("test-metrics-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Failed to send request")
}

async fn scrape_metrics(client: &reqwest::Client) -> String {
    client
        .get(format!("{AURA_SERVER}/metrics"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("Failed to scrape /metrics")
        .text()
        .await
        .expect("Failed to read metrics body")
}

/// TC-001.1.1.1: GET /metrics returns 200 with Prometheus text format
#[tokio::test]
async fn test_metrics_endpoint_returns_prometheus_format() {
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{AURA_SERVER}/metrics"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("Failed to reach /metrics");

    assert_eq!(response.status(), 200);

    let content_type = response
        .headers()
        .get("content-type")
        .expect("missing content-type")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/plain"),
        "expected text/plain, got: {content_type}"
    );

    let body = response.text().await.unwrap();
    assert!(
        body.contains("# TYPE") || body.is_empty(),
        "expected Prometheus format with # TYPE declarations"
    );
}

/// TC-001.1.2.1: Request duration histogram present after a chat request
#[tokio::test]
async fn test_metrics_request_duration_after_chat() {
    let client = reqwest::Client::new();

    let response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "Say hello"})],
    )
    .await;
    assert_eq!(response.status(), 200);

    let metrics = scrape_metrics(&client).await;

    assert!(
        metrics.contains("aura_http_request_duration_seconds_count"),
        "missing request duration histogram count"
    );
    assert!(
        metrics.contains("aura_http_request_duration_seconds_bucket"),
        "missing request duration histogram buckets"
    );
    assert!(
        metrics.contains("method=\"POST\""),
        "missing method label"
    );
    assert!(
        metrics.contains("status_code=\"200\""),
        "missing status_code label"
    );
}

/// TC-001.2.1.1: Token counters present after a chat request
#[tokio::test]
async fn test_metrics_token_counters_after_chat() {
    let client = reqwest::Client::new();

    let response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "Say the number 42"})],
    )
    .await;
    assert_eq!(response.status(), 200);

    let metrics = scrape_metrics(&client).await;

    assert!(
        metrics.contains("aura_llm_tokens_total"),
        "missing token counter"
    );
    assert!(
        metrics.contains("type=\"prompt\""),
        "missing prompt token label"
    );
    assert!(
        metrics.contains("type=\"completion\""),
        "missing completion token label"
    );
}

/// TC-001.3.1.1: Tool duration histogram present after a tool call
#[tokio::test]
async fn test_metrics_tool_duration_after_tool_call() {
    let client = reqwest::Client::new();

    let response = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "Call mock_tool with message metrics_test"})],
    )
    .await;
    assert_eq!(response.status(), 200);

    let metrics = scrape_metrics(&client).await;

    assert!(
        metrics.contains("aura_mcp_tool_duration_seconds_count"),
        "missing tool duration histogram"
    );
    assert!(
        metrics.contains("tool=\"mock_tool\""),
        "missing tool label for mock_tool"
    );
}

/// TC-001.4.1.1: Error counter increments on validation error
#[tokio::test]
async fn test_metrics_error_counter_on_validation_error() {
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({"messages": []}))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), 400);

    let metrics = scrape_metrics(&client).await;

    assert!(
        metrics.contains("aura_errors_total"),
        "missing error counter"
    );
    assert!(
        metrics.contains("error_type=\"request_validation\""),
        "missing request_validation error type"
    );
}

/// TC-001.5.1.1: In-flight gauge is present after a request
#[tokio::test]
async fn test_metrics_in_flight_gauge_present() {
    let client = reqwest::Client::new();

    // Send a request to trigger the gauge (metrics crate registers on first use)
    let _ = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "gauge test"})],
    )
    .await;

    let metrics = scrape_metrics(&client).await;

    assert!(
        metrics.contains("aura_http_requests_in_flight"),
        "missing in-flight gauge"
    );
}

/// TC-001.6.1.1: MCP server connection gauge present when MCP is configured
#[tokio::test]
async fn test_metrics_mcp_connection_gauge() {
    let client = reqwest::Client::new();

    // Send a request to trigger agent build (which records MCP connection state)
    let _ = send_chat_request(
        &client,
        vec![json!({"role": "user", "content": "hi"})],
    )
    .await;

    let metrics = scrape_metrics(&client).await;

    assert!(
        metrics.contains("aura_mcp_server_connected"),
        "missing MCP connection gauge"
    );
}

/// TC-001.P.1: Metrics scrape performance
#[tokio::test]
async fn test_metrics_scrape_performance() {
    let client = reqwest::Client::new();

    // Send a few requests to populate metrics
    for i in 0..5 {
        let _ = send_chat_request(
            &client,
            vec![json!({"role": "user", "content": format!("perf test {i}")})],
        )
        .await;
    }

    // Time the scrape
    let start = std::time::Instant::now();
    let metrics = scrape_metrics(&client).await;
    let duration = start.elapsed();

    assert!(
        !metrics.is_empty(),
        "metrics response should not be empty"
    );
    assert!(
        duration.as_millis() < 50,
        "metrics scrape took {}ms, expected < 50ms",
        duration.as_millis()
    );
}

/// TC-001.1.3.1: Request duration records 400 status for validation errors
#[tokio::test]
async fn test_metrics_records_400_status() {
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({"messages": []}))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), 400);

    let metrics = scrape_metrics(&client).await;

    assert!(
        metrics.contains("status_code=\"400\""),
        "missing 400 status code in request duration histogram"
    );
}
