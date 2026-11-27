#![cfg(feature = "integration-header-forwarding")]

//! Integration tests for MCP header forwarding functionality
//!
//! These tests verify that the aura-web-server correctly forwards HTTP headers
//! to MCP servers based on `headers_from_request` mappings in the TOML config,
//! with static TOML headers as fallback. Each test uses the echo_headers tool
//! in the mock MCP server to verify header forwarding.

use aura_test_utils::server_urls::AURA_SERVER;
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Helper to send a chat completion request with custom headers
async fn send_chat_request_with_headers(
    client: &reqwest::Client,
    headers: Vec<(&str, &str)>,
    message: &str,
) -> reqwest::Response {
    let mut request = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .json(&json!({
            "model": "test-assistant",
            "messages": [{"role": "user", "content": message}],
            "stream": false,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("test-session-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(TEST_TIMEOUT);

    // Add custom headers
    for (key, value) in headers {
        request = request.header(key, value);
    }

    request.send().await.expect("Failed to send request")
}

/// Extract the echo_headers tool response from LLM response
/// The LLM will include the JSON string from echo_headers in its response content
fn extract_headers_from_response(response_json: &Value) -> Option<Value> {
    let content = response_json["choices"][0]["message"]["content"]
        .as_str()
        .expect("Response missing content");

    // Try to find JSON in the response content
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    let json_str = &content[start..=end];

    serde_json::from_str(json_str).ok()
}

#[tokio::test]
async fn test_static_config_headers_used_as_fallback() {
    let client = reqwest::Client::new();

    // Send request with NO dynamic headers - should use static config headers only
    let headers = vec![];

    let response = send_chat_request_with_headers(
        &client,
        headers,
        "I need to see what HTTP headers you received. Please use the echo_headers tool to show me all the headers that were sent to the MCP server.",
    )
    .await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    let response_json: Value = response
        .json()
        .await
        .expect("Failed to parse response JSON");

    let received_headers = extract_headers_from_response(&response_json)
        .expect("Failed to extract headers from response");

    // Verify static Authorization from config IS present (no override, using config)
    let auth_header = received_headers
        .get("authorization")
        .and_then(|v| v.as_str())
        .expect("Authorization header from config should be present");

    assert!(
        auth_header.contains("static-config-token"),
        "Static config Authorization should be present, got: {auth_header}"
    );

    // Verify x-static-header from config IS present
    assert_eq!(
        received_headers
            .get("x-static-header")
            .and_then(|v| v.as_str()),
        Some("static-value-from-config"),
        "Static x-static-header from config should be present"
    );
}

#[tokio::test]
async fn test_token_authorization_scheme() {
    let client = reqwest::Client::new();

    // Send request with Token authorization (PagerDuty-style)
    let headers = vec![("authorization", "Token pd_test_token_12345")];

    let response = send_chat_request_with_headers(
        &client,
        headers,
        "I need to see what HTTP headers you received. Please use the echo_headers tool to show me all the headers that were sent to the MCP server.",
    )
    .await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    // Parse response
    let response_json: Value = response
        .json()
        .await
        .expect("Failed to parse response JSON");

    // Extract headers from echo_headers tool response
    let received_headers = extract_headers_from_response(&response_json)
        .expect("Failed to extract headers from response");

    // Verify Token authorization header was forwarded unchanged
    let auth_header = received_headers
        .get("authorization")
        .and_then(|v| v.as_str())
        .expect("Authorization header should be present");

    // CRITICAL: Verify Token scheme is preserved (PR #17 fix)
    assert!(
        auth_header.contains("Token"),
        "Authorization header should contain 'Token' scheme, got: {auth_header}"
    );
    assert!(
        auth_header.contains("pd_test_token_12345"),
        "Authorization header should contain the token value, got: {auth_header}"
    );

    // Verify the FULL header value was passed through (not stripped)
    assert!(
        auth_header == "Token pd_test_token_12345",
        "Full Authorization header should be preserved unchanged, got: {auth_header}"
    );
}

#[tokio::test]
async fn test_custom_authorization_scheme() {
    let client = reqwest::Client::new();

    // Send request with custom authorization scheme
    let headers = vec![("authorization", "ApiKey custom_api_key_xyz789")];

    let response = send_chat_request_with_headers(
        &client,
        headers,
        "I need to see what HTTP headers you received. Please use the echo_headers tool to show me all the headers that were sent to the MCP server.",
    )
    .await;

    assert_eq!(response.status(), 200, "Expected 200 OK status");

    let response_json: Value = response
        .json()
        .await
        .expect("Failed to parse response JSON");

    let received_headers = extract_headers_from_response(&response_json)
        .expect("Failed to extract headers from response");

    // Verify custom authorization header was forwarded unchanged
    let auth_header = received_headers
        .get("authorization")
        .and_then(|v| v.as_str())
        .expect("Authorization header should be present");

    // Verify custom scheme is preserved
    assert!(
        auth_header.contains("ApiKey"),
        "Authorization header should contain 'ApiKey' scheme, got: {auth_header}"
    );
    assert!(
        auth_header.contains("custom_api_key_xyz789"),
        "Authorization header should contain the key value, got: {auth_header}"
    );

    // Verify the FULL header value was passed through
    assert!(
        auth_header == "ApiKey custom_api_key_xyz789",
        "Full Authorization header should be preserved unchanged, got: {auth_header}"
    );
}
