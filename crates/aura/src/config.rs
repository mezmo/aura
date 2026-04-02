/// Configuration structs for building Rig agents
/// These are pure Rust structs without any TOML-specific dependencies
use crate::orchestration::OrchestrationConfig;
use crate::tool_wrapper::{ToolCallContext, ToolWrapper};
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
use std::sync::Arc;

/// Type alias for tool context factory function.
pub type ToolContextFactory = Arc<dyn Fn(&str) -> ToolCallContext + Send + Sync>;

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
#[derive(Serialize, Deserialize)]
pub struct AgentConfig {
    pub llm: LlmConfig,
    pub agent: AgentSettings,
    pub vector_stores: Vec<VectorStoreConfig>,
    pub mcp: Option<McpConfig>,
    pub tools: Option<ToolsConfig>,
    /// Orchestration mode configuration (multi-agent workflows)
    #[serde(default)]
    pub orchestration: Option<OrchestrationConfig>,

    // === Extension fields (not serialized) ===
    // These allow callers to customize agent building without modifying the builder.
    // The orchestrator uses these to inject tool wrappers and override preambles.
    /// Optional tool wrapper applied to all MCP tools (not serialized).
    /// When set, all HTTP/SSE MCP tools are wrapped with this wrapper.
    #[serde(skip)]
    pub tool_wrapper: Option<Arc<dyn ToolWrapper + Send + Sync>>,

    /// Factory for creating ToolCallContext per tool (not serialized).
    /// Allows callers to inject metadata (task_id, attempt) into wrapped tool calls.
    #[serde(skip)]
    pub tool_context_factory: Option<ToolContextFactory>,

    /// Override for system prompt (not serialized).
    /// When set, this replaces agent.system_prompt entirely.
    #[serde(skip)]
    pub preamble_override: Option<String>,

    /// Glob patterns for filtering which MCP tools to include.
    /// When set, only tools matching at least one pattern are added.
    /// Supports glob syntax: `*` (any chars), `?` (single char).
    /// Empty or None means all tools are included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_filter: Option<Vec<String>>,

    /// Shared persistence for injecting `read_artifact` tool into workers (not serialized).
    /// When set, workers get access to result artifacts via the read_artifact tool.
    #[serde(skip)]
    pub orchestration_persistence:
        Option<Arc<tokio::sync::Mutex<crate::orchestration::ExecutionPersistence>>>,

    /// Shared conversation history for injecting `get_conversation_context` tool into workers (not serialized).
    /// When set, workers can retrieve conversation history on demand.
    #[serde(skip)]
    pub orchestration_chat_history: Option<Arc<Vec<rig::completion::Message>>>,

    /// Session ID for grouping orchestration runs under a shared namespace (not serialized).
    /// When set, persistence paths become `{memory_dir}/{session_id}/{run_id}/...`.
    /// Threaded from the web server's `chat_session_id`.
    #[serde(skip)]
    pub session_id: Option<String>,
}

/// Configuration for TodoWrite/ReadTodos tool injection.
#[derive(Debug, Clone, Default)]
pub struct TodoToolsConfig {
    /// Optional directory for persisting plans.
    /// If None, plans are stored in-memory only.
    pub plan_dir: Option<String>,
}

