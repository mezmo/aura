//! Governance catalog envelope types.
//!
//! These types form the JSON payload POSTed to the governance webhook.
//! They are intentionally distinct from the `aura-events` types used by
//! `GET /aura/info`: the governance wire format prioritizes a flat,
//! explicit structure suitable for ingestion by external governance systems.

use aura_config::{Config, McpServerConfig};
use aura_events::{McpToolAnnotations, McpToolOverview};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Top-level envelope for the catalog sync webhook payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CatalogEnvelope {
    /// Event type identifier.
    pub event: String,
    /// Schema version.
    pub version: String,
    /// Unique event identifier (format: `cat_<ULID>`).
    pub event_id: String,
    /// ISO 8601 timestamp when the event was emitted.
    pub emitted_at: String,
    /// AURA process version.
    pub aura_version: String,
    /// Catalog entries for each agent.
    pub agents: Vec<AgentCatalogEntry>,
}

impl CatalogEnvelope {
    /// Generate a new unique event ID.
    pub fn generate_event_id() -> String {
        format!("cat_{}", ulid::Ulid::new())
    }
}

/// Catalog entry for a single agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AgentCatalogEntry {
    /// Agent identifier (from config).
    pub id: String,
    /// Agent name (from config).
    pub name: String,
    /// Agent description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// LLM model name.
    pub model: String,
    /// Flat array of MCP server reports, sorted by name.
    pub mcp_servers: Vec<McpServerReport>,
}

/// Report for a single MCP server with connection status.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct McpServerReport {
    /// Server name (from config).
    pub name: String,
    /// Transport type: `"http_streamable"`, `"sse"`, or `"stdio"`.
    pub transport: String,
    /// URL for http_streamable/sse transports (origin only, no credentials).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Command basename for stdio transport.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Server description from config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Connection status.
    pub status: ServerStatus,
    /// Failure message (present iff `status == ServerStatus::Failed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    /// Discovered tools (present iff `status == ServerStatus::Connected`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolReport>>,
}

impl McpServerReport {
    /// Create a report for a connected server.
    pub fn connected(
        name: String,
        transport: String,
        url: Option<String>,
        command: Option<String>,
        description: Option<String>,
        tools: Vec<McpToolReport>,
    ) -> Self {
        Self {
            name,
            transport,
            url,
            command,
            description,
            status: ServerStatus::Connected,
            failure_message: None,
            tools: Some(tools),
        }
    }

    /// Create a report for a failed server.
    pub fn failed(
        name: String,
        transport: String,
        url: Option<String>,
        command: Option<String>,
        description: Option<String>,
        failure_message: String,
    ) -> Self {
        Self {
            name,
            transport,
            url,
            command,
            description,
            status: ServerStatus::Failed,
            failure_message: Some(failure_message),
            tools: None,
        }
    }

    /// Create a report for a server that was not attempted.
    pub fn not_attempted(
        name: String,
        transport: String,
        url: Option<String>,
        command: Option<String>,
        description: Option<String>,
    ) -> Self {
        Self {
            name,
            transport,
            url,
            command,
            description,
            status: ServerStatus::NotAttempted,
            failure_message: None,
            tools: None,
        }
    }
}

/// Connection status for an MCP server.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    Connected,
    Failed,
    NotAttempted,
}

impl From<&crate::mcp::ConnectionStatus> for ServerStatus {
    fn from(status: &crate::mcp::ConnectionStatus) -> Self {
        match status {
            crate::mcp::ConnectionStatus::Connected => Self::Connected,
            crate::mcp::ConnectionStatus::Failed(_) => Self::Failed,
            crate::mcp::ConnectionStatus::NotAttempted => Self::NotAttempted,
        }
    }
}

/// Tool report mirroring `McpToolOverview` but using snake_case field names.
///
/// The governance format uses snake_case throughout for consistency; the
/// `McpToolOverview` type uses camelCase to match the MCP spec.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct McpToolReport {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

impl From<McpToolOverview> for McpToolReport {
    fn from(tool: McpToolOverview) -> Self {
        Self {
            name: tool.name,
            title: tool.title,
            description: tool.description,
            input_schema: tool.input_schema,
            output_schema: tool.output_schema,
            annotations: tool.annotations,
        }
    }
}

impl From<&rmcp::model::Tool> for McpToolReport {
    fn from(tool: &rmcp::model::Tool) -> Self {
        Self {
            name: tool.name.to_string(),
            title: tool.title.clone(),
            description: tool.description.as_ref().map(|d| d.to_string()),
            input_schema: Some(serde_json::Value::Object((*tool.input_schema).clone())),
            output_schema: tool
                .output_schema
                .as_ref()
                .map(|schema| serde_json::Value::Object((**schema).clone())),
            annotations: tool
                .annotations
                .as_ref()
                .map(|annotations| McpToolAnnotations {
                    title: annotations.title.clone(),
                    read_only_hint: annotations.read_only_hint,
                    destructive_hint: annotations.destructive_hint,
                    idempotent_hint: annotations.idempotent_hint,
                    open_world_hint: annotations.open_world_hint,
                }),
        }
    }
}

