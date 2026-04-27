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
//! 4. **ScratchpadStorage** — file I/O with path validation. Files persist
//!    alongside orchestration artifacts under `{memory_dir}/.../scratchpad/`
//!    for post-hoc debugging; explicit cleanup is exposed but not auto-called.

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
    ScratchpadBuild, ScratchpadBuildInputs, build_scratchpad, count_mcp_tool_schema_tokens,
    estimate_scratchpad_overhead,
};
pub use storage::ScratchpadStorage;
pub use tools::{
    GetInTool, GrepTool, HeadTool, ItemSchemaTool, IterateOverTool, ReadTool, SchemaTool,
    SliceTool, all_tool_definitions, emit_scratchpad_tool_events_enabled, is_scratchpad_tool,
};
pub use wrapper::ScratchpadWrapper;

use crate::config::{McpConfig, glob_match};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Resolve `[mcp.servers.*.scratchpad]` patterns into a flat
/// `tool_name → min_tokens` map at boot time.
///
/// **Why server-aware resolution?** `[mcp.servers.<name>.scratchpad]`
/// blocks are scoped to a specific server, but at runtime
/// `ToolCallContext` only carries the tool's bare name (server identity is
/// dropped after rig registration). A server-agnostic pattern map would
/// silently apply Server A's `"*"` to Server B's tools and vice-versa.
///
/// Resolution rule: for each `(server, tool_name)` pair from
/// `tool_names_per_server`, glob-match against THAT server's patterns and
/// pick the **most-specific** match (longest pattern; on length ties,
/// smallest threshold). Record `tool_name → threshold` only if at least
/// one of that server's patterns matched. The runtime wrapper then does an
/// exact `HashMap::get` — fast, deterministic, and server-correct.
///
/// **Cross-server tool-name collisions** (the same tool name exposed by
/// two servers — uncommon, since rig registers tools by name) are merged
/// by taking the smaller threshold and emitting a `tracing::warn!` at
/// boot so the ambiguity is visible.
///
/// STDIO tools are not included: `McpManager::tool_names_per_server`
/// currently only enumerates HTTP-streamable servers. STDIO scratchpad
/// support requires plumbing server_name through `tool_definitions`.
pub fn scratchpad_tool_map(
    mcp: Option<&McpConfig>,
    tool_names_per_server: &HashMap<String, Vec<String>>,
) -> HashMap<String, usize> {
    let Some(mcp) = mcp else {
        return HashMap::new();
    };

    let mut resolved: HashMap<String, usize> = HashMap::new();
    for (server_name, tools) in tool_names_per_server {
        let Some(server_cfg) = mcp.servers.get(server_name) else {
            continue;
        };
        let patterns = server_cfg.scratchpad();
        if patterns.is_empty() {
            continue;
        }

        for tool_name in tools {
            // Find the most-specific pattern (longest; tie → smallest threshold)
            // that matches THIS server's tool.
            let best = patterns
                .iter()
                .filter(|(pattern, _)| glob_match(pattern, tool_name))
                .min_by(|(pa, ea), (pb, eb)| {
                    pb.len()
                        .cmp(&pa.len())
                        .then(ea.min_tokens.cmp(&eb.min_tokens))
                });

            if let Some((_, entry)) = best {
                use std::collections::hash_map::Entry;
                match resolved.entry(tool_name.clone()) {
                    Entry::Vacant(slot) => {
                        slot.insert(entry.min_tokens);
                    }
                    Entry::Occupied(mut slot) => {
                        let existing = *slot.get();
                        let incoming = entry.min_tokens;
                        if existing != incoming {
                            tracing::warn!(
                                "scratchpad: tool '{tool_name}' exposed by multiple servers \
                                 with mismatched resolved min_tokens (existing={existing}, \
                                 from server '{server_name}'={incoming}). Taking the smaller \
                                 value ({}). Tool-name collisions across servers are \
                                 unusual — verify your MCP server configs.",
                                existing.min(incoming)
                            );
                        }
                        slot.insert(existing.min(incoming));
                    }
                }
            }
        }
    }
    resolved
}

