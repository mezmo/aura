use crate::lenient_int;
use crate::orchestration::OrchestrationConfig;
use crate::scratchpad::{ScratchpadConfig, ScratchpadToolEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Root configuration structure for our POC
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Config {
    /// Top-level persistence directory shared by scratchpad and orchestration
    /// artifacts. `[orchestration.artifacts].memory_dir` is honored as a
    /// legacy fallback.
    #[serde(default)]
    pub memory_dir: Option<String>,
    pub mcp: Option<McpConfig>,
    /// Vector stores for RAG - optional, defaults to empty
    #[serde(default)]
    pub vector_stores: Vec<VectorStoreConfig>,
    pub tools: Option<ToolsConfig>,
    pub agent: AgentConfig,
    /// Orchestration mode configuration (multi-agent workflows)
    #[serde(default)]
    pub orchestration: Option<OrchestrationConfig>,
}

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
    OpenRouter {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        max_tokens: Option<u64>,
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
        context_window: Option<u64>,
        #[serde(default)]
        temperature: Option<f64>,
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
            | LlmConfig::Ollama { max_tokens, .. }
            | LlmConfig::OpenRouter { max_tokens, .. } => *max_tokens,
        }
    }

    /// Get context_window regardless of provider.
    pub fn context_window(&self) -> Option<u64> {
        match self {
            LlmConfig::OpenAI { context_window, .. }
            | LlmConfig::Anthropic { context_window, .. }
            | LlmConfig::Bedrock { context_window, .. }
            | LlmConfig::Gemini { context_window, .. }
            | LlmConfig::Ollama { context_window, .. }
            | LlmConfig::OpenRouter { context_window, .. } => *context_window,
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
            }
            | LlmConfig::OpenRouter {
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
            | LlmConfig::Ollama { model, .. }
            | LlmConfig::OpenRouter { model, .. } => model,
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
            LlmConfig::OpenRouter { model, .. } => ("openrouter", model),
        }
    }

    /// Accessor for temperature regardless of provider.
    pub fn temperature(&self) -> Option<f64> {
        match self {
            LlmConfig::OpenAI { temperature, .. }
            | LlmConfig::Anthropic { temperature, .. }
            | LlmConfig::Bedrock { temperature, .. }
            | LlmConfig::Gemini { temperature, .. }
            | LlmConfig::Ollama { temperature, .. }
            | LlmConfig::OpenRouter { temperature, .. } => *temperature,
        }
    }
}

