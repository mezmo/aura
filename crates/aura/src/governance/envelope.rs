//! Catalog envelope types for governance webhook payloads.

use crate::mcp::McpManager;
use anyhow::anyhow;
use aura_config::Config;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MCP_CATALOG_EVENT_NAME: &str = "mcp_catalog";
const MCP_CATALOG_ENVELOPE_VERSION: &str = "1";

/// Wire envelope for the MCP catalog sync event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEnvelope {
    /// Event type identifier.
    pub event: &'static str,
    /// Schema version for forward compatibility.
    pub version: &'static str,
    /// Unique event ID (UUID v1).
    pub event_id: String,
    /// ISO 8601 timestamp when the event was emitted.
    pub emitted_at: DateTime<Utc>,
    /// AURA version that generated this catalog.
    pub aura_version: String,
    /// Agent entries with their MCP server configurations and tools.
    pub agent: AgentEntry,
}

/// Agent entry in the catalog envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    /// Agent identifier (from config).
    pub id: String,
    /// Human-readable agent name.
    pub name: String,
    /// LLM model identifier.
    pub model: String,
    /// MCP servers configured for this agent.
    pub mcp_servers: Vec<McpServerEntry>,
}

/// MCP server entry with connection status and discovered tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    /// Server name (config key).
    pub name: String,
    /// Transport type: `http_streamable`, `sse`, or `stdio`.
    pub transport: String,
    /// Connection status.
    pub status: McpServerStatus,
    /// Failure message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    /// Discovered tools from this server.
    pub tools: Vec<ToolEntry>,
}

impl TryFrom<&aura_events::McpServerStatus> for McpServerEntry {
    type Error = anyhow::Error;
    fn try_from(value: &aura_events::McpServerStatus) -> anyhow::Result<Self> {
        Ok(Self {
            name: value.server_name.clone(),
            transport: value.transport.clone(),
            status: McpServerStatus::try_from(value.status.as_str())?,
            failure_message: value.reason.clone(),
            tools: vec![],
        })
    }
}

/// Connection status for an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatus {
    Connected,
    Failed,
    NotAttempted,
}

impl TryFrom<&str> for McpServerStatus {
    type Error = anyhow::Error;
    fn try_from(value: &str) -> anyhow::Result<Self> {
        match value {
            "connected" => Ok(McpServerStatus::Connected),
            "failed" => Ok(McpServerStatus::Failed),
            "not_attempted" => Ok(McpServerStatus::NotAttempted),
            _ => Err(anyhow!("unsupported MCP server status string: {value}")),
        }
    }
}

/// Tool entry with full schema information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEntry {
    /// Tool name.
    pub name: String,
    /// Tool description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for tool input parameters.
    pub input_schema: serde_json::Value,
}

impl From<&rmcp::model::Tool> for ToolEntry {
    fn from(value: &rmcp::model::Tool) -> Self {
        Self {
            name: value.name.to_string(),
            description: value.description.as_ref().map(|d| d.to_string()),
            input_schema: value.schema_as_json_value(),
        }
    }
}

/// Build a catalog envelope from a single config and its MCP manager.
///
/// This is the single-agent path used when only one config is loaded.
pub async fn build_catalog(config: &aura_config::Config) -> Option<CatalogEnvelope> {
    if let Some(mcp_config) = &config.mcp
        && let Ok(manager) = McpManager::initialize_from_config(mcp_config).await
    {
        let server_entries = build_server_entries(&manager);
        let agent = build_agent_entry(config, server_entries);
        let event_id = Uuid::new_v4().to_string();
        let emitted_at = Utc::now();
        let aura_version = env!("CARGO_PKG_VERSION").to_string();
        return Some(CatalogEnvelope {
            event: MCP_CATALOG_EVENT_NAME,
            version: MCP_CATALOG_ENVELOPE_VERSION,
            event_id,
            emitted_at,
            aura_version,
            agent,
        });
    }
    None
}

/// Build an agent entry from a config and its MCP manager.
fn build_agent_entry(config: &Config, mcp_servers: Vec<McpServerEntry>) -> AgentEntry {
    // Use alias as id if set, otherwise fall back to name
    let id = config
        .agent
        .alias
        .clone()
        .unwrap_or_else(|| config.agent.name.clone());
    let name = config.agent.name.clone();
    let model = config.agent.llm.model_name().to_string();

    AgentEntry {
        id,
        name,
        model,
        mcp_servers,
    }
}

