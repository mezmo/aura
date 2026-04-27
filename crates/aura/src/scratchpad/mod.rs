//! Scratchpad module — intercepts large MCP tool outputs and provides
//! read-only tools for the LLM to selectively explore the data.
//!
//! # Architecture
//!
//! 1. **ScratchpadWrapper** (ToolWrapper) — intercepts flagged tool outputs
//!    exceeding a size threshold, writes them to disk, returns a summary pointer.
//! 2. **Scratchpad tools** — eight Rig native tools that read scratchpad files:
//!    `head`, `slice`, `grep`, `schema`, `get_in`, `iterate_over`, `item_schema`, `read`.
//! 3. **ContextBudget** — tracks estimated token usage to prevent overflow.
//! 4. **ScratchpadStorage** — file I/O with path validation and cleanup.

pub mod context_budget;
pub mod schema;
pub mod setup;
pub mod storage;
pub mod tools;
pub mod wrapper;

pub use context_budget::{
    ContextBudget, ExtractionLimitExceeded, TiktokenCounter, TokenCounter,
    token_counter_for_provider,
};
pub use setup::{
    ScratchpadBuild, ScratchpadBuildInputs, build_scratchpad, estimate_scratchpad_overhead,
};
pub use storage::ScratchpadStorage;
pub use tools::{
    GetInTool, GrepTool, HeadTool, ItemSchemaTool, IterateOverTool, ReadTool, SchemaTool,
    SliceTool, all_tool_definitions,
};
pub use wrapper::ScratchpadWrapper;

use crate::config::{McpConfig, glob_match};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Rough per-MCP-tool schema token estimate used when seeding the initial
/// `ContextBudget`. The LLM's input_tokens feedback corrects drift each turn,
/// so this only needs to be in the right order of magnitude.
pub const MCP_TOOL_SCHEMA_TOKEN_ESTIMATE: usize = 200;

/// Build the map of tool-name glob patterns → `min_tokens` threshold, collected
/// from every `[mcp.servers.*.scratchpad]` section. Patterns are matched
/// against tool names via `glob_match` at interception time.
pub fn scratchpad_tool_map(mcp: Option<&McpConfig>) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    if let Some(mcp) = mcp {
        for server in mcp.servers.values() {
            for (pattern, entry) in server.scratchpad() {
                map.insert(pattern.clone(), entry.min_tokens);
            }
        }
    }
    map
}

/// True when at least one tool reachable through `mcp_filter` matches a
/// scratchpad threshold pattern — i.e. there's something for the wrapper
/// to intercept. Empty `mcp_filter` means "all tools are reachable".
pub fn has_accessible_scratchpad_tool(
    tool_names: &[String],
    mcp_filter: &[String],
    scratchpad_tool_map: &HashMap<String, usize>,
) -> bool {
    if scratchpad_tool_map.is_empty() {
        return false;
    }
    tool_names.iter().any(|name| {
        let reachable = mcp_filter.is_empty() || mcp_filter.iter().any(|p| glob_match(p, name));
        reachable
            && scratchpad_tool_map
                .keys()
                .any(|pattern| glob_match(pattern, name))
    })
}

/// Token cost of the 8 scratchpad tool definitions (name + description + params).
/// Used to seed `ContextBudget::initial_used`.
pub fn scratchpad_tool_schema_tokens(counter: &dyn TokenCounter) -> usize {
    all_tool_definitions()
        .iter()
        .map(|def| {
            counter.count_tokens(&def.name)
                + counter.count_tokens(&def.description)
                + counter.count_tokens(&def.parameters.to_string())
        })
        .sum()
}

// ============================================================================
// ScratchpadConfig (agent + worker level)
// ============================================================================

/// Scratchpad configuration.
///
/// Configured at `[agent.scratchpad]` for the default (inherited by all workers),
/// and optionally overridden at `[orchestration.worker.<name>.scratchpad]`.
/// A worker's effective config is the agent defaults merged with any overrides
/// on the worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScratchpadConfig {
    /// Whether scratchpad is active for this agent/worker.
    #[serde(default)]
    pub enabled: bool,
    /// Fraction (0.0–1.0) of the context window reserved for reasoning + output.
    #[serde(default = "default_context_safety_margin")]
    pub context_safety_margin: f32,
    /// Maximum tokens a single extraction tool may return.
    #[serde(default = "default_max_extraction_tokens")]
    pub max_extraction_tokens: usize,
    /// Extra turns added when scratchpad is active (to account for exploration).
    #[serde(default = "default_turn_depth_bonus")]
    pub turn_depth_bonus: usize,
}

impl Default for ScratchpadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            context_safety_margin: default_context_safety_margin(),
            max_extraction_tokens: default_max_extraction_tokens(),
            turn_depth_bonus: default_turn_depth_bonus(),
        }
    }
}