impl Config {
    /// Parse configuration from a TOML string
    pub fn parse_toml(contents: &str) -> Result<Self, crate::ConfigError> {
        let config: Config = toml::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    /// Check if fallback tool parsing is enabled for the LLM.
    ///
    /// This is only supported for Ollama models.
    pub fn is_fallback_tool_parsing_enabled(&self) -> bool {
        self.agent.llm.is_fallback_tool_parsing_enabled()
    }

    /// Get provider name and model for response formatting.
    pub fn get_provider_info(&self) -> (&str, &str) {
        self.agent.llm.model_info()
    }

    /// Check if orchestration mode is enabled.
    pub fn orchestration_enabled(&self) -> bool {
        self.orchestration.as_ref().is_some_and(|o| o.enabled)
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        validate_llm_api_key(&self.agent.llm, "agent.llm")?;

        if let Some(orch) = &self.orchestration {
            for (name, worker) in &orch.workers {
                if let Some(worker_llm) = &worker.llm {
                    validate_llm_api_key(worker_llm, &format!("orchestration.worker.{name}.llm"))?;
                }
            }
        }

        // Validate each vector store
        for store in &self.vector_stores {
            match &store.store {
                VectorStoreType::InMemory { embedding_model }
                | VectorStoreType::Qdrant {
                    embedding_model, ..
                } => match embedding_model {
                    EmbeddingConfig::OpenAI { api_key, .. } => {
                        if api_key.is_empty() {
                            return Err(crate::ConfigError::Validation(format!(
                                "Embedding model API key is required for vector store '{}'",
                                store.name
                            )));
                        }
                    }
                    EmbeddingConfig::Bedrock { region, .. } => {
                        if region.is_empty() {
                            return Err(crate::ConfigError::Validation(format!(
                                "Embedding model region is required for Bedrock provider in vector store '{}'",
                                store.name
                            )));
                        }
                    }
                },
                VectorStoreType::BedrockKb { .. } => {
                    // All required fields are enforced by the enum structure
                }
            }
        }

        // Scratchpad validation
        self.validate_scratchpad()?;
        self.validate_glob_patterns()?;

        if let Some(orch) = &self.orchestration {
            orch.validate_worker_names()?;
        }

        Ok(())
    }

    fn validate_glob_patterns(&self) -> Result<(), crate::ConfigError> {
        if let Some(patterns) = self.agent.mcp_filter.as_deref() {
            validate_glob_patterns(patterns, "agent.mcp_filter")?;
        }
        if let Some(patterns) = self.agent.client_tool_filter.as_deref() {
            validate_glob_patterns(patterns, "agent.client_tool_filter")?;
        }

        if let Some(orch) = &self.orchestration {
            validate_glob_patterns(
                &orch.coordinator_mcp_filter,
                "orchestration.coordinator_mcp_filter",
            )?;
            for (name, worker) in &orch.workers {
                validate_glob_patterns(
                    &worker.mcp_filter,
                    &format!("orchestration.worker.{name}.mcp_filter"),
                )?;
            }
        }

        if let Some(mcp) = &self.mcp {
            for (server_name, server_config) in &mcp.servers {
                let patterns: Vec<&String> = server_config.scratchpad().keys().collect();
                validate_glob_patterns(patterns, &format!("mcp.servers.{server_name}.scratchpad"))?;
            }
        }

        Ok(())
    }

    /// When scratchpad is enabled on the agent or any worker, require a
    /// `memory_dir` (top-level, with legacy fallback to
    /// `[orchestration.artifacts].memory_dir`) and a `context_window` on each
    /// scratchpad-enabled agent's effective LLM.
    fn validate_scratchpad(&self) -> Result<(), crate::ConfigError> {
        let agent_sp_enabled = self.agent.scratchpad.as_ref().is_some_and(|sp| sp.enabled);
        let orch = self.orchestration.as_ref().filter(|o| o.enabled);

        let worker_sp_enabled = |w: &crate::orchestration::WorkerConfig| {
            w.scratchpad
                .as_ref()
                .map(|sp| sp.enabled)
                .unwrap_or(agent_sp_enabled)
        };
        let any_worker_enabled = orch
            .map(|o| o.workers.values().any(worker_sp_enabled))
            .unwrap_or(false);

        if !agent_sp_enabled && !any_worker_enabled {
            return Ok(());
        }

        let effective_memory_dir = self.memory_dir.as_deref().or_else(|| {
            self.orchestration
                .as_ref()
                .and_then(|o| o.artifacts.memory_dir.as_deref())
        });
        if effective_memory_dir.is_none() {
            return Err(crate::ConfigError::Validation(
                "Scratchpad enabled but top-level `memory_dir` is not set".to_string(),
            ));
        }

        if agent_sp_enabled && self.agent.llm.context_window().is_none() {
            return Err(crate::ConfigError::Validation(
                "Scratchpad enabled but [agent.llm].context_window is not set".to_string(),
            ));
        }
        if let Some(o) = orch {
            for (name, worker) in &o.workers {
                if worker_sp_enabled(worker) {
                    let effective_llm = worker.llm.as_ref().unwrap_or(&self.agent.llm);
                    if effective_llm.context_window().is_none() {
                        return Err(crate::ConfigError::Validation(format!(
                            "Scratchpad enabled for worker '{name}' but its effective LLM has no context_window"
                        )));
                    }
                }
            }
        }

        Ok(())
    }
}

fn validate_llm_api_key(llm: &LlmConfig, location: &str) -> Result<(), crate::ConfigError> {
    match llm {
        LlmConfig::OpenAI { api_key, .. }
        | LlmConfig::Anthropic { api_key, .. }
        | LlmConfig::Gemini { api_key, .. }
        | LlmConfig::OpenRouter { api_key, .. } => {
            if api_key.is_empty() {
                return Err(crate::ConfigError::Validation(format!(
                    "LLM API key is required for [{location}]"
                )));
            }
        }
        LlmConfig::Bedrock { .. } | LlmConfig::Ollama { .. } => {}
    }
    Ok(())
}

/// MCP servers configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct McpConfig {
    pub servers: HashMap<String, McpServerConfig>,
    /// Enable OpenAI-compatible tool schema sanitization (default: true)
    #[serde(default = "default_sanitize_schemas")]
    pub sanitize_schemas: bool,
}

fn default_sanitize_schemas() -> bool {
    true
}

