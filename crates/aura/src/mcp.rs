use crate::{config::McpServerConfig, error::BuilderError, mcp_streamable_http::McpClient};
use futures::{StreamExt, stream::BoxStream};
use rig::{completion::ToolDefinition, tool::Tool as RigTool};
use rmcp::transport::streamable_http_client::StreamableHttpClient;
use serde_json::Value;
use sse_stream::{Sse, SseStream};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Simple error type for tool execution
#[derive(Debug)]
pub struct ToolExecutionError {
    message: String,
}

impl std::fmt::Display for ToolExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tool execution error: {}", self.message)
    }
}

impl std::error::Error for ToolExecutionError {}

impl From<String> for ToolExecutionError {
    fn from(message: String) -> Self {
        Self { message }
    }
}

impl From<&str> for ToolExecutionError {
    fn from(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

/// Custom HTTP client that captures the underlying HTTP status when a request
/// fails.
///
/// rmcp's streamable-HTTP transport runs in a background worker: when a request
/// fails (e.g. 404/401), the worker logs the `reqwest` error and closes the
/// channel, so `serve_client` only sees "channel closed" — the status code is
/// lost. By implementing `StreamableHttpClient` ourselves we record the status
/// into `first_error` *at the layer it occurs*, then read it back after the
/// connection fails to produce a precise reason.
#[derive(Clone, Default)]
pub struct CustomHttpClient {
    client: reqwest::Client,
    /// First failing HTTP status observed on this client, if any. Shared across
    /// clones (the transport worker clones the client) via `Arc`.
    first_error: Arc<Mutex<Option<String>>>,
}

impl CustomHttpClient {
    /// Wrap an existing `reqwest::Client` (already carrying any forwarded
    /// headers, including auth) so transport HTTP errors can be captured.
    pub fn from_reqwest(client: reqwest::Client) -> Self {
        Self {
            client,
            first_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Shared handle to the first captured failing HTTP status, if any.
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

        let response = request_builder
            .json(&message)
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

/// MCP client for managing connections to MCP servers
pub struct McpManager {
    pub server_info: HashMap<String, ServerInfo>,
    /// Store streamable HTTP clients for http_streamable transport
    pub streamable_clients: HashMap<String, McpClient>,
    pub streamable_tools: HashMap<String, Vec<rmcp::model::Tool>>,
    /// Store SSE clients for sse transport
    pub sse_clients: HashMap<String, McpClient>,
    pub sse_tools: HashMap<String, Vec<rmcp::model::Tool>>,
    /// Store STDIO clients for stdio transport
    pub stdio_clients: HashMap<String, McpClient>,
    pub stdio_tools: HashMap<String, Vec<rmcp::model::Tool>>,
    /// Whether to sanitize tool schemas for OpenAI compatibility
    pub sanitize_schemas: bool,
}

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub description: Option<String>,
    pub tools_count: usize,
    pub status: ConnectionStatus,
    /// Transport kind for this server: `"http_streamable"`, `"sse"`, or `"stdio"`.
    pub transport: String,
}

#[derive(Debug, Clone)]
pub enum ConnectionStatus {
    Connected,
    Failed(String),
    NotAttempted,
}

impl McpManager {
    pub fn new() -> Self {
        Self::with_sanitization(true)
    }

    pub fn with_sanitization(sanitize_schemas: bool) -> Self {
        Self {
            server_info: HashMap::new(),
            streamable_clients: HashMap::new(),
            streamable_tools: HashMap::new(),
            sse_clients: HashMap::new(),
            sse_tools: HashMap::new(),
            stdio_clients: HashMap::new(),
            stdio_tools: HashMap::new(),
            sanitize_schemas,
        }
    }

    /// Initialize MCP connections and discover tools from all configured servers
    pub async fn initialize_from_config(
        mcp_config: &crate::config::McpConfig,
    ) -> Result<Self, BuilderError> {
        let mut manager = Self::with_sanitization(mcp_config.sanitize_schemas);

        info!(
            "Initializing MCP servers ({} configured)",
            mcp_config.servers.len()
        );
        if mcp_config.sanitize_schemas {
            info!("Schema sanitization: ENABLED (OpenAI compatibility)");
        } else {
            info!("Schema sanitization: DISABLED (raw MCP schemas)");
        }

        for (server_name, server_config) in &mcp_config.servers {
            info!("Connecting to MCP server: {}", server_name);

            let transport = Self::transport_label(server_config).to_string();
            match manager
                .connect_and_discover_tools(server_name, server_config)
                .await
            {
                Ok(tools_count) => {
                    manager.server_info.insert(
                        server_name.clone(),
                        ServerInfo {
                            name: server_name.clone(),
                            description: manager.get_server_description(server_config),
                            tools_count,
                            status: ConnectionStatus::Connected,
                            transport,
                        },
                    );
                    info!(
                        "{} - Connected successfully, {} tools discovered",
                        server_name, tools_count
                    );
                }
                Err(e) => {
                    let error_msg = format!("Connection failed: {e}");
                    manager.server_info.insert(
                        server_name.clone(),
                        ServerInfo {
                            name: server_name.clone(),
                            description: manager.get_server_description(server_config),
                            tools_count: 0,
                            status: ConnectionStatus::Failed(error_msg.clone()),
                            transport,
                        },
                    );
                    warn!("{} - {}", server_name, error_msg);
                }
            }
        }

        // Count ALL tools across all transport types
        let stdio_tools: usize = manager.stdio_tools.values().map(|v| v.len()).sum();
        let streamable_tools: usize = manager.streamable_tools.values().map(|v| v.len()).sum();
        let sse_tools: usize = manager.sse_tools.values().map(|v| v.len()).sum();
        let total_tools = stdio_tools + streamable_tools + sse_tools;
        let successful_connections = manager
            .server_info
            .values()
            .filter(|info| matches!(info.status, ConnectionStatus::Connected))
            .count();
        let failed_connections = manager
            .server_info
            .values()
            .filter(|info| matches!(info.status, ConnectionStatus::Failed(_)))
            .count();

        info!("MCP initialization complete:");
        info!("  - Total tools available: {}", total_tools);
        info!(
            "  - Successful connections: {}/{}",
            successful_connections,
            mcp_config.servers.len()
        );
        if failed_connections > 0 {
            warn!("  - Failed connections: {}", failed_connections);
        }

        Ok(manager)
    }

    /// Connect to a single MCP server and discover its tools
    async fn connect_and_discover_tools(
        &mut self,
        server_name: &str,
        server_config: &McpServerConfig,
    ) -> Result<usize, BuilderError> {
        match server_config {
            McpServerConfig::HttpStreamable { url, headers, .. } => {
                self.connect_http_streamable(server_name, url, headers)
                    .await
            }
            McpServerConfig::Sse { url, headers, .. } => {
                self.connect_sse(server_name, url, headers).await
            }
            McpServerConfig::Stdio { cmd, args, env, .. } => {
                self.connect_stdio(server_name, cmd, args, env).await
            }
        }
    }

    /// Connect to HTTP streamable MCP server
    async fn connect_http_streamable(
        &mut self,
        server_name: &str,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<usize, BuilderError> {
        info!("  Connecting to HTTP streamable server at: {}", url);

        match self
            .try_connect_http_streamable(server_name, url, headers)
            .await
        {
            Ok(tools_count) => {
                info!("  HTTP Streamable connection successful");
                Ok(tools_count)
            }
            Err(e) => {
                // Auth failures get a clearer, actionable message. Every other
                // failure (connection refused, timeout, closed transport,
                // unexpected content type, non-401 HTTP status, tool-discovery
                // errors) bubbles up as an error so the server is recorded as
                // `Failed`. A genuine empty server is represented by `Ok(0)`
                // only after `discover_tools()` succeeds with an empty tool list.
                if e.to_string().contains("401 Unauthorized")
                    || e.to_string().contains("HTTP status client error (401")
                {
                    Err(BuilderError::McpInitError(format!(
                        "HTTP MCP server '{server_name}' authentication failed (401 Unauthorized). Check that your headers, forwarded headers, and/or credentials are correct."
                    )))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Attempt to connect to HTTP Streamable MCP server using McpClient
    async fn try_connect_http_streamable(
        &mut self,
        server_name: &str,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<usize, BuilderError> {
        debug!("  Creating HTTP Streamable client for: {}", url);
        debug!("  Headers to be applied: {:?}", headers.keys());

        if !headers.is_empty() {
            info!("  Forwarding {} headers to MCP client", headers.len());
        }

        // Use McpClient. Render with `{e:#}` so anyhow's full cause chain (e.g.
        // the captured HTTP status → transport error) is included, not just the
        // outermost context.
        let client = McpClient::new(url.to_string(), headers)
            .await
            .map_err(|e| {
                BuilderError::McpInitError(format!(
                    "Failed to connect to HTTP MCP server '{server_name}': {e:#}"
                ))
            })?;

        info!(
            "  Successfully connected to HTTP streamable server '{}'",
            server_name
        );

        // Discover available tools using the new client API
        info!("  🔍 Discovering tools from server '{}'...", server_name);
        let tools = client.discover_tools().await.map_err(|e| {
            BuilderError::McpInitError(format!(
                "Failed to discover tools from server '{server_name}': {e:#}"
            ))
        })?;

        info!(
            "  Discovered {} tools from HTTP streamable server '{}'",
            tools.len(),
            server_name
        );

        // Sanitize tools at build time (instead of per-request)
        let sanitized_tools =
            Self::sanitize_and_collect_tools(tools, self.sanitize_schemas, "HTTP Streamable");

        // Store the client for later use in tool execution
        self.streamable_clients
            .insert(server_name.to_string(), client);

        // Store SANITIZED tools (not raw)
        self.streamable_tools
            .insert(server_name.to_string(), sanitized_tools.clone());

        Ok(sanitized_tools.len())
    }

    /// Connect to legacy SSE MCP server
    async fn connect_sse(
        &mut self,
        server_name: &str,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<usize, BuilderError> {
        info!("  Connecting to SSE server at: {}", url);

        match self.try_connect_sse(server_name, url, headers).await {
            Ok(tools_count) => {
                info!("  SSE connection successful");
                Ok(tools_count)
            }
            Err(e) => {
                if e.to_string().contains("401 Unauthorized")
                    || e.to_string().contains("HTTP status client error (401")
                {
                    Err(BuilderError::McpInitError(format!(
                        "SSE MCP server '{server_name}' authentication failed (401 Unauthorized). Check that your headers, forwarded headers, and/or credentials are correct."
                    )))
                } else {
                    // Bubble the failure so the server is recorded as `Failed`.
                    Err(e)
                }
            }
        }
    }

    /// Attempt to connect to SSE MCP server
    async fn try_connect_sse(
        &mut self,
        server_name: &str,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<usize, BuilderError> {
        debug!("  Creating SSE client for: {}", url);

        let transport = crate::mcp_sse::SseTransport::connect(url, headers)
            .await
            .map_err(BuilderError::SseTransport)?;

        let client = McpClient::from_transport(transport, url.to_string())
            .await
            .map_err(|e| {
                BuilderError::McpInitError(format!(
                    "Failed to establish SSE MCP connection to '{server_name}': {e:#}"
                ))
            })?;

        info!("  SSE connection established, discovering tools");

        let tools = client.discover_tools().await.map_err(|e| {
            BuilderError::McpInitError(format!(
                "Failed to discover tools from SSE server '{server_name}': {e:#}"
            ))
        })?;

        info!(
            "  Discovered {} tools from SSE server '{}'",
            tools.len(),
            server_name
        );

        let sanitized_tools = Self::sanitize_and_collect_tools(tools, self.sanitize_schemas, "SSE");

        self.sse_clients.insert(server_name.to_string(), client);
        self.sse_tools
            .insert(server_name.to_string(), sanitized_tools.clone());

        Ok(sanitized_tools.len())
    }

    /// Connect to STDIO MCP server
    async fn connect_stdio(
        &mut self,
        server_name: &str,
        cmd: &[String],
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<usize, BuilderError> {
        info!("  Spawning STDIO server: {:?} {:?}", cmd, args);

        // This is more likely to work as rmcp has good STDIO support
        match self.try_connect_stdio(server_name, cmd, args, env).await {
            Ok(tools_count) => {
                info!("  STDIO connection successful");
                Ok(tools_count)
            }
            Err(e) => {
                warn!("  STDIO MCP connection failed: {}", e);
                // Bubble the failure so the server is recorded as `Failed`.
                Err(e)
            }
        }
    }

    /// Attempt to connect to STDIO MCP server using rmcp
    async fn try_connect_stdio(
        &mut self,
        server_name: &str,
        cmd: &[String],
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<usize, BuilderError> {
        use rmcp::transport::TokioChildProcess;
        use tokio::process::Command;

        // Build the command
        if cmd.is_empty() {
            return Err(BuilderError::McpInitError(
                "Empty command for STDIO server".to_owned(),
            ));
        }

        let mut process = Command::new(&cmd[0]);

        process.args(&cmd[1..]);
        process.args(args);
        process.envs(env);

        debug!("  Spawning process: {:?}", process);

        // TokioChildProcess::new defaults stderr to Stdio::inherit(), which
        // leaks MCP server debug output (often raw JSON-RPC frames) to the
        // host terminal. Pipe stderr to null instead.
        let (transport, _stderr) = TokioChildProcess::builder(process)
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                BuilderError::McpInitError(format!("Failed to spawn MCP server process: {e}"))
            })?;

        let client = McpClient::from_transport(transport, format!("stdio://{server_name}"))
            .await
            .map_err(|e| {
                BuilderError::McpInitError(format!(
                    "Failed to establish STDIO MCP connection to '{server_name}': {e}"
                ))
            })?;

        info!("  STDIO connection established, discovering tools");

        let tools = client.discover_tools().await.map_err(|e| {
            BuilderError::McpInitError(format!(
                "Failed to discover tools from STDIO server '{server_name}': {e}"
            ))
        })?;

        info!(
            "  Discovered {} tools from STDIO server '{}'",
            tools.len(),
            server_name
        );

        let sanitized_tools =
            Self::sanitize_and_collect_tools(tools, self.sanitize_schemas, "STDIO");

        let tools_count = sanitized_tools.len();
        let owned_name = server_name.to_string();
        self.stdio_clients.insert(owned_name.clone(), client);
        self.stdio_tools.insert(owned_name, sanitized_tools);

        Ok(tools_count)
    }

    /// Sanitize an MCP tool for LLM compatibility (shared by all transports)
    ///
    /// 1. ALWAYS sanitizes tool name (general LLM requirement)
    /// 2. Conditionally sanitizes schema (OpenAI-specific, if flag enabled)
    /// 3. Returns Ok(sanitized_tool) or Err(rejection_reason)
    fn sanitize_mcp_tool(
        mut tool: rmcp::model::Tool,
        sanitize_schemas: bool,
    ) -> Result<rmcp::model::Tool, String> {
        // ALWAYS sanitize tool name (general LLM requirement)
        let original_name = tool.name.to_string();
        let sanitized_name = Self::sanitize_tool_name(&original_name);
        tool.name = sanitized_name.into();

        // Conditionally sanitize schema (OpenAI-specific)
        if sanitize_schemas {
            let mut schema_value = serde_json::Value::Object((*tool.input_schema).clone());

            // Validate and sanitize the schema
            Self::sanitize_schema_for_openai(&mut schema_value)?;

            // Update tool with sanitized schema
            if let serde_json::Value::Object(sanitized_map) = schema_value {
                tool.input_schema = std::sync::Arc::new(sanitized_map);
            }
        }

        Ok(tool)
    }

    /// Sanitize MCP tool name for general LLM compatibility
    ///
    /// Tool names must be alphanumeric with underscores/hyphens only, max 64 chars.
    /// This is a general requirement for most LLM providers, not specific to OpenAI.
    ///
    /// Always applied regardless of sanitize_schemas flag.
    fn sanitize_tool_name(tool_name: &str) -> String {
        // Replace spaces and invalid characters with underscores
        let sanitized_name = tool_name
            .replace(' ', "_")
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .trim_start_matches('_')
            .trim_end_matches('_')
            .to_string();

        // Ensure name isn't empty and doesn't start with number
        let mut final_name = if sanitized_name.is_empty()
            || sanitized_name.chars().next().unwrap().is_ascii_digit()
        {
            format!("tool_{sanitized_name}")
        } else {
            sanitized_name
        };

        // Most LLM providers have a 64 character limit on function names
        if final_name.len() > 64 {
            final_name.truncate(64);
        }

        if tool_name != final_name {
            debug!("Sanitized tool name '{}' -> '{}'", tool_name, final_name);
        }

        final_name
    }

    /// Sanitize MCP tool schema for OpenAI compatibility
    ///
    /// OpenAI-specific schema transformations:
    /// - Validates root schema is type: "object" (rejects type definitions)
    /// - Fixes incomplete required arrays (makes optional fields nullable)
    /// - Adds additionalProperties: false everywhere (strict mode requirement)
    ///
    /// Only applied when sanitize_schemas flag is true.
    ///
    /// Returns Ok(()) if successful, or Err(reason) if schema is invalid and should be rejected.
    fn sanitize_schema_for_openai(schema: &mut serde_json::Value) -> Result<(), String> {
        use crate::schema_sanitize::{
            fix_empty_root_required, recursive_set_additional_properties_false,
        };

        // VALIDATION: OpenAI requires tool schemas to have type: "object" at root level
        // MCP servers sometimes incorrectly return type definitions (e.g., type: "string") as tools
        // These are not valid tools and must be rejected
        if let Some(schema_type) = schema.get("type").and_then(|t| t.as_str())
            && schema_type != "object"
        {
            return Err(format!(
                "Invalid MCP tool schema: root must be type 'object', got '{schema_type}'. \
                    This is an MCP server bug - the server is returning a type definition as a tool."
            ));
        }

        // Step 1: Fix incomplete required arrays (makes optional fields nullable)
        fix_empty_root_required(schema);

        // Step 2: Add additionalProperties: false everywhere (required by OpenAI strict mode)
        recursive_set_additional_properties_false(schema);

        Ok(())
    }

    /// Centralized MCP tool → Rig ToolDefinition conversion.
    /// Always sanitizes tool names; conditionally sanitizes schemas for OpenAI.
    /// Returns `None` for tools with invalid schemas.
    pub fn convert_tool_to_rig_definition(
        mcp_tool: &rmcp::model::Tool,
        sanitize_schemas: bool,
    ) -> Option<ToolDefinition> {
        let original_name = mcp_tool.name.to_string();
        let mut tool_schema = serde_json::Value::Object((*mcp_tool.input_schema).clone());

        // ALWAYS sanitize tool name (general LLM requirement)
        let tool_name = Self::sanitize_tool_name(&original_name);

        // Conditionally sanitize schema (OpenAI-specific)
        if sanitize_schemas {
            info!(
                "    🧽 Sanitizing schema for tool '{}' for OpenAI compatibility",
                tool_name
            );

            match Self::sanitize_schema_for_openai(&mut tool_schema) {
                Ok(()) => {
                    debug!("    Schema sanitization successful for '{}'", tool_name);
                }
                Err(reason) => {
                    warn!("Rejecting invalid MCP tool '{}': {}", original_name, reason);
                    return None;
                }
            }
        } else {
            debug!("    Using raw MCP schema for tool '{}'", tool_name);
        }

        Some(ToolDefinition {
            name: tool_name,
            description: mcp_tool
                .description
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            parameters: tool_schema,
        })
    }

    /// Sanitize and collect tools for a given transport type
    ///
    /// Filters out invalid tools and logs rejections for debugging.
    ///
    fn sanitize_and_collect_tools(
        tools: Vec<rmcp::model::Tool>,
        sanitize_schemas: bool,
        transport_name: &str,
    ) -> Vec<rmcp::model::Tool> {
        tools
            .into_iter()
            .filter_map(|tool| {
                let tool_name = tool.name.to_string();
                match Self::sanitize_mcp_tool(tool, sanitize_schemas) {
                    Ok(sanitized) => {
                        debug!("{} tool '{}' sanitized", transport_name, sanitized.name);
                        Some(sanitized)
                    }
                    Err(reason) => {
                        warn!(
                            "Rejecting invalid {} tool '{}': {}",
                            transport_name, tool_name, reason
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// Get description from server config
    fn get_server_description(&self, server_config: &McpServerConfig) -> Option<String> {
        match server_config {
            McpServerConfig::HttpStreamable { description, .. }
            | McpServerConfig::Sse { description, .. }
            | McpServerConfig::Stdio { description, .. } => description.clone(),
        }
    }

    /// Stable transport label for a server config (used in status reporting).
    fn transport_label(server_config: &McpServerConfig) -> &'static str {
        match server_config {
            McpServerConfig::HttpStreamable { .. } => "http_streamable",
            McpServerConfig::Sse { .. } => "sse",
            McpServerConfig::Stdio { .. } => "stdio",
        }
    }

    /// Project the per-server connection state into wire-friendly status
    /// records for the `aura.mcp_status` SSE event.
    ///
    /// This is a thin projection of `server_info` — the same state that drives
    /// `log_summary()` — so degraded servers surface to the user with the same
    /// reason string recorded at connection time. Sorted by server name for
    /// deterministic output (the underlying map is unordered).
    pub fn server_status_snapshot(&self) -> Vec<aura_events::McpServerStatus> {
        let mut statuses: Vec<aura_events::McpServerStatus> = self
            .server_info
            .values()
            .map(|info| {
                let (status, reason) = match &info.status {
                    ConnectionStatus::Connected => ("connected", None),
                    ConnectionStatus::Failed(reason) => ("failed", Some(reason.clone())),
                    ConnectionStatus::NotAttempted => ("not_attempted", None),
                };
                aura_events::McpServerStatus {
                    server_name: info.name.clone(),
                    transport: info.transport.clone(),
                    status: status.to_string(),
                    tools_count: info.tools_count,
                    reason,
                }
            })
            .collect();
        statuses.sort_by(|a, b| a.server_name.cmp(&b.server_name));
        statuses
    }

    /// Log the summary of MCP connections and tools
    pub fn log_summary(&self) {
        info!("📊 MCP Manager Summary:");
        for (name, info) in &self.server_info {
            match &info.status {
                ConnectionStatus::Connected => {
                    info!("  {} - {} tools", name, info.tools_count);
                }
                ConnectionStatus::Failed(error) => {
                    warn!("  ❌ {} - {}", name, error);
                }
                ConnectionStatus::NotAttempted => {
                    info!("  {} - not attempted", name);
                }
            }
        }
        // Count ALL tools across all transport types
        let total_tools = self
            .streamable_tools
            .values()
            .map(|v| v.len())
            .sum::<usize>()
            + self.sse_tools.values().map(|v| v.len()).sum::<usize>()
            + self.stdio_tools.values().map(|v| v.len()).sum::<usize>();
        info!("  Total tools available: {}", total_tools);
    }

    /// Cancel all in-flight MCP requests for an HTTP request.
    pub async fn cancel_all_for_request(&self, http_request_id: &str, reason: &str) -> usize {
        let mut total_cancelled = 0;

        for (server_name, client) in &self.streamable_clients {
            let cancelled = client.cancel_all_for_request(http_request_id, reason).await;
            if cancelled > 0 {
                info!(
                    "Cancelled {} request(s) on MCP server '{}' for HTTP request {}",
                    cancelled, server_name, http_request_id
                );
            }
            total_cancelled += cancelled;
        }

        for (server_name, client) in &self.sse_clients {
            let cancelled = client.cancel_all_for_request(http_request_id, reason).await;
            if cancelled > 0 {
                info!(
                    "Cancelled {} request(s) on SSE MCP server '{}' for HTTP request {}",
                    cancelled, server_name, http_request_id
                );
            }
            total_cancelled += cancelled;
        }

        for (server_name, client) in &self.stdio_clients {
            let cancelled = client.cancel_all_for_request(http_request_id, reason).await;
            if cancelled > 0 {
                info!(
                    "Cancelled {} request(s) on STDIO MCP server '{}' for HTTP request {}",
                    cancelled, server_name, http_request_id
                );
            }
            total_cancelled += cancelled;
        }

        total_cancelled
    }

    /// Cancel in-flight requests and close all MCP client connections.
    /// After calling this, all MCP clients become unusable until reinitialized.
    pub async fn cancel_and_close_all(&self, http_request_id: &str, reason: &str) -> usize {
        let mut total_cancelled = 0;

        for (server_name, client) in &self.streamable_clients {
            let cancelled = client.cancel_and_close(http_request_id, reason).await;
            if cancelled > 0 {
                info!(
                    "Cancelled {} request(s) and closed MCP server '{}' for HTTP request {}",
                    cancelled, server_name, http_request_id
                );
            }
            total_cancelled += cancelled;
        }

        for (server_name, client) in &self.sse_clients {
            let cancelled = client.cancel_and_close(http_request_id, reason).await;
            if cancelled > 0 {
                info!(
                    "Cancelled {} request(s) and closed SSE MCP server '{}' for HTTP request {}",
                    cancelled, server_name, http_request_id
                );
            }
            total_cancelled += cancelled;
        }

        for (server_name, client) in &self.stdio_clients {
            let cancelled = client.cancel_and_close(http_request_id, reason).await;
            if cancelled > 0 {
                info!(
                    "Cancelled {} request(s) and closed STDIO MCP server '{}' for HTTP request {}",
                    cancelled, server_name, http_request_id
                );
            }
            total_cancelled += cancelled;
        }

        total_cancelled
    }

    /// Set the current HTTP request ID for cancellation tracking.
    pub async fn set_current_request(&self, http_request_id: &str) {
        for client in self.streamable_clients.values() {
            client.set_current_request(http_request_id).await;
        }
        for client in self.sse_clients.values() {
            client.set_current_request(http_request_id).await;
        }
        for client in self.stdio_clients.values() {
            client.set_current_request(http_request_id).await;
        }
        let total_clients =
            self.streamable_clients.len() + self.sse_clients.len() + self.stdio_clients.len();
        debug!(
            "Set current HTTP request ID on {} MCP client(s): {}",
            total_clients, http_request_id
        );
    }

    pub async fn clear_current_request(&self) {
        for client in self.streamable_clients.values() {
            client.clear_current_request().await;
        }
        for client in self.sse_clients.values() {
            client.clear_current_request().await;
        }
        for client in self.stdio_clients.values() {
            client.clear_current_request().await;
        }
        let total_clients =
            self.streamable_clients.len() + self.sse_clients.len() + self.stdio_clients.len();
        debug!(
            "Cleared current HTTP request ID on {} MCP client(s)",
            total_clients
        );
    }

    /// Get all available tool names across all transports.
    ///
    /// Returns a list of tool names that can be used for fallback tool execution.
    pub fn get_available_tool_names(&self) -> Vec<String> {
        let mut names = Vec::new();

        // HTTP Streamable tools
        for tools in self.streamable_tools.values() {
            for tool in tools {
                names.push(tool.name.to_string());
            }
        }

        // SSE tools
        for tools in self.sse_tools.values() {
            for tool in tools {
                names.push(tool.name.to_string());
            }
        }

        // STDIO tools
        for tools in self.stdio_tools.values() {
            for tool in tools {
                names.push(tool.name.to_string());
            }
        }

        names
    }

    /// Iterate over every discovered MCP tool across both HTTP-streamable and
    /// STDIO transports.
    ///
    /// Used by the scratchpad budget seed to BPE-count the actual JSON
    /// schemas the LLM sees in its tool list, instead of falling back to a
    /// per-tool constant heuristic.
    pub fn tool_definitions_iter(&self) -> impl Iterator<Item = &rmcp::model::Tool> {
        self.streamable_tools
            .values()
            .flat_map(|tools| tools.iter())
            .chain(self.sse_tools.values().flat_map(|tools| tools.iter()))
            .chain(self.stdio_tools.values().flat_map(|tools| tools.iter()))
    }

    /// Returns a `server_name → tool_names` map for all transports.
    ///
    /// Used by the scratchpad layer to resolve per-server `min_tokens`
    /// patterns to concrete tool names at boot time, so the runtime
    /// interception lookup is a server-aware exact match (not a
    /// server-agnostic glob — see `scratchpad::scratchpad_tool_map`).
    pub fn tool_names_per_server(&self) -> HashMap<String, Vec<String>> {
        let mut map: HashMap<String, Vec<String>> = self
            .streamable_tools
            .iter()
            .map(|(server_name, tools)| {
                let names = tools.iter().map(|t| t.name.to_string()).collect();
                (server_name.clone(), names)
            })
            .collect();
        for (server_name, tools) in &self.sse_tools {
            let names = tools.iter().map(|t| t.name.to_string()).collect();
            map.insert(server_name.clone(), names);
        }
        for (server_name, tools) in &self.stdio_tools {
            let names = tools.iter().map(|t| t.name.to_string()).collect();
            map.insert(server_name.clone(), names);
        }
        map
    }

    /// Execute a tool by name (used by Ollama text-to-tool fallback).
    ///
    /// Called by `FallbackToolExecutor` when it detects tool calls in streamed text.
    /// Routes to the appropriate MCP transport (HTTP Streamable, SSE, or STDIO).
    ///
    /// Normal Rig tool execution goes through `Tool::call()` trait implementations;
    /// this method exists specifically for the fallback parsing path.
    pub async fn execute_fallback_tool(
        &self,
        tool_name: &str,
        arguments: &str,
    ) -> Result<String, String> {
        // Parse arguments as JSON
        let args: Value = serde_json::from_str(arguments)
            .map_err(|e| format!("Failed to parse arguments: {}", e))?;

        let args_map = match args {
            Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
            _ => HashMap::new(),
        };

        // Try HTTP Streamable clients first
        for (server_name, client) in &self.streamable_clients {
            if let Some(tools) = self.streamable_tools.get(server_name)
                && tools.iter().any(|t| t.name.as_ref() == tool_name)
            {
                info!(
                    "Executing fallback tool '{}' via HTTP Streamable",
                    tool_name
                );
                return client
                    .call_tool(tool_name, args_map)
                    .await
                    .map_err(|e| format!("Tool execution failed: {}", e));
            }
        }

        // Try SSE clients
        for (server_name, client) in &self.sse_clients {
            if let Some(tools) = self.sse_tools.get(server_name)
                && tools.iter().any(|t| t.name.as_ref() == tool_name)
            {
                info!("Executing fallback tool '{}' via SSE", tool_name);
                return client
                    .call_tool(tool_name, args_map)
                    .await
                    .map_err(|e| format!("Tool execution failed: {}", e));
            }
        }

        // Try STDIO clients
        for (server_name, client) in &self.stdio_clients {
            if let Some(tools) = self.stdio_tools.get(server_name)
                && tools.iter().any(|t| t.name.as_ref() == tool_name)
            {
                info!("Executing fallback tool '{}' via STDIO", tool_name);
                return client
                    .call_tool(tool_name, args_map)
                    .await
                    .map_err(|e| format!("Tool execution failed: {}", e));
            }
        }

        Err(format!("Tool '{}' not found", tool_name))
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Macro to create unique MCP tool structs for each tool name
/// This is required because Rig's Tool trait requires each tool to have a unique static NAME
macro_rules! create_mcp_tool_struct {
    ($struct_name:ident, $tool_name:literal) => {
        #[derive(Clone)]
        pub struct $struct_name {
            pub tool_name: String,
            pub server_name: String,
            pub client: Arc<McpClient>,
            pub tool_definition: ToolDefinition,
        }

        impl $struct_name {
            pub fn new(
                tool_name: String,
                server_name: String,
                client: Arc<McpClient>,
                mcp_tool: rmcp::model::Tool,
                sanitize_schemas: bool,
            ) -> Option<Self> {
                // Use centralized conversion method
                let tool_definition =
                    McpManager::convert_tool_to_rig_definition(&mcp_tool, sanitize_schemas)?;
                Some(Self {
                    tool_name,
                    server_name,
                    client,
                    tool_definition,
                })
            }
        }

        impl RigTool for $struct_name {
            const NAME: &'static str = $tool_name;

            type Error = ToolExecutionError;
            type Args = Value;
            type Output = String;

            async fn definition(&self, _prompt: String) -> ToolDefinition {
                self.tool_definition.clone()
            }

            async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
                debug!(
                    "{}::call - Tool: {}, Server: {}",
                    stringify!($struct_name),
                    self.tool_name,
                    self.server_name
                );
                info!(
                    "Calling tool '{}' on server '{}'",
                    self.tool_name, self.server_name
                );

                let arguments = match args {
                    Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
                    _ => {
                        return Err(ToolExecutionError::from(
                            "Invalid arguments: expected JSON object",
                        ));
                    }
                };

                debug!("  Arguments: {:?}", arguments);

                match self.client.call_tool(&self.tool_name, arguments).await {
                    Ok(response) => {
                        debug!("  Response: {}", response);
                        let response_summary = if response.len() > 200 {
                            format!("{}... ({} chars)", &response[..200], response.len())
                        } else {
                            response.clone()
                        };
                        info!("Tool '{}' completed: {}", self.tool_name, response_summary);
                        Ok(response)
                    }
                    Err(e) => {
                        let error_msg = format!("MCP tool call failed: {}", e);
                        info!("❌ Tool '{}' failed: {}", self.tool_name, e);
                        Err(ToolExecutionError::from(error_msg))
                    }
                }
            }
        }
    };
}

// Create unique tool structs for each known Mezmo tool
create_mcp_tool_struct!(ExportLogsRelativeTimeTool, "export_logs_relative_time");
create_mcp_tool_struct!(ExportLogsTimeRangeTool, "export_logs_time_range");
create_mcp_tool_struct!(
    AnalyzeLogsRelativeTimeTool,
    "analyze_logs_for_root_cause_relative_time"
);
create_mcp_tool_struct!(
    AnalyzeLogsTimeRangeTool,
    "analyze_logs_for_root_cause_time_range"
);
create_mcp_tool_struct!(GetCurrentTimeTool, "get_current_time");
create_mcp_tool_struct!(GetPipelineTool, "get_pipeline");
create_mcp_tool_struct!(ListPipelinesTool, "list_pipelines");

/// Generic fallback for unknown tools - A Rig-compatible tool wrapper for MCP client tools
#[derive(Clone)]
pub struct StreamableHttpMcpTool {
    pub tool_name: String,
    pub server_name: String,
    pub client: Arc<McpClient>,
    pub tool_definition: ToolDefinition,
}

impl StreamableHttpMcpTool {
    pub fn new(
        tool_name: String,
        server_name: String,
        client: Arc<McpClient>,
        mcp_tool: rmcp::model::Tool,
        sanitize_schemas: bool,
    ) -> Option<Self> {
        // Use centralized conversion method from McpManager
        let tool_definition =
            McpManager::convert_tool_to_rig_definition(&mcp_tool, sanitize_schemas)?;

        Some(Self {
            tool_name,
            server_name,
            client,
            tool_definition,
        })
    }
}

impl RigTool for StreamableHttpMcpTool {
    const NAME: &'static str = "streamable_http_mcp_tool";

    type Error = ToolExecutionError;
    type Args = Value;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        self.tool_definition.clone()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        debug!(
            "StreamableHttpMcpTool::call - Tool: {}, Server: {}",
            self.tool_name, self.server_name
        );
        info!(
            "Calling HTTP streamable tool '{}' on server '{}'",
            self.tool_name, self.server_name
        );

        // Convert Value to HashMap<String, Value> for the streamable client
        let arguments = match args {
            Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
            _ => {
                return Err(ToolExecutionError::from(
                    "Invalid arguments: expected JSON object",
                ));
            }
        };

        debug!("  Arguments: {:?}", arguments);

        // Call the streamable HTTP client
        match self.client.call_tool(&self.tool_name, arguments).await {
            Ok(result) => {
                debug!("  Tool execution successful");
                let response_summary = if result.len() > 200 {
                    format!("{}... ({} chars)", &result[..200], result.len())
                } else {
                    result.clone()
                };
                info!(
                    "HTTP streamable tool '{}' completed: {}",
                    self.tool_name, response_summary
                );
                Ok(result)
            }
            Err(err) => {
                info!(
                    "❌ HTTP streamable tool '{}' failed: {}",
                    self.tool_name, err
                );
                Err(ToolExecutionError::from(format!(
                    "Tool execution failed: {err}"
                )))
            }
        }
    }
}

impl McpManager {
    /// Get Rig-compatible tools for streamable HTTP MCP clients
    pub fn get_streamable_http_tools(&self) -> Vec<StreamableHttpMcpTool> {
        let mut tools = Vec::new();

        for (server_name, client) in &self.streamable_clients {
            if let Some(server_tools) = self.streamable_tools.get(server_name) {
                debug!(
                    "Processing {} streamable HTTP tools for server: {}",
                    server_tools.len(),
                    server_name
                );

                for mcp_tool in server_tools {
                    if let Some(rig_tool) = StreamableHttpMcpTool::new(
                        mcp_tool.name.to_string(),
                        server_name.clone(),
                        Arc::new(client.clone()),
                        mcp_tool.clone(),
                        self.sanitize_schemas,
                    ) {
                        tools.push(rig_tool);
                    }
                    // If None, tool was rejected due to invalid schema - already logged as warning
                }
            } else {
                debug!("No tools found for streamable server: {}", server_name);
            }
        }

        debug!(
            "Created {} Rig-compatible tools from streamable HTTP clients",
            tools.len()
        );
        tools
    }
}

/// Create a fallback tool with a unique name to avoid Claude API "Tool names must be unique" errors
///
/// Returns None if the tool has an invalid schema and should be rejected.
pub fn create_fallback_http_tool(
    unique_tool_name: String,
    original_tool_name: String,
    server_name: String,
    client: Arc<McpClient>,
    mcp_tool: rmcp::model::Tool,
    sanitize_schemas: bool,
) -> Option<FallbackHttpMcpTool> {
    FallbackHttpMcpTool::new(
        unique_tool_name,
        original_tool_name,
        server_name,
        client,
        mcp_tool,
        sanitize_schemas,
    )
}

/// Fallback tool with dynamic unique naming to avoid Claude API conflicts
#[derive(Clone)]
pub struct FallbackHttpMcpTool {
    pub unique_name: String,
    pub original_tool_name: String,
    pub server_name: String,
    pub client: Arc<McpClient>,
    pub tool_definition: ToolDefinition,
}

impl FallbackHttpMcpTool {
    pub fn new(
        unique_name: String,
        original_tool_name: String,
        server_name: String,
        client: Arc<McpClient>,
        mcp_tool: rmcp::model::Tool,
        sanitize_schemas: bool,
    ) -> Option<Self> {
        // Use centralized conversion method from McpManager
        let mut tool_definition =
            McpManager::convert_tool_to_rig_definition(&mcp_tool, sanitize_schemas)?;

        // Override the name with our unique name to avoid conflicts
        tool_definition.name = unique_name.clone();

        Some(Self {
            unique_name,
            original_tool_name,
            server_name,
            client,
            tool_definition,
        })
    }
}

impl RigTool for FallbackHttpMcpTool {
    const NAME: &'static str = "fallback_http_mcp_tool";

    type Error = ToolExecutionError;
    type Args = Value;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        self.tool_definition.clone()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        debug!(
            "FallbackHttpMcpTool::call - Original Tool: {}, Unique Name: {}, Server: {}",
            self.original_tool_name, self.unique_name, self.server_name
        );
        info!(
            "Calling fallback HTTP tool '{}' (original: '{}') on server '{}'",
            self.unique_name, self.original_tool_name, self.server_name
        );

        // Convert Value to HashMap<String, Value> for the streamable client
        let arguments = match args {
            Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
            _ => {
                return Err(ToolExecutionError::from(
                    "Invalid arguments: expected JSON object",
                ));
            }
        };

        debug!("  Arguments: {:?}", arguments);

        // Call the streamable HTTP client using the original tool name
        match self
            .client
            .call_tool(&self.original_tool_name, arguments)
            .await
        {
            Ok(result) => {
                debug!("  Fallback tool execution successful");
                let response_summary = if result.len() > 200 {
                    format!("{}... ({} chars)", &result[..200], result.len())
                } else {
                    result.clone()
                };
                info!(
                    "Fallback HTTP tool '{}' completed: {}",
                    self.unique_name, response_summary
                );
                Ok(result)
            }
            Err(err) => {
                info!(
                    "❌ Fallback HTTP tool '{}' failed: {}",
                    self.unique_name, err
                );
                Err(ToolExecutionError::from(format!(
                    "Fallback tool execution failed: {err}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpConfig, McpServerConfig};
    use serde_json::json;
    use std::collections::HashMap;

    // ========================================
    // Connection Status Tests
    // ========================================

    /// An unreachable HTTP-streamable server must be recorded as `Failed`, not
    /// as a connected zero-tool server. This is the regression guard for the
    /// bug where transport failures were swallowed into `Ok(0)` and logged as
    /// "Connected successfully, 0 tools discovered".
    #[tokio::test]
    async fn unreachable_http_server_is_recorded_as_failed() {
        let mut servers = HashMap::new();
        servers.insert(
            "pagerduty".to_string(),
            McpServerConfig::HttpStreamable {
                // Port 1 on loopback refuses connections immediately, so this
                // exercises the transport/connection-error path deterministically.
                url: "http://127.0.0.1:1/mcp".to_string(),
                headers: HashMap::new(),
                description: None,
                headers_from_request: HashMap::new(),
                scratchpad: HashMap::new(),
            },
        );
        let config = McpConfig {
            sanitize_schemas: true,
            servers,
        };

        let manager = McpManager::initialize_from_config(&config)
            .await
            .expect("initialize_from_config should succeed even when a server fails");

        let info = manager
            .server_info
            .get("pagerduty")
            .expect("pagerduty server_info should be present");
        assert!(
            matches!(info.status, ConnectionStatus::Failed(_)),
            "unreachable server should be Failed, got {:?}",
            info.status
        );
        assert_eq!(info.tools_count, 0);
        assert_eq!(info.transport, "http_streamable");

        // The status snapshot (what the aura.mcp_status event projects) must
        // surface the failure with a reason, distinct from an empty server.
        let snapshot = manager.server_status_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].server_name, "pagerduty");
        assert_eq!(snapshot[0].status, "failed");
        assert_eq!(snapshot[0].transport, "http_streamable");
        assert!(
            snapshot[0].reason.is_some(),
            "failed server should carry a reason"
        );
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
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

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

    /// A server that responds with an HTTP error status must surface that
    /// status in the failure reason — not the generic "Failed to establish MCP
    /// client connection". Guards the `CustomHttpClient` status-capture path
    /// (Level 2a): rmcp's transport worker would otherwise hide the 404 behind
    /// a "channel closed" error.
    #[tokio::test]
    async fn http_error_status_surfaces_in_reason() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Minimal server: reply 404 to every request, then close. Loops so the
        // transport worker sees a 404 on whatever request it makes first.
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = sock.flush().await;
            }
        });

        let mut servers = HashMap::new();
        servers.insert(
            "ghost".to_string(),
            McpServerConfig::HttpStreamable {
                url: format!("http://{addr}/mcp"),
                headers: HashMap::new(),
                description: None,
                headers_from_request: HashMap::new(),
                scratchpad: HashMap::new(),
            },
        );
        let config = McpConfig {
            sanitize_schemas: true,
            servers,
        };

        let manager = McpManager::initialize_from_config(&config).await.unwrap();
        server.abort();

        let info = manager.server_info.get("ghost").unwrap();
        let reason = match &info.status {
            ConnectionStatus::Failed(reason) => reason.clone(),
            other => panic!("expected Failed, got {other:?}"),
        };
        assert!(
            reason.contains("404"),
            "reason should include the HTTP status code, got: {reason}"
        );
    }

    /// A connected server with no tools projects as `connected` (status), not
    /// `failed` — the distinction the issue asks us to preserve.
    #[test]
    fn snapshot_distinguishes_connected_empty_from_failed() {
        let mut manager = McpManager::with_sanitization(true);
        manager.server_info.insert(
            "empty".to_string(),
            ServerInfo {
                name: "empty".to_string(),
                description: None,
                tools_count: 0,
                status: ConnectionStatus::Connected,
                transport: "http_streamable".to_string(),
            },
        );
        manager.server_info.insert(
            "down".to_string(),
            ServerInfo {
                name: "down".to_string(),
                description: None,
                tools_count: 0,
                status: ConnectionStatus::Failed("connection refused".to_string()),
                transport: "sse".to_string(),
            },
        );

        let snapshot = manager.server_status_snapshot();
        // Sorted by name: "down", then "empty".
        assert_eq!(snapshot[0].server_name, "down");
        assert_eq!(snapshot[0].status, "failed");
        assert_eq!(snapshot[0].reason.as_deref(), Some("connection refused"));
        assert_eq!(snapshot[1].server_name, "empty");
        assert_eq!(snapshot[1].status, "connected");
        assert_eq!(snapshot[1].tools_count, 0);
        assert_eq!(snapshot[1].reason, None);
    }

    // ========================================
    // Tool Conversion Tests
    // ========================================

    /// Test successful tool conversion with valid schema (sanitize_schemas=false)
    #[test]
    fn test_convert_tool_valid_schema_no_sanitization() {
        use std::borrow::Cow;
        use std::sync::Arc;

        let mcp_tool = rmcp::model::Tool {
            name: Cow::Borrowed("test_tool"),
            title: None,
            description: Some(Cow::Borrowed("A test tool")),
            input_schema: Arc::new(serde_json::Map::from_iter(vec![
                ("type".to_string(), json!("object")),
                (
                    "properties".to_string(),
                    json!({
                        "param1": {"type": "string"}
                    }),
                ),
            ])),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        };

        let result = McpManager::convert_tool_to_rig_definition(&mcp_tool, false);

        assert!(result.is_some(), "Valid tool should convert successfully");
        let tool_def = result.unwrap();
        assert_eq!(tool_def.name, "test_tool");
        assert_eq!(tool_def.description, "A test tool");
        assert_eq!(tool_def.parameters.get("type").unwrap(), "object");
    }

    /// Test successful tool conversion with valid schema (sanitize_schemas=true)
    #[test]
    fn test_convert_tool_valid_schema_with_sanitization() {
        use std::borrow::Cow;
        use std::sync::Arc;

        let mcp_tool = rmcp::model::Tool {
            name: Cow::Borrowed("test_tool"),
            title: None,
            description: Some(Cow::Borrowed("A test tool")),
            input_schema: Arc::new(serde_json::Map::from_iter(vec![
                ("type".to_string(), json!("object")),
                (
                    "properties".to_string(),
                    json!({
                        "param1": {"type": "string"}
                    }),
                ),
            ])),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        };

        let result = McpManager::convert_tool_to_rig_definition(&mcp_tool, true);

        assert!(
            result.is_some(),
            "Valid tool should convert successfully with sanitization"
        );
        let tool_def = result.unwrap();
        assert_eq!(tool_def.name, "test_tool");
        assert_eq!(tool_def.description, "A test tool");

        // Sanitization should add additionalProperties: false
        assert_eq!(
            tool_def.parameters.get("additionalProperties").unwrap(),
            &json!(false)
        );
    }

    /// Test tool conversion rejects invalid schema (non-object root type)
    #[test]
    fn test_convert_tool_invalid_root_type_rejected() {
        use std::borrow::Cow;
        use std::sync::Arc;

        let mcp_tool = rmcp::model::Tool {
            name: Cow::Borrowed("invalid_tool"),
            title: None,
            description: Some(Cow::Borrowed("Tool with invalid schema")),
            input_schema: Arc::new(serde_json::Map::from_iter(vec![
                // Invalid: root type should be "object", not "string"
                ("type".to_string(), json!("string")),
            ])),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        };

        // Without sanitization, invalid schema passes through (no validation)
        let result_no_sanitize = McpManager::convert_tool_to_rig_definition(&mcp_tool, false);
        assert!(
            result_no_sanitize.is_some(),
            "Without sanitization, schema passes through unchanged"
        );

        // With sanitization, invalid schema should be rejected
        let result_with_sanitize = McpManager::convert_tool_to_rig_definition(&mcp_tool, true);
        assert!(
            result_with_sanitize.is_none(),
            "Invalid schema should be rejected when sanitization is enabled"
        );
    }

    /// Test tool name sanitization (spaces, invalid characters)
    #[test]
    fn test_convert_tool_name_sanitization() {
        use std::borrow::Cow;
        use std::sync::Arc;

        let mcp_tool = rmcp::model::Tool {
            name: Cow::Borrowed("test tool with spaces"),
            title: None,
            description: Some(Cow::Borrowed("Test")),
            input_schema: Arc::new(serde_json::Map::from_iter(vec![(
                "type".to_string(),
                json!("object"),
            )])),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        };

        let result = McpManager::convert_tool_to_rig_definition(&mcp_tool, false);

        assert!(result.is_some());
        let tool_def = result.unwrap();
        // Spaces should be replaced with underscores
        assert_eq!(tool_def.name, "test_tool_with_spaces");
    }

    /// Test tool conversion with missing description
    #[test]
    fn test_convert_tool_missing_description() {
        use std::borrow::Cow;
        use std::sync::Arc;

        let mcp_tool = rmcp::model::Tool {
            name: Cow::Borrowed("test_tool"),
            title: None,
            description: None, // No description
            input_schema: Arc::new(serde_json::Map::from_iter(vec![(
                "type".to_string(),
                json!("object"),
            )])),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        };

        let result = McpManager::convert_tool_to_rig_definition(&mcp_tool, false);

        assert!(result.is_some());
        let tool_def = result.unwrap();
        // Missing description should default to empty string
        assert_eq!(tool_def.description, "");
    }

    /// Test tool conversion with complex nested schema
    #[test]
    fn test_convert_tool_complex_nested_schema() {
        use std::borrow::Cow;
        use std::sync::Arc;

        let mcp_tool = rmcp::model::Tool {
            name: Cow::Borrowed("complex_tool"),
            title: None,
            description: Some(Cow::Borrowed("Complex nested schema")),
            input_schema: Arc::new(serde_json::Map::from_iter(vec![
                ("type".to_string(), json!("object")),
                (
                    "properties".to_string(),
                    json!({
                        "nested": {
                            "type": "object",
                            "properties": {
                                "inner": {"type": "string"}
                            }
                        }
                    }),
                ),
            ])),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        };

        let result = McpManager::convert_tool_to_rig_definition(&mcp_tool, true);

        assert!(result.is_some(), "Complex schema should convert");
        let tool_def = result.unwrap();

        // Verify basic properties
        assert_eq!(tool_def.name, "complex_tool");
        assert_eq!(tool_def.description, "Complex nested schema");

        // Verify sanitization added additionalProperties at root
        assert_eq!(
            tool_def.parameters.get("additionalProperties").unwrap(),
            &json!(false),
            "Root should have additionalProperties: false"
        );

        // Verify properties field exists and nested structure is present
        assert!(
            tool_def.parameters.get("properties").is_some(),
            "Should have properties field"
        );
    }

    /// Test tool name with leading number (should be prefixed with "tool_")
    #[test]
    fn test_convert_tool_name_starting_with_number() {
        use std::borrow::Cow;
        use std::sync::Arc;

        let mcp_tool = rmcp::model::Tool {
            name: Cow::Borrowed("123_tool"),
            title: None,
            description: Some(Cow::Borrowed("Test")),
            input_schema: Arc::new(serde_json::Map::from_iter(vec![(
                "type".to_string(),
                json!("object"),
            )])),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        };

        let result = McpManager::convert_tool_to_rig_definition(&mcp_tool, false);

        assert!(result.is_some());
        let tool_def = result.unwrap();
        // Names starting with numbers should be prefixed
        assert_eq!(tool_def.name, "tool_123_tool");
    }

    // ========================================
    // Authorization Header Tests (existing)
    // ========================================

    #[test]
    fn test_authorization_header_extraction() {
        let mut headers = HashMap::new();

        // Test 1: Bearer token should be passed through unchanged
        headers.insert(
            "Authorization".to_string(),
            "Bearer my-secret-token".to_string(),
        );
        let bearer_auth = headers.get("Authorization").map(|auth| auth.to_string());
        assert_eq!(bearer_auth, Some("Bearer my-secret-token".to_string()));

        // Test 2: Token scheme should be passed through unchanged
        headers.insert(
            "Authorization".to_string(),
            "Token pd_abc123xyz".to_string(),
        );
        let token_auth = headers.get("Authorization").map(|auth| auth.to_string());
        assert_eq!(token_auth, Some("Token pd_abc123xyz".to_string()));

        // Test 3: Custom scheme should be passed through unchanged
        headers.insert(
            "Authorization".to_string(),
            "CustomScheme value123".to_string(),
        );
        let custom_auth = headers.get("Authorization").map(|auth| auth.to_string());
        assert_eq!(custom_auth, Some("CustomScheme value123".to_string()));

        // Test 4: Missing Authorization header returns None
        headers.remove("Authorization");
        let no_auth = headers.get("Authorization").map(|auth| auth.to_string());
        assert_eq!(no_auth, None);
    }

    /// Test that old behavior (stripping Bearer prefix) would fail with Token scheme
    #[test]
    fn test_bearer_prefix_stripping_would_fail() {
        let mut headers = HashMap::new();

        // This demonstrates the OLD bug: strip_prefix("Bearer ") would fail for Token scheme
        headers.insert(
            "Authorization".to_string(),
            "Token pd_abc123xyz".to_string(),
        );

        // Old code would do: auth.strip_prefix("Bearer ")
        let old_behavior = headers
            .get("Authorization")
            .and_then(|auth| auth.strip_prefix("Bearer "))
            .map(|token| token.to_string());

        // This would return None because Token doesn't start with "Bearer "
        assert_eq!(old_behavior, None, "Old code would reject Token scheme");

        // New code should preserve the full header
        let new_behavior = headers.get("Authorization").map(|auth| auth.to_string());

        assert_eq!(
            new_behavior,
            Some("Token pd_abc123xyz".to_string()),
            "New code preserves Token scheme"
        );
    }
}