/// Build a catalog envelope for the given config by discovering MCP tools.
///
/// This function connects to all configured MCP servers, discovers their
/// tools, and builds a complete catalog snapshot. Servers that fail to
/// connect are included with their failure status and message.
pub async fn build_catalog(
    config: &Config,
    req_headers: Option<&HashMap<String, String>>,
    timeout: Duration,
) -> CatalogEnvelope {
    build_catalog_multi(std::slice::from_ref(config), req_headers, timeout).await
}

/// Build a catalog envelope for multiple configs by discovering MCP tools.
///
/// Each config becomes an agent entry in the catalog. Servers that fail to
/// connect are included with their failure status and message.
pub async fn build_catalog_multi(
    configs: &[Config],
    req_headers: Option<&HashMap<String, String>>,
    timeout: Duration,
) -> CatalogEnvelope {
    let mut agents = Vec::with_capacity(configs.len());

    for config in configs {
        let mut mcp_config = config.mcp.clone();
        if let Some(mcp) = mcp_config.as_mut() {
            crate::rig_builder::resolve_mcp_headers_in(mcp, req_headers);
        }

        let mcp_servers = discover_mcp_servers(&mcp_config, timeout).await;

        agents.push(AgentCatalogEntry {
            id: config.agent_id().to_owned(),
            name: config.agent.name.clone(),
            description: config.agent.description.clone(),
            model: config.agent.llm.model_info().1.to_owned(),
            mcp_servers,
        });
    }

    CatalogEnvelope {
        event: "mcp_catalog".to_string(),
        version: "1".to_string(),
        event_id: CatalogEnvelope::generate_event_id(),
        emitted_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        aura_version: env!("CARGO_PKG_VERSION").to_string(),
        agents,
    }
}

/// Discover MCP servers and their tools, returning a flat list sorted by name.
async fn discover_mcp_servers(
    mcp_config: &Option<aura_config::McpConfig>,
    timeout: Duration,
) -> Vec<McpServerReport> {
    let Some(mcp_config) = mcp_config else {
        return Vec::new();
    };

    let manager = match tokio::time::timeout(
        timeout,
        crate::mcp::McpManager::initialize_from_config(mcp_config),
    )
    .await
    {
        Ok(Ok(manager)) => manager,
        Ok(Err(e)) => {
            tracing::warn!("MCP tool discovery for governance catalog failed: {e}");
            // Return servers from config with NotAttempted status
            return servers_from_config_only(mcp_config);
        }
        Err(_) => {
            tracing::warn!(
                "MCP tool discovery for governance catalog timed out after {}s",
                timeout.as_secs()
            );
            // Return servers from config with NotAttempted status
            return servers_from_config_only(mcp_config);
        }
    };

    // Build server reports from the manager's discovery results
    let mut servers: Vec<McpServerReport> = manager
        .server_info
        .iter()
        .map(|(name, info)| {
            let server_config = mcp_config.servers.get(name);
            let (url, command) = server_config.map(extract_endpoint).unwrap_or((None, None));

            match &info.status {
                crate::mcp::ConnectionStatus::Connected => {
                    // Collect tools for this server
                    let tools = manager
                        .streamable_tools
                        .get(name)
                        .or_else(|| manager.sse_tools.get(name))
                        .or_else(|| manager.stdio_tools.get(name))
                        .map(|tools| tools.iter().map(McpToolReport::from).collect())
                        .unwrap_or_default();

                    McpServerReport::connected(
                        name.clone(),
                        info.transport.clone(),
                        url,
                        command,
                        info.description.clone(),
                        tools,
                    )
                }
                crate::mcp::ConnectionStatus::Failed(reason) => McpServerReport::failed(
                    name.clone(),
                    info.transport.clone(),
                    url,
                    command,
                    info.description.clone(),
                    reason.clone(),
                ),
                crate::mcp::ConnectionStatus::NotAttempted => McpServerReport::not_attempted(
                    name.clone(),
                    info.transport.clone(),
                    url,
                    command,
                    info.description.clone(),
                ),
            }
        })
        .collect();

    // Sort by name for deterministic output
    servers.sort_by(|a, b| a.name.cmp(&b.name));

    // Close all MCP connections
    manager
        .cancel_and_close_all("governance-catalog", "catalog sync completed")
        .await;

    servers
}

