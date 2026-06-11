//! Scratchpad configuration types.
//!
//! These are the pure, serializable knobs parsed from TOML. The runtime
//! machinery that uses them (storage, context budget, interception wrapper,
//! the eight exploration tools) lives in the `aura` crate's `scratchpad`
//! module.

use serde::{Deserialize, Serialize};

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
