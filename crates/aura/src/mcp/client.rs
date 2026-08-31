//! Streamable HTTP MCP client with progress and cancellation support.

use anyhow::{Context, Result};
use futures::{StreamExt, stream::BoxStream};
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
        StreamableHttpClientTransport,
        streamable_http_client::{StreamableHttpClient, StreamableHttpClientTransportConfig},
    },
};
use serde_json::{Map, Value};
use sse_stream::{Sse, SseStream};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::approver_headers::ApproverHeaders;
use crate::mcp::progress::ProgressEnabledHandler;
use crate::mcp::response::extract_tool_result;
use crate::tool_event_broker::peek_tool_call_id;
use aura_events::agent::{AgentEvent, AgentEventPayload};

/// Custom HTTP client that captures the underlying HTTP status when a request
/// fails.
#[derive(Clone, Default)]
pub struct CustomHttpClient {
    client: reqwest::Client,
    /// First failing HTTP status observed on this client.
    first_error: Arc<Mutex<Option<String>>>,
}

impl CustomHttpClient {
    /// Wrap an existing `reqwest::Client` (already carrying any forwarded
    /// headers, including auth) so transport HTTP errors can be captured.
    ///
    /// rmcp's streamable-HTTP transport runs in a background worker: when a
    /// request fails (e.g. 404/401), the worker logs the `reqwest` error and
    /// closes the channel, so `serve_client` only sees "channel closed" — the
    /// status code is lost. This client records the status into `first_error`
    /// at the layer it occurs (in `post_message`/`get_stream`), then
    /// `McpClient::new` reads it back after the connection fails to surface a
    /// precise reason.
    pub fn from_reqwest(client: reqwest::Client) -> Self {
        Self {
            client,
            first_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Shared handle to the first captured failing HTTP status, if any.
    ///
    /// The `Arc` is shared across clones because the rmcp transport worker
    /// clones the client when setting up the background task.
    pub fn first_error(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.first_error)
    }

    /// Record the first non-success HTTP status seen (first error wins, so the
    /// root cause isn't overwritten by any follow-on failures).
    fn record_http_status(&self, status: reqwest::StatusCode) {
        if status.is_success() {
            return;
        }
        if let Ok(mut guard) = self.first_error.lock()
            && guard.is_none()
        {
            *guard = Some(format!("{}{status}", aura_events::HTTP_STATUS_MARKER));
        }
    }
}

impl StreamableHttpClient for CustomHttpClient {
    type Error = reqwest::Error;

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        _auth_token: Option<String>, // auth flows through the client's default headers
    ) -> Result<
        BoxStream<'static, Result<Sse, sse_stream::Error>>,
        rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
    > {
        use reqwest::header::ACCEPT;
        use rmcp::transport::common::http_header::{
            EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
        };

        let mut request_builder = self
            .client
            .get(uri.as_ref())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(HEADER_SESSION_ID, session_id.as_ref());

        if let Some(last_event_id) = last_event_id {
            request_builder = request_builder.header(HEADER_LAST_EVENT_ID, last_event_id);
        }

        let response = request_builder
            .send()
            .await
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            // Not a failure — the server just doesn't support the SSE GET.
            return Err(rmcp::transport::streamable_http_client::StreamableHttpError::ServerDoesNotSupportSse);
        }
        self.record_http_status(response.status());
        let response = response
            .error_for_status()
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;

