use aura::{LlmConfig, lenient_int};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root configuration structure for our POC
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Config {
    pub llm: LlmConfig,
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

/// Per-worker configuration for specialized workers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Short description for planning prompt.
    pub description: String,
    /// System prompt for this worker.
    pub preamble: String,
    /// Glob patterns for which MCP tools this worker gets access to.
    #[serde(default)]
    pub mcp_filter: Vec<String>,
    /// Vector stores this worker has access to (explicit names).
    #[serde(default)]
    pub vector_stores: Vec<String>,
    /// Per-worker turn depth limit (overrides [agent].turn_depth).
    #[serde(default)]
    pub turn_depth: Option<usize>,
}

/// Timeout configuration for orchestration (aura-config side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutsConfig {
    #[serde(default = "default_per_call_timeout_secs")]
    pub per_call_timeout_secs: u64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            per_call_timeout_secs: default_per_call_timeout_secs(),
        }
    }
}

/// Artifact configuration for orchestration (aura-config side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactsConfig {
    #[serde(default, alias = "memory_path")]
    pub memory_dir: Option<String>,
    #[serde(default = "default_result_artifact_threshold")]
    pub result_artifact_threshold: usize,
    #[serde(default = "default_result_summary_length")]
    pub result_summary_length: usize,
    #[serde(default = "default_session_history_turns")]
    pub session_history_turns: usize,
}

fn default_session_history_turns() -> usize {
    3
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            memory_dir: None,
            result_artifact_threshold: default_result_artifact_threshold(),
            result_summary_length: default_result_summary_length(),
            session_history_turns: default_session_history_turns(),
        }
    }
}

/// Configuration for orchestration mode.
///
/// Uses custom deserialization for backward compatibility with flat field format.
#[derive(Debug, Clone, Serialize)]
pub struct OrchestrationConfig {
    pub enabled: bool,
    pub max_planning_cycles: usize,
    pub quality_threshold: f32,
    pub max_plan_parse_retries: usize,
    pub max_phases: usize,
    pub worker_system_prompt: Option<String>,
    pub workers: HashMap<String, WorkerConfig>,
    pub coordinator_vector_stores: Vec<String>,
    pub allow_direct_answers: bool,
    pub allow_clarification: bool,
    pub tools_in_planning: ToolVisibility,
    pub max_tools_per_worker: usize,
    pub max_consecutive_duplicate_tool_calls: Option<usize>,
    pub timeouts: TimeoutsConfig,
    pub artifacts: ArtifactsConfig,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_planning_cycles: default_max_planning_cycles(),
            quality_threshold: default_quality_threshold(),
            max_plan_parse_retries: default_max_plan_parse_retries(),
            max_phases: default_max_phases(),
            worker_system_prompt: None,
            workers: HashMap::new(),
            coordinator_vector_stores: Vec::new(),
            allow_direct_answers: true,
            allow_clarification: true,
            tools_in_planning: ToolVisibility::default(),
            max_tools_per_worker: default_max_tools_per_worker(),
            max_consecutive_duplicate_tool_calls: None,
            timeouts: TimeoutsConfig::default(),
            artifacts: ArtifactsConfig::default(),
        }
    }
}

/// Intermediate struct for backward-compatible deserialization.
#[derive(Deserialize)]
struct RawOrchestrationConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_max_planning_cycles")]
    max_planning_cycles: usize,
    #[serde(default = "default_quality_threshold")]
    quality_threshold: f32,
    #[serde(default = "default_max_plan_parse_retries")]
    max_plan_parse_retries: usize,
    #[serde(default = "default_max_phases")]
    max_phases: usize,
    #[serde(default)]
    worker_system_prompt: Option<String>,
    #[serde(default, rename = "worker")]
    workers: HashMap<String, WorkerConfig>,
    #[serde(default)]
    coordinator_vector_stores: Vec<String>,
    #[serde(default = "default_true")]
    allow_direct_answers: bool,
    #[serde(default = "default_true")]
    allow_clarification: bool,
    #[serde(default)]
    tools_in_planning: ToolVisibility,
    #[serde(default = "default_max_tools_per_worker")]
    max_tools_per_worker: usize,
    #[serde(default)]
    max_consecutive_duplicate_tool_calls: Option<usize>,
    // Sub-tables
    #[serde(default)]
    timeouts: Option<TimeoutsConfig>,
    #[serde(default)]
    artifacts: Option<ArtifactsConfig>,
    // Flat artifact fields (backward compat)
    #[serde(default, alias = "memory_path")]
    memory_dir: Option<String>,
    #[serde(default)]
    result_artifact_threshold: Option<usize>,
    #[serde(default)]
    result_summary_length: Option<usize>,
    #[serde(default)]
    session_history_turns: Option<usize>,
}

