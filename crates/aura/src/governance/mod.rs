//! Governance integration for MCP tool catalog sync.
//!
//! This module provides the ability to discover all MCP tools from configured
//! servers and send a catalog snapshot to a governance webhook for policy
//! management and auditing.

mod client;
mod envelope;

pub use client::CatalogClient;
pub use envelope::{
    AgentEntry, CatalogEnvelope, McpServerEntry, McpServerStatus, ToolEntry, build_catalog,
};