        match response.headers().get(reqwest::header::CONTENT_TYPE) {
            Some(ct) => {
                // Accept both `text/event-stream` and `application/json`, matching
                // rmcp's reference reqwest client — a server may answer the GET
                // stream with either. Rejecting JSON here would mark an otherwise
                // healthy server as `Failed`.
                if !ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                    && !ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes())
                {
                    return Err(rmcp::transport::streamable_http_client::StreamableHttpError::UnexpectedContentType(Some(
                        String::from_utf8_lossy(ct.as_bytes()).to_string(),
                    )));
                }
            }
            None => {
                return Err(rmcp::transport::streamable_http_client::StreamableHttpError::UnexpectedContentType(None));
            }
        }

        let event_stream = SseStream::from_byte_stream(response.bytes_stream()).boxed();
        Ok(event_stream)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        _auth_token: Option<String>, // auth flows through the client's default headers
    ) -> Result<(), rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>> {
        use rmcp::transport::common::http_header::HEADER_SESSION_ID;

        let response = self
            .client
            .delete(uri.as_ref())
            .header(HEADER_SESSION_ID, session.as_ref())
            .send()
            .await
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;

        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(rmcp::transport::streamable_http_client::StreamableHttpError::ServerDoesNotSupportDeleteSession);
        }
        response
            .error_for_status()
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;
        Ok(())
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_token: Option<String>, // auth flows through the client's default headers
    ) -> Result<
        rmcp::transport::streamable_http_client::StreamableHttpPostResponse,
        rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
    > {
        use rmcp::transport::common::http_header::{HEADER_SESSION_ID, JSON_MIME_TYPE};

        // Approver header overrides ride the request's extensions: present
        // only on the one gated call whose approval captured them, and
        // never serialized into the JSON body below (the rmcp serializer
        // emits only `_meta`).
        let approver_overrides = crate::approver_headers::extract_from_client_message(&message);

        let mut request_builder = self
            .client
            .post(uri.as_ref())
            .header(reqwest::header::CONTENT_TYPE, JSON_MIME_TYPE)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            );

        // Forward the negotiated session id on every post after `initialize`.
        // The server returns the id in the initialize response and requires it
        // on subsequent requests (`notifications/initialized`, tool calls, …);
        // dropping it makes the server reject them (FastMCP: 400, rmcp: 422),
        // which the transport worker then collapses into "channel closed".
        if let Some(session_id) = session_id {
            request_builder = request_builder.header(HEADER_SESSION_ID, session_id.as_ref());
        }

        // Per-request override headers beat the client's frozen
        // `default_headers` for exactly this request. Applied last, after
        // every transport-owned header above; reserved names are
        // additionally rejected at config parse, so framing and session
        // routing stay intact.
        let mut request_builder = request_builder.json(&message);
        if let Some(overrides) = approver_overrides {
            request_builder = overrides.apply_to(request_builder);
        }

        let response = request_builder
            .send()
            .await
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;
        // Capture the status before error_for_status consumes the response — this
        // is the initialize/JSON-RPC POST, where auth (401) and endpoint (404)
        // failures surface, and where the transport worker would otherwise hide
        // them behind a "channel closed" error.
        let status = response.status();
        self.record_http_status(status);
        let response = response
            .error_for_status()
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;

        // A notification/response-less POST (e.g. `notifications/initialized`)
        // comes back as 202 Accepted / 204 No Content with an empty body. Some
        // servers (FastMCP) still tag the empty body `application/json`; parsing
        // it as a JSON-RPC message fails and the worker reports "channel closed".
        // Short-circuit on these statuses before touching the body, matching
        // rmcp's reference reqwest client.
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(
                rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Accepted,
            );
        }

        // Extract session ID from headers before consuming response
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        // Snapshot the content type as an owned string before consuming the
        // response body in the branches below.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).to_string());

        match content_type.as_deref() {
            Some(ct) if ct.starts_with("application/json") => {
                let json_text = response.text().await.map_err(
                    rmcp::transport::streamable_http_client::StreamableHttpError::Client,
                )?;
                let json_message: rmcp::model::ServerJsonRpcMessage =
                    serde_json::from_str(&json_text).map_err(
                        rmcp::transport::streamable_http_client::StreamableHttpError::Deserialize,
                    )?;
                Ok(
                    rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Json(
                        json_message,
                        session_id,
                    ),
                )
            }
            Some(ct) if ct.starts_with("text/event-stream") => {
                let event_stream =
                    sse_stream::SseStream::from_byte_stream(response.bytes_stream()).boxed();
                Ok(
                    rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Sse(
                        event_stream,
                        session_id,
                    ),
                )
            }
            // A 2xx body that is neither JSON nor SSE (or carries no content
            // type) is unexpected for a JSON-RPC request — the response-less
            // 202/204 acks are already handled above. Surface it as an error
            // like rmcp's reference client rather than silently reporting
            // `Accepted`, which would drop the real response and hang the
            // request until it times out.
            other => Err(
                rmcp::transport::streamable_http_client::StreamableHttpError::UnexpectedContentType(
                    other.map(|s| s.to_string()),
                ),
            ),
        }
    }
}

