//! Streamable HTTP MCP client with progress and cancellation support.

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::{
    RoleClient,
    model::{
        CallToolRequestParam, CancelledNotificationParam, ClientRequest, ProgressNotificationParam,
        Request, RequestId, Tool,
    },
    serve_client,
    service::{PeerRequestOptions, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::mcp_progress::ProgressEnabledHandler;
use crate::mcp_response::extract_tool_result;
use crate::tool_event_broker::{peek_tool_call_id, publish_tool_start};

const CANCEL_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(2);

/// Tracks in-flight MCP requests for cancellation support.
/// Maps HTTP request_id → set of MCP request_ids that are in-flight.
#[derive(Default)]
pub struct InFlightRequests {
    requests: RwLock<HashMap<String, HashSet<RequestId>>>,
}

impl InFlightRequests {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an in-flight MCP request for an HTTP request
    pub async fn register(&self, http_request_id: &str, mcp_request_id: RequestId) {
        let mut map = self.requests.write().await;
        map.entry(http_request_id.to_string())
            .or_default()
            .insert(mcp_request_id);
    }

    /// Remove an MCP request (completed or cancelled)
    pub async fn remove(&self, http_request_id: &str, mcp_request_id: &RequestId) {
        let mut map = self.requests.write().await;
        if let Some(set) = map.get_mut(http_request_id) {
            set.remove(mcp_request_id);
            if set.is_empty() {
                map.remove(http_request_id);
            }
        }
    }

    /// Get all in-flight MCP request IDs for an HTTP request
    pub async fn get_all(&self, http_request_id: &str) -> Vec<RequestId> {
        let map = self.requests.read().await;
        map.get(http_request_id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Clear all in-flight requests for an HTTP request (cleanup)
    pub async fn clear(&self, http_request_id: &str) {
        let mut map = self.requests.write().await;
        map.remove(http_request_id);
    }
}

/// MCP client for HTTP streamable connections with progress notification support
pub struct StreamableHttpMcpClient {
    client: Arc<RunningService<RoleClient, ProgressEnabledHandler>>,
    server_url: String,
    /// Tracks in-flight MCP requests for cancellation support
    in_flight: Arc<InFlightRequests>,
    /// Current HTTP request ID for automatic cancellation tracking.
    current_http_request_id: Arc<RwLock<Option<String>>>,
}

impl Clone for StreamableHttpMcpClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            server_url: self.server_url.clone(),
            in_flight: self.in_flight.clone(),
            current_http_request_id: self.current_http_request_id.clone(),
        }
    }
}

impl StreamableHttpMcpClient {
    pub async fn new(
        server_url: String,
        forwarded_headers: &HashMap<String, String>,
    ) -> Result<Self> {
        info!("Creating streamable HTTP MCP client for: {}", server_url);

        let mut header_map = HeaderMap::new();
        if !forwarded_headers.is_empty() {
            debug!("Adding {} headers to MCP client", forwarded_headers.len());
            for (key, value) in forwarded_headers {
                match (
                    HeaderName::from_bytes(key.as_bytes()),
                    HeaderValue::from_str(value),
                ) {
                    (Ok(name), Ok(val)) => {
                        header_map.insert(name, val);
                    }
                    _ => {
                        tracing::warn!("Skipping invalid header '{}' (failed to convert)", key);
                    }
                }
            }
        }

        let http_client = reqwest::Client::builder()
            .default_headers(header_map)
            .build()
            .context("Failed to build HTTP client")?;

        let transport = StreamableHttpClientTransport::with_client(
            http_client,
            StreamableHttpClientTransportConfig {
                uri: server_url.clone().into(),
                ..Default::default()
            },
        );

        let current_http_request_id = Arc::new(RwLock::new(None));

        let handler = ProgressEnabledHandler::new(current_http_request_id.clone());

        let client = serve_client(handler, transport)
            .await
            .context("Failed to establish MCP client connection")?;

        info!(
            "Successfully established streamable HTTP MCP client: {}",
            server_url
        );

        Ok(Self {
            client: Arc::new(client),
            server_url,
            in_flight: Arc::new(InFlightRequests::new()),
            current_http_request_id,
        })
    }

    /// Set the current HTTP request ID for cancellation tracking.
    pub async fn set_current_request(&self, http_request_id: &str) {
        let mut guard = self.current_http_request_id.write().await;
        *guard = Some(http_request_id.to_string());
        debug!(
            "Set current HTTP request ID for MCP client: {}",
            http_request_id
        );
    }

    /// Clear the current HTTP request ID.
    pub async fn clear_current_request(&self) {
        let mut guard = self.current_http_request_id.write().await;
        if let Some(ref id) = *guard {
            debug!("Cleared current HTTP request ID: {}", id);
        }
        *guard = None;
    }

    pub async fn get_current_request(&self) -> Option<String> {
        self.current_http_request_id.read().await.clone()
    }

    pub async fn discover_tools(&self) -> Result<Vec<Tool>> {
        debug!(
            "🔍 Starting tool discovery from MCP server: {}",
            self.server_url
        );

        let tools_response = self
            .client
            .list_tools(Default::default())
            .await
            .context("Failed to list tools from MCP server")?;

        info!(
            "Discovered {} tools from server: {}",
            tools_response.tools.len(),
            self.server_url
        );

        for tool in &tools_response.tools {
            debug!(
                "  - Tool: {} ({})",
                tool.name,
                tool.description.as_deref().unwrap_or("no description")
            );
        }

        Ok(tools_response.tools)
    }

    /// Execute a tool. Auto-tracks for cancellation if `set_current_request` was called.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: HashMap<String, Value>,
    ) -> Result<String> {
        if let Some(http_request_id) = self.get_current_request().await {
            info!(
                "Tool '{}' executing WITH automatic tracking (http_request_id={})",
                tool_name, http_request_id
            );
            return self
                .call_tool_tracked(tool_name, arguments, &http_request_id)
                .await;
        }

        debug!(
            "Calling tool '{}' without tracking (no current request set)",
            tool_name
        );

        let args_map: Map<String, Value> = arguments.into_iter().collect();

        let request_param = CallToolRequestParam {
            name: tool_name.to_string().into(),
            arguments: Some(args_map),
        };

        match self.client.call_tool(request_param).await {
            Ok(result) => {
                debug!("Tool '{}' executed successfully", tool_name);
                // Use shared response handler (supports both structured_content and text)
                extract_tool_result(result, tool_name)
            }
            Err(err) => {
                error!("Failed to execute tool '{}': {}", tool_name, err);
                Err(anyhow::anyhow!("Tool execution failed: {}", err))
            }
        }
    }

    /// Execute a tool and return progress notifications via channel.
    pub async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: HashMap<String, Value>,
    ) -> Result<(String, mpsc::Receiver<ProgressNotificationParam>)> {
        debug!(
            "Calling tool '{}' with progress tracking, args: {:?}",
            tool_name, arguments
        );

        let args_map: Map<String, Value> = arguments.into_iter().collect();
        let request_param = CallToolRequestParam {
            name: tool_name.to_string().into(),
            arguments: Some(args_map),
        };

        let handle = self
            .client
            .send_cancellable_request(
                ClientRequest::CallToolRequest(Request::new(request_param)),
                PeerRequestOptions::no_options(),
            )
            .await
            .context("Failed to send tool call request")?;

        let progress_token = handle.progress_token.clone();
        info!(
            "Tool '{}' started with progress token: {:?}",
            tool_name, progress_token
        );

        let mut progress_subscriber = self
            .client
            .service()
            .progress_dispatcher()
            .subscribe(progress_token.clone())
            .await;

        let (progress_tx, progress_rx) = mpsc::channel::<ProgressNotificationParam>(16);

        let tool_name_for_task = tool_name.to_string();
        tokio::spawn(async move {
            while let Some(notification) = progress_subscriber.next().await {
                debug!(
                    "Progress for '{}': {}/{:?} - {:?}",
                    tool_name_for_task,
                    notification.progress,
                    notification.total,
                    notification.message
                );
                if progress_tx.send(notification).await.is_err() {
                    // Receiver dropped, stop forwarding
                    break;
                }
            }
            debug!("Progress stream ended for '{}'", tool_name_for_task);
        });

        let response = handle
            .await_response()
            .await
            .context(format!("Tool '{}' execution failed", tool_name))?;

        match response {
            rmcp::model::ServerResult::CallToolResult(result) => {
                debug!("Tool '{}' completed with progress tracking", tool_name);
                let extracted = extract_tool_result(result, tool_name)?;
                Ok((extracted, progress_rx))
            }
            _ => Err(anyhow::anyhow!("Unexpected response type for tool call")),
        }
    }

    /// Execute a tool with cancellation support. Sends `notifications/cancelled` on cancel.
    pub async fn call_tool_with_cancellation(
        &self,
        tool_name: &str,
        arguments: HashMap<String, Value>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<String> {
        use rmcp::model::{CancelledNotification, CancelledNotificationParam};

        debug!(
            "Calling tool '{}' with cancellation support, args: {:?}",
            tool_name, arguments
        );

        let args_map: Map<String, Value> = arguments.into_iter().collect();
        let request_param = CallToolRequestParam {
            name: tool_name.to_string().into(),
            arguments: Some(args_map),
        };

        let handle = self
            .client
            .send_cancellable_request(
                ClientRequest::CallToolRequest(Request::new(request_param)),
                PeerRequestOptions::no_options(),
            )
            .await
            .context("Failed to send tool call request")?;

        // Extract what we need for cancellation before moving handle
        let request_id = handle.id.clone();
        let peer = handle.peer.clone();

        // Race the response against cancellation
        tokio::select! {
            result = handle.await_response() => {
                match result {
                    Ok(rmcp::model::ServerResult::CallToolResult(call_result)) => {
                        debug!("Tool '{}' completed successfully", tool_name);
                        extract_tool_result(call_result, tool_name)
                    }
                    Ok(_) => Err(anyhow::anyhow!("Unexpected response type for tool call")),
                    Err(err) => {
                        error!("Tool '{}' failed: {}", tool_name, err);
                        Err(anyhow::anyhow!("Tool execution failed: {}", err))
                    }
                }
            }
            _ = cancel_token.cancelled() => {
                // Send cancellation notification to MCP server
                info!("Sending notifications/cancelled to MCP server for tool '{}' (request_id: {:?})", tool_name, request_id);
                let notification = CancelledNotification {
                    params: CancelledNotificationParam {
                        request_id,
                        reason: Some("Client disconnected or timeout".to_string()),
                    },
                    method: rmcp::model::CancelledNotificationMethod,
                    extensions: Default::default(),
                };
                if let Err(e) = peer.send_notification(notification.into()).await {
                    error!("Failed to send cancellation notification: {}", e);
                }
                Err(anyhow::anyhow!("Request cancelled"))
            }
        }
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    /// Execute a tool with explicit tracking for later cancellation via `cancel_all_for_request`.
    ///
    /// Also emits `aura.tool_start` event with the progress_token for UI correlation.
    /// The tool_call_id is retrieved from the FIFO queue (pushed by the hook earlier).
    pub async fn call_tool_tracked(
        &self,
        tool_name: &str,
        arguments: HashMap<String, Value>,
        http_request_id: &str,
    ) -> Result<String> {
        debug!(
            "Calling tool '{}' with tracking (http_request_id={})",
            tool_name, http_request_id
        );

        let args_map: Map<String, Value> = arguments.into_iter().collect();
        let request_param = CallToolRequestParam {
            name: tool_name.to_string().into(),
            arguments: Some(args_map),
        };
        let handle = self
            .client
            .send_cancellable_request(
                ClientRequest::CallToolRequest(Request::new(request_param)),
                PeerRequestOptions::no_options(),
            )
            .await
            .context("Failed to send tool call request")?;

        // Track this request for potential cancellation
        let mcp_request_id = handle.id.clone();
        self.in_flight
            .register(http_request_id, mcp_request_id.clone())
            .await;
        debug!(
            "Registered MCP request {:?} for HTTP request {}",
            mcp_request_id, http_request_id
        );

        // Emit tool_start event with progress_token for UI correlation.
        // The tool_call_id was pushed to the FIFO queue by the hook earlier.
        // We peek (not pop) here - the pop happens in on_tool_result to ensure
        // push/pop pairing for ALL tools (MCP and non-MCP like vector stores).
        let progress_token = Some(handle.progress_token.clone());
        let request_id_string = http_request_id.to_string();
        if let Some(tool_call_id) = peek_tool_call_id(&request_id_string).await {
            publish_tool_start(
                http_request_id,
                tool_call_id.clone(),
                tool_name.to_string(),
                progress_token.clone(),
            )
            .await;
            debug!(
                "Emitted tool_start for tool '{}' (tool_call_id={}, progress_token={:?})",
                tool_name, tool_call_id, progress_token
            );
        } else {
            warn!(
                "No tool_call_id in queue for tool '{}' on request '{}' - hook may not have fired or FIFO queue mismatch",
                tool_name, http_request_id
            );
        }

        // Await the tool result
        let result = handle.await_response().await;

        // Remove from tracking (completed or failed)
        self.in_flight
            .remove(http_request_id, &mcp_request_id)
            .await;

        match result {
            Ok(rmcp::model::ServerResult::CallToolResult(call_result)) => {
                debug!("Tool '{}' completed successfully", tool_name);
                extract_tool_result(call_result, tool_name)
            }
            Ok(_) => Err(anyhow::anyhow!("Unexpected response type for tool call")),
            Err(err) => {
                error!("Tool '{}' failed: {}", tool_name, err);
                Err(anyhow::anyhow!("Tool execution failed: {}", err))
            }
        }
    }

    /// Cancel all in-flight MCP requests for an HTTP request.
    pub async fn cancel_all_for_request(&self, http_request_id: &str, reason: &str) -> usize {
        let mcp_request_ids = self.in_flight.get_all(http_request_id).await;

        if mcp_request_ids.is_empty() {
            debug!(
                "No in-flight MCP requests to cancel for HTTP request {}",
                http_request_id
            );
            return 0;
        }

        info!(
            "Cancelling {} in-flight MCP request(s) for HTTP request {}: {}",
            mcp_request_ids.len(),
            http_request_id,
            reason
        );

        let peer = self.client.peer();
        let mut cancelled_count = 0;

        for request_id in &mcp_request_ids {
            debug!(
                "Sending notifications/cancelled for MCP request {:?}",
                request_id
            );
            match tokio::time::timeout(
                CANCEL_NOTIFICATION_TIMEOUT,
                peer.notify_cancelled(CancelledNotificationParam {
                    request_id: request_id.clone(),
                    reason: Some(reason.to_string()),
                }),
            )
            .await
            {
                Ok(Ok(())) => cancelled_count += 1,
                Ok(Err(e)) => {
                    warn!(
                        "Failed to send cancellation notification for {:?}: {}",
                        request_id, e
                    );
                }
                Err(_) => {
                    warn!(
                        "Timeout sending cancellation notification for {:?}",
                        request_id
                    );
                }
            }
        }

        self.in_flight.clear(http_request_id).await;

        cancelled_count
    }

    /// Get the in-flight request tracker (for sharing across clones)
    pub fn in_flight_tracker(&self) -> Arc<InFlightRequests> {
        self.in_flight.clone()
    }

    /// Forcefully close the MCP connection. Client is unusable after this call.
    pub fn close_connection(&self) {
        info!("Forcefully closing MCP connection to: {}", self.server_url);
        self.client.cancellation_token().cancel();
    }

    /// Cancel all in-flight requests and close the connection.
    pub async fn cancel_and_close(&self, http_request_id: &str, reason: &str) -> usize {
        let count = self.cancel_all_for_request(http_request_id, reason).await;

        // Also clear the request ID to stop routing any straggler progress notifications
        self.clear_current_request().await;

        // Forcefully close connection - server is ignoring cancellation anyway
        self.close_connection();

        info!(
            "Cancelled {} request(s) and closed MCP connection for HTTP request {}",
            count, http_request_id
        );

        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_in_flight_requests_tracking() {
        let tracker = InFlightRequests::new();
        let http_id = "http-123";
        let mcp_id = RequestId::Number(1);

        tracker.register(http_id, mcp_id.clone()).await;
        assert_eq!(tracker.get_all(http_id).await.len(), 1);

        tracker.remove(http_id, &mcp_id).await;
        assert_eq!(tracker.get_all(http_id).await.len(), 0);
    }
}
