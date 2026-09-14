//! Scratchpad configuration types.
//!
//! These are the pure, serializable knobs parsed from TOML. The runtime
//! machinery that uses them (storage, context budget, interception wrapper,
//! the eight exploration tools) lives in the `aura` crate's `scratchpad`
//! module.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

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

/// Per-server scratchpad settings, configured at `[mcp.servers.<name>.scratchpad]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerScratchpadConfig {
    /// Tool glob pattern → argument fields that accept a scratchpad file
    /// reference, from `[mcp.servers.<name>.scratchpad.by_reference]`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub by_reference: HashMap<String, Vec<FieldPath>>,
    /// Tool glob pattern → output interception threshold (every other key of
    /// the table).
    #[serde(flatten)]
    pub tools: HashMap<String, ScratchpadToolEntry>,
}

/// Path to a string field inside a tool's arguments: dot-separated property
/// names, with `[]` marking a property that holds an array of objects
/// (`files[].content`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FieldPath {
    raw: String,
    segments: Vec<FieldSegment>,
}

/// One property step of a [`FieldPath`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldSegment {
    pub name: String,
    /// The property holds an array whose items the path continues into.
    pub array: bool,
}

impl FieldPath {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let invalid = |why: &str| format!("invalid by_reference field path '{raw}': {why}");
        let mut segments = Vec::new();
        for part in raw.split('.') {
            let (name, array) = match part.strip_suffix("[]") {
                Some(name) => (name, true),
                None => (part, false),
            };
            if name.is_empty() {
                return Err(invalid("empty property name"));
            }
            if name.contains(['[', ']']) {
                return Err(invalid("`[]` may only follow a property name"));
            }
            segments.push(FieldSegment {
                name: name.to_string(),
                array,
            });
        }
        if segments.last().is_some_and(|s| s.array) {
            return Err(invalid(
                "the last segment must name a string field, not an array",
            ));
        }
        Ok(Self {
            raw: raw.to_string(),
            segments,
        })
    }

    /// Property steps leading to the object that holds the field.
    pub fn parents(&self) -> &[FieldSegment] {
        &self.segments[..self.segments.len() - 1]
    }

    /// Name of the string field itself.
    pub fn field(&self) -> &str {
        &self.segments[self.segments.len() - 1].name
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl TryFrom<String> for FieldPath {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<FieldPath> for String {
    fn from(path: FieldPath) -> Self {
        path.raw
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
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

    #[test]
    fn field_path_parses_nested_and_array_segments() {
        let path = FieldPath::parse("changes[].file.body").unwrap();
        assert_eq!(path.field(), "body");
        assert_eq!(
            path.parents(),
            [
                FieldSegment {
                    name: "changes".into(),
                    array: true
                },
                FieldSegment {
                    name: "file".into(),
                    array: false
                },
            ]
        );
        assert_eq!(path.to_string(), "changes[].file.body");
    }

    #[test]
    fn field_path_rejects_malformed_paths() {
        for bad in [
            "", "a..b", ".a", "a.", "files[]", "a[0].b", "a[].[]", "[].a",
        ] {
            assert!(FieldPath::parse(bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn server_scratchpad_config_empty_table_is_default() {
        let cfg: ServerScratchpadConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, ServerScratchpadConfig::default());
    }
}