/// Build MCP server entries from config and runtime state.
fn build_server_entries(mcp_manager: &McpManager) -> Vec<McpServerEntry> {
    let mut res = vec![];
    for server in &mcp_manager.server_status_snapshot() {
        let mut e = McpServerEntry::try_from(server).unwrap();
        e.tools = mcp_manager
            .get_tool_definition_by_server(&server.server_name)
            .iter()
            .map(ToolEntry::from)
            .collect();
        res.push(e);
    }

    // Sort by server name for deterministic output
    res.sort_by(|a, b| a.name.cmp(&b.name));
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{ConnectionStatus, ServerInfo};
    use std::borrow::Cow;
    use std::sync::Arc;

    // -----------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------

    fn tool_fixture(name: &str, description: Option<&str>) -> rmcp::model::Tool {
        rmcp::model::Tool {
            name: Cow::Owned(name.to_string()),
            title: None,
            description: description.map(|d| Cow::Owned(d.to_string())),
            input_schema: Arc::new(serde_json::Map::from_iter(vec![(
                "type".to_string(),
                serde_json::json!("object"),
            )])),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        }
    }

    fn server_info_fixture(name: &str, transport: &str, status: ConnectionStatus) -> ServerInfo {
        ServerInfo {
            name: name.to_string(),
            description: None,
            tools_count: 0,
            status,
            transport: transport.to_string(),
        }
    }

    /// Minimal agent config: only `[agent]`/`[agent.llm]`, plus caller-supplied
    /// extra agent fields (e.g. `alias = "..."`) and extra top-level sections
    /// (e.g. `[mcp.servers.dead]`).
    fn agent_config_toml(
        name: &str,
        extra_agent_fields: &str,
        extra_sections: &str,
    ) -> aura_config::Config {
        aura_config::load_config_from_str(&format!(
            r#"
[agent]
name = "{name}"
system_prompt = "p"
{extra_agent_fields}
[agent.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"
{extra_sections}
"#
        ))
        .unwrap_or_else(|e| panic!("config should parse: {e}"))
    }

    // -----------------------------------------------------------------
    // McpServerStatus::try_from(&str)
    // -----------------------------------------------------------------

    #[test]
    fn server_status_try_from_str_parses_all_variants() {
        assert_eq!(
            McpServerStatus::try_from("connected").unwrap(),
            McpServerStatus::Connected
        );
        assert_eq!(
            McpServerStatus::try_from("failed").unwrap(),
            McpServerStatus::Failed
        );
        assert_eq!(
            McpServerStatus::try_from("not_attempted").unwrap(),
            McpServerStatus::NotAttempted
        );
    }

    #[test]
    fn server_status_try_from_str_rejects_unknown_value() {
        assert!(McpServerStatus::try_from("connecting").is_err());
    }

    #[test]
    fn server_status_try_from_str_is_case_sensitive() {
        // The wire status comes from aura_events::McpServerStatus.status, which
        // is always lowercased by its producer; the match must not accept
        // near-miss casing as if it were normalized.
        assert!(McpServerStatus::try_from("Connected").is_err());
        assert!(McpServerStatus::try_from("CONNECTED").is_err());
        assert!(McpServerStatus::try_from("Failed").is_err());
        assert!(McpServerStatus::try_from("Not_Attempted").is_err());
    }

    #[test]
    fn server_status_try_from_str_rejects_empty_string() {
        assert!(McpServerStatus::try_from("").is_err());
    }

    #[test]
    fn server_status_try_from_str_rejects_surrounding_whitespace() {
        // No trimming: a value with incidental whitespace must not silently match.
        assert!(McpServerStatus::try_from(" connected").is_err());
        assert!(McpServerStatus::try_from("connected ").is_err());
        assert!(McpServerStatus::try_from("connected\n").is_err());
    }

    #[test]
    fn server_status_try_from_str_rejects_near_miss_separators() {
        // Guards the underscore convention specifically: a hyphen or missing
        // separator must not be treated as equivalent to "not_attempted".
        assert!(McpServerStatus::try_from("not-attempted").is_err());
        assert!(McpServerStatus::try_from("notattempted").is_err());
    }

    #[test]
    fn server_status_try_from_str_error_message_names_the_offending_value() {
        let err = McpServerStatus::try_from("connecting").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("connecting"),
            "error must name the offending value for triage, got: {message}"
        );
    }

    // -----------------------------------------------------------------
    // McpServerEntry: TryFrom<&aura_events::McpServerStatus>
    // -----------------------------------------------------------------

    #[test]
    fn server_entry_from_connected_status_has_no_failure_message() {
        let status = aura_events::McpServerStatus {
            server_name: "kubernetes".to_string(),
            transport: "http_streamable".to_string(),
            status: "connected".to_string(),
            tools_count: 3,
            reason: None,
        };

        let entry = McpServerEntry::try_from(&status).unwrap();
        assert_eq!(entry.name, "kubernetes");
        assert_eq!(entry.transport, "http_streamable");
        assert_eq!(entry.status, McpServerStatus::Connected);
        assert_eq!(entry.failure_message, None);
        assert!(
            entry.tools.is_empty(),
            "tools are attached later by build_server_entries, not by this conversion"
        );
    }

    #[test]
    fn server_entry_from_failed_status_carries_failure_message() {
        let status = aura_events::McpServerStatus {
            server_name: "pagerduty".to_string(),
            transport: "sse".to_string(),
            status: "failed".to_string(),
            tools_count: 0,
            reason: Some("connection refused".to_string()),
        };

        let entry = McpServerEntry::try_from(&status).unwrap();
        assert_eq!(entry.status, McpServerStatus::Failed);
        assert_eq!(entry.failure_message.as_deref(), Some("connection refused"));
    }

    #[test]
    fn server_entry_from_not_attempted_status_has_no_failure_message() {
        let status = aura_events::McpServerStatus {
            server_name: "unused".to_string(),
            transport: "stdio".to_string(),
            status: "not_attempted".to_string(),
            tools_count: 0,
            reason: None,
        };

        let entry = McpServerEntry::try_from(&status).unwrap();
        assert_eq!(entry.status, McpServerStatus::NotAttempted);
        assert_eq!(entry.failure_message, None);
    }

    #[test]
    fn server_entry_from_unrecognized_status_string_errs() {
        let status = aura_events::McpServerStatus {
            server_name: "weird".to_string(),
            transport: "http_streamable".to_string(),
            status: "connecting".to_string(),
            tools_count: 0,
            reason: None,
        };

        assert!(McpServerEntry::try_from(&status).is_err());
    }

    // -----------------------------------------------------------------
    // ToolEntry: From<&rmcp::model::Tool>
    // -----------------------------------------------------------------

    #[test]
    fn tool_entry_from_tool_maps_name_description_and_schema() {
        let tool = tool_fixture("list_pods", Some("List pods in a namespace"));

        let entry = ToolEntry::from(&tool);
        assert_eq!(entry.name, "list_pods");
        assert_eq!(
            entry.description.as_deref(),
            Some("List pods in a namespace")
        );
        assert_eq!(entry.input_schema.get("type").unwrap(), "object");
    }

    #[test]
    fn tool_entry_from_tool_without_description_is_none() {
        let tool = tool_fixture("no_desc", None);

        let entry = ToolEntry::from(&tool);
        assert_eq!(entry.description, None);
    }

    // -----------------------------------------------------------------
    // Serialization: skip_serializing_if / rename_all behavior
    // -----------------------------------------------------------------

    #[test]
    fn mcp_server_entry_omits_failure_message_field_when_none() {
        let entry = McpServerEntry {
            name: "kubernetes".to_string(),
            transport: "http_streamable".to_string(),
            status: McpServerStatus::Connected,
            failure_message: None,
            tools: vec![],
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("failure_message"),
            "failure_message must be omitted, not null, when None: {json}"
        );
    }

    #[test]
    fn mcp_server_entry_includes_failure_message_field_when_some() {
        let entry = McpServerEntry {
            name: "pagerduty".to_string(),
            transport: "sse".to_string(),
            status: McpServerStatus::Failed,
            failure_message: Some("connection refused".to_string()),
            tools: vec![],
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"failure_message\":\"connection refused\""));
    }

    #[test]
    fn tool_entry_omits_description_field_when_none() {
        let entry = ToolEntry {
            name: "no_desc".to_string(),
            description: None,
            input_schema: serde_json::json!({}),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("description"));
    }

    #[test]
    fn mcp_server_status_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&McpServerStatus::Connected).unwrap(),
            "\"connected\""
        );
        assert_eq!(
            serde_json::to_string(&McpServerStatus::Failed).unwrap(),
            "\"failed\""
        );
        assert_eq!(
            serde_json::to_string(&McpServerStatus::NotAttempted).unwrap(),
            "\"not_attempted\""
        );
    }

    // -----------------------------------------------------------------
    // build_agent_entry
    // -----------------------------------------------------------------

    #[test]
    fn build_agent_entry_uses_alias_as_id_when_set() {
        let config = agent_config_toml("sre-agent", "alias = \"prod-sre\"", "");
        let entry = build_agent_entry(&config, vec![]);
        assert_eq!(entry.id, "prod-sre");
        assert_eq!(entry.name, "sre-agent");
        assert_eq!(entry.model, "gpt-4o");
    }

    #[test]
    fn build_agent_entry_falls_back_to_name_when_alias_unset() {
        let config = agent_config_toml("sre-agent", "", "");
        let entry = build_agent_entry(&config, vec![]);
        assert_eq!(entry.id, "sre-agent");
        assert_eq!(entry.name, "sre-agent");
    }

    #[test]
    fn build_agent_entry_carries_through_mcp_servers_unchanged() {
        let config = agent_config_toml("sre-agent", "", "");
        let servers = vec![McpServerEntry {
            name: "kubernetes".to_string(),
            transport: "http_streamable".to_string(),
            status: McpServerStatus::Connected,
            failure_message: None,
            tools: vec![],
        }];

        let entry = build_agent_entry(&config, servers);
        assert_eq!(entry.mcp_servers.len(), 1);
        assert_eq!(entry.mcp_servers[0].name, "kubernetes");
    }

    // -----------------------------------------------------------------
    // build_server_entries
    // -----------------------------------------------------------------

    #[test]
    fn build_server_entries_empty_manager_yields_empty_vec() {
        let manager = McpManager::with_sanitization(true);
        assert!(build_server_entries(&manager).is_empty());
    }

    #[test]
    fn build_server_entries_attaches_tools_from_matching_transport_map() {
        let mut manager = McpManager::with_sanitization(true);
        manager.server_info.insert(
            "kubernetes".to_string(),
            server_info_fixture("kubernetes", "http_streamable", ConnectionStatus::Connected),
        );
        manager.streamable_tools.insert(
            "kubernetes".to_string(),
            vec![tool_fixture("list_pods", None)],
        );

        let entries = build_server_entries(&manager);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "kubernetes");
        assert_eq!(entries[0].transport, "http_streamable");
        assert_eq!(entries[0].status, McpServerStatus::Connected);
        assert_eq!(entries[0].tools.len(), 1);
        assert_eq!(entries[0].tools[0].name, "list_pods");
    }

    #[test]
    fn build_server_entries_finds_tools_regardless_of_which_transport_map_holds_them() {
        // A server's tools live in whichever transport-keyed map matches its
        // transport; build_server_entries must find them via any of the three.
        let mut manager = McpManager::with_sanitization(true);
        manager.server_info.insert(
            "legacy".to_string(),
            server_info_fixture("legacy", "sse", ConnectionStatus::Connected),
        );
        manager.sse_tools.insert(
            "legacy".to_string(),
            vec![tool_fixture("legacy_tool", None)],
        );

        let entries = build_server_entries(&manager);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tools[0].name, "legacy_tool");
    }

    #[test]
    fn build_server_entries_failed_server_has_no_tools_and_carries_reason() {
        let mut manager = McpManager::with_sanitization(true);
        manager.server_info.insert(
            "pagerduty".to_string(),
            server_info_fixture(
                "pagerduty",
                "sse",
                ConnectionStatus::Failed("connection refused".to_string()),
            ),
        );

        let entries = build_server_entries(&manager);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, McpServerStatus::Failed);
        assert_eq!(
            entries[0].failure_message.as_deref(),
            Some("connection refused")
        );
        assert!(entries[0].tools.is_empty());
    }

    #[test]
    fn build_server_entries_sorted_by_name_regardless_of_insertion_order() {
        let mut manager = McpManager::with_sanitization(true);
        for name in ["zeta", "alpha", "mu"] {
            manager.server_info.insert(
                name.to_string(),
                server_info_fixture(name, "stdio", ConnectionStatus::NotAttempted),
            );
        }

        let entries = build_server_entries(&manager);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
    }

    // -----------------------------------------------------------------
    // build_catalog
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn build_catalog_skips_configs_without_an_mcp_table() {
        let config = agent_config_toml("solo", "", "");
        let envelope = build_catalog(&config).await;
        // Current behavior: a config with no [mcp] table produces no catalog
        // entry at all (build_catalog only pushes when config.mcp is Some).
        assert!(envelope.is_none());
    }

    #[tokio::test]
    async fn build_catalog_reports_a_failed_server_for_an_unreachable_mcp_server() {
        let config = agent_config_toml(
            "sre-agent",
            "",
            "[mcp.servers.dead]\ntransport = \"http_streamable\"\nurl = \"http://127.0.0.1:9/mcp\"\n",
        );

        let envelope = build_catalog(&config).await.unwrap();
        assert_eq!(envelope.agent.id, "sre-agent");
        assert_eq!(envelope.agent.model, "gpt-4o");
        assert_eq!(envelope.agent.mcp_servers.len(), 1);
        assert_eq!(envelope.agent.mcp_servers[0].name, "dead");
        assert_eq!(
            envelope.agent.mcp_servers[0].status,
            McpServerStatus::Failed
        );
        assert!(envelope.agent.mcp_servers[0].tools.is_empty());
    }
}
