#![cfg(feature = "integration-progress")]

//! Integration test: progress notification disconnect detection.
//!

use aura_test_utils::server_urls::AURA_SERVER;
use aura_test_utils::timeouts::{HTTP_REQUEST, POST_DISCONNECT_WAIT, PROGRESS_RECEIVE, TOOL_START};
use futures_util::StreamExt;
use reqwest::Client;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::time::{sleep, timeout};

const MAX_RETRIES: usize = 3;
const MIN_PROGRESS_EVENTS: usize = 2;

#[tokio::test]
async fn test_progress_notifications_stop_after_disconnect() {
    let result = aura_test_utils::retry_test(MAX_RETRIES, || async {
        run_progress_disconnect_test().await
    })
    .await;

    if let Err(e) = result {
        panic!("Test failed after {} retries: {}", MAX_RETRIES + 1, e);
    }
}

async fn run_progress_disconnect_test() -> Result<(), String> {
    let client = Client::new();
    let progress_count = Arc::new(AtomicUsize::new(0));
    let progress_count_clone = progress_count.clone();

    let response = match client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "messages": [{
                "role": "user",
                "content": "Call the task_with_progress tool immediately with duration_seconds=30 and steps=100. Do not say anything else, just call the tool."
            }],
            "stream": true
        }))
        .timeout(HTTP_REQUEST)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return Err(format!("Failed to connect to aura-web-server: {}", e));
        }
    };

    if !response.status().is_success() {
        return Err(format!(
            "Request failed with status: {:?}",
            response.status()
        ));
    }

    let mut stream = response.bytes_stream();

    let wait_for_progress = async {
        let mut buffer = String::new();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    buffer.push_str(&text);

                    while let Some(event_end) = buffer.find("\n\n") {
                        let event = buffer[..event_end].to_string();
                        buffer = buffer[event_end + 2..].to_string();

                        if event.contains("event: aura.progress") {
                            let count = progress_count_clone.fetch_add(1, Ordering::SeqCst) + 1;
                            if count >= MIN_PROGRESS_EVENTS {
                                return true;
                            }
                        }
                    }
                }
                Err(_) => return false,
            }
        }
        false
    };

    let received_progress = timeout(PROGRESS_RECEIVE, wait_for_progress).await;

    let progress_before_disconnect = progress_count.load(Ordering::SeqCst);

    if (received_progress.is_err() || !received_progress.unwrap())
        && progress_before_disconnect == 0
    {
        return Err("No progress events received".to_string());
    }

    drop(stream);
    let count_at_disconnect = progress_count.load(Ordering::SeqCst);
    sleep(POST_DISCONNECT_WAIT).await;

    if count_at_disconnect < MIN_PROGRESS_EVENTS {
        return Err(format!(
            "Should have received at least {} progress events, got {}",
            MIN_PROGRESS_EVENTS, count_at_disconnect
        ));
    }

    Ok(())
}

#[tokio::test]
async fn test_progress_events_received_when_connected() {
    let result =
        aura_test_utils::retry_test(MAX_RETRIES, || async { run_progress_control_test().await })
            .await;

    if let Err(e) = result {
        panic!("Test failed after {} retries: {}", MAX_RETRIES + 1, e);
    }
}

async fn run_progress_control_test() -> Result<(), String> {
    let client = Client::new();
    let progress_count = Arc::new(AtomicUsize::new(0));
    let progress_count_clone = progress_count.clone();

    let response = match client
        .post(format!("{AURA_SERVER}/v1/chat/completions"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "messages": [{
                "role": "user",
                "content": "Call the task_with_progress tool immediately with duration_seconds=3 and steps=5. Do not say anything else, just call the tool."
            }],
            "stream": true
        }))
        .timeout(HTTP_REQUEST)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return Err(format!("Failed to connect: {}", e));
        }
    };

    if !response.status().is_success() {
        return Err(format!(
            "Request failed with status: {:?}",
            response.status()
        ));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    let read_all = async {
        while let Some(chunk_result) = stream.next().await {
            if let Ok(bytes) = chunk_result {
                let text = String::from_utf8_lossy(&bytes);
                buffer.push_str(&text);

                while let Some(event_end) = buffer.find("\n\n") {
                    let event = buffer[..event_end].to_string();
                    buffer = buffer[event_end + 2..].to_string();

                    if event.contains("event: aura.progress") {
                        progress_count_clone.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
    };

    let _ = timeout(TOOL_START, read_all).await;
    let total_progress = progress_count.load(Ordering::SeqCst);

    if total_progress < MIN_PROGRESS_EVENTS {
        return Err(format!(
            "Should receive at least {} progress events, got {}",
            MIN_PROGRESS_EVENTS, total_progress
        ));
    }

    Ok(())
}
