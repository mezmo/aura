//! Shared test utilities for aura integration tests.

pub mod sse;

use std::future::Future;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

/// Retry a test function with exponential backoff.
pub async fn retry_test<F, Fut, E>(max_retries: usize, test_fn: F) -> Result<(), E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    let mut last_error = None;

    for attempt in 0..=max_retries {
        match test_fn().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt < max_retries {
                    let delay_ms = 100 * (1 << attempt); // 100, 200, 400, 800...
                    eprintln!(
                        "Test attempt {} failed: {}. Retrying in {}ms...",
                        attempt + 1,
                        e,
                        delay_ms
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap())
}

pub mod server_urls {
    use std::fmt;
    use std::ops::Deref;
    use std::sync::LazyLock;

    /// A lazily-initialized URL that implements Display for use in format strings.
    pub struct LazyUrl {
        inner: LazyLock<String>,
    }

    impl LazyUrl {
        const fn new(f: fn() -> String) -> Self {
            Self {
                inner: LazyLock::new(f),
            }
        }

        pub fn as_str(&self) -> &str {
            self.inner.as_str()
        }
    }

    impl Deref for LazyUrl {
        type Target = str;
        fn deref(&self) -> &Self::Target {
            self.as_str()
        }
    }

    impl fmt::Display for LazyUrl {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.as_str())
        }
    }

    /// Aura web server URL. Defaults to localhost:8080 for local development.
    /// Override with AURA_SERVER_URL env var for container testing.
    pub static AURA_SERVER: LazyUrl = LazyUrl::new(|| {
        std::env::var("AURA_SERVER_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
    });

    /// Single-agent aura web server URL. Used by the scratchpad integration
    /// suite to exercise the single-agent path side-by-side with the
    /// orchestration path. Defaults to localhost:8081 for local development.
    /// Override with AURA_SINGLE_AGENT_SERVER_URL env var for container testing.
    pub static AURA_SINGLE_AGENT_SERVER: LazyUrl = LazyUrl::new(|| {
        std::env::var("AURA_SINGLE_AGENT_SERVER_URL")
            .unwrap_or_else(|_| "http://localhost:8081".to_string())
    });

    /// MCP cancellation test server URL. Defaults to localhost:9998.
    /// Override with MCP_SERVER_URL env var for container testing.
    pub static MCP_SERVER: LazyUrl = LazyUrl::new(|| {
        std::env::var("MCP_SERVER_URL").unwrap_or_else(|_| "http://localhost:9998".to_string())
    });
}

pub mod timeouts {
    use std::time::Duration;

    pub const HTTP_REQUEST: Duration = Duration::from_secs(60);
    pub const TOOL_START: Duration = Duration::from_secs(30);
    pub const CANCELLATION_CHECK: Duration = Duration::from_secs(3);
    pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
    pub const PROGRESS_RECEIVE: Duration = Duration::from_secs(20);
    pub const POST_DISCONNECT_WAIT: Duration = Duration::from_secs(5);
}

/// Response from the MCP server's /tasks endpoint
#[derive(Debug, Deserialize)]
pub struct TaskStatusResponse {
    pub task_id: String,
    pub status: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

/// Helper for checking task status via HTTP instead of filesystem polling.
/// This works reliably across container boundaries.
pub struct TaskStatusChecker {
    client: Client,
    task_id: String,
    mcp_server_url: String,
}

impl TaskStatusChecker {
    pub fn new(task_id: &str) -> Self {
        Self {
            client: Client::new(),
            task_id: task_id.to_string(),
            mcp_server_url: server_urls::MCP_SERVER.to_string(),
        }
    }

    /// Get current task status from MCP server
    pub async fn get_status(&self) -> Result<TaskStatusResponse, String> {
        let url = format!("{}/tasks?id={}", self.mcp_server_url, self.task_id);
        let response = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("Failed to query task status: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("Task status query failed: {:?}", response.status()));
        }

        response
            .json::<TaskStatusResponse>()
            .await
            .map_err(|e| format!("Failed to parse task status response: {}", e))
    }

    /// Check if task has started (status is "started", "completed", or "cancelled")
    pub async fn has_started(&self) -> Result<bool, String> {
        let status = self.get_status().await?;
        Ok(status.status != "unknown")
    }

    /// Check if task has completed (status is "completed")
    pub async fn has_completed(&self) -> Result<bool, String> {
        let status = self.get_status().await?;
        Ok(status.status == "completed")
    }

    /// Check if task was cancelled (status is "cancelled")
    pub async fn was_cancelled(&self) -> Result<bool, String> {
        let status = self.get_status().await?;
        Ok(status.status == "cancelled")
    }

    /// Poll until task starts or timeout expires
    pub async fn wait_for_start(&self, timeout: Duration) -> Result<bool, String> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self.has_started().await.unwrap_or(false) {
                return Ok(true);
            }
            tokio::time::sleep(timeouts::POLL_INTERVAL).await;
        }
        Ok(false)
    }

    /// Clear task state on the MCP server
    pub async fn cleanup(&self) -> Result<(), String> {
        let url = format!("{}/tasks?id={}", self.mcp_server_url, self.task_id);
        self.client
            .delete(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("Failed to cleanup task: {}", e))?;
        Ok(())
    }
}
