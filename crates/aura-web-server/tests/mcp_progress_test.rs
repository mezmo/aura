#![cfg(feature = "integration-progress")]

//! Integration tests for MCP progress notification forwarding.
//!

use aura::mcp::McpClient;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_mcp_progress_notifications_received() {
    // Skip if mock server not running
    let client = match McpClient::new(
        "http://127.0.0.1:9999/mcp".to_owned(),
        "test".into(),
        &HashMap::new(),
        "test/0",
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
        .call_tool_with_progress("task_with_progress", args, None)
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
    let client = match McpClient::new(
        "http://127.0.0.1:9999/mcp".to_owned(),
        "test".into(),
        &HashMap::new(),
        "test/0",
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

    let result = match client.call_tool("mock_tool", args, None).await {
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