/// Build the CallTool request, attaching approver header overrides as a
/// request extension when present. The extension rides on this one
/// request value through the transport worker and is never serialized to
/// the wire, so one-call scoping is structural. Every `call_tool*` variant
/// constructs its request here.
pub(crate) fn call_tool_request(
    request_param: CallToolRequestParam,
    approver_overrides: Option<ApproverHeaders>,
) -> ClientRequest {
    let mut request = Request::new(request_param);
    if let Some(overrides) = approver_overrides {
        request.extensions.insert(overrides);
    }
    ClientRequest::CallToolRequest(request)
}

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
pub struct McpClient {
    client: Arc<RunningService<RoleClient, ProgressEnabledHandler>>,
    server_url: String,
    /// Tracks in-flight MCP requests for cancellation support
    in_flight: Arc<InFlightRequests>,
    /// Current HTTP request ID for automatic cancellation tracking.
    current_http_request_id: Arc<RwLock<Option<String>>>,
}

impl Clone for McpClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            server_url: self.server_url.clone(),
            in_flight: self.in_flight.clone(),
            current_http_request_id: self.current_http_request_id.clone(),
        }
    }
}

impl McpClient {
    /// Create an MCP client from any transport implementing `Transport<RoleClient>`.
    ///
    /// This is the transport-agnostic constructor used by both HTTP streamable
    /// and legacy SSE transports.
    pub(crate) async fn from_transport<T>(transport: T, server_url: String) -> Result<Self>
    where
        T: rmcp::transport::Transport<RoleClient> + Send + 'static,
        T::Error: std::error::Error + Send + Sync + 'static,
    {
        let current_http_request_id = Arc::new(RwLock::new(None));
        let handler = ProgressEnabledHandler::new(Arc::clone(&current_http_request_id));

        let client = serve_client(handler, transport)
            .await
            .context("Failed to establish MCP client connection")?;

        Ok(Self {
            client: Arc::new(client),
            server_url,
            in_flight: Arc::new(InFlightRequests::new()),
            current_http_request_id,
        })
    }

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

        // Use our own StreamableHttpClient so a failing HTTP status (404/401/…)
        // is captured at the transport layer. rmcp's worker otherwise collapses
        // it into a generic "channel closed", losing the actionable detail.
        let custom_client = CustomHttpClient::from_reqwest(http_client);
        let captured_status = custom_client.first_error();

        let transport = StreamableHttpClientTransport::with_client(
            custom_client,
            StreamableHttpClientTransportConfig {
                uri: server_url.clone().into(),
                ..Default::default()
            },
        );

        let client = match Self::from_transport(transport, server_url.clone()).await {
            Ok(client) => client,
            Err(e) => {
                // Surface the real HTTP status when the transport captured one,
                // as the outermost context so it leads the rendered chain.
                if let Some(status) = captured_status.lock().ok().and_then(|mut g| g.take()) {
                    return Err(e.context(status));
                }
                return Err(e);
            }
        };

        info!(
            "Successfully established streamable HTTP MCP client: {}",
            server_url
        );

