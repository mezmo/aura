//#![cfg(feature = "integration-a2a")]

//! Integration tests for Aura invocation of A2A tools via the a2a-rs-server crate.
//!
//! These tests verify that the aura-web-server correctly invokes A2A tooling.
//! Validates agent-card is presented as expected, tasks can be generated via
//! both http and rpc, and tasks can be monitored for their output.
//!
use a2a::{PartContent, Task, TaskState};
use aura_test_utils::server_urls::AURA_SERVER;
use serde_json::{Value, json};
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

// This test verifies that an A2A tool can be invoked via the /.well-known/agent-card.json endpoint
#[tokio::test]
async fn test_a2a_agent_card() {
    let client = reqwest::Client::new();
    let web_request = client
        .get(format!("{}/.well-known/agent-card.json", AURA_SERVER))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e));

    let response = web_request.unwrap();

    println!("Agent card response status: {}", response.status());
    assert_eq!(response.status(), 200);

    let expected_body: Value = json!({
      "name": "Test Assistant",
      "description": "You are a test assistant. Call tools immediately when requested.\n\nCRITICAL RULES (must follow):\n1. When a user names a tool, call it immediately with specified parameters\n2. Do NOT explain, ask for co...",
      "version": "1.0",
      "supportedInterfaces": [
        {
          "url": "/a2a/v1",
          "protocolBinding": "HTTP+JSON",
          "protocolVersion": "1.0"
        },
        {
          "url": "/a2a/v1/rpc",
          "protocolBinding": "JSONRPC",
          "protocolVersion": "1.0"
        }
      ],
      "capabilities": { "streaming": true, "pushNotifications": false },
      "defaultInputModes": ["text/plain"],
      "defaultOutputModes": ["text/plain"],
      "skills": [
        {
          "id": "chat",
          "name": "Chat",
          "description": "Send a message and receive a task. Use the task to track the progression of the AI to completion.",
          "tags": [],
          "inputModes": ["text/plain"],
          "outputModes": ["text/plain"]
        }
      ]
    }

        );

    let body = response.text().await.expect("Failed to read response");
    println!("Agent card response body: {}", body);

    let actual_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");
    assert_eq!(
        expected_body, actual_body,
        "Agent card response did not match expected"
    );
}

