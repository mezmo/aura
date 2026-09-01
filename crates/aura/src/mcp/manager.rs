use crate::config::{McpServerConfig, McpUserAgent, default_mcp_user_agent};
use crate::error::BuilderError;
use crate::mcp::client::McpClient;
use crate::mcp::types::{AuraTool, ToolName};
use aura_config::GlobPattern;
use rig::completion::ToolDefinition;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use tracing::{debug, info, warn};

/// MCP client for managing connections to MCP servers
pub struct McpManager {
    pub server_info: HashMap<String, ServerInfo>,
    /// Store streamable HTTP clients for http_streamable transport
    pub streamable_clients: HashMap<String, McpClient>,
    pub streamable_tools: HashMap<String, Vec<AuraTool>>,
    /// Store SSE clients for sse transport
    pub sse_clients: HashMap<String, McpClient>,
    pub sse_tools: HashMap<String, Vec<AuraTool>>,
    /// Store STDIO clients for stdio transport
    pub stdio_clients: HashMap<String, McpClient>,
    pub stdio_tools: HashMap<String, Vec<AuraTool>>,
    /// Whether to sanitize tool schemas for OpenAI compatibility
    pub sanitize_schemas: bool,
    /// Manager-wide client identity.
    pub user_agent: McpUserAgent,
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
            user_agent: default_mcp_user_agent(),
        }
    }

    /// Initialize MCP connections and discover tools from all configured servers
    pub async fn initialize_from_config(
        mcp_config: &crate::config::McpConfig,
    ) -> Result<Self, BuilderError> {
        let mut manager = Self::with_sanitization(mcp_config.sanitize_schemas);
        manager.user_agent = mcp_config.user_agent.clone();

        info!(
            "Initializing MCP servers ({} configured) as {:?}",
            mcp_config.servers.len(),
            manager.user_agent
        );
        if mcp_config.sanitize_schemas {
            info!("Schema sanitization: ENABLED (OpenAI compatibility)");
        } else {
            info!("Schema sanitization: DISABLED (raw MCP schemas)");
        }

        for (server_name, server_config) in &mcp_config.servers {
            info!("Connecting to MCP server: {}", server_name);

            let transport = Self::transport_label(server_config).to_string();
            let connect_result = tokio::time::timeout(
                std::time::Duration::from_secs(mcp_config.connect_timeout_secs),
                manager.connect_and_discover_tools(server_name, server_config),
            )
            .await
            .unwrap_or_else(|_| {
                Err(BuilderError::McpInitError(format!(
                    "Connection timed out after {}s",
                    mcp_config.connect_timeout_secs
                )))
            });
            match connect_result {
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
        // A server-level identity replaces the manager-wide one for this
        // server alone.
        let user_agent = server_config
            .user_agent()
            .unwrap_or(&self.user_agent)
            .clone();
        match server_config {
            McpServerConfig::HttpStreamable { url, headers, .. } => {
                self.connect_http_streamable(server_name, url, headers, &user_agent)
                    .await
            }
            McpServerConfig::Sse { url, headers, .. } => {
                self.connect_sse(server_name, url, headers, &user_agent)
                    .await
            }
            McpServerConfig::Stdio { cmd, args, env, .. } => {
                self.connect_stdio(server_name, cmd, args, env, &user_agent)
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
        user_agent: &str,
    ) -> Result<usize, BuilderError> {
        info!("  Connecting to HTTP streamable server at: {}", url);

        match self
            .try_connect_http_streamable(server_name, url, headers, user_agent)
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
        user_agent: &str,
    ) -> Result<usize, BuilderError> {
        debug!("  Creating HTTP Streamable client for: {}", url);
        debug!("  Headers to be applied: {:?}", headers.keys());

        if !headers.is_empty() {
            info!("  Forwarding {} headers to MCP client", headers.len());
        }

        // Use McpClient. Render with `{e:#}` so anyhow's full cause chain (e.g.
        // the captured HTTP status → transport error) is included, not just the
        // outermost context.
        let client = McpClient::new(url.to_string(), server_name.into(), headers, user_agent)
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
        let sanitized_tools = Self::sanitize_and_collect_tools(
            tools,
            self.sanitize_schemas,
            "HTTP Streamable",
            server_name,
        );

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
        user_agent: &str,
    ) -> Result<usize, BuilderError> {
        info!("  Connecting to SSE server at: {}", url);

        match self
            .try_connect_sse(server_name, url, headers, user_agent)
            .await
        {
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
        user_agent: &str,
    ) -> Result<usize, BuilderError> {
        debug!("  Creating SSE client for: {}", url);

        let transport = crate::mcp::sse::SseTransport::connect(url, headers, user_agent)
            .await
            .map_err(BuilderError::SseTransport)?;

        let client =
            McpClient::from_transport(transport, url.to_string(), server_name.into(), user_agent)
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

        let sanitized_tools =
            Self::sanitize_and_collect_tools(tools, self.sanitize_schemas, "SSE", server_name);

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
        user_agent: &str,
    ) -> Result<usize, BuilderError> {
        info!("  Spawning STDIO server: {:?} {:?}", cmd, args);

        // This is more likely to work as rmcp has good STDIO support
        match self
            .try_connect_stdio(server_name, cmd, args, env, user_agent)
            .await
        {
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
        user_agent: &str,
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

        let client = McpClient::from_transport(
            transport,
            format!("stdio://{server_name}"),
            server_name.into(),
            user_agent,
        )
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
            Self::sanitize_and_collect_tools(tools, self.sanitize_schemas, "STDIO", server_name);

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
            fix_empty_root_required, normalize_bare_boolean_schemas,
            recursive_set_additional_properties_false,
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

        // Step 0: Normalize bare boolean schema nodes before anything else touches
        // the tree, so later passes only ever see object schemas.
        normalize_bare_boolean_schemas(schema);

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
        namespace: &str,
    ) -> Vec<AuraTool> {
        tools
            .into_iter()
            .filter_map(|tool| {
                let tool_name = tool.name.to_string();
                match Self::sanitize_mcp_tool(tool, sanitize_schemas) {
                    Ok(sanitized) => {
                        let tool = AuraTool::new(sanitized, namespace);
                        debug!("{} tool '{}' sanitized", transport_name, tool.name());
                        Some(tool)
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

    /// Every connected client, across all three transports.
    fn clients(&self) -> impl Iterator<Item = &McpClient> {
        self.streamable_clients
            .values()
            .chain(self.sse_clients.values())
            .chain(self.stdio_clients.values())
    }

    /// Bind these clients to the call they serve and the agent it belongs to.
    pub async fn bind_call(&self, http_request_id: &str, agent: aura_events::AgentContext) {
        let mut total_clients = 0;
        for client in self.clients() {
            client.bind_call(http_request_id, agent.clone()).await;
            total_clients += 1;
        }
        debug!(
            "Bound {} MCP client(s) to call: {}",
            total_clients, http_request_id
        );
    }

    pub async fn clear_current_call(&self) {
        let mut total_clients = 0;
        for client in self.clients() {
            client.clear_current_call().await;
            total_clients += 1;
        }
        debug!("Cleared current call on {} MCP client(s)", total_clients);
    }

    /// Get all available tool names across all transports.
    ///
    /// Returns a list of tool names that can be used for fallback tool execution.
    pub fn get_available_tool_names(&self) -> Vec<String> {
        self.tool_definitions_iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }

    /// Snapshot of every discovered tool across all transports, for
    /// namespace-aware `mcp_filter` matching (e.g. the scratchpad
    /// accessibility check) where the caller needs both the bare name and
    /// the namespace, not just the model-facing name.
    pub fn all_tools(&self) -> Vec<AuraTool> {
        self.tool_definitions_iter().cloned().collect()
    }

    /// Iterate over every discovered MCP tool across both HTTP-streamable and
    /// STDIO transports.
    ///
    /// Used by the scratchpad budget seed to BPE-count the actual JSON
    /// schemas the LLM sees in its tool list, instead of falling back to a
    /// per-tool constant heuristic.
    pub fn tool_definitions_iter(&self) -> impl Iterator<Item = &AuraTool> {
        self.streamable_tools
            .values()
            .flat_map(|tools| tools.iter())
            .chain(self.sse_tools.values().flat_map(|tools| tools.iter()))
            .chain(self.stdio_tools.values().flat_map(|tools| tools.iter()))
    }

    /// Serialize tool definitions as OpenAI-style function schemas, for the
    /// OpenInference `llm.tools.{i}.tool.json_schema` span attributes.
    ///
    /// `filter` narrows by glob patterns (worker `mcp_filter` semantics);
    /// `None` includes every tool.
    pub fn tool_schemas_json(&self, filter: Option<&[GlobPattern]>) -> Vec<String> {
        self.tool_definitions_iter()
            .filter(|tool| match filter {
                None => true,
                Some(patterns) => patterns.iter().any(|p| tool.is_match(p)),
            })
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name().to_string(),
                        "description": tool.description(),
                        "parameters": tool.input_schema(),
                    }
                })
                .to_string()
            })
            .collect()
    }

    /// Returns a `server_name → tool_names` map for all transports.
    ///
    /// Used by the scratchpad layer to resolve per-server `min_tokens`
    /// patterns to concrete tool names at boot time, so the runtime
    /// interception lookup is a server-aware exact match (not a
    /// server-agnostic glob — see `scratchpad::scratchpad_tool_map`).
    pub fn tool_names_per_server(&self) -> HashMap<String, Vec<ToolName>> {
        let mut map: HashMap<String, Vec<ToolName>> = self
            .streamable_tools
            .iter()
            .map(|(server_name, tools)| {
                let names = tools.iter().map(|t| t.name().clone()).collect();
                (server_name.clone(), names)
            })
            .collect();
        for (server_name, tools) in &self.sse_tools {
            let names = tools.iter().map(|t| t.name().clone()).collect();
            map.insert(server_name.clone(), names);
        }
        for (server_name, tools) in &self.stdio_tools {
            let names = tools.iter().map(|t| t.name().clone()).collect();
            map.insert(server_name.clone(), names);
        }
        map
    }

    /// Bare tool names advertised by more than one server, mapped to the
    /// servers claiming them.
    ///
    /// Tools register under their bare name, so a name claimed twice resolves
    /// to a single winner and the losing server's tool is unreachable.
    pub fn colliding_tool_names(&self) -> BTreeMap<ToolName, Vec<String>> {
        let mut claims: BTreeMap<ToolName, Vec<String>> = BTreeMap::new();
        for (server_name, names) in self.tool_names_per_server() {
            for name in names {
                claims.entry(name).or_default().push(server_name.clone());
            }
        }
        claims.retain(|_, servers| servers.len() > 1);
        for servers in claims.values_mut() {
            servers.sort();
        }
        claims
    }

    /// One human-readable line per colliding tool name, naming every server
    /// that claims it and the one that wins.
    pub fn collision_report(&self) -> Vec<String> {
        self.colliding_tool_names()
            .into_iter()
            .map(|(tool, servers)| {
                let winner = servers.first().map(String::as_str).unwrap_or("");
                format!(
                    "MCP tool '{}' is advertised by {} servers ({}); only '{}' will be reachable. \
                     Scope each agent with [agent].mcp_filter, or rename the tool on all but one \
                     server.",
                    tool,
                    servers.len(),
                    servers.join(", "),
                    winner,
                )
            })
            .collect()
    }

    /// Resolve every bare tool name with at least one tool passing `filter`
    /// to the server that would win its registration.
    ///
    /// Mirrors `Agent::add_all_tools`'s registration rule exactly: across
    /// all transports, in sorted server-id order, the first server whose
    /// tool passes `filter` claims the name — later claims are shadowed,
    /// same as `colliding_tool_names` reports. A caller that resolves a bare
    /// tool name to a server outside Rig's registered `ToolSet` (e.g.
    /// fallback tool execution, which dispatches on whatever name a model
    /// echoes back) must go through this, or it could reach a server the
    /// filter excluded or that registration shadowed.
    pub fn resolve_winning_tools(
        &self,
        mut filter: impl FnMut(&AuraTool) -> bool,
    ) -> BTreeMap<String, (String, McpClient)> {
        let mut servers: Vec<(&String, &McpClient, &Vec<AuraTool>)> = self
            .streamable_clients
            .iter()
            .filter_map(|(name, client)| {
                self.streamable_tools
                    .get(name)
                    .map(|tools| (name, client, tools))
            })
            .chain(self.sse_clients.iter().filter_map(|(name, client)| {
                self.sse_tools.get(name).map(|tools| (name, client, tools))
            }))
            .chain(self.stdio_clients.iter().filter_map(|(name, client)| {
                self.stdio_tools
                    .get(name)
                    .map(|tools| (name, client, tools))
            }))
            .collect();
        servers.sort_by_key(|(server_id, ..)| *server_id);

        let mut winners: BTreeMap<String, (String, McpClient)> = BTreeMap::new();
        for (server_name, client, tools) in servers {
            for tool in tools.iter().filter(|t| filter(t)) {
                winners
                    .entry(tool.name().to_string())
                    .or_insert_with(|| (server_name.clone(), client.clone()));
            }
        }
        winners
    }

    pub fn get_tool_definition_by_server(&self, name: &str) -> Vec<AuraTool> {
        if let Some(tools) = self.streamable_tools.get(name) {
            tools.clone()
        } else if let Some(tools) = self.sse_tools.get(name) {
            tools.clone()
        } else if let Some(tools) = self.stdio_tools.get(name) {
            tools.clone()
        } else {
            vec![]
        }
    }

    /// Execute a tool by name (used by Ollama text-to-tool fallback).
    ///
    /// Called by `FallbackToolExecutor` when it detects tool calls in streamed
    /// text. Normal Rig tool execution goes through `Tool::call()` trait
    /// implementations; this method exists specifically for the fallback
    /// parsing path, which dispatches on whatever bare name the model echoed
    /// back rather than through Rig's registered `ToolSet`.
    ///
    /// `filter` must be the same effective `mcp_filter` the agent registered
    /// tools under (`None` matches everything). Resolution otherwise uses
    /// [`resolve_winning_tools`](Self::resolve_winning_tools), so a name the
    /// filter excludes or that another server's tool shadows is unreachable
    /// here exactly as it would be through normal registration.
    pub async fn execute_fallback_tool(
        &self,
        tool_name: &str,
        arguments: &str,
        filter: Option<&[GlobPattern]>,
    ) -> Result<String, String> {
        // Parse arguments as JSON
        let args: Value = serde_json::from_str(arguments)
            .map_err(|e| format!("Failed to parse arguments: {}", e))?;

        let args_map = match args {
            Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
            _ => HashMap::new(),
        };

        // `tool_name` is whatever the model echoed back — the bare tool
        // name, since that's the only thing ever sent to the model. The MCP
        // wire call dispatches by the same sanitized bare name (`inner.name`).
        let Some((server_name, client)) = self
            .resolve_winning_tools(|tool| match filter {
                None => true,
                Some(patterns) => patterns.iter().any(|p| tool.is_match(p)),
            })
            .remove(tool_name)
        else {
            return Err(format!("Tool '{}' not found", tool_name));
        };

        info!(
            "Executing fallback tool '{}' via server '{}'",
            tool_name, server_name
        );
        client
            .call_tool(tool_name, args_map, None)
            .await
            .map_err(|e| format!("Tool execution failed: {}", e))
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpConfig, McpServerConfig};
    use serde_json::json;
    use std::collections::HashMap;

    /// Tools register under their bare name, so a name claimed by two servers
    /// leaves one unreachable. The detector is what turns that from silent
    /// into reported.
    mod tool_name_collisions {
        use super::*;

        fn tool(name: &str, namespace: &str) -> AuraTool {
            AuraTool::new(
                rmcp::model::Tool::new(
                    name.to_owned(),
                    "test tool".to_owned(),
                    std::sync::Arc::new(serde_json::Map::new()),
                ),
                namespace,
            )
        }

        fn manager_with(servers: &[(&str, &[&str])]) -> McpManager {
            let mut manager = McpManager::with_sanitization(false);
            for (server, names) in servers {
                manager.streamable_tools.insert(
                    (*server).to_owned(),
                    names.iter().map(|n| tool(n, server)).collect(),
                );
            }
            manager
        }

        #[test]
        fn disjoint_servers_report_nothing() {
            let manager = manager_with(&[
                ("github", &["list_repos", "get_pr"]),
                ("k8s", &["get_pods"]),
            ]);
            assert!(manager.colliding_tool_names().is_empty());
            assert!(manager.collision_report().is_empty());
        }

        #[test]
        fn a_shared_name_names_every_claiming_server() {
            let manager = manager_with(&[
                ("victoria", &["query_range", "series"]),
                ("sysdig", &["query_range"]),
                ("k8s", &["get_pods"]),
            ]);

            let collisions = manager.colliding_tool_names();
            assert_eq!(collisions.len(), 1, "only query_range collides");
            assert_eq!(
                collisions.get(&ToolName::new("query_range")),
                Some(&vec!["sysdig".to_owned(), "victoria".to_owned()]),
                "claiming servers are sorted, so the report does not vary per run",
            );

            let report = manager.collision_report();
            assert_eq!(report.len(), 1);
            assert!(report[0].contains("query_range"), "{}", report[0]);
            assert!(report[0].contains("sysdig"), "{}", report[0]);
            assert!(report[0].contains("victoria"), "{}", report[0]);
        }

        /// The winner named in the report has to be the one registration
        /// actually keeps: first server in sorted id order.
        #[test]
        fn the_report_names_the_lowest_sorted_server_as_the_winner() {
            let manager =
                manager_with(&[("victoria", &["query_range"]), ("sysdig", &["query_range"])]);
            let report = manager.collision_report();
            assert!(
                report[0].contains("only 'sysdig' will be reachable"),
                "{}",
                report[0],
            );
        }
    }

    /// `execute_fallback_tool` dispatches on a bare name a model echoed back,
    /// outside Rig's registered `ToolSet`, so it has its own chance to
    /// diverge from what `add_all_tools` actually registered. These pin it
    /// to the same rule: sorted-server-id winner, and never a server the
    /// filter excludes.
    mod fallback_tool_resolution {
        use super::*;
        use crate::mcp::client::tests::RecordingMcpServer;

        fn tool(name: &str, namespace: &str) -> AuraTool {
            AuraTool::new(
                rmcp::model::Tool::new(
                    name.to_owned(),
                    "test tool".to_owned(),
                    std::sync::Arc::new(serde_json::Map::new()),
                ),
                namespace,
            )
        }

        async fn client_for(namespace: &str, server: &RecordingMcpServer) -> McpClient {
            McpClient::new(
                server.url.clone(),
                namespace.into(),
                &HashMap::new(),
                "test/0",
            )
            .await
            .expect("the loopback server completes the handshake")
        }

        #[tokio::test]
        async fn picks_the_same_winner_registration_would() {
            let sysdig = RecordingMcpServer::start().await;
            let victoria = RecordingMcpServer::start().await;
            let manager = McpManager {
                streamable_clients: HashMap::from([
                    (
                        "victoria".to_owned(),
                        client_for("victoria", &victoria).await,
                    ),
                    ("sysdig".to_owned(), client_for("sysdig", &sysdig).await),
                ]),
                streamable_tools: HashMap::from([
                    ("victoria".to_owned(), vec![tool("query_range", "victoria")]),
                    ("sysdig".to_owned(), vec![tool("query_range", "sysdig")]),
                ]),
                ..McpManager::with_sanitization(false)
            };

            manager
                .execute_fallback_tool("query_range", "{}", None)
                .await
                .expect("the surviving tool is callable");

            assert_eq!(
                sysdig.tool_calls().len(),
                1,
                "'sysdig' sorts before 'victoria', so it wins the name",
            );
            assert!(
                victoria.tool_calls().is_empty(),
                "the shadowed server must not receive the call",
            );
        }

        /// The whole point of the filter parameter: a server that sorts
        /// first must still be unreachable if its tool fails the filter.
        #[tokio::test]
        async fn never_reaches_a_server_the_filter_excludes() {
            let internal = RecordingMcpServer::start().await;
            let public = RecordingMcpServer::start().await;
            let manager = McpManager {
                streamable_clients: HashMap::from([
                    (
                        "aaa_internal".to_owned(),
                        client_for("aaa_internal", &internal).await,
                    ),
                    ("public".to_owned(), client_for("public", &public).await),
                ]),
                streamable_tools: HashMap::from([
                    (
                        "aaa_internal".to_owned(),
                        vec![tool("list_pods", "aaa_internal")],
                    ),
                    ("public".to_owned(), vec![tool("list_pods", "public")]),
                ]),
                ..McpManager::with_sanitization(false)
            };

            // "aaa_internal" sorts before "public" and would win by id alone;
            // the filter must exclude it regardless.
            let filter = vec![aura_config::GlobPattern::from("public:*")];
            manager
                .execute_fallback_tool("list_pods", "{}", Some(&filter))
                .await
                .expect("the filter-passing tool is callable");

            assert_eq!(public.tool_calls().len(), 1);
            assert!(
                internal.tool_calls().is_empty(),
                "a filtered-out server must be unreachable even though its id sorts first",
            );
        }

        #[tokio::test]
        async fn an_unknown_name_is_an_error() {
            let manager = McpManager::with_sanitization(false);
            let err = manager
                .execute_fallback_tool("does_not_exist", "{}", None)
                .await
                .expect_err("no server advertises this name");
            assert!(err.contains("does_not_exist"), "{err}");
        }
    }

    // ========================================
    // Connection Status Tests
    // ========================================

    /// A server-level `user_agent` replaces the manager-wide identity for
    /// that server alone; its neighbour still announces `[mcp].user_agent`.
    #[tokio::test]
    async fn server_level_user_agent_replaces_the_global_one_for_that_server() {
        use crate::mcp::client::tests::{RecordingMcpServer, announced_client};

        let tagged = RecordingMcpServer::start().await;
        let plain = RecordingMcpServer::start().await;
        let http = |url: &str, user_agent: Option<&str>| McpServerConfig::HttpStreamable {
            url: url.to_owned(),
            headers: HashMap::new(),
            description: None,
            headers_from_request: HashMap::new(),
            scratchpad: HashMap::new(),
            user_agent: user_agent.map(|token| McpUserAgent::new(token).unwrap()),
        };
        let config = McpConfig {
            servers: HashMap::from([
                (
                    "tagged".to_owned(),
                    http(&tagged.url, Some("aura-prod-us/1")),
                ),
                ("plain".to_owned(), http(&plain.url, None)),
            ]),
            user_agent: McpUserAgent::new("aura/0.0.0").unwrap(),
            ..Default::default()
        };

        McpManager::initialize_from_config(&config).await.unwrap();

        let handshake = tagged.initialize();
        assert_eq!(
            handshake.header_values("user-agent"),
            vec!["aura-prod-us/1"]
        );
        assert_eq!(
            announced_client(&handshake),
            ("aura-prod-us".to_owned(), "1".to_owned())
        );
        let handshake = plain.initialize();
        assert_eq!(handshake.header_values("user-agent"), vec!["aura/0.0.0"]);
        assert_eq!(
            announced_client(&handshake),
            ("aura".to_owned(), "0.0.0".to_owned())
        );
    }

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
                user_agent: None,
            },
        );
        let config = McpConfig {
            servers,
            ..Default::default()
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
                user_agent: None,
            },
        );
        let config = McpConfig {
            servers,
            ..Default::default()
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

    /// A server that accepts the TCP connection but never speaks HTTP must
    /// not hang `initialize_from_config` past `connect_timeout_secs` (#305).
    #[tokio::test]
    async fn initialize_from_config_times_out_on_hung_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept and hold the connection open without ever responding.
            // Keeping the accepted stream alive (not just the listener) is
            // what makes the client hang instead of seeing a reset.
            let (_stream, _addr) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let mut servers = HashMap::new();
        servers.insert(
            "hung".to_string(),
            McpServerConfig::HttpStreamable {
                url: format!("http://{addr}"),
                headers: HashMap::new(),
                description: None,
                headers_from_request: HashMap::new(),
                scratchpad: HashMap::new(),
                user_agent: None,
            },
        );
        let mcp_config = McpConfig {
            servers,
            connect_timeout_secs: 1,
            ..Default::default()
        };

        let manager = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            McpManager::initialize_from_config(&mcp_config),
        )
        .await
        .expect("initialize_from_config must not hang past connect_timeout_secs")
        .expect("manager construction itself does not fail on a per-server timeout");

        let status = &manager.server_info["hung"].status;
        match status {
            ConnectionStatus::Failed(reason) => {
                assert!(reason.contains("timed out"), "got reason: {reason}")
            }
            other => panic!("expected Failed(timed out), got {other:?}"),
        }
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