/// True when at least one tool reachable through `mcp_filter` is keyed in
/// the resolved scratchpad map — i.e. there's something for the wrapper to
/// intercept. Empty `mcp_filter` means "all tools are reachable".
///
/// Now an exact lookup since `scratchpad_tool_map` is keyed by tool name
/// (not pattern) — no glob iteration on the hot path.
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
        reachable && scratchpad_tool_map.contains_key(name)
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
    /// Map of bare tool name → min_tokens threshold. Glob patterns from
    /// `[mcp.servers.<name>.scratchpad]` are expanded by `scratchpad_tool_map`
    /// when the agent is constructed for a request (per-server,
    /// longest-match-wins, ties broken by smallest threshold) so per-tool-call
    /// interception is an exact `HashMap::get`.
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

    // ---------------------------------------------------------------------
    // scratchpad_tool_map — server-aware boot-time pattern resolution.
    //
    // Patterns from `[mcp.servers.<name>.scratchpad]` are resolved against
    // each server's tool list and collapsed into a flat `tool_name →
    // threshold` map. Server scoping is the key invariant — Server A's
    // patterns must NOT apply to Server B's tools.
    // ---------------------------------------------------------------------

    use crate::config::{McpConfig, McpServerConfig};

    fn server_with_scratchpad(patterns: &[(&str, usize)]) -> McpServerConfig {
        let scratchpad = patterns
            .iter()
            .map(|(p, t)| ((*p).to_string(), ScratchpadToolEntry { min_tokens: *t }))
            .collect();
        McpServerConfig::HttpStreamable {
            url: "http://test".to_string(),
            headers: HashMap::new(),
            description: None,
            headers_from_request: HashMap::new(),
            scratchpad,
        }
    }

    fn mcp_with_servers(servers: Vec<(&str, McpServerConfig)>) -> McpConfig {
        McpConfig {
            servers: servers
                .into_iter()
                .map(|(name, cfg)| (name.to_string(), cfg))
                .collect(),
            sanitize_schemas: false,
        }
    }

    #[test]
    fn scratchpad_tool_map_returns_empty_when_no_mcp_config() {
        let resolved = scratchpad_tool_map(None, &HashMap::new());
        assert!(resolved.is_empty());
    }

    #[test]
    fn scratchpad_tool_map_returns_empty_when_server_has_no_scratchpad() {
        let mcp = mcp_with_servers(vec![("github", server_with_scratchpad(&[]))]);
        let tools_per_server = HashMap::from([(
            "github".to_string(),
            vec!["search_code".to_string(), "list_branches".to_string()],
        )]);
        let resolved = scratchpad_tool_map(Some(&mcp), &tools_per_server);
        assert!(resolved.is_empty());
    }

    #[test]
    fn scratchpad_tool_map_wildcard_matches_every_tool_on_that_server() {
        // Reproduces the user's original config:
        //   [mcp.servers.github.scratchpad]
        //   "*" = { min_tokens = 256 }
        // Every github tool should resolve to threshold 256.
        let mcp = mcp_with_servers(vec![("github", server_with_scratchpad(&[("*", 256)]))]);
        let tools_per_server = HashMap::from([(
            "github".to_string(),
            vec![
                "search_code".to_string(),
                "list_branches".to_string(),
                "get_file_contents".to_string(),
            ],
        )]);
        let resolved = scratchpad_tool_map(Some(&mcp), &tools_per_server);
        assert_eq!(resolved.get("search_code"), Some(&256));
        assert_eq!(resolved.get("list_branches"), Some(&256));
        assert_eq!(resolved.get("get_file_contents"), Some(&256));
        assert_eq!(resolved.len(), 3);
    }

    /// Critical invariant — the bug Option B fixes: per-server patterns
    /// only apply to that server's tools.
    #[test]
    fn scratchpad_tool_map_does_not_leak_patterns_across_servers() {
        let mcp = mcp_with_servers(vec![
            ("github", server_with_scratchpad(&[("*", 100)])),
            ("mezmo", server_with_scratchpad(&[("*", 5000)])),
        ]);
        let tools_per_server = HashMap::from([
            ("github".to_string(), vec!["search_code".to_string()]),
            ("mezmo".to_string(), vec!["list_pipelines".to_string()]),
        ]);
        let resolved = scratchpad_tool_map(Some(&mcp), &tools_per_server);
        // github tool gets github's threshold; mezmo tool gets mezmo's.
        assert_eq!(resolved.get("search_code"), Some(&100));
        assert_eq!(resolved.get("list_pipelines"), Some(&5000));
    }

    #[test]
    fn scratchpad_tool_map_most_specific_pattern_wins_within_a_server() {
        // README example: broad `*_list_*`, specific name override, catch-all.
        let mcp = mcp_with_servers(vec![(
            "k8s",
            server_with_scratchpad(&[
                ("*", 256),
                ("k8s_list_*", 512),
                ("k8s_list_service_monitors", 384),
            ]),
        )]);
        let tools_per_server = HashMap::from([(
            "k8s".to_string(),
            vec![
                "k8s_list_service_monitors".to_string(),
                "k8s_list_namespaces".to_string(),
                "get_log_histogram".to_string(),
            ],
        )]);
        let resolved = scratchpad_tool_map(Some(&mcp), &tools_per_server);
        assert_eq!(
            resolved.get("k8s_list_service_monitors"),
            Some(&384),
            "exact name (most specific) wins over `k8s_list_*`",
        );
        assert_eq!(
            resolved.get("k8s_list_namespaces"),
            Some(&512),
            "`k8s_list_*` wins over `*`",
        );
        assert_eq!(
            resolved.get("get_log_histogram"),
            Some(&256),
            "only `*` matches → falls through to wildcard",
        );
    }

    #[test]
    fn scratchpad_tool_map_skips_tools_with_no_matching_pattern() {
        // Server has a narrow pattern; tools that don't match aren't included.
        let mcp = mcp_with_servers(vec![(
            "k8s",
            server_with_scratchpad(&[("k8s_list_*", 512)]),
        )]);
        let tools_per_server = HashMap::from([(
            "k8s".to_string(),
            vec![
                "k8s_list_namespaces".to_string(),
                "get_log_histogram".to_string(),
            ],
        )]);
        let resolved = scratchpad_tool_map(Some(&mcp), &tools_per_server);
        assert_eq!(resolved.get("k8s_list_namespaces"), Some(&512));
        assert!(
            !resolved.contains_key("get_log_histogram"),
            "non-matching tool must not be in the map"
        );
    }

    #[test]
    fn scratchpad_tool_map_cross_server_tool_name_collision_takes_min() {
        // Pathological: two servers each register a tool called
        // `list_pipelines` with different per-server thresholds. Rig
        // registers tools by name so this would normally cause a
        // registration collision, but if it slips through the resolver
        // takes the smaller threshold deterministically.
        let mcp = mcp_with_servers(vec![
            ("server_a", server_with_scratchpad(&[("*", 100)])),
            ("server_b", server_with_scratchpad(&[("*", 5000)])),
        ]);
        let tools_per_server = HashMap::from([
            ("server_a".to_string(), vec!["list_pipelines".to_string()]),
            ("server_b".to_string(), vec!["list_pipelines".to_string()]),
        ]);
        let resolved = scratchpad_tool_map(Some(&mcp), &tools_per_server);
        assert_eq!(
            resolved.get("list_pipelines"),
            Some(&100),
            "min(100, 5000) = 100 — most aggressive interception wins",
        );
    }

    #[test]
    fn scratchpad_tool_map_ignores_servers_with_no_tools() {
        // Server is configured with patterns but `tool_names_per_server`
        // didn't list any tools (e.g., MCP discovery hasn't completed).
        let mcp = mcp_with_servers(vec![("ghost", server_with_scratchpad(&[("*", 256)]))]);
        let resolved = scratchpad_tool_map(Some(&mcp), &HashMap::new());
        assert!(resolved.is_empty());
    }

    // ---------------------------------------------------------------------
    // has_accessible_scratchpad_tool — exact lookup against the resolved
    // map (no glob iteration).
    // ---------------------------------------------------------------------

    #[test]
    fn has_accessible_returns_false_when_map_is_empty() {
        assert!(!has_accessible_scratchpad_tool(
            &["foo".to_string()],
            &[],
            &HashMap::new(),
        ));
    }

    #[test]
    fn has_accessible_returns_true_when_any_tool_is_keyed() {
        let map = HashMap::from([("foo".to_string(), 256)]);
        assert!(has_accessible_scratchpad_tool(
            &["foo".to_string(), "bar".to_string()],
            &[],
            &map,
        ));
    }

    #[test]
    fn has_accessible_filter_excludes_unreachable_tools() {
        let map = HashMap::from([("foo".to_string(), 256)]);
        // mcp_filter restricts to `bar_*`, so `foo` is not reachable even
        // though it's in the map.
        assert!(!has_accessible_scratchpad_tool(
            &["foo".to_string()],
            &["bar_*".to_string()],
            &map,
        ));
    }

    #[test]
    fn has_accessible_filter_glob_matches_against_reachable_set() {
        let map = HashMap::from([("foo_get".to_string(), 256)]);
        assert!(has_accessible_scratchpad_tool(
            &["foo_get".to_string()],
            &["foo_*".to_string()],
            &map,
        ));
    }
}