/// Individual MCP server configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "transport")]
pub enum McpServerConfig {
    #[serde(rename = "stdio")]
    Stdio {
        cmd: Vec<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        description: Option<String>,
        /// Per-tool scratchpad interception thresholds (glob-matched on tool name).
        #[serde(default)]
        scratchpad: HashMap<String, ScratchpadToolEntry>,
    },
    #[serde(rename = "http_streamable")]
    HttpStreamable {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        headers_from_request: HashMap<String, String>,
        /// Per-tool scratchpad interception thresholds (glob-matched on tool name).
        #[serde(default)]
        scratchpad: HashMap<String, ScratchpadToolEntry>,
    },
    #[serde(rename = "sse")]
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        headers_from_request: HashMap<String, String>,
        /// Per-tool scratchpad interception thresholds (glob-matched on tool name).
        #[serde(default)]
        scratchpad: HashMap<String, ScratchpadToolEntry>,
    },
}

impl McpServerConfig {
    /// Get the per-tool scratchpad thresholds for this server.
    pub fn scratchpad(&self) -> &HashMap<String, ScratchpadToolEntry> {
        match self {
            McpServerConfig::Stdio { scratchpad, .. } => scratchpad,
            McpServerConfig::HttpStreamable { scratchpad, .. } => scratchpad,
            McpServerConfig::Sse { scratchpad, .. } => scratchpad,
        }
    }
}

/// Vector store configuration (in-memory, Qdrant, and Bedrock KB)
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct VectorStoreConfig {
    /// Unique name to identify this vector store
    pub name: String,
    /// Optional context string describing what the vector store contains (for better LLM guidance)
    #[serde(default)]
    pub context_prefix: Option<String>,
    /// Store-type-specific configuration
    #[serde(flatten)]
    pub store: VectorStoreType,
}

/// Type-specific vector store configuration, tagged by `type` field
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VectorStoreType {
    InMemory {
        embedding_model: EmbeddingConfig,
    },
    Qdrant {
        embedding_model: EmbeddingConfig,
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

impl Default for VectorStoreConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            context_prefix: None,
            store: VectorStoreType::InMemory {
                embedding_model: EmbeddingConfig::default(),
            },
        }
    }
}

/// Embedding model configuration with strong typing per provider
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum EmbeddingConfig {
    OpenAI {
        api_key: String,
        model: String,
    },
    Bedrock {
        model: String,
        region: String,
        /// AWS profile name (optional, uses default credentials if not specified)
        #[serde(default)]
        profile: Option<String>,
    },
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        EmbeddingConfig::OpenAI {
            api_key: String::new(),
            model: "text-embedding-3-small".to_string(),
        }
    }
}

impl EmbeddingConfig {
    /// Get the provider name
    pub fn provider(&self) -> &str {
        match self {
            EmbeddingConfig::OpenAI { .. } => "openai",
            EmbeddingConfig::Bedrock { .. } => "bedrock",
        }
    }

    /// Get the model name
    pub fn model(&self) -> &str {
        match self {
            EmbeddingConfig::OpenAI { model, .. } | EmbeddingConfig::Bedrock { model, .. } => model,
        }
    }
}

/// Tools configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub filesystem: bool,
    #[serde(default)]
    pub custom_tools: Vec<String>,
}

/// Configuration for TodoWrite/ReadTodos tool injection.
#[derive(Debug, Clone, Default)]
pub struct TodoToolsConfig {
    /// Optional directory for persisting plans.
    /// If None, plans are stored in-memory only.
    pub plan_dir: Option<String>,
}

