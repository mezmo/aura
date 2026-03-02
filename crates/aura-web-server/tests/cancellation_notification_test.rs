#![cfg(feature = "integration-cancellation")]

//! Integration test: client disconnect triggers `notifications/cancelled` to MCP server.
//!

use aura_test_utils::server_urls::{AURA_SERVER, MCP_SERVER};
use aura_test_utils::timeouts::{HTTP_REQUEST, POST_DISCONNECT_WAIT, TOOL_START};
use aura_test_utils::TaskStatusChecker;
use reqwest::Client;
use serde::Deserialize;
use tokio::time::sleep;

const MAX_RETRIES: usize = 3;

/// Cancellation record from the test MCP server
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CancellationRecord {
    request_id: String,
    reason: Option<String>,
    received_at_ms: u64,
    /// Test ID for filtering in parallel tests
    id: Option<String>,
}

/// Test: MCP server receives notifications/cancelled when client disconnects
///
/// Verifies the end-to-end flow:
/// 1. Client sends streaming request
/// 2. LLM calls slow_task tool
/// 3. Client disconnects mid-execution
/// 4. aura-web-server sends notifications/cancelled to MCP server
/// 5. MCP server records the cancellation
#[tokio::test]
async fn test_mcp_receives_cancellation_notification() {
    let result = aura_test_utils::retry_test(MAX_RETRIES, || async {
        run_cancellation_notification_test().await
    })
    .await;

    if let Err(e) = result {
        panic!("Test failed after {} retries: {}", MAX_RETRIES + 1, e);
    }
}

async fn run_cancellation_notification_test() -> Result<(), String> {
    let client = Client::new();
    let task_id = format!("cancel_notify_test_{}", uuid::Uuid::new_v4());
    let status_checker = TaskStatusChecker::new(&task_id);

    // Clear any existing state for this test's ID
    let _ = status_checker.cleanup().await;

    let clear_result = client
        .delete(format!("{MCP_SERVER}/cancellations?id={task_id}"))
        .send()
        .await;

    if let Err(e) = clear_result {
        return Err(format!("MCP server connection failed: {}", e));
    }

    let initial_cancellations: Vec<CancellationRecord> = client
        .get(format!("{MCP_SERVER}/cancellations?id={task_id}"))
        .send()
        .await
        .map_err(|e| format!("Failed to get cancellations: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse cancellations: {}", e))?;

    if !initial_cancellations.is_empty() {
        return Err("Cancellations should be empty after clear".to_string());
    }

    let request_future = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "messages": [{
                "role": "user",
                "content": format!(
                    "Call the slow_task tool immediately with task_id='{}' and duration_seconds=30. \
                     Do not say anything else, just call the tool.",
                    task_id
                )
            }],
            "stream": true
        }))
        .timeout(HTTP_REQUEST)
        .send();

    let response = match request_future.await {
        Ok(resp) => resp,
        Err(e) => {
            return Err(format!(
                "Failed to connect to aura-web-server at {}: {}",
                AURA_SERVER, e
            ));
        }
    };

    if !response.status().is_success() {
        return Err(format!(
            "Request failed with status: {:?}",
            response.status()
        ));
    }

    // Wait for tool to start executing (poll via HTTP)
    let started = status_checker.wait_for_start(TOOL_START).await?;

    if !started {
        return Err("Tool did not start - LLM may not have called slow_task".to_string());
    }

    drop(response);
    sleep(POST_DISCONNECT_WAIT).await;

    // Query cancellations filtered by this test's ID
    let cancellations: Vec<CancellationRecord> = client
        .get(format!("{MCP_SERVER}/cancellations?id={task_id}"))
        .send()
        .await
        .map_err(|e| format!("Failed to get cancellations: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse cancellations: {}", e))?;

    // Check task was NOT completed via HTTP
    if status_checker.has_completed().await? {
        return Err("CANCELLATION FAILED: Task completed despite client disconnect!".to_string());
    }

    if cancellations.is_empty() {
        return Err(
            "MCP server should have received at least one cancellation notification.\n\
             This means notifications/cancelled was NOT sent when client disconnected.\n\
             Check that cancel_and_close_mcp() is called on client disconnect."
                .to_string(),
        );
    }

    // Clean up
    let _ = status_checker.cleanup().await;

    Ok(())
}

#[tokio::test]
async fn test_mcp_cancellation_endpoint_works() {
    let client = Client::new();

    let result = client
        .delete(format!("{MCP_SERVER}/cancellations"))
        .send()
        .await;

    result.expect("MCP test server not available");

    let cancellations: Vec<CancellationRecord> = client
        .get(format!("{MCP_SERVER}/cancellations"))
        .send()
        .await
        .expect("Failed to get cancellations")
        .json()
        .await
        .expect("Failed to parse cancellations");

    assert!(
        cancellations.is_empty(),
        "Cancellations should be empty after clear"
    );
}