impl<'de> Deserialize<'de> for OrchestrationConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawOrchestrationConfig::deserialize(deserializer)?;

        let timeouts = raw.timeouts.unwrap_or_default();

        let mut artifacts = raw.artifacts.unwrap_or_default();
        if let Some(v) = raw.memory_dir {
            artifacts.memory_dir = Some(v);
        }
        if let Some(v) = raw.result_artifact_threshold {
            artifacts.result_artifact_threshold = v;
        }
        if let Some(v) = raw.result_summary_length {
            artifacts.result_summary_length = v;
        }
        if let Some(v) = raw.session_history_turns {
            artifacts.session_history_turns = v;
        }

        Ok(OrchestrationConfig {
            enabled: raw.enabled,
            max_planning_cycles: raw.max_planning_cycles,
            quality_threshold: raw.quality_threshold,
            max_plan_parse_retries: raw.max_plan_parse_retries,
            max_phases: raw.max_phases,
            worker_system_prompt: raw.worker_system_prompt,
            workers: raw.workers,
            coordinator_vector_stores: raw.coordinator_vector_stores,
            allow_direct_answers: raw.allow_direct_answers,
            allow_clarification: raw.allow_clarification,
            tools_in_planning: raw.tools_in_planning,
            max_tools_per_worker: raw.max_tools_per_worker,
            max_consecutive_duplicate_tool_calls: raw.max_consecutive_duplicate_tool_calls,
            timeouts,
            artifacts,
        })
    }
}

/// Tool visibility in planning prompts: none, summary (names only), or full (with descriptions).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolVisibility {
    None,
    #[default]
    Summary,
    Full,
}

fn default_true() -> bool {
    true
}

fn default_max_planning_cycles() -> usize {
    3
}

fn default_max_tools_per_worker() -> usize {
    10
}

fn default_quality_threshold() -> f32 {
    0.8
}

fn default_per_call_timeout_secs() -> u64 {
    0
}

fn default_max_plan_parse_retries() -> usize {
    3
}

fn default_max_phases() -> usize {
    5
}

fn default_result_artifact_threshold() -> usize {
    4000
}

fn default_result_summary_length() -> usize {
    2000
}

impl Config {
    /// Check if fallback tool parsing is enabled for the LLM.
    ///
    /// This is only supported for Ollama models.
    pub fn is_fallback_tool_parsing_enabled(&self) -> bool {
        self.llm.is_fallback_tool_parsing_enabled()
    }
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
    },
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

/// Agent configuration
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

    /// Get provider name and model for response formatting.
    pub fn get_provider_info(&self) -> (&str, &str) {
        match &self.llm {
            LlmConfig::OpenAI { model, .. } => ("openai", model),
            LlmConfig::Anthropic { model, .. } => ("anthropic", model),
            LlmConfig::Bedrock { model, .. } => ("bedrock", model),
            LlmConfig::Gemini { model, .. } => ("gemini", model),
            LlmConfig::Ollama { model, .. } => ("ollama", model),
        }
    }

    /// Check if orchestration mode is enabled.
    pub fn orchestration_enabled(&self) -> bool {
        self.orchestration.as_ref().is_some_and(|o| o.enabled)
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        // Basic validation - check API key for OpenAI/Anthropic, skip for Bedrock
        match &self.llm {
            LlmConfig::OpenAI { api_key, .. }
            | LlmConfig::Anthropic { api_key, .. }
            | LlmConfig::Gemini { api_key, .. } => {
                if api_key.is_empty() {
                    return Err(crate::ConfigError::Validation(
                        "LLM API key is required".to_string(),
                    ));
                }
            }
            LlmConfig::Bedrock { .. } => {
                // Bedrock uses AWS credentials, no API key needed
            }
            LlmConfig::Ollama { .. } => {
                // Ollama runs locally, no API key needed
            }
        }

        // Validate each vector store
        for store in &self.vector_stores {
            match &store.store {
                VectorStoreType::InMemory { embedding_model } | VectorStoreType::Qdrant { embedding_model, .. } => {
                    match embedding_model {
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
                    }
                }
                VectorStoreType::BedrockKb { .. } => {
                    // All required fields are enforced by the enum structure
                }
            }
        }

        Ok(())
    }
}