/// Per-tool scratchpad override, configured via `[mcp.servers.<name>.scratchpad]`.
///
/// Controls when a tool's output gets intercepted and diverted to scratchpad
/// storage instead of being returned inline to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScratchpadToolEntry {
    /// Minimum output size (in tokens) before interception kicks in.
    #[serde(default = "default_scratchpad_min_tokens")]
    pub min_tokens: usize,
}

impl Default for ScratchpadToolEntry {
    fn default() -> Self {
        Self {
            min_tokens: default_scratchpad_min_tokens(),
        }
    }
}

fn default_context_safety_margin() -> f32 {
    0.20
}

fn default_max_extraction_tokens() -> usize {
    10_000
}

fn default_turn_depth_bonus() -> usize {
    6
}

fn default_scratchpad_min_tokens() -> usize {
    5_120
}

/// Runtime configuration for scratchpad tools, passed via AgentConfig extension field.
#[derive(Debug, Clone)]
pub struct ScratchpadToolsConfig {
    /// Shared storage for this request's scratchpad files.
    pub storage: Arc<ScratchpadStorage>,
    /// Context budget tracker shared across all scratchpad tools.
    pub budget: ContextBudget,
    /// Map of tool name → min_tokens threshold for scratchpad interception.
    pub scratchpad_tools: HashMap<String, usize>,
}

/// Scratchpad usage instructions appended to worker preambles.
pub const SCRATCHPAD_PREAMBLE: &str = r#"
## Scratchpad Tools

Some tool outputs are too large for the context window and have been saved to scratchpad files.
When you see a `[scratchpad: ...]` message instead of direct output, use these tools to explore:

1. **schema** — See the structure with line ranges. Works on JSON (keys, types) and Markdown (sections, keys). Start here.
2. **item_schema** — See all unique keys across items in a JSON array (e.g., `item_schema(file, 'results')`).
3. **head** — Preview the first N lines.
4. **grep** — Search for specific content with regex.
5. **get_in** — Extract a value at a nested JSON path (e.g., `results.0.title`). For large string values, use `offset` and `limit` to paginate by line.
6. **iterate_over** — Extract selected fields from every item in a JSON array (e.g., `iterate_over(file, 'results', 'id,title')`).
7. **slice** — Extract a specific line range.
8. **read** — Read the entire file (WARNING: may be large, prefer targeted tools).

**Companion files**: Large structured string values inside JSON (escaped JSON → `.json`, markdown → `.md`) are automatically extracted to companion files. Use `schema` on the companion file to see its structure, then `slice` or `grep` to explore specific sections.

**Strategy**: Use `schema` first to understand structure. For JSON arrays, use `item_schema` to discover fields, then `iterate_over` to extract them. For companion `.md` files, use `schema` to see sections, then `slice` to extract a specific section by line range. Use `get_in` or `grep` for targeted lookups. Avoid `read` unless the file is small.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratchpad_config_default_values() {
        let cfg = ScratchpadConfig::default();
        assert!(!cfg.enabled, "scratchpad should default to disabled");
        assert!(
            (cfg.context_safety_margin - 0.20).abs() < f32::EPSILON,
            "default safety margin should be 20%",
        );
        assert_eq!(cfg.max_extraction_tokens, 10_000);
        assert_eq!(cfg.turn_depth_bonus, 6);
    }

    #[test]
    fn scratchpad_config_deserialize_with_all_defaults() {
        // An empty TOML table should apply all serde defaults.
        let cfg: ScratchpadConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, ScratchpadConfig::default());
    }

    #[test]
    fn scratchpad_config_deserialize_partial_override() {
        let toml = r#"
            enabled = true
            max_extraction_tokens = 5000
        "#;
        let cfg: ScratchpadConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_extraction_tokens, 5000);
        // Untouched fields keep their defaults
        assert!((cfg.context_safety_margin - 0.20).abs() < f32::EPSILON);
        assert_eq!(cfg.turn_depth_bonus, 6);
    }

    #[test]
    fn scratchpad_tool_entry_default_min_tokens() {
        let entry = ScratchpadToolEntry::default();
        assert_eq!(entry.min_tokens, 5_120);
    }

    #[test]
    fn scratchpad_tool_entry_deserialize_defaults_when_empty() {
        let entry: ScratchpadToolEntry = toml::from_str("").unwrap();
        assert_eq!(entry, ScratchpadToolEntry::default());
    }

    #[test]
    fn scratchpad_tool_entry_custom_min_tokens() {
        let entry: ScratchpadToolEntry = toml::from_str("min_tokens = 256").unwrap();
        assert_eq!(entry.min_tokens, 256);
    }
}