// Manual Clone implementation because Arc<dyn Trait> fields require special handling
impl Clone for AgentConfig {
    fn clone(&self) -> Self {
        Self {
            llm: self.llm.clone(),
            agent: self.agent.clone(),
            vector_stores: self.vector_stores.clone(),
            mcp: self.mcp.clone(),
            tools: self.tools.clone(),
            orchestration: self.orchestration.clone(),
            // Arc fields clone the Arc (shared reference)
            tool_wrapper: self.tool_wrapper.clone(),
            tool_context_factory: self.tool_context_factory.clone(),
            preamble_override: self.preamble_override.clone(),
            mcp_filter: self.mcp_filter.clone(),
            orchestration_persistence: self.orchestration_persistence.clone(),
            orchestration_chat_history: self.orchestration_chat_history.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

// Manual Debug implementation because Arc<dyn Trait> fields don't implement Debug
impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("llm", &self.llm)
            .field("agent", &self.agent)
            .field("vector_stores", &self.vector_stores)
            .field("mcp", &self.mcp)
            .field("tools", &self.tools)
            .field("orchestration", &self.orchestration)
            .field(
                "tool_wrapper",
                &self.tool_wrapper.as_ref().map(|_| "<wrapper>"),
            )
            .field(
                "tool_context_factory",
                &self.tool_context_factory.as_ref().map(|_| "<factory>"),
            )
            .field("preamble_override", &self.preamble_override)
            .field("mcp_filter", &self.mcp_filter)
            .field(
                "orchestration_persistence",
                &self
                    .orchestration_persistence
                    .as_ref()
                    .map(|_| "<persistence>"),
            )
            .field(
                "orchestration_chat_history",
                &self
                    .orchestration_chat_history
                    .as_ref()
                    .map(|h| format!("<{} messages>", h.len())),
            )
            .field("session_id", &self.session_id)
            .finish()
    }
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

    /// Get provider name and model identifier as a tuple.
    pub fn model_info(&self) -> (&str, &str) {
        match self {
            LlmConfig::OpenAI { model, .. } => ("openai", model),
            LlmConfig::Anthropic { model, .. } => ("anthropic", model),
            LlmConfig::Bedrock { model, .. } => ("bedrock", model),
            LlmConfig::Gemini { model, .. } => ("gemini", model),
            LlmConfig::Ollama { model, .. } => ("ollama", model),
        }
    }
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
    /// Context window size in tokens. Used for usage percentage reporting.
    #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
    pub context_window: Option<u32>,
    /// Additional provider-specific parameters merged into the API request.
    /// Provider-agnostic: works for Anthropic thinking, Gemini thinking budget, etc.
    pub additional_params: Option<serde_json::Value>,
    /// Glob patterns for filtering which MCP tools to include.
    /// When set, only tools matching at least one pattern are added.
    /// Supports glob syntax: `*` (any chars), `?` (single char).
    /// Empty or None means all tools are included.
    /// Can be set via `[agent].mcp_filter` in TOML for single-agent configs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_filter: Option<Vec<String>>,
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
                context_window: None,
                additional_params: None,
                mcp_filter: None,
            },
            vector_stores: Vec::new(),
            mcp: None,
            tools: None,
            orchestration: None,
            // Extension fields default to None
            tool_wrapper: None,
            tool_context_factory: None,
            preamble_override: None,
            mcp_filter: None,
            orchestration_persistence: None,
            orchestration_chat_history: None,
            session_id: None,
        }
    }
}

impl AgentConfig {
    /// Check if orchestration mode is enabled.
    ///
    /// Returns true if the `[orchestration]` section exists and `enabled = true`.
    /// Used by `build_streaming_agent()` to decide between Agent and Orchestrator.
    pub fn orchestration_enabled(&self) -> bool {
        self.orchestration
            .as_ref()
            .map(|o| o.enabled)
            .unwrap_or(false)
    }

    /// Check if a tool name matches the mcp_filter patterns.
    ///
    /// Returns true if:
    /// - No filter is set (None) - all tools pass
    /// - Filter is empty - all tools pass
    /// - Tool name matches at least one pattern
    ///
    /// Checks the extension field (`self.mcp_filter`, set by orchestrator) first,
    /// then falls back to the TOML-parseable field (`self.agent.mcp_filter`).
    ///
    /// Supports simple glob patterns:
    /// - `*` matches any sequence of characters
    /// - `?` matches any single character
    pub fn tool_matches_filter(&self, tool_name: &str) -> bool {
        let effective = self.mcp_filter.as_ref().or(self.agent.mcp_filter.as_ref());
        match effective {
            None => true,
            Some(patterns) if patterns.is_empty() => true,
            Some(patterns) => patterns.iter().any(|p| glob_match(p, tool_name)),
        }
    }

