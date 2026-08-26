//! Governance subsystem: catalog-sync webhooks for MCP tool discovery.
//!
//! The `/governance sync` command (CLI/REPL) discovers all configured MCP
//! servers, collects their tool catalogs, and POSTs a single bundled snapshot
//! to a configured webhook. This is standalone-mode only — no HTTP endpoint.
//!
//! ## Wire format
//!
//! The envelope is a JSON object with:
//! - `event`: always `"mcp_catalog"`
//! - `version`: schema version (currently `"1"`)
//! - `event_id`: unique identifier (format: `cat_<ULID>`)
//! - `emitted_at`: ISO 8601 timestamp
//! - `aura_version`: the AURA process version
//! - `agents`: array of agent catalog entries
//!
//! Each agent entry contains:
//! - `id`: agent identifier (from config)
//! - `name`: agent name (from config)
//! - `description`: optional agent description
//! - `model`: the LLM model name
//! - `mcp_servers`: flat array of MCP server reports, sorted by name
//!
//! Each MCP server report contains:
//! - `name`: server name
//! - `transport`: `"http_streamable"`, `"sse"`, or `"stdio"`
//! - `url` or `command`: transport-specific endpoint
//! - `description`: optional server description
//! - `status`: `"connected"`, `"failed"`, or `"not_attempted"`
//! - `failure_message`: present iff `status == "failed"`
//! - `tools`: present iff `status == "connected"`

mod client;
mod envelope;

pub use client::{DeliveryError, DeliveryReport, deliver};
pub use envelope::{
    AgentCatalogEntry, CatalogEnvelope, McpServerReport, McpToolReport, ServerStatus,
    build_catalog, build_catalog_multi,
};
