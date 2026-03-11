/// Configuration structs for building Rig agents
/// These are pure Rust structs without any TOML-specific dependencies
use serde::{Deserialize, Serialize};

/// Serde helpers for accepting whole-number floats (e.g. `8000.0`) as integers.
///
/// Helm's YAML parser represents all numbers as Go float64, so `toToml`
/// renders `max_tokens = 8000.0` instead of `8000`. Rather than fixing this
/// in Helm templates, we accept both forms on the Rust side.
pub mod lenient_int {
    use serde::{Deserialize, Deserializer};

    /// Deserialize a value that may be either an integer or a whole-number float.
    pub fn deserialize_option_u32<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u32>, D::Error> {
        Option::<f64>::deserialize(deserializer)?
            .map(|f| float_to_int(f, "u32"))
            .transpose()
    }

    pub fn deserialize_option_u64<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Option::<f64>::deserialize(deserializer)?
            .map(|f| float_to_int(f, "u64"))
            .transpose()
    }

    pub fn deserialize_option_usize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<usize>, D::Error> {
        Option::<f64>::deserialize(deserializer)?
            .map(|f| float_to_int(f, "usize"))
            .transpose()
    }

    fn float_to_int<T, E>(f: f64, type_name: &str) -> Result<T, E>
    where
        T: TryFrom<u64>,
        E: serde::de::Error,
    {
        if f < 0.0 {
            return Err(E::custom(format!(
                "expected non-negative number for {type_name}, got {f}"
            )));
        }
        if f.fract() != 0.0 {
            return Err(E::custom(format!(
                "expected whole number for {type_name}, got {f}"
            )));
        }
        let n = f as u64;
        T::try_from(n).map_err(|_| E::custom(format!("{f} out of range for {type_name}")))
    }
}
use std::collections::HashMap;

/// Reasoning effort level for GPT-5 models
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

/// Complete configuration for building an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub llm: LlmConfig,
    pub agent: AgentSettings,
    pub vector_stores: Vec<VectorStoreConfig>,
    pub mcp: Option<McpConfig>,
    pub tools: Option<ToolsConfig>,
}

/// LLM provider configuration with strong typing per provider
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum LlmConfig {
    OpenAI {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        max_tokens: Option<u32>,
    },
    Anthropic {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        max_tokens: Option<u32>,
    },
    Bedrock {
        model: String,
        region: String,
        /// AWS profile name (optional, uses default credentials if not specified)
        #[serde(default)]
        profile: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        max_tokens: Option<u32>,
    },
    Gemini {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        max_tokens: Option<u32>,
    },
    Ollama {
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        max_tokens: Option<u32>,
        /// Parse tool calls from text output (Ollama-specific workaround).
        ///
        /// Some Ollama models (especially qwen3-coder) output tool calls as text
        /// (JSON, XML, etc.) instead of using native tool_call structures. When
        /// enabled, the agent intercepts streamed text, detects tool call patterns,
        /// and executes them via MCP.
        ///
        /// Requires MCP servers to be configured - logs a warning otherwise.
        ///
        /// Flow: Config → Agent::maybe_wrap_with_fallback → FallbackToolExecutor
        /// See: `fallback_tool_parser` module for supported formats.
        #[serde(default)]
        fallback_tool_parsing: bool,
        /// Context window size (number of tokens). Maps to Ollama's `num_ctx` option.
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        num_ctx: Option<u32>,
        /// Maximum number of tokens to predict. Maps to Ollama's `num_predict` option.
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        num_predict: Option<u32>,
        /// Additional Ollama-specific parameters passed directly to the API.
        #[serde(default)]
        additional_params: Option<HashMap<String, serde_json::Value>>,
    },
}

impl LlmConfig {
    /// Check if Ollama text-to-tool fallback is enabled.
    ///
    /// Returns true only for `LlmConfig::Ollama` with `fallback_tool_parsing = true`.
    /// Other providers always return false (they use native tool calling).
    pub fn is_fallback_tool_parsing_enabled(&self) -> bool {
        matches!(
            self,
            LlmConfig::Ollama {
                fallback_tool_parsing: true,
                ..
            }
        )
    }

