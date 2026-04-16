use aura::config::lenient_int;
use serde::{Deserialize, Serialize};
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
}

impl Config {
    /// Check if fallback tool parsing is enabled for the LLM.
    ///
    /// This is only supported for Ollama models.
    pub fn is_fallback_tool_parsing_enabled(&self) -> bool {
        self.llm.is_fallback_tool_parsing_enabled()
    }
}

/// LLM provider configuration with strong typing per provider
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum LlmConfig {
    OpenAI {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
    },
    Anthropic {
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
    Gemini {
        api_key: String,
        model: String,
        #[serde(default)]
        base_url: Option<String>,
    },
    Ollama {
        model: String,
        #[serde(default = "default_ollama_base_url")]
        base_url: String,
        /// Enable fallback tool call parsing from text content.
        /// Some Ollama models output tool calls as JSON text instead of proper tool_call structures.
        /// When enabled, the system will attempt to parse tool calls from text responses.
        /// Default: false
        #[serde(default)]
        fallback_tool_parsing: bool,
        /// Context window size (number of tokens). Maps to Ollama's `num_ctx` option.
        /// Default: None (uses Ollama's default, typically 2048)
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        num_ctx: Option<u32>,
        /// Maximum number of tokens to predict. Maps to Ollama's `num_predict` option.
        /// Default: None (uses Ollama's default, typically 128)
        #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
        num_predict: Option<u32>,
        /// Additional Ollama-specific parameters passed directly to the API.
        /// Examples: seed, top_k, top_p, mirostat, etc.
        /// See: https://github.com/ollama/ollama/blob/main/docs/modelfile.md#valid-parameters-and-values
        #[serde(default)]
        additional_params: Option<HashMap<String, serde_json::Value>>,
    },
}

fn default_ollama_base_url() -> String {
    "http://localhost:11434".to_string()
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig::OpenAI {
            api_key: String::new(),
            model: "gpt-4o".to_string(),
            base_url: None,
        }
    }
}

impl LlmConfig {
    /// Check if fallback tool parsing is enabled.
    ///
    /// This is only supported for Ollama models.
    pub fn is_fallback_tool_parsing_enabled(&self) -> bool {
        matches!(
            self,
            LlmConfig::Ollama {
                fallback_tool_parsing: true,
                ..
            }
        )
    }

    /// Get LLM's model name
    pub fn model_info(&self) -> (&str, &str) {
        match self {
            LlmConfig::OpenAI {
                api_key: _,
                model,
                base_url: _,
            } => ("openai", model),
            LlmConfig::Anthropic {
                api_key: _,
                model,
                base_url: _,
            } => ("anthropic", model),
            LlmConfig::Bedrock {
                model,
                region: _,
                profile: _,
            } => ("bedrock", model),
            LlmConfig::Gemini {
                api_key: _,
                model,
                base_url: _,
            } => ("gemini", model),
            LlmConfig::Ollama {
                model,
                base_url: _,
                fallback_tool_parsing: _,
                num_ctx: _,
                num_predict: _,
                additional_params: _,
            } => ("ollama", model),
        }
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
pub struct AgentConfig {
    pub name: String,
    #[serde(default)]
    pub alias: Option<String>,
    pub system_prompt: String,
    #[serde(default)]
    pub context: Vec<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default, deserialize_with = "lenient_int::deserialize_option_u64")]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Maximum depth of tool calls per turn (default: 5, set to 0 to disable)
    #[serde(default = "default_turn_depth")]
    #[serde(deserialize_with = "lenient_int::deserialize_option_usize")]
    pub turn_depth: Option<usize>,
    /// Context window size in tokens for this agent. Used for usage percentage
    /// reporting in streaming events (aura.session_info).
    #[serde(default, deserialize_with = "lenient_int::deserialize_option_u32")]
    pub context_window: Option<u32>,
    /// Creation timestamp in milliseconds since epoch (defaults to current time)
    #[serde(default = "default_created_at")]
    pub created_at: u64,
    /// Override the `owned_by` field in /v1/models responses.
    /// When omitted, defaults to the underlying LLM provider (e.g. "openai", "anthropic").
    #[serde(default)]
    pub model_owner: Option<String>,
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
            temperature: Some(0.7),
            reasoning_effort: None,
            max_tokens: None,
            turn_depth: default_turn_depth(),
            context_window: None,
            created_at: default_created_at(),
            model_owner: None,
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