/// Agent configuration (TOML `[agent]` table shape).
///
/// This is the TOML-facing view of the agent config. The runtime
/// `AgentRuntimeConfig` in the `aura` crate extracts a subset (`AgentSettings`)
/// for agent construction.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default)]
    pub alias: Option<String>,
    pub system_prompt: String,
    #[serde(default)]
    pub context: Vec<String>,
    /// Maximum depth of tool calls per turn (default: 5, set to 0 to disable)
    #[serde(default = "default_turn_depth")]
    #[serde(deserialize_with = "lenient_int::deserialize_option_usize")]
    pub turn_depth: Option<usize>,
    /// Creation timestamp in milliseconds since epoch (defaults to current time)
    #[serde(default = "default_created_at")]
    pub created_at: u64,
    /// Override the `owned_by` field in /v1/models responses.
    /// When omitted, defaults to the underlying LLM provider (e.g. "openai", "anthropic").
    #[serde(default)]
    pub model_owner: Option<String>,
    /// Glob patterns for filtering which MCP tools to include.
    /// When set, only tools matching at least one pattern are added.
    /// Example: `mcp_filter = ["sin", "cos", "degreesToRadians"]`
    #[serde(default)]
    pub mcp_filter: Option<Vec<String>>,
    /// Whether this agent (single-agent or orchestration coordinator) may
    /// invoke client-side tools advertised on the request. When false
    /// (default), client tools are not attached even if the request
    /// supplied them.
    #[serde(default)]
    pub enable_client_tools: bool,
    /// Glob patterns selecting which client-side tools this agent can call.
    /// `None` or empty means all client tools are available when
    /// `enable_client_tools = true`.
    #[serde(default)]
    pub client_tool_filter: Option<Vec<String>>,
    /// LLM configuration for this agent.
    ///
    /// Parsed from the `[agent.llm]` TOML table. Workers inherit this config
    /// when no `[orchestration.worker.<name>.llm]` is provided.
    #[serde(default)]
    pub llm: LlmConfig,
    /// Agent-level scratchpad config. Workers inherit this unless they
    /// provide `[orchestration.worker.<name>.scratchpad]`.
    #[serde(default)]
    pub scratchpad: Option<ScratchpadConfig>,
}

fn default_turn_depth() -> Option<usize> {
    Some(5)
}

fn default_created_at() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "Assistant".to_string(),
            alias: None,
            system_prompt: "You are a helpful assistant.".to_string(),
            context: Vec::new(),
            turn_depth: default_turn_depth(),
            created_at: default_created_at(),
            model_owner: None,
            mcp_filter: None,
            enable_client_tools: false,
            client_tool_filter: None,
            llm: LlmConfig::default(),
            scratchpad: None,
        }
    }
}

/// Agent behavior settings (runtime-facing subset of [`AgentConfig`]).
///
/// This is the struct embedded in `aura::AgentRuntimeConfig`. It is produced by
/// flattening `AgentConfig` inside the Rig builder, dropping TOML-only fields
/// (`alias`, `created_at`, `model_owner`, `llm`) that runtime agent construction
/// receives through other channels.
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
    /// Agent-level scratchpad configuration (applies to single-agent and to
    /// workers that don't provide an override).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratchpad: Option<ScratchpadConfig>,
    /// Whether this agent (single-agent or orchestration coordinator) may
    /// invoke client-side tools advertised on the request.
    #[serde(default)]
    pub enable_client_tools: bool,
    /// Glob patterns selecting which client-side tools this agent can call.
    /// `None` or empty means all client tools are available when
    /// `enable_client_tools = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_tool_filter: Option<Vec<String>>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            name: "Assistant".to_string(),
            system_prompt: "You are a helpful assistant.".to_string(),
            context: Vec::new(),
            turn_depth: Some(5),
            mcp_filter: None,
            scratchpad: None,
            enable_client_tools: false,
            client_tool_filter: None,
        }
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
    match glob::Pattern::new(pattern) {
        Ok(pattern) => pattern.matches(text),
        Err(err) => {
            tracing::warn!(pattern, %err, "invalid glob pattern");
            false
        }
    }
}

fn validate_glob_patterns<'a, I>(patterns: I, location: &str) -> Result<(), crate::ConfigError>
where
    I: IntoIterator<Item = &'a String>,
{
    for pattern in patterns {
        glob::Pattern::new(pattern).map_err(|err| {
            crate::ConfigError::Validation(format!(
                "Invalid glob pattern '{pattern}' in {location}: {err}"
            ))
        })?;
    }
    Ok(())
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
    fn test_glob_match_character_class() {
        assert!(glob_match("tool_[ab]", "tool_a"));
        assert!(glob_match("tool_[ab]", "tool_b"));
        assert!(!glob_match("tool_[ab]", "tool_c"));
    }

    #[test]
    fn test_glob_match_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_invalid_coordinator_mcp_filter_fails_validation() {
        let toml = r#"
            [agent]
            name = "test"
            system_prompt = "test"

            [agent.llm]
            provider = "ollama"
            model = "qwen"

            [orchestration]
            enabled = true
            coordinator_mcp_filter = ["tool_["]
        "#;

        let err = Config::parse_toml(toml).unwrap_err();
        match err {
            crate::ConfigError::Validation(msg) => {
                assert!(msg.contains("orchestration.coordinator_mcp_filter"));
                assert!(msg.contains("tool_["));
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }
}
