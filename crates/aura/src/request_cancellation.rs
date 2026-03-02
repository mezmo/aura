//! Per-request cancellation that propagates to MCP tool execution.
//!
//! Register a request, then cancel by ID on timeout/disconnect.
//! MCP cancellation is handled via client-level tracking (Arc-based) in
//! `mcp_streamable_http.rs`, which survives Rig's thread-jumping.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::mcp_streamable_http::StreamableHttpMcpClient;

pub type RequestId = String;

struct Registry {
    requests: RwLock<HashMap<RequestId, CancellationToken>>,
}

impl Registry {
    fn new() -> Self {
        Self {
            requests: RwLock::new(HashMap::new()),
        }
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::new)
}

/// Per-request cancellation context
#[derive(Clone)]
pub struct RequestCancellation {
    pub token: CancellationToken,
    pub request_id: RequestId,
}

impl RequestCancellation {
    /// Register a new request in the global cancellation registry.
    pub fn register(request_id: impl Into<RequestId>) -> Self {
        let request_id = request_id.into();
        let token = CancellationToken::new();

        registry()
            .requests
            .write()
            .unwrap_or_else(|poisoned| {
                tracing::error!("Cancellation registry write lock poisoned, recovering");
                poisoned.into_inner()
            })
            .insert(request_id.clone(), token.clone());

        debug!("Registered cancellation for request '{}'", request_id);

        Self { token, request_id }
    }

    /// Cancel a request by ID. Called on timeout or client disconnect.
    pub fn cancel(request_id: &str, reason: &str) {
        let requests = registry().requests.read().unwrap_or_else(|poisoned| {
            tracing::error!("Cancellation registry read lock poisoned, recovering");
            poisoned.into_inner()
        });
        if let Some(token) = requests.get(request_id) {
            info!("Cancelling request '{}': {}", request_id, reason);
            token.cancel();
        }
    }

    /// Remove a completed request from the registry.
    pub fn unregister(request_id: &str) {
        registry()
            .requests
            .write()
            .unwrap_or_else(|poisoned| {
                tracing::error!("Cancellation registry write lock poisoned, recovering");
                poisoned.into_inner()
            })
            .remove(request_id);
        debug!("Unregistered cancellation for request '{}'", request_id);
    }

    /// Check if this request has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

pub async fn call_http_tool_cancellable(
    client: &StreamableHttpMcpClient,
    tool_name: &str,
    args: HashMap<String, Value>,
) -> Result<String> {
    client.call_tool(tool_name, args).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_cancel() {
        let ctx = RequestCancellation::register("test_req_cancel_1");
        assert!(!ctx.is_cancelled());

        RequestCancellation::cancel("test_req_cancel_1", "test");
        assert!(ctx.is_cancelled());

        RequestCancellation::unregister("test_req_cancel_1");
    }

    #[test]
    fn test_multiple_requests_independent() {
        let ctx1 = RequestCancellation::register("test_req_cancel_a");
        let ctx2 = RequestCancellation::register("test_req_cancel_b");

        RequestCancellation::cancel("test_req_cancel_a", "test");

        assert!(ctx1.is_cancelled());
        assert!(!ctx2.is_cancelled()); // Independent

        RequestCancellation::unregister("test_req_cancel_a");
        RequestCancellation::unregister("test_req_cancel_b");
    }

    #[test]
    fn test_cancel_nonexistent_request_safe() {
        // Should not panic
        RequestCancellation::cancel("nonexistent_request", "test");
    }

    #[test]
    fn test_unregister_nonexistent_request_safe() {
        // Should not panic
        RequestCancellation::unregister("nonexistent_request");
    }
}