    /// Get the model name regardless of provider.
    pub fn model_name(&self) -> &str {
        match self {
            LlmConfig::OpenAI { model, .. }
            | LlmConfig::Anthropic { model, .. }
            | LlmConfig::Bedrock { model, .. }
            | LlmConfig::Gemini { model, .. }
            | LlmConfig::Ollama { model, .. } => model,
        }
    }
}

/// Skill configuration for on-demand loading via the load_skill tool.
///
/// Skills follow the Agent Skills specification (<https://agentskills.io/specification>).
/// Each skill is a directory containing a `SKILL.md` file with YAML frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillConfig {
    /// Unique name for this skill (must match directory name).
    /// Lowercase alphanumeric and hyphens only, 1-64 chars.
    pub name: String,
    /// Human-readable description from SKILL.md frontmatter
    pub description: String,
    /// Absolute path to the skill directory
    pub path: std::path::PathBuf,
}

/// Agent behavior settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    pub name: String,
    pub system_prompt: String,
    pub context: Vec<String>,
    pub temperature: Option<f64>,
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
    pub max_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient_int::deserialize_option_usize")]
    pub turn_depth: Option<usize>,
    /// On-demand skill definitions loaded via the load_skill tool
    #[serde(default)]
    pub skills: Vec<SkillConfig>,
}

/// Vector store configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStoreConfig {
    pub name: String,
    pub store_type: String,
    pub embedding_model: EmbeddingModelConfig,
    pub connection_string: Option<String>,
    pub url: Option<String>,
    pub collection_name: Option<String>,
    /// Optional context string to prepend to search results for better RAG integration
    pub context_prefix: Option<String>,
}

/// Embedding model configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelConfig {
    pub provider: String,
    pub model: String,
    pub api_key: String,
    pub base_url: Option<String>,
}

/// MCP (Model Context Protocol) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub sanitize_schemas: bool,
    pub servers: HashMap<String, McpServerConfig>,
}

/// Individual MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport")]
pub enum McpServerConfig {
    #[serde(rename = "stdio")]
    Stdio {
        cmd: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        description: Option<String>,
    },
    #[serde(rename = "http_streamable")]
    HttpStreamable {
        url: String,
        headers: HashMap<String, String>,
        description: Option<String>,
        headers_from_request: HashMap<String, String>,
    },
}

/// Tools configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    pub filesystem: bool,
    pub custom_tools: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            llm: LlmConfig::OpenAI {
                api_key: String::new(),
                model: "gpt-4o-mini".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "Assistant".to_string(),
                system_prompt: "You are a helpful assistant.".to_string(),
                context: vec![],
                temperature: Some(0.7),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(5),
                skills: Vec::new(),
            },
            vector_stores: Vec::new(),
            mcp: None,
            tools: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::lenient_int;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestU32 {
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        val: Option<u32>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestU64 {
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        val: Option<u64>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestUsize {
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_usize")]
        val: Option<usize>,
    }

    fn from_json<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn accepts_integer() {
        let t: TestU32 = from_json(r#"{"val": 8000}"#).unwrap();
        assert_eq!(t.val, Some(8000));
    }

    #[test]
    fn accepts_whole_float() {
        let t: TestU32 = from_json(r#"{"val": 8000.0}"#).unwrap();
        assert_eq!(t.val, Some(8000));
    }

    #[test]
    fn accepts_zero_float() {
        let t: TestU32 = from_json(r#"{"val": 0.0}"#).unwrap();
        assert_eq!(t.val, Some(0));
    }

    #[test]
    fn rejects_fractional_float() {
        let result = from_json::<TestU32>(r#"{"val": 3.14}"#);
        assert!(result.is_err());
    }

    #[test]
    fn accepts_none() {
        let t: TestU32 = from_json(r#"{}"#).unwrap();
        assert_eq!(t.val, None);
    }

    #[test]
    fn u64_whole_float() {
        let t: TestU64 = from_json(r#"{"val": 100000.0}"#).unwrap();
        assert_eq!(t.val, Some(100000));
    }

    #[test]
    fn usize_whole_float() {
        let t: TestUsize = from_json(r#"{"val": 5.0}"#).unwrap();
        assert_eq!(t.val, Some(5));
    }

    #[test]
    fn rejects_negative() {
        let result = from_json::<TestU32>(r#"{"val": -1.0}"#);
        assert!(result.is_err());
    }
}