// validates the input content is only text
#[tokio::test]
async fn test_validation_http_a2a_tool_invocation_text_input_only() {
    let message_id = format!("{}", uuid::Uuid::new_v4());
    let request_text = format!(
        "Call the slow_task tool immediately with message_id='{}' and duration_seconds=10. \
            Do not say anything else, just call the tool.",
        message_id
    );

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/a2a/v1/message:send", AURA_SERVER))
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .json(&json!({
            "message": {
                "messageId": message_id,
                "role": "ROLE_USER",
                "parts": [{
                    "data": { // the server should throw an error for this
                        "text": request_text
                    }
                }]
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .expect("Request should succeed at network level");

    // should have errored because 'data' parts are not supported in this implementation
    assert_eq!(response.status(), 400);

    let body = response.text().await.expect("Failed to read response");
    let response_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");

    const EXPECTED_ERROR: &str = "All message parts are expected to be text for this implementation; file and data parts are not supported.";
    const EXPECTED_STATUS: &str = "INVALID_ARGUMENT";

    assert_eq!(
        EXPECTED_ERROR, response_body["error"]["message"],
        "error message is correct"
    );
    assert_eq!(
        EXPECTED_STATUS, response_body["error"]["status"],
        "status is correct"
    );
}

// validates a call to an A2A tool via the /a2a/v1/message:send endpoint returns a task with expected structure and content
#[tokio::test]
async fn test_http_a2a_tool_invocation_returns_task() {
    let message_id = format!("{}", uuid::Uuid::new_v4());
    let request_text = format!(
        "Call the slow_task tool immediately with message_id='{}' and duration_seconds=10. \
            Do not say anything else, just call the tool.",
        message_id
    );

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/a2a/v1/message:send", AURA_SERVER))
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .json(&json!({
            "message": {
                "messageId": message_id,
                "role": "ROLE_USER",
                "parts": [{
                    "text": request_text
                }]
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e));

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read response");
    let response_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");

    println!("Tool invocation response body: {}", body);

    let task = response_body
        .get("task")
        .expect("Response did not contain 'task' field");
    assert!(task.is_object(), "'task' field is not an object");

    let task_id = task
        .get("id")
        .expect("Task object did not contain 'id' field");
    assert!(task_id.is_string(), "Task 'id' field is not a string");

    let context_id = task
        .get("contextId")
        .expect("Task object did not contain 'contextId' field");
    assert!(
        context_id.is_string(),
        "Task 'contextId' field is not a string"
    );

    let status = task
        .get("status")
        .expect("Task object did not contain 'status' field");
    assert!(status.is_object(), "'status' field is not an object");

    let state = status
        .get("state")
        .expect("Status did not contain 'state' field");
    assert_eq!(
        "TASK_STATE_WORKING",
        state.as_str().unwrap(),
        "status is correct"
    );

    let timestamp = status
        .get("timestamp")
        .expect("Status did not contain 'timestamp' field");
    assert!(timestamp.is_string(), "Status 'timestamp' is not a string");

    let history = task
        .get("history")
        .expect("Task did not contain 'history' field");
    assert!(history.is_array(), "'history' field is not an array");
    let history_arr = history.as_array().unwrap();
    assert!(!history_arr.is_empty(), "'history' array is empty");

    let first_message = &history_arr[0];
    let hist_msg_id = first_message
        .get("messageId")
        .expect("History message missing 'messageId'");
    assert_eq!(
        hist_msg_id, &message_id,
        "message ID in history should match request message ID"
    );

    let role = first_message
        .get("role")
        .expect("History message missing 'role'");
    assert_eq!(
        role.as_str().unwrap(),
        "ROLE_USER",
        "History message role should be ROLE_USER"
    );

    let parts = first_message
        .get("parts")
        .expect("History message missing 'parts'");
    assert!(parts.is_array(), "'parts' field is not an array");
    let parts_arr = parts.as_array().unwrap();
    assert!(!parts_arr.is_empty(), "'parts' array is empty");

    let first_part = &parts_arr[0];
    let text = first_part.get("text").expect("Part missing 'text' field");
    assert_eq!(
        request_text,
        text.as_str().unwrap(),
        "Part 'text' does not match request"
    );
}

// validates a call to an A2A tool via the /a2a/v1/rpc endpoint returns a task with expected structure and content
#[tokio::test]
async fn test_rpc_a2a_tool_invocation_returns_task() {
    let rpc_message_id = format!("{}", uuid::Uuid::new_v4());
    let message_id = format!("{}", uuid::Uuid::new_v4());
    let request_text = format!(
        "Call the slow_task tool immediately with message_id='{}' and duration_seconds=10. \
            Do not say anything else, just call the tool.",
        message_id
    );

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/a2a/v1/rpc", AURA_SERVER))
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": rpc_message_id,
            "method": "SendMessage",
            "params": {
                "message": {
                    "messageId": message_id,
                    "role": "ROLE_USER",
                    "parts": [{
                        "text": request_text
                    }]
                }
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e));

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read response");
    let response_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");

    println!("Tool invocation response body: {}", body);

    let jsonrpc = response_body
        .get("jsonrpc")
        .expect("Response did not contain 'jsonrpc' field");
    assert_eq!(
        jsonrpc.as_str().unwrap(),
        "2.0",
        "'jsonrpc' field is not '2.0'"
    );

    let id = response_body
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Response did not contain 'id' field");
    assert_eq!(
        id, rpc_message_id,
        "Response 'id' field does not match expected rpc_message_id"
    );

    let result = response_body
        .get("result")
        .expect("Response did not contain 'result' field");
    let task = result
        .get("task")
        .expect("Response did not contain 'task' field");
    assert!(task.is_object(), "'task' field is not an object");

    let task_id = task
        .get("id")
        .expect("Task object did not contain 'id' field");
    assert!(task_id.is_string(), "'task_id' field is not a string");

    let context_id = task
        .get("contextId")
        .expect("Task object did not contain 'contextId' field");
    assert!(
        context_id.is_string(),
        "Task 'contextId' field is not a string"
    );

    let status = task
        .get("status")
        .expect("Task object did not contain 'status' field");
    assert!(status.is_object(), "'status' field is not an object");

    let state = status
        .get("state")
        .expect("Status did not contain 'state' field");
    assert_eq!(
        "TASK_STATE_WORKING",
        state.as_str().unwrap(),
        "status is correct"
    );

    let timestamp = status
        .get("timestamp")
        .expect("Status did not contain 'timestamp' field");
    assert!(timestamp.is_string(), "Status 'timestamp' is not a string");

    let history = task
        .get("history")
        .expect("Task did not contain 'history' field");
    assert!(history.is_array(), "'history' field is not an array");
    let history_arr = history.as_array().unwrap();
    assert!(!history_arr.is_empty(), "'history' array is empty");

    let first_message = &history_arr[0];
    let hist_msg_id = first_message
        .get("messageId")
        .expect("History message missing 'messageId'");
    assert_eq!(
        hist_msg_id, &message_id,
        "message ID in history should match request message ID"
    );

    let role = first_message
        .get("role")
        .expect("History message missing 'role'");
    assert_eq!(
        role.as_str().unwrap(),
        "ROLE_USER",
        "History message role should be ROLE_USER"
    );

    let parts = first_message
        .get("parts")
        .expect("History message missing 'parts'");
    assert!(parts.is_array(), "'parts' field is not an array");
    let parts_arr = parts.as_array().unwrap();
    assert!(!parts_arr.is_empty(), "'parts' array is empty");

    let first_part = &parts_arr[0];
    let text = first_part.get("text").expect("Part missing 'text' field");
    assert_eq!(
        request_text,
        text.as_str().unwrap(),
        "Part 'text' does not match request"
    );
}

// validates that a task created via the /a2a/v1/message:send endpoint is returned in the list of tasks
// and has expected details when retrieved individually
#[tokio::test]
async fn test_list_task_get_details() {
    let now = chrono::Utc::now();
    let message_id = format!("{}", uuid::Uuid::new_v4());
    let request_text = format!(
        "Call the slow_task tool immediately with message_id='{}' and duration_seconds=10. \
            Do not say anything else, just call the tool.",
        message_id
    );

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/a2a/v1/message:send", AURA_SERVER))
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .json(&json!({
            "message": {
                "messageId": message_id,
                "role": "ROLE_USER",
                "parts": [{
                    "text": request_text
                }]
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e));

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read response");
    let response_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");

    println!(
        "Tool invocation (/a2a/v1/message:send) response body: {}",
        body
    );

    let task = response_body
        .get("task")
        .expect("Response did not contain 'task' field");
    assert!(task.is_object(), "'task' field is not an object");

    let task_id = task
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Task object did not contain 'id' field");

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/a2a/v1/tasks", AURA_SERVER))
        .query(&[
            ("pageSize", "1".into()), // force multiple result pages so we get tokens
            ("statusTimestampAfter", now.to_string()), // attempt to get this test item on this page.
        ])
        .header("A2A-Version", "1.0")
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e));

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read response");
    let response_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");

    println!("Tool invocation (/a2a/v1/tasks)response body: {}", body);

    let next_page_token = response_body
        .get("nextPageToken")
        .expect("Response did not contain 'nextPageToken' field");
    assert!(
        next_page_token.is_string(),
        "'nextPageToken' field is not a string"
    );

    let page_size = response_body
        .get("pageSize")
        .expect("Response did not contain 'pageSize' field");
    assert!(page_size.is_i64(), "'pageSize' field is not an integer");

    let tasks = response_body
        .get("tasks")
        .expect("Response did not contain 'tasks' field");
    assert!(tasks.is_array(), "'tasks' field is not an array");

    assert!(!tasks.as_array().unwrap().is_empty(), "tasks were returned");

    for task in tasks.as_array().unwrap() {
        let _taskid = task.get("id").expect("Task missing 'id' field");
    }

    // now validate we can get the individual task details
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/a2a/v1/tasks/{}", AURA_SERVER, task_id))
        .header("A2A-Version", "1.0")
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e));

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read response");
    let response_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");

    println!(
        "Tool invocation (/a2a/v1/tasks/{}) response body: {}",
        task_id, body
    );

    if let Some(artifacts) = response_body.get("artifacts") {
        assert!(artifacts.is_array(), "'artifacts' field is not an array");
    }

    let context_id = response_body
        .get("contextId")
        .expect("Task object did not contain 'contextId' field");
    assert!(
        context_id.is_string(),
        "Task 'contextId' field is not a string"
    );

    let history = response_body
        .get("history")
        .expect("Task did not contain 'history' field");
    assert!(history.is_array(), "'history' field is not an array");

    let history_arr = history.as_array().unwrap();
    let first_message = &history_arr[0];
    let hist_msg_id = first_message
        .get("messageId")
        .expect("History message missing 'messageId'");
    assert_eq!(
        hist_msg_id, &message_id,
        "message ID in history should match request message ID"
    );

    let role = first_message
        .get("role")
        .expect("History message missing 'role'");
    assert_eq!(
        role.as_str().unwrap(),
        "ROLE_USER",
        "History message role should be ROLE_USER"
    );

    let parts = first_message
        .get("parts")
        .expect("History message missing 'parts'");
    assert!(parts.is_array(), "'parts' field is not an array");
    let parts_arr = parts.as_array().unwrap();
    assert!(!parts_arr.is_empty(), "'parts' array is empty");

    let first_part = &parts_arr[0];
    let text = first_part.get("text").expect("Part missing 'text' field");
    assert_eq!(
        request_text,
        text.as_str().unwrap(),
        "Part 'text' does not match request"
    );

    let status = response_body
        .get("status")
        .expect("Task object did not contain 'status' field");
    assert!(status.is_object(), "'status' field is not an object");

    let state = status
        .get("state")
        .expect("Status did not contain 'state' field");
    assert!(state.is_string(), "Task state should be TASK_STATE_WORKING");

    let timestamp = status
        .get("timestamp")
        .expect("Status did not contain 'timestamp' field");
    assert!(timestamp.is_string(), "Status 'timestamp' is not a string");
}

// validates that a task created via the /a2a/v1/message:send endpoint can
// be cancelled by a call back to a2a
#[tokio::test]
async fn test_cancel_task() {
    let message_id = format!("{}", uuid::Uuid::new_v4());
    let request_text = format!(
        "Call the slow_task tool immediately with message_id='{}' and duration_seconds=30. \
            Do not say anything else, just call the tool.",
        message_id
    );

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/a2a/v1/message:send", AURA_SERVER))
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .json(&json!({
            "message": {
                "messageId": message_id,
                "role": "ROLE_USER",
                "parts": [{
                    "text": request_text
                }]
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e));

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read response");
    let response_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");

    println!(
        "Tool invocation (/a2a/v1/message:send) response body: {}",
        body
    );

    let task = response_body
        .get("task")
        .expect("Response did not contain 'task' field");
    assert!(task.is_object(), "'task' field is not an object");

    let task_id = task
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Task object did not contain 'id' field");

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/a2a/v1/tasks/{}/cancel", AURA_SERVER, task_id))
        .header("A2A-Version", "1.0")
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Failed to send request - is aura-web-server running? {}", e));

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read response");
    let response_body: Value = serde_json::from_str(&body).expect("Response was not valid JSON");

    println!(
        "Tool invocation (/a2a/v1/tasks/id/cancel)response body: {}",
        body
    );

    assert_eq!(
        "TASK_STATE_CANCELED", response_body["status"]["state"],
        "The task is cancelled"
    );
}

// validates that two sequential messages sent with the same contextId result in two tasks
// sharing that context so history is fed into the 2nd aura reasoning
#[tokio::test]
async fn test_sequential_tasks_share_context() {
    let client = reqwest::Client::new();

    let message_id_1 = format!("{}", uuid::Uuid::new_v4());
    let request_text_1 = format!(
        "Call mock_tool with message='context-test-{}'. Do not say anything else.",
        message_id_1
    );

    let response = client
        .post(format!("{}/a2a/v1/message:send", AURA_SERVER))
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .json(&json!({
            "message": {
                "messageId": message_id_1,
                "role": "ROLE_USER",
                "parts": [{"text": request_text_1}]
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            format!(
                "Failed to send first request - is aura-web-server running? {}",
                e
            )
        });

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read first response");
    let response_body: Value =
        serde_json::from_str(&body).expect("First response was not valid JSON");

    println!("First task response body: {}", body);

    let task1 = response_body
        .get("task")
        .expect("First response did not contain 'task' field");
    let task1_id = task1
        .get("id")
        .and_then(|v| v.as_str())
        .expect("First task did not contain 'id' field");
    let context_id = task1
        .get("contextId")
        .and_then(|v| v.as_str())
        .expect("First task did not contain 'contextId' field");

    // second message links to the same context via message.contextId
    let message_id_2 = format!("{}", uuid::Uuid::new_v4());
    let request_text_2 = "Tell me what was asked in the first call for this context.";

    let response = client
        .post(format!("{}/a2a/v1/message:send", AURA_SERVER))
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .json(&json!({
            "message": {
                "messageId": message_id_2,
                "contextId": context_id,
                "role": "ROLE_USER",
                "parts": [{"text": request_text_2}]
            }
        }))
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            format!(
                "Failed to send second request - is aura-web-server running? {}",
                e
            )
        });

    let body = response
        .unwrap()
        .text()
        .await
        .expect("Failed to read second response");
    let response_body: Value =
        serde_json::from_str(&body).expect("Second response was not valid JSON");

    println!("Second task response body: {}", body);

    let task2 = response_body
        .get("task")
        .expect("Second response did not contain 'task' field");

    let task2_id = task2
        .get("id")
        .and_then(|v| v.as_str())
        .expect("Second task did not contain 'id' field");
    assert_ne!(
        task1_id, task2_id,
        "Second task should have a different id than the first"
    );

    let task2_context_id = task2
        .get("contextId")
        .and_then(|v| v.as_str())
        .expect("Second task did not contain 'contextId' field");
    assert_eq!(
        context_id, task2_context_id,
        "Second task should share the same contextId as the first"
    );

    let status = task2
        .get("status")
        .expect("Second task did not contain 'status' field");
    let state = status
        .get("state")
        .expect("Second task status did not contain 'state' field");
    assert_eq!(
        "TASK_STATE_WORKING",
        state.as_str().unwrap(),
        "Second task should be in WORKING state"
    );

    let history = task2
        .get("history")
        .expect("Second task did not contain 'history' field");
    assert!(
        history.is_array(),
        "Second task 'history' should be an array"
    );
    let history_arr = history.as_array().unwrap();
    assert!(
        !history_arr.is_empty(),
        "Second task 'history' should not be empty"
    );

    let first_hist_msg = &history_arr[0];
    let hist_msg_id = first_hist_msg
        .get("messageId")
        .expect("Second task history message missing 'messageId'");
    assert_eq!(
        hist_msg_id, &message_id_2,
        "Second task history should contain the second message"
    );

    // now validate that the 2nd task returns what I had asked the first go around
    // within a reasonable timeframe, otherwise just fail.
    let poll_interval = Duration::from_millis(500);
    let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;

    let completed_task = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Task did not reach TASK_STATE_COMPLETED within timeout"
        );

        let poll_resp = client
            .get(format!("{}/a2a/v1/tasks/{}", AURA_SERVER, task2_id))
            .header("A2A-Version", "1.0")
            .timeout(TEST_TIMEOUT)
            .send()
            .await
            .expect("Failed to poll task status");

        let body = poll_resp
            .text()
            .await
            .expect("Failed to read second response");
        println!("Poll response was: {}", body.clone());

        let polled: Task =
            serde_json::from_str(body.as_str()).expect("poll response was not valid JSON");

        match polled.status.state {
            TaskState::Completed => break polled,
            TaskState::Failed | TaskState::Canceled => {
                panic!("Task ended in unexpected state: {:?}", polled.status.state)
            }
            _ => tokio::time::sleep(poll_interval).await,
        }
    };

    let artifacts = completed_task
        .artifacts
        .as_ref()
        .expect("Completed task has no 'artifacts' field");

    let final_info = artifacts
        .iter()
        .find(|a| a.name.as_deref() == Some("Final Info"))
        .expect("No artifact with name 'Final Info' found");

    let part_text = final_info
        .parts
        .iter()
        .find_map(|p| match &p.content {
            PartContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .expect("'Final Info' artifact has no text part");

    // Example: "In the first call, you asked to call `mock_tool` with:\n\n`message='context-test-bf34de53-5fbe-45fd-865d-52a93d805793'`."
    // The history artifact should have something like this example, but
    // shortening the lookup as it might not be consistent.
    let expected = format!("message='context-test-{}'", message_id_1);
    assert!(
        part_text.contains(&*expected),
        "Final Info part does not contain \"message='context-text'\": {}",
        part_text
    );
}
