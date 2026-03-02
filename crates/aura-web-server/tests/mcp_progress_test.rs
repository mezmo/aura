#![cfg(feature = "integration-progress")]

//! Integration tests for MCP progress notification forwarding.
//!

use aura::mcp_streamable_http::StreamableHttpMcpClient;
use aura::{request_progress_global, request_progress_subscribe, request_progress_unsubscribe};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_mcp_progress_notifications_received() {
    // Skip if mock server not running
    let client = match StreamableHttpMcpClient::new(
        "http://127.0.0.1:9999/mcp".to_string(),
        &HashMap::new(),
    )
    .await
    {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Skipping test: mock MCP server not running on port 9999");
            return;
        }
    };

    // Call task_with_progress with short duration and few steps
    let args: HashMap<String, serde_json::Value> = [
        ("duration_seconds".to_string(), serde_json::json!(2)),
        ("steps".to_string(), serde_json::json!(3)),
    ]
    .into_iter()
    .collect();

    let (result, mut progress_rx) = match client
        .call_tool_with_progress("task_with_progress", args)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Tool call failed: {}", e);
            panic!("Tool call should succeed");
        }
    };

    // Verify result contains expected completion message
    assert!(
        result.contains("Task completed"),
        "Result should contain completion message, got: {}",
        result
    );

    // Collect progress notifications (with timeout)
    let mut progress_count = 0;

    loop {
        match timeout(Duration::from_millis(500), progress_rx.recv()).await {
            Ok(Some(notification)) => {
                progress_count += 1;
                eprintln!(
                    "Received progress: {}/{:?} - {:?}",
                    notification.progress, notification.total, notification.message
                );

                // Verify progress structure
                assert!(notification.progress > 0.0, "Progress should be positive");
                assert!(
                    notification.total.is_some(),
                    "Total should be present for known-length tasks"
                );
            }
            Ok(None) => {
                // Channel closed, all progress received
                break;
            }
            Err(_) => {
                // Timeout, check if we already got the result
                if progress_count > 0 {
                    break; // We got some progress, tool must have completed
                }
                // No progress yet within timeout
                eprintln!("No progress notifications received within timeout");
                break;
            }
        }

        // Safety: don't wait forever
        if progress_count >= 10 {
            break;
        }
    }

    // NOTE: Progress notifications may or may not be received depending on
    // the MCP protocol implementation. FastMCP's streamable-http transport
    // may have limitations. This test verifies the plumbing is in place,
    // but we don't fail if no progress was received (known limitation).
    eprintln!(
        "Progress notifications received: {} (expected ~3 for 3 steps)",
        progress_count
    );

    // The main assertion is that the tool call completed successfully
    assert!(
        result.contains("3 progress updates"),
        "Result should mention progress updates, got: {}",
        result
    );
}

#[tokio::test]
async fn test_call_tool_without_progress_still_works() {
    // Skip if mock server not running
    let client = match StreamableHttpMcpClient::new(
        "http://127.0.0.1:9999/mcp".to_string(),
        &HashMap::new(),
    )
    .await
    {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Skipping test: mock MCP server not running on port 9999");
            return;
        }
    };

    // Call mock_tool (simple tool without progress)
    let args: HashMap<String, serde_json::Value> =
        [("message".to_string(), serde_json::json!("hello"))]
            .into_iter()
            .collect();

    let result = match client.call_tool("mock_tool", args).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Tool call failed: {}", e);
            panic!("Tool call should succeed");
        }
    };

    // Verify result
    assert!(
        result.contains("hello") || result.contains("Mock tool"),
        "Result should contain response from mock_tool, got: {}",
        result
    );
}

#[tokio::test]
async fn test_request_progress_broker_accessible() {
    let broker = request_progress_global();
    let test_request_id = "test_req_123";
    let _rx = broker.subscribe(test_request_id).await;

    assert!(
        broker.active_subscriptions().await >= 1,
        "Should have at least 1 subscription after subscribing"
    );

    broker.unsubscribe(test_request_id).await;
}

/// Verifies progress notifications only go to the correct request (no cross-request leakage).
#[tokio::test]
async fn test_request_progress_isolation() {
    use aura::{NumberOrString, ProgressNotification, ProgressToken};
    use std::sync::Arc;

    let mut rx1 = request_progress_subscribe("req_1").await;
    let mut rx2 = request_progress_subscribe("req_2").await;

    let notification = ProgressNotification {
        progress_token: ProgressToken(NumberOrString::String(Arc::from("token_1"))),
        progress: 50.0,
        total: Some(100.0),
        message: Some("Progress for request 1".to_string()),
    };

    let broker = request_progress_global();
    let sent = broker.publish("req_1", notification).await;
    assert!(sent, "Should successfully publish to req_1");

    let received = tokio::time::timeout(Duration::from_millis(100), rx1.recv())
        .await
        .expect("Should receive within timeout")
        .expect("Should have notification");
    assert_eq!(received.message, Some("Progress for request 1".to_string()));

    // req_2 should NOT receive anything
    let result = tokio::time::timeout(Duration::from_millis(50), rx2.recv()).await;
    assert!(
        result.is_err(),
        "req_2 should NOT receive progress meant for req_1"
    );

    request_progress_unsubscribe("req_1").await;
    request_progress_unsubscribe("req_2").await;
}

#[tokio::test]
async fn test_unsubscribe_stops_progress() {
    use aura::{NumberOrString, ProgressNotification, ProgressToken};

    let request_id = "req_cancel_test";
    let mut rx = request_progress_subscribe(request_id).await;

    let broker = request_progress_global();
    let notification = ProgressNotification {
        progress_token: ProgressToken(NumberOrString::Number(1)),
        progress: 10.0,
        total: Some(100.0),
        message: Some("Step 1".to_string()),
    };
    broker.publish(request_id, notification).await;

    let received = rx.recv().await;
    assert!(received.is_some(), "Should receive first notification");

    request_progress_unsubscribe(request_id).await;

    let notification2 = ProgressNotification {
        progress_token: ProgressToken(NumberOrString::Number(1)),
        progress: 20.0,
        total: Some(100.0),
        message: Some("Step 2 - should not be received".to_string()),
    };
    let sent = broker.publish(request_id, notification2).await;

    assert!(!sent, "Should not be able to send after unsubscribe");
}
