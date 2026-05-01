use crate::{
    config::McpServerConfig, error::BuilderError, mcp_streamable_http::StreamableHttpMcpClient,
};
use futures::{StreamExt, stream::BoxStream};
use rig::tool::rmcp::McpTool;
use rig::{completion::ToolDefinition, tool::Tool as RigTool};
use rmcp::ServiceExt;
use rmcp::model::{ClientCapabilities, ClientInfo, Implementation, Tool};
use rmcp::transport::streamable_http_client::StreamableHttpClient;
use serde_json::Value;
use sse_stream::{Sse, SseStream};
use std::collections::HashMap;
use std::sync::Arc;
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

/// Custom HTTP client that supports Bearer token authentication
#[derive(Clone, Default)]
pub struct CustomHttpClient {
    client: reqwest::Client,
    auth_token: Option<String>,
}

impl CustomHttpClient {
    pub fn new(auth_token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth_token,
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
        _auth_token: Option<String>, // We use our own auth_token field
    ) -> Result<
        BoxStream<'static, Result<Sse, sse_stream::Error>>,
        rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
    > {
        use reqwest::header::ACCEPT;
        use rmcp::transport::common::http_header::{
            EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID,
        };

        let mut request_builder = self
            .client
            .get(uri.as_ref())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(HEADER_SESSION_ID, session_id.as_ref());

        if let Some(last_event_id) = last_event_id {
            request_builder = request_builder.header(HEADER_LAST_EVENT_ID, last_event_id);
        }

        // Add Bearer token authentication if available
        if let Some(ref auth_header) = self.auth_token {
            request_builder = request_builder.bearer_auth(auth_header);
        }

        let response = request_builder
            .send()
            .await
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(rmcp::transport::streamable_http_client::StreamableHttpError::ServerDoesNotSupportSse);
        }
        let response = response
            .error_for_status()
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;

        match response.headers().get(reqwest::header::CONTENT_TYPE) {
            Some(ct) => {
                if !ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) {
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
        _auth_token: Option<String>, // We use our own auth_token field
    ) -> Result<(), rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>> {
        use rmcp::transport::common::http_header::HEADER_SESSION_ID;

        let mut request_builder = self.client.delete(uri.as_ref());

        // Add Bearer token authentication if available
        if let Some(ref auth_header) = self.auth_token {
            request_builder = request_builder.bearer_auth(auth_header);
        }

        let response = request_builder
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
        _session_id: Option<Arc<str>>,
        _auth_token: Option<String>, // We use our own auth_token field
    ) -> Result<
        rmcp::transport::streamable_http_client::StreamableHttpPostResponse,
        rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
    > {
        use rmcp::transport::common::http_header::JSON_MIME_TYPE;

        let mut request_builder = self
            .client
            .post(uri.as_ref())
            .header(reqwest::header::CONTENT_TYPE, JSON_MIME_TYPE)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&message);

        // Add Bearer token authentication if available
        if let Some(ref auth_header) = self.auth_token {
            request_builder = request_builder.bearer_auth(auth_header);
        }

        let response = request_builder
            .send()
            .await
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;
        let response = response
            .error_for_status()
            .map_err(rmcp::transport::streamable_http_client::StreamableHttpError::Client)?;

        // Extract session ID from headers before consuming response
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        // Check response content type to determine how to handle it
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE);

        if let Some(ct) = content_type {
            let ct_str = ct.to_str().unwrap_or("");
            if ct_str.starts_with("application/json") {
                // Parse JSON response
                let json_text = response.text().await.map_err(
                    rmcp::transport::streamable_http_client::StreamableHttpError::Client,
                )?;
                let json_message: rmcp::model::ServerJsonRpcMessage =
                    serde_json::from_str(&json_text).map_err(
                        rmcp::transport::streamable_http_client::StreamableHttpError::Deserialize,
                    )?;

                return Ok(
                    rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Json(
                        json_message,
                        session_id,
                    ),
                );
            } else if ct_str.starts_with("text/event-stream") {
                // Handle SSE stream - convert response to stream
                let event_stream =
                    sse_stream::SseStream::from_byte_stream(response.bytes_stream()).boxed();

                return Ok(
                    rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Sse(
                        event_stream,
                        session_id,
                    ),
                );
            }
        }

        // Fallback to Accepted for other content types
        Ok(rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Accepted)
    }
}