    /// Get the effective system prompt for agent building.
    ///
    /// Returns `preamble_override` if set, otherwise `agent.system_prompt`.
    /// This allows callers (like Orchestrator) to customize the preamble
    /// without the builder knowing about orchestration.
    pub fn effective_preamble(&self) -> &str {
        self.preamble_override
            .as_deref()
            .unwrap_or(&self.agent.system_prompt)
    }
}

/// Simple glob pattern matching for tool name filtering.
///
/// Supports:
/// - `*` matches any sequence of characters (including empty)
/// - `?` matches exactly one character
///
/// Examples:
/// - `mezmo_*` matches `mezmo_logs`, `mezmo_pipelines`
/// - `*Query*` matches `ListQuery`, `QueryKnowledgeBases`
/// - `tool_?` matches `tool_a`, `tool_b`
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();

    fn match_recursive(pattern: &[char], text: &[char]) -> bool {
        match (pattern.first(), text.first()) {
            // Both exhausted - match!
            (None, None) => true,
            // Pattern exhausted but text remains - no match
            (None, Some(_)) => false,
            // Wildcard * - try matching zero or more characters
            (Some('*'), _) => {
                // Try matching zero characters (skip *)
                if match_recursive(&pattern[1..], text) {
                    return true;
                }
                // Try matching one character and continue with *
                if !text.is_empty() && match_recursive(pattern, &text[1..]) {
                    return true;
                }
                false
            }
            // Text exhausted but pattern has non-* remaining - check for trailing *s
            (Some(p), None) => *p == '*' && match_recursive(&pattern[1..], text),
            // Single character wildcard ?
            (Some('?'), Some(_)) => match_recursive(&pattern[1..], &text[1..]),
            // Literal character match
            (Some(p), Some(t)) => *p == *t && match_recursive(&pattern[1..], &text[1..]),
        }
    }

    match_recursive(&pattern, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn test_glob_match_star() {
        assert!(glob_match("mezmo_*", "mezmo_logs"));
        assert!(glob_match("mezmo_*", "mezmo_pipelines"));
        assert!(glob_match("mezmo_*", "mezmo_")); // empty suffix
        assert!(!glob_match("mezmo_*", "other_logs"));
    }

    #[test]
    fn test_glob_match_star_middle() {
        assert!(glob_match("*Query*", "QueryKnowledgeBases"));
        assert!(glob_match("*Query*", "ListQuery"));
        assert!(glob_match("*Query*", "Query"));
        assert!(!glob_match("*Query*", "ListKnowledge"));
    }

    #[test]
    fn test_glob_match_question() {
        assert!(glob_match("tool_?", "tool_a"));
        assert!(glob_match("tool_?", "tool_1"));
        assert!(!glob_match("tool_?", "tool_ab")); // too long
        assert!(!glob_match("tool_?", "tool_")); // too short
    }

    #[test]
    fn test_glob_match_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_tool_matches_filter_none() {
        let config = AgentConfig::default();
        assert!(config.tool_matches_filter("any_tool"));
    }

    #[test]
    fn test_tool_matches_filter_empty() {
        let config = AgentConfig {
            mcp_filter: Some(vec![]),
            ..Default::default()
        };
        assert!(config.tool_matches_filter("any_tool"));
    }

    #[test]
    fn test_tool_matches_filter_patterns() {
        let config = AgentConfig {
            mcp_filter: Some(vec![
                "mezmo_*".to_string(),
                "QueryKnowledgeBases".to_string(),
            ]),
            ..Default::default()
        };

        assert!(config.tool_matches_filter("mezmo_logs"));
        assert!(config.tool_matches_filter("mezmo_pipelines"));
        assert!(config.tool_matches_filter("QueryKnowledgeBases"));
        assert!(!config.tool_matches_filter("other_tool"));
    }

    // --- lenient_int tests (from helm YAML config rendering) ---
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

#[cfg(test)]
mod lenient_int_tests {
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