        Ok(client)
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
        approver_overrides: Option<ApproverHeaders>,
    ) -> Result<String> {
        if let Some(http_request_id) = self.get_current_request().await {
            info!(
                "Tool '{}' executing WITH automatic tracking (http_request_id={})",
                tool_name, http_request_id
            );
            return self
                .call_tool_tracked(tool_name, arguments, &http_request_id, approver_overrides)
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

        // Constructed explicitly (not rmcp's convenience `call_tool`) so
        // this branch shares the one request-construction site that
        // accepts the override extension; same shape as the tracked
        // branch, minus tracking.
        let handle = self
            .client
            .send_cancellable_request(
                call_tool_request(request_param, approver_overrides),
                PeerRequestOptions::no_options(),
            )
            .await
            .context("Failed to send tool call request")?;

        match handle.await_response().await {
            Ok(rmcp::model::ServerResult::CallToolResult(result)) => {
                debug!("Tool '{}' executed successfully", tool_name);
                extract_tool_result(result, tool_name).map(|outcome| outcome.into_prefixed_string())
            }
            Ok(_) => Err(anyhow::anyhow!("Unexpected response type for tool call")),
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
        approver_overrides: Option<ApproverHeaders>,
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
                call_tool_request(request_param, approver_overrides),
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
                let extracted = extract_tool_result(result, tool_name)?.into_prefixed_string();
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
        approver_overrides: Option<ApproverHeaders>,
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
                call_tool_request(request_param, approver_overrides),
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
                            .map(|outcome| outcome.into_prefixed_string())
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
        approver_overrides: Option<ApproverHeaders>,
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
                call_tool_request(request_param, approver_overrides),
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
            crate::agent_events::emit(
                http_request_id,
                AgentEvent::single_agent(AgentEventPayload::ToolStart {
                    tool_id: tool_call_id.clone(),
                    tool_name: tool_name.to_string(),
                    progress_token: progress_token.clone(),
                }),
            )
            .await;
            debug!(
                "Emitted tool_start for tool '{}' (tool_call_id={}, progress_token={:?})",
                tool_name, tool_call_id, progress_token
            );
        } else {
            // Expected in orchestration mode: workers stream via `agent.stream_chat()`
            // without `StreamingRequestHook`, so nothing pushes onto the FIFO queue.
            // Orchestration emits its own `aura.orchestrator.tool_call_*` events via
            // `ObserverWrapper`, so the missing `aura.tool_start` is by design.
            debug!(
                "No tool_call_id in queue for tool '{}' on request '{}' - hook not attached (orchestration) or queue mismatch",
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
                    .map(|outcome| outcome.into_prefixed_string())
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
pub(crate) mod tests {
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::approver_headers::tests::captured_overrides;

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

    /// `post_message` must (a) forward the negotiated `mcp-session-id` header on
    /// every post after `initialize`, and (b) treat a `202 Accepted` /
    /// `204 No Content` response as `Accepted` *without* parsing the (empty)
    /// body — even when the server tags that empty body `application/json`
    /// (FastMCP does).
    ///
    /// Both are regression guards for the `notifications/initialized` post:
    /// dropping the session id makes the server reject it (FastMCP 400, rmcp
    /// 422); parsing the empty 202 body as JSON-RPC fails to deserialize. Either
    /// bug collapses into a generic "channel closed" and the whole server is
    /// recorded as `Failed`, so no tools are available.
    #[tokio::test]
    async fn post_message_forwards_session_id_and_accepts_empty_202() {
        use serde_json::json;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Capture the raw request bytes the server received so the test can
        // assert the session-id header was actually sent on the wire.
        let seen_request = Arc::new(Mutex::new(String::new()));
        let seen_for_server = Arc::clone(&seen_request);

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            *seen_for_server.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
            // FastMCP-style ack: 202 Accepted, application/json content-type,
            // and an empty body.
            sock.write_all(
                b"HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: 0\r\n\r\n",
            )
            .await
            .unwrap();
            let _ = sock.flush().await;
        });

        let client = CustomHttpClient::from_reqwest(reqwest::Client::new());
        let uri: Arc<str> = format!("http://{addr}/mcp").into();
        let message: rmcp::model::ClientJsonRpcMessage = serde_json::from_value(
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .unwrap();
        let session: Arc<str> = "test-session-abc123".into();

        let result = client
            .post_message(uri, message, Some(Arc::clone(&session)), None)
            .await;

        server.await.unwrap();

        // (b) Empty 202 → Accepted, not a deserialize error.
        assert!(
            matches!(
                result,
                Ok(rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Accepted)
            ),
            "empty 202 should yield Accepted, got {result:?}"
        );

        // (a) The session id was forwarded as the mcp-session-id header.
        let request = seen_request.lock().unwrap();
        let lowered = request.to_lowercase();
        assert!(
            lowered.contains("mcp-session-id: test-session-abc123"),
            "post_message must forward the session id header; request was:\n{request}"
        );
    }

    /// One HTTP request the server received, kept whole so a test can ask
    /// both what rode in the headers and what rode in the body.
    #[derive(Clone)]
    pub(crate) struct RecordedRequest {
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl RecordedRequest {
        /// Every value sent under `name`, in arrival order. A test counting values asks whether an override replaced a header or merely joined it.
        pub(crate) fn header_values(&self, name: &str) -> Vec<&str> {
            self.headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
                .collect()
        }

        pub(crate) fn body_text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }

        /// The JSON-RPC method this request carried.
        fn rpc_method(&self) -> Option<String> {
            serde_json::from_slice::<Value>(&self.body)
                .ok()?
                .get("method")?
                .as_str()
                .map(str::to_owned)
        }

        /// The tool this request asked the server to run, for correlating a
        /// recorded request back to the call that produced it.
        pub(crate) fn called_tool(&self) -> Option<String> {
            serde_json::from_slice::<Value>(&self.body)
                .ok()?
                .get("params")?
                .get("name")?
                .as_str()
                .map(str::to_owned)
        }
    }

    /// A loopback MCP server over streamable HTTP that answers the handshake and every tool call, keeping each request it receives. It exists so a test can watch what actually reaches the wire: the approver override is applied deep inside the transport, after rmcp's worker has taken the request value.
    pub(crate) struct RecordingMcpServer {
        pub(crate) url: String,
        received: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl RecordingMcpServer {
        pub(crate) async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/mcp", listener.local_addr().unwrap());
            let received = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&received);
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let sink = Arc::clone(&sink);
                    tokio::spawn(serve_one_request(socket, sink));
                }
            });
            Self { url, received }
        }

        /// Snapshot of every `tools/call` the server received, oldest first.
        pub(crate) fn tool_calls(&self) -> Vec<RecordedRequest> {
            self.received
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.rpc_method().as_deref() == Some("tools/call"))
                .cloned()
                .collect()
        }

        /// The handshake request, which no approval has gated and which must
        /// therefore carry only the identity the client was built with.
        pub(crate) fn initialize(&self) -> RecordedRequest {
            self.received
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.rpc_method().as_deref() == Some("initialize"))
                .cloned()
                .expect("the client completed a handshake")
        }
    }

    /// The session id the handshake hands back, echoed by the client on every
    /// later request.
    const SESSION_ID: &str = "recording-session";

    /// Read one HTTP request off `socket`, record it, answer it, and close. Answering `connection: close` keeps the framing to one request per connection.
    async fn serve_one_request(mut socket: TcpStream, sink: Arc<Mutex<Vec<RecordedRequest>>>) {
        let mut buf = Vec::new();
        let head_end = loop {
            let mut chunk = [0u8; 4096];
            let Ok(n) = socket.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_owned();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < content_length {
            let mut chunk = [0u8; 4096];
            let Ok(n) = socket.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                return;
            }
            body.extend_from_slice(&chunk[..n]);
        }

        let http_method = request_line.split_whitespace().next().unwrap_or_default();
        let (status, extra_headers, payload) = match http_method {
            // rmcp opens a server-to-client stream once it has a session; 405 is the documented "this server has none", which the worker absorbs instead of failing the connection.
            "GET" => ("405 Method Not Allowed", Vec::new(), String::new()),
            "DELETE" => ("202 Accepted", Vec::new(), String::new()),
            _ => reply_to_rpc(&body),
        };

        sink.lock().unwrap().push(RecordedRequest { headers, body });

        let mut response = format!(
            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n",
            payload.len()
        );
        if !payload.is_empty() {
            response.push_str("content-type: application/json\r\n");
        }
        for (name, value) in extra_headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(&payload);
        socket.write_all(response.as_bytes()).await.ok();
        socket.shutdown().await.ok();
    }

    /// Answer a JSON-RPC POST: a result for a request, a bare ack for a
    /// notification.
    fn reply_to_rpc(body: &[u8]) -> (&'static str, Vec<(String, String)>, String) {
        let message = serde_json::from_slice::<Value>(body).expect("client sends valid JSON-RPC");
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(id) = message
            .get("id")
            .cloned()
            .and_then(|id| serde_json::from_value::<RequestId>(id).ok())
        else {
            return ("202 Accepted", Vec::new(), String::new());
        };

        let result = match method.as_str() {
            "initialize" => {
                rmcp::model::ServerResult::InitializeResult(rmcp::model::InitializeResult::default())
            }
            "tools/call" => rmcp::model::ServerResult::CallToolResult(
                rmcp::model::CallToolResult::success(vec![rmcp::model::Content::text("ok")]),
            ),
            other => panic!("unexpected rpc method {other:?}"),
        };
        let payload =
            serde_json::to_string(&rmcp::model::ServerJsonRpcMessage::response(result, id))
                .expect("server result serializes");
        let extra_headers = if method == "initialize" {
            vec![("mcp-session-id".to_owned(), SESSION_ID.to_owned())]
        } else {
            Vec::new()
        };
        ("200 OK", extra_headers, payload)
    }

    /// The identity frozen into the client at build time, standing in for the
    /// original requester's forwarded headers.
    fn requester_headers() -> HashMap<String, String> {
        HashMap::from([("authorization".to_owned(), "Bearer requester".to_owned())])
    }

    /// A running recording server and a client connected to it, frozen with
    /// `headers` as its request-time identity.
    pub(crate) async fn client_and_server(
        headers: &HashMap<String, String>,
    ) -> (RecordingMcpServer, McpClient) {
        let server = RecordingMcpServer::start().await;
        let client = McpClient::new(server.url.clone(), headers)
            .await
            .expect("the loopback server completes the handshake");
        (server, client)
    }

    fn no_args() -> HashMap<String, Value> {
        HashMap::new()
    }

    /// The override rides exactly the call it was captured for: the gated call carries it, and the very next call on the same client (the one no approval unblocked) reverts to the client's own identity.
    #[tokio::test]
    async fn override_rides_one_call_and_no_later_one() {
        let (server, client) = client_and_server(&requester_headers()).await;

        client
            .call_tool(
                "gated",
                no_args(),
                Some(captured_overrides("x-forwarded-user", "alice")),
            )
            .await
            .expect("the gated call succeeds");
        client
            .call_tool("ungated", no_args(), None)
            .await
            .expect("the ungated call succeeds");

        let calls = server.tool_calls();
        assert_eq!(calls.len(), 2, "both calls reached the server");
        assert_eq!(calls[0].header_values("x-forwarded-user"), vec!["alice"]);
        assert!(
            calls[1].header_values("x-forwarded-user").is_empty(),
            "the approver's identity must not persist onto the next call",
        );
    }

    /// The override never becomes part of the message: it rides the request
    /// value as an extension, and the serializer emits no trace of it.
    #[tokio::test]
    async fn override_never_reaches_the_json_body() {
        let (server, client) = client_and_server(&requester_headers()).await;

        client
            .call_tool(
                "gated",
                no_args(),
                Some(captured_overrides("x-forwarded-user", "alice")),
            )
            .await
            .expect("the gated call succeeds");

        let body = server.tool_calls()[0].body_text();
        assert!(!body.contains("x-forwarded-user"), "body was: {body}");
        assert!(!body.contains("alice"), "body was: {body}");
    }

    /// The whole point of the frozen-header problem: the approver's value must
    /// stand in place of the requester's on the gated call, not beside it.
    #[tokio::test]
    async fn override_replaces_the_clients_frozen_identity_for_that_call_only() {
        let (server, client) = client_and_server(&requester_headers()).await;

        client
            .call_tool(
                "gated",
                no_args(),
                Some(captured_overrides("authorization", "Bearer approver")),
            )
            .await
            .expect("the gated call succeeds");
        client
            .call_tool("ungated", no_args(), None)
            .await
            .expect("the ungated call succeeds");

        let calls = server.tool_calls();
        assert_eq!(
            calls[0].header_values("authorization"),
            vec!["Bearer approver"],
            "the requester's identity must be replaced, not joined",
        );
        assert_eq!(
            calls[1].header_values("authorization"),
            vec!["Bearer requester"],
        );
        assert_eq!(
            server.initialize().header_values("authorization"),
            vec!["Bearer requester"],
            "the handshake predates any approval",
        );
    }

    /// Two gated calls in flight on one client keep their own identities. Scoping is structural; each override rides its own request value. This test catches a regression to shared state.
    #[tokio::test]
    async fn concurrent_gated_calls_keep_their_own_identity() {
        let (server, client) = client_and_server(&requester_headers()).await;

        let (first, second) = tokio::join!(
            client.call_tool(
                "for_alice",
                no_args(),
                Some(captured_overrides("x-forwarded-user", "alice")),
            ),
            client.call_tool(
                "for_bob",
                no_args(),
                Some(captured_overrides("x-forwarded-user", "bob")),
            ),
        );
        first.expect("alice's call succeeds");
        second.expect("bob's call succeeds");

        let calls = server.tool_calls();
        assert_eq!(calls.len(), 2);
        for call in calls {
            let expected = match call.called_tool().as_deref() {
                Some("for_alice") => "alice",
                Some("for_bob") => "bob",
                other => panic!("unexpected tool call {other:?}"),
            };
            assert_eq!(call.header_values("x-forwarded-user"), vec![expected]);
        }
    }

    /// `set_current_request` selects the tracked branch, so this is the same entry point a gated call takes in the server and the branch choice must not decide whether identity is delivered.
    #[tokio::test]
    async fn call_tool_delivers_the_override_on_either_branch() {
        let (server, client) = client_and_server(&requester_headers()).await;

        client
            .call_tool(
                "untracked",
                no_args(),
                Some(captured_overrides("x-forwarded-user", "alice")),
            )
            .await
            .expect("the untracked call succeeds");

        client.set_current_request("http-req-1").await;
        client
            .call_tool(
                "tracked",
                no_args(),
                Some(captured_overrides("x-forwarded-user", "bob")),
            )
            .await
            .expect("the tracked call succeeds");

        let calls = server.tool_calls();
        assert_eq!(calls[0].header_values("x-forwarded-user"), vec!["alice"]);
        assert_eq!(calls[1].header_values("x-forwarded-user"), vec!["bob"]);
    }
}