/// MCP client for managing connections to MCP servers
pub struct McpManager {
    pub tools: Vec<McpTool>,
    pub server_info: HashMap<String, ServerInfo>,
    /// Store raw tool definitions with their associated client for agent integration
    pub tool_definitions: Vec<(Tool, rmcp::service::ServerSink)>,
    /// Store streamable HTTP clients for http_streamable transport
    pub streamable_clients: HashMap<String, StreamableHttpMcpClient>,
    pub streamable_tools: HashMap<String, Vec<rmcp::model::Tool>>,
    /// Whether to sanitize tool schemas for OpenAI compatibility
    pub sanitize_schemas: bool,
}

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub description: Option<String>,
    pub tools_count: usize,
    pub status: ConnectionStatus,
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
            tools: Vec::new(),
            server_info: HashMap::new(),
            tool_definitions: Vec::new(),
            streamable_clients: HashMap::new(),
            streamable_tools: HashMap::new(),
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
                        },
                    );
                    warn!("{} - {}", server_name, error_msg);
                }
            }
        }

        // Count ALL tools across all transport types
        let stdio_tools = manager.tools.len();
        let streamable_tools: usize = manager.streamable_tools.values().map(|v| v.len()).sum();
        let total_tools = stdio_tools + streamable_tools;
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
            McpServerConfig::Stdio { cmd, args, env, .. } => {
                self.connect_stdio(server_name, std::slice::from_ref(cmd), args, env)
                    .await
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
                // Check if this is an authentication error
                if e.to_string().contains("401 Unauthorized")
                    || e.to_string().contains("HTTP status client error (401")
                {
                    Err(BuilderError::McpInitError(format!(
                        "HTTP MCP server '{server_name}' authentication failed (401 Unauthorized). This is likely because rmcp does not yet support custom headers for authentication. Your Authorization header cannot be sent."
                    )))
                } else {
                    warn!("  HTTP Streamable MCP connection failed: {}", e);
                    // Continue without this server for now
                    Ok(0)
                }
            }
        }
    }

    /// Attempt to connect to HTTP Streamable MCP server using the new StreamableHttpMcpClient
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

        // Use our new StreamableHttpMcpClient
        let client = StreamableHttpMcpClient::new(url.to_string(), headers)
            .await
            .map_err(|e| {
                BuilderError::McpInitError(format!(
                    "Failed to connect to HTTP MCP server '{server_name}': {e}"
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
                "Failed to discover tools from server '{server_name}': {e}"
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
                // For now, we'll continue without this server
                Ok(0)
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
                "Empty command for STDIO server".to_string(),
            ));
        }

        let mut process = Command::new(&cmd[0]);

        // Add additional command args if present
        if cmd.len() > 1 {
            for arg in &cmd[1..] {
                process.arg(arg);
            }
        }

        // Add the specified args
        for arg in args {
            process.arg(arg);
        }

        // Set environment variables
        for (key, value) in env {
            process.env(key, value);
        }

        debug!("  Spawning process: {:?}", process);

        // Create the transport
        let transport = TokioChildProcess::new(process).map_err(|e| {
            BuilderError::McpInitError(format!("Failed to spawn MCP server process: {e}"))
        })?;

        // Create client info
        let client_info = ClientInfo {
            protocol_version: Default::default(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "aura-config".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("Aura Configuration System".to_string()),
                website_url: None,
                icons: None,
            },
        };

        // Connect to the server
        let client = client_info.serve(transport).await.map_err(|e| {
            BuilderError::McpInitError(format!(
                "Failed to connect to MCP server '{server_name}': {e}"
            ))
        })?;

        // Get server info
        let server_info = client.peer_info();
        if let Some(init_result) = server_info {
            info!(
                "  Connected to MCP server '{}': {} v{}",
                server_name, init_result.server_info.name, init_result.server_info.version
            );
        } else {
            info!(
                "  Connected to MCP server '{}' (no server info available)",
                server_name
            );
        }

        // List available tools
        let tools_response = client.list_tools(Default::default()).await.map_err(|e| {
            BuilderError::McpInitError(format!("Failed to list tools from '{server_name}': {e}"))
        })?;

        let tools = tools_response.tools;
        info!("  Discovered {} tools from '{}'", tools.len(), server_name);

        // Store tools and their definitions for later integration
        let sanitized_tools =
            Self::sanitize_and_collect_tools(tools, self.sanitize_schemas, "STDIO");
        let tools_count = sanitized_tools.len();

        for sanitized_tool in sanitized_tools {
            self.tool_definitions
                .push((sanitized_tool.clone(), client.clone()));
            self.tools
                .push(McpTool::from_mcp_server(sanitized_tool, client.clone()));
        }

        // Store the client for later use (client is already cloned above)
        // self.clients.push(client);

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
            fix_empty_root_required, inline_refs, recursive_set_additional_properties_false,
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

        // Step 0: Inline $ref/$defs so subsequent passes process the full schema
        inline_refs(schema);

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
            | McpServerConfig::Stdio { description, .. } => description.clone(),
        }
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
        let total_tools = self.tools.len()
            + self
                .streamable_tools
                .values()
                .map(|v| v.len())
                .sum::<usize>();
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

        total_cancelled
    }

    /// Set the current HTTP request ID for cancellation tracking.
    pub async fn set_current_request(&self, http_request_id: &str) {
        for client in self.streamable_clients.values() {
            client.set_current_request(http_request_id).await;
        }
        debug!(
            "Set current HTTP request ID on {} MCP client(s): {}",
            self.streamable_clients.len(),
            http_request_id
        );
    }

    pub async fn clear_current_request(&self) {
        for client in self.streamable_clients.values() {
            client.clear_current_request().await;
        }
        debug!(
            "Cleared current HTTP request ID on {} MCP client(s)",
            self.streamable_clients.len()
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

        // STDIO tools
        for (tool, _) in &self.tool_definitions {
            names.push(tool.name.to_string());
        }

        names
    }

    /// Execute a tool by name (used by Ollama text-to-tool fallback).
    ///
    /// Called by `FallbackToolExecutor` when it detects tool calls in streamed text.
    /// Routes to the appropriate MCP transport (HTTP Streamable or STDIO).
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

        // Try STDIO tools
        for (tool, client) in &self.tool_definitions {
            if tool.name.as_ref() == tool_name {
                info!("Executing fallback tool '{}' via STDIO", tool_name);

                // Convert HashMap to serde_json::Map for RMCP API
                let args_json_map: serde_json::Map<String, Value> = args_map.into_iter().collect();

                let call_request = rmcp::model::CallToolRequestParam {
                    name: tool.name.clone(),
                    arguments: Some(args_json_map),
                };

                let result = client
                    .call_tool(call_request)
                    .await
                    .map_err(|e| format!("STDIO tool execution failed: {}", e))?;

                // Format the result - content items are Annotated<RawContent>
                let content_strs: Vec<String> = result
                    .content
                    .iter()
                    .map(|annotated| match &annotated.raw {
                        rmcp::model::RawContent::Text(t) => t.text.to_string(),
                        rmcp::model::RawContent::Image(_) => "[Image content]".to_string(),
                        rmcp::model::RawContent::Audio(_) => "[Audio content]".to_string(),
                        rmcp::model::RawContent::Resource(res) => {
                            crate::mcp_response::extract_resource_contents(&res.resource)
                        }
                        rmcp::model::RawContent::ResourceLink(link) => {
                            format!("[Resource link: {} ({})]", link.name, link.uri)
                        }
                    })
                    .collect();

                return Ok(content_strs.join("\n"));
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
            pub client: Arc<StreamableHttpMcpClient>,
            pub tool_definition: ToolDefinition,
        }

        impl $struct_name {
            pub fn new(
                tool_name: String,
                server_name: String,
                client: Arc<StreamableHttpMcpClient>,
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

/// Generic fallback for unknown tools - A Rig-compatible tool wrapper for StreamableHttpMcpClient tools
#[derive(Clone)]
pub struct StreamableHttpMcpTool {
    pub tool_name: String,
    pub server_name: String,
    pub client: Arc<StreamableHttpMcpClient>,
    pub tool_definition: ToolDefinition,
}

impl StreamableHttpMcpTool {
    pub fn new(
        tool_name: String,
        server_name: String,
        client: Arc<StreamableHttpMcpClient>,
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
    client: Arc<StreamableHttpMcpClient>,
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
    pub client: Arc<StreamableHttpMcpClient>,
    pub tool_definition: ToolDefinition,
}

impl FallbackHttpMcpTool {
    pub fn new(
        unique_name: String,
        original_tool_name: String,
        server_name: String,
        client: Arc<StreamableHttpMcpClient>,
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
    use serde_json::json;
    use std::collections::HashMap;

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
