//! Integration tests for client disconnect/cancellation behavior.
//!
//! Tests verify that client disconnect cancels in-flight tool execution.
//!
//! Requires:
//! - aura-web-server running on localhost:8080
//! - cancellation_test_server running on localhost:9998
//!

#![cfg(feature = "integration-cancellation")]

use aura_test_utils::TaskStatusChecker;
use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::timeouts::{CANCELLATION_CHECK, HTTP_REQUEST, TOOL_START};
use reqwest::Client;
use tokio::time::sleep;
use uuid::Uuid;

const MAX_RETRIES: usize = 2;

/// Test: Client disconnect cancels in-flight tool execution
#[tokio::test]
async fn test_client_disconnect_cancels_tool_execution() {
    let result = aura_test_utils::retry_test(MAX_RETRIES, || async {
        run_disconnect_cancels_test().await
    })
    .await;

    if let Err(e) = result {
        panic!("Test failed after {} retries: {}", MAX_RETRIES + 1, e);
    }
}

async fn run_disconnect_cancels_test() -> Result<(), String> {
    let client = Client::new();
    let task_id = Uuid::new_v4().to_string();
    let status_checker = TaskStatusChecker::new(&task_id);

    // Clean up any prior state for this task_id
    let _ = status_checker.cleanup().await;

    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "messages": [{
                "role": "user",
                "content": format!(
                    "Call the slow_task tool immediately with task_id='{}' and duration_seconds=10. \
                     Do not say anything else, just call the tool.",
                    task_id
                )
            }],
            "stream": true
        }))
        .timeout(HTTP_REQUEST)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Request failed: {:?}", response.status()));
    }

    // Wait for tool to start executing (poll via HTTP)
    let started = status_checker.wait_for_start(TOOL_START).await?;

    if !started {
        return Err("Tool did not start - LLM may not have called slow_task".to_string());
    }

    // Abort the request (simulates client disconnect)
    drop(response);

    // Wait - the 10-second task should NOT complete
    sleep(CANCELLATION_CHECK).await;

    // Check task was NOT completed (should be cancelled or still started)
    if status_checker.has_completed().await? {
        return Err("Cancellation failed: task completed despite client disconnect".to_string());
    }

    // Clean up
    let _ = status_checker.cleanup().await;

    Ok(())
}

/// Control test: Verifies slow_task completes without disconnect.
#[tokio::test]
async fn test_slow_task_completes_normally_without_disconnect() {
    let result = aura_test_utils::retry_test(MAX_RETRIES, || async {
        run_slow_task_completes_test().await
    })
    .await;

    if let Err(e) = result {
        panic!("Test failed after {} retries: {}", MAX_RETRIES + 1, e);
    }
}

async fn run_slow_task_completes_test() -> Result<(), String> {
    let client = Client::new();
    let task_id = Uuid::new_v4().to_string();
    let status_checker = TaskStatusChecker::new(&task_id);

    // Clean up any prior state for this task_id
    let _ = status_checker.cleanup().await;

    // Use a shorter duration for the control test (3 seconds)
    let response = client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "messages": [{
                "role": "user",
                "content": format!(
                    "Call the slow_task tool immediately with task_id='{}' and duration_seconds=3. \
                     Do not say anything else, just call the tool.",
                    task_id
                )
            }],
            "stream": true
        }))
        .timeout(HTTP_REQUEST)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Request failed: {:?}", response.status()));
    }

    // Consume the entire response (don't disconnect)
    let _body = response.text().await.unwrap_or_default();

    // Check task started and completed via HTTP
    let status = status_checker.get_status().await?;

    if status.status == "unknown" {
        return Err("Task should have started".to_string());
    }

    if status.status != "completed" {
        return Err(format!(
            "Task should have completed (no disconnect), but status is '{}'",
            status.status
        ));
    }

    // Clean up
    let _ = status_checker.cleanup().await;

    Ok(())
}