/// Build server reports from config only (when discovery fails entirely).
fn servers_from_config_only(mcp_config: &aura_config::McpConfig) -> Vec<McpServerReport> {
    let mut servers: Vec<McpServerReport> = mcp_config
        .servers
        .iter()
        .map(|(name, config)| {
            let transport = transport_label(config).to_string();
            let (url, command) = extract_endpoint(config);
            let description = extract_description(config);

            McpServerReport::not_attempted(name.clone(), transport, url, command, description)
        })
        .collect();

    servers.sort_by(|a, b| a.name.cmp(&b.name));
    servers
}

/// Extract the transport label from an MCP server config.
fn transport_label(config: &McpServerConfig) -> &'static str {
    match config {
        McpServerConfig::HttpStreamable { .. } => "http_streamable",
        McpServerConfig::Sse { .. } => "sse",
        McpServerConfig::Stdio { .. } => "stdio",
    }
}

/// Extract the endpoint (url or command) from an MCP server config.
/// URLs are sanitized to origin-only (no path, query, or credentials).
/// Commands are sanitized to basename only.
fn extract_endpoint(config: &McpServerConfig) -> (Option<String>, Option<String>) {
    match config {
        McpServerConfig::HttpStreamable { url, .. } | McpServerConfig::Sse { url, .. } => {
            (Some(sanitize_url(url)), None)
        }
        McpServerConfig::Stdio { cmd, .. } => {
            // cmd is a Vec<String> where the first element is the command
            let command = cmd.first().map(|c| command_basename(c));
            (None, command)
        }
    }
}

/// Extract the description from an MCP server config.
fn extract_description(config: &McpServerConfig) -> Option<String> {
    match config {
        McpServerConfig::HttpStreamable { description, .. }
        | McpServerConfig::Sse { description, .. }
        | McpServerConfig::Stdio { description, .. } => description.clone(),
    }
}

/// Sanitize a URL to origin-only (scheme + host + port).
fn sanitize_url(url: &str) -> String {
    url::Url::parse(url)
        .map(|u| u.origin().unicode_serialization())
        .unwrap_or_else(|_| url.to_string())
}

/// Extract the basename from a command path.
fn command_basename(command: &str) -> String {
    std::path::Path::new(command)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| command.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_status_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&ServerStatus::Connected).unwrap(),
            r#""connected""#
        );
        assert_eq!(
            serde_json::to_string(&ServerStatus::Failed).unwrap(),
            r#""failed""#
        );
        assert_eq!(
            serde_json::to_string(&ServerStatus::NotAttempted).unwrap(),
            r#""not_attempted""#
        );
    }

    #[test]
    fn connected_server_has_tools_no_failure_message() {
        let report = McpServerReport::connected(
            "test".to_string(),
            "http_streamable".to_string(),
            Some("https://example.com".to_string()),
            None,
            None,
            vec![],
        );

        assert_eq!(report.status, ServerStatus::Connected);
        assert!(report.failure_message.is_none());
        assert!(report.tools.is_some());

        // Verify JSON output has no failure_message field
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("failure_message"));
    }

    #[test]
    fn failed_server_has_failure_message_no_tools() {
        let report = McpServerReport::failed(
            "test".to_string(),
            "sse".to_string(),
            Some("https://example.com".to_string()),
            None,
            None,
            "connection refused".to_string(),
        );

        assert_eq!(report.status, ServerStatus::Failed);
        assert_eq!(
            report.failure_message,
            Some("connection refused".to_string())
        );
        assert!(report.tools.is_none());

        // Verify JSON output has failure_message but no tools
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("failure_message"));
        assert!(!json.contains("\"tools\""));
    }

    #[test]
    fn not_attempted_server_has_neither_failure_nor_tools() {
        let report = McpServerReport::not_attempted(
            "test".to_string(),
            "stdio".to_string(),
            None,
            Some("some-cmd".to_string()),
            None,
        );

        assert_eq!(report.status, ServerStatus::NotAttempted);
        assert!(report.failure_message.is_none());
        assert!(report.tools.is_none());

        // Verify JSON output has neither failure_message nor tools
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("failure_message"));
        assert!(!json.contains("\"tools\""));
    }

    #[test]
    fn event_id_has_correct_prefix() {
        let id = CatalogEnvelope::generate_event_id();
        assert!(id.starts_with("cat_"));
        assert!(id.len() > 4); // cat_ + ULID
    }

    #[test]
    fn sanitize_url_extracts_origin() {
        assert_eq!(
            sanitize_url("https://user:pass@example.com:8080/path?query=1"),
            "https://example.com:8080"
        );
        assert_eq!(
            sanitize_url("http://localhost:3000/mcp"),
            "http://localhost:3000"
        );
    }

    #[test]
    fn command_basename_extracts_filename() {
        assert_eq!(command_basename("/usr/local/bin/some-mcp"), "some-mcp");
        assert_eq!(command_basename("./node_modules/.bin/mcp"), "mcp");
        assert_eq!(command_basename("uvx"), "uvx");
    }
}
