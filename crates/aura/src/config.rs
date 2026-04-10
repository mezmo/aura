/// Configuration structs for building Rig agents
/// These are pure Rust structs without any TOML-specific dependencies
use crate::orchestration::OrchestrationConfig;
use crate::tool_wrapper::{ToolCallContext, ToolWrapper};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::lenient_int;
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

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        };
        write!(f, "{s}")
    }
}

/// Complete configuration for building an agent
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub enum LlmConfig {
    OpenAI {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        max_tokens: Option<u64>,
        /// Context window size in tokens.
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        context_window: Option<u64>,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
        /// Controls the randomness and creativity of the llm
        #[serde(default)]
        temperature: Option<f64>,
        /// Additional provider-specific parameters merged into the API request.
        /// Provider-agnostic: works for Anthropic thinking, Gemini thinking budget, etc.
        /// Example: `{ thinking = { type = "adaptive", budget_tokens = 8000 } }`
        #[serde(default)]
        additional_params: Option<serde_json::Value>,
    },
    Anthropic {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        max_tokens: Option<u64>,
        /// Context window size in tokens.
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        context_window: Option<u64>,
        /// Controls the randomness and creativity of the llm
        #[serde(default)]
        temperature: Option<f64>,
        /// Additional provider-specific parameters merged into the API request.
        /// Provider-agnostic: works for Anthropic thinking, Gemini thinking budget, etc.
        /// Example: `{ thinking = { type = "adaptive", budget_tokens = 8000 } }`
        #[serde(default)]
        additional_params: Option<serde_json::Value>,
    },
    Bedrock {
        model: String,
        region: String,
        /// AWS profile name (optional, uses default credentials if not specified)
        #[serde(default)]
        profile: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        max_tokens: Option<u64>,
        /// Context window size in tokens.
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        context_window: Option<u64>,
        #[serde(default)]
        temperature: Option<f64>,
        /// Additional provider-specific parameters merged into the API request.
        /// Provider-agnostic: works for Anthropic thinking, Gemini thinking budget, etc.
        /// Example: `{ thinking = { type = "adaptive", budget_tokens = 8000 } }`
        #[serde(default)]
        additional_params: Option<serde_json::Value>,
    },
    Gemini {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        max_tokens: Option<u64>,
        /// Context window size in tokens.
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        context_window: Option<u64>,
        /// Controls the randomness and creativity of the llm
        #[serde(default)]
        temperature: Option<f64>,
        /// Additional provider-specific parameters merged into the API request.
        /// Provider-agnostic: works for Anthropic thinking, Gemini thinking budget, etc.
        /// Example: `{ thinking = { type = "adaptive", budget_tokens = 8000 } }`
        #[serde(default)]
        additional_params: Option<serde_json::Value>,
    },
    Ollama {
        model: String,
        #[serde(default = "default_ollama_base_url")]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        max_tokens: Option<u64>,
        /// Context window size in tokens.
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        context_window: Option<u64>,
        /// Controls the randomness and creativity of the llm
        #[serde(default)]
        temperature: Option<f64>,
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
        /// Additional Ollama-specific parameters passed directly to the API.
        /// Examples: seed, top_k, top_p, mirostat, etc.
        /// See: https://github.com/ollama/ollama/blob/main/docs/modelfile.mdx#valid-parameters-and-values
        #[serde(default)]
        additional_params: Option<serde_json::Value>,
    },
}

fn default_ollama_base_url() -> Option<String> {
    Some("http://localhost:11434".to_string())
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig::OpenAI {
            api_key: String::new(),
            model: "gpt-4o".to_string(),
            base_url: None,
            reasoning_effort: None,
            max_tokens: None,
            context_window: None,
            temperature: None,
            additional_params: None,
        }
    }
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

    /// Get max_tokens regardless of provider.
    pub fn max_tokens(&self) -> Option<u64> {
        match self {
            LlmConfig::OpenAI { max_tokens, .. }
            | LlmConfig::Anthropic { max_tokens, .. }
            | LlmConfig::Bedrock { max_tokens, .. }
            | LlmConfig::Gemini { max_tokens, .. }
            | LlmConfig::Ollama { max_tokens, .. } => *max_tokens,
        }
    }

    /// Get context_window regardless of provider.
    pub fn context_window(&self) -> Option<u64> {
        match self {
            LlmConfig::OpenAI { context_window, .. }
            | LlmConfig::Anthropic { context_window, .. }
            | LlmConfig::Bedrock { context_window, .. }
            | LlmConfig::Gemini { context_window, .. }
            | LlmConfig::Ollama { context_window, .. } => *context_window,
        }
    }

    /// Get additional_params regardless of provider.
    pub fn additional_params(&self) -> Option<serde_json::Value> {
        match self {
            LlmConfig::OpenAI {
                additional_params, ..
            }
            | LlmConfig::Anthropic {
                additional_params, ..
            }
            | LlmConfig::Bedrock {
                additional_params, ..
            }
            | LlmConfig::Gemini {
                additional_params, ..
            }
            | LlmConfig::Ollama {
                additional_params, ..
            } => additional_params.clone(),
        }
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

    /// Accessor for temperature regardless of provider.
    pub fn temperature(&self) -> Option<f64> {
        match self {
            LlmConfig::OpenAI { temperature, .. }
            | LlmConfig::Anthropic { temperature, .. }
            | LlmConfig::Bedrock { temperature, .. }
            | LlmConfig::Gemini { temperature, .. }
            | LlmConfig::Ollama { temperature, .. } => *temperature,
        }
    }
}

/// Agent behavior settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    pub name: String,
    pub system_prompt: String,
    pub context: Vec<String>,
    #[serde(default, deserialize_with = "lenient_int::deserialize_option_usize")]
    pub turn_depth: Option<usize>,
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
    /// Optional context string to prepend to search results for better RAG integration
    pub context_prefix: Option<String>,
    /// Store-type-specific configuration
    #[serde(flatten)]
    pub store: VectorStoreType,
}

/// Type-specific vector store configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VectorStoreType {
    InMemory {
        embedding_model: EmbeddingModelConfig,
    },
    Qdrant {
        embedding_model: EmbeddingModelConfig,
        url: String,
        collection_name: String,
    },
    BedrockKb {
        knowledge_base_id: String,
        region: String,
        #[serde(default)]
        profile: Option<String>,
    },
}

/// Embedding model configuration with strong typing per provider
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum EmbeddingModelConfig {
    OpenAI {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
    },
    Bedrock {
        model: String,
        region: String,
        /// AWS profile name (optional, uses default credentials if not specified)
        #[serde(default)]
        profile: Option<String>,
    },
}

impl EmbeddingModelConfig {
    /// Get the provider name
    pub fn provider(&self) -> &str {
        match self {
            EmbeddingModelConfig::OpenAI { .. } => "openai",
            EmbeddingModelConfig::Bedrock { .. } => "bedrock",
        }
    }

    /// Get the model name
    pub fn model(&self) -> &str {
        match self {
            EmbeddingModelConfig::OpenAI { model, .. }
            | EmbeddingModelConfig::Bedrock { model, .. } => model,
        }
    }
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
                context_window: None,
                temperature: None,
                reasoning_effort: None,
                additional_params: None,
            },
            agent: AgentSettings {
                name: "Assistant".to_string(),
                system_prompt: "You are a helpful assistant.".to_string(),
                context: vec![],
                turn_depth: Some(5),
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
}
