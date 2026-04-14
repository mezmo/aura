use crate::{
    config::{AgentConfig, LlmConfig, McpServerConfig},
    error::{BuilderError, BuilderResult},
    mcp::McpManager,
    provider_agent::{
        BuilderState, CompletionResponse, ProviderAgent, StreamError, StreamItem,
        StreamedAssistantContent,
    },
    tool_wrapper::WrappedTool,
    tools::{FilesystemTool, ListDirTool, ReadFileTool, WriteFileTool},
    vector_dynamic::DynamicVectorSearchTool,
    vector_store::VectorStoreManager,
};
use futures::StreamExt;
use rig::client::CompletionClient;
use rig::completion::Usage;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Default maximum depth for multi-turn conversations.
///
/// This is the system-wide fallback when `[agent].turn_depth` is not set in TOML.
/// Models with extended thinking (e.g. qwen3-coder) burn ReAct turns on reasoning
/// before tool calls, so 12 gives adequate headroom.
pub const DEFAULT_MAX_DEPTH: usize = 12;

/// Shallow-merge two JSON values at the top level.
///
/// If both are objects, keys from `b` are inserted into `a` (overwriting on conflict).
/// Otherwise, returns `a` unchanged.
///
/// Used to combine multiple sources of `additional_params` before passing to
/// `AgentBuilder::additional_params()` which replaces (not merges) on each call.
pub(crate) fn merge_json(a: serde_json::Value, b: serde_json::Value) -> serde_json::Value {
    match (a, b) {
        (serde_json::Value::Object(mut a_map), serde_json::Value::Object(b_map)) => {
            for (key, value) in b_map {
                a_map.insert(key, value);
            }
            serde_json::Value::Object(a_map)
        }
        (a, _) => a,
    }
}

/// Log tool count with optional filter indication
fn log_filtered_tools(emoji: &str, transport: &str, server: &str, filtered: usize, total: usize) {
    if filtered < total {
        tracing::info!(
            "{} Adding {}/{} {} tools from {}",
            emoji,
            filtered,
            total,
            transport,
            server
        );
    } else {
        tracing::info!(
            "{} Adding {} {} tools from {}",
            emoji,
            total,
            transport,
            server
        );
    }
}

/// Filesystem tools collection
#[derive(Clone)]
pub struct FilesystemTools {
    pub read_file: ReadFileTool,
    pub list_dir: ListDirTool,
    pub write_file: WriteFileTool,
}

/// Rig-native agent wrapper using provider-specific agents.
///
/// # Tool Execution Paths
///
/// Tools can be executed via two paths:
///
/// 1. **Native (Rig)**: When the LLM emits a `tool_call` structure, Rig invokes
///    the tool's `Tool::call()` implementation directly. This is the normal path
///    for OpenAI, Anthropic, and other providers with native tool support.
///
/// 2. **Fallback (Ollama)**: When `fallback_tool_parsing` is enabled, the stream
///    is wrapped with `FallbackToolExecutor`. It buffers text, detects tool call
///    patterns (JSON/XML), and executes via `McpManager::execute_fallback_tool()`.
///    This bypasses Rig's tool infrastructure entirely.
pub struct Agent {
    pub(crate) inner: ProviderAgent,
    pub(crate) model: String,
    pub(crate) max_depth: usize,
    pub(crate) mcp_manager: Option<Arc<crate::mcp::McpManager>>,
    /// Ollama text-to-tool parsing: when enabled, intercepts text output containing
    /// tool calls (JSON/XML) and executes them. Only applies to Ollama provider.
    /// See `maybe_wrap_with_fallback()` for the wrapping logic.
    pub(crate) fallback_tool_parsing: bool,
    /// Cached tool names for fallback parsing (avoids recomputing on each stream).
    /// Only populated when `fallback_tool_parsing` is enabled.
    pub(crate) fallback_tool_names: Vec<String>,
    /// Configured context window size in tokens (from LLM TOML config).
    /// Used for usage percentage reporting in streaming events.
    pub(crate) context_window: Option<u64>,
}

impl Agent {
    /// Create a new agent from configuration.
    pub async fn new(
        config: &AgentConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Initialize MCP manager first (shared across all providers)
        let mcp_manager = if let Some(mcp_config) = &config.mcp {
            tracing::info!("Initializing MCP tools using dynamic adaptors");
            Some(Arc::new(
                McpManager::initialize_from_config(mcp_config).await?,
            ))
        } else {
            None
        };

        // Get max depth for tool calls per turn
        let max_depth = config.agent.turn_depth.unwrap_or(DEFAULT_MAX_DEPTH);
        tracing::info!("  Max turn depth: {} (tool calls per turn)", max_depth);

        // Ollama fallback: parse tool calls from text output when native tool_call
        // structures aren't used. Requires MCP tools to be available.
        let fallback_tool_parsing = config.llm.is_fallback_tool_parsing_enabled();
        let fallback_tool_names = if fallback_tool_parsing {
            match &mcp_manager {
                Some(mgr) => {
                    let names = mgr.get_available_tool_names();
                    if names.is_empty() {
                        tracing::warn!(
                            "fallback_tool_parsing enabled but no MCP tools discovered - \
                             text-based tool calls will not be executed"
                        );
                    } else {
                        tracing::info!(
                            "  Fallback tool parsing: ENABLED ({} tools available)",
                            names.len()
                        );
                    }
                    names
                }
                None => {
                    tracing::warn!(
                        "fallback_tool_parsing enabled but no MCP servers configured - \
                         feature will have no effect"
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        // Extract model name for logging
        let model_name = config.llm.model_name().to_string();

        // Build provider-specific agent with tools
        // Each branch creates its own completion model and agent builder,
        // adds tools using the shared BuilderState helper, then wraps in ProviderAgent
        let provider_agent = match &config.llm {
            LlmConfig::OpenAI {
                api_key,
                model,
                base_url,
                reasoning_effort,
                temperature,
                additional_params,
                ..
            } => {
                tracing::info!("Initializing OpenAI provider");
                tracing::debug!("  Model: {}", model);

                let mut client_builder =
                    rig::providers::openai::Client::<reqwest::Client>::builder().api_key(api_key);

                if let Some(url) = base_url {
                    tracing::info!("  Using custom base URL: {}", url);
                    client_builder = client_builder.base_url(url);
                }

                let client = client_builder.build()?;
                tracing::info!("OpenAI client initialized successfully");

                // Use completions_api() for OpenAI Chat Completions API (not Responses API)
                // Note: rig 0.25.0 defaults to Responses API, so we must switch to completions_api first
                let completions_client = client.completions_api();
                let completion_model = completions_client.completion_model(model);

                // Create agent builder with system prompt and temperature
                let mut agent_builder = rig::agent::AgentBuilder::new(completion_model);
                agent_builder = agent_builder.preamble(config.effective_preamble());
                if let Some(temp) = temperature {
                    agent_builder = agent_builder.temperature(*temp);
                }
                // Build combined additional_params: reasoning_effort
                // Must be a single call — AgentBuilder::additional_params() replaces, not merges.
                let mut combined_params: Option<serde_json::Value> = None;
                if let Some(effort) = *reasoning_effort {
                    combined_params =
                        Some(serde_json::json!({"reasoning_effort": effort.to_string()}));
                }
                if let Some(params) = additional_params {
                    combined_params = Some(match combined_params {
                        Some(existing) => merge_json(existing, params.clone()),
                        None => params.clone(),
                    });
                }
                if let Some(params) = combined_params {
                    agent_builder = agent_builder.additional_params(params);
                }

                if let Some(max) = config.llm.max_tokens() {
                    agent_builder = agent_builder.max_tokens(max);
                }

                // Add tools using the BuilderState helper
                let builder_state = BuilderState::Initial(agent_builder);
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager).await?;
                let agent = builder_state.build();

                ProviderAgent::OpenAI(agent)
            }
            LlmConfig::Anthropic {
                api_key,
                model,
                base_url,
                temperature,
                additional_params,
                ..
            } => {
                tracing::info!("Initializing Anthropic provider");
                tracing::debug!("  Model: {}", model);

                let mut client_builder =
                    rig::providers::anthropic::Client::<reqwest::Client>::builder()
                        .api_key(api_key);

                if let Some(url) = base_url {
                    tracing::info!("  Using custom base URL: {}", url);
                    client_builder = client_builder.base_url(url);
                }

                let client = client_builder.build()?;
                tracing::info!("Anthropic client initialized successfully");

                let completion_model = client.completion_model(model);

                // Create agent builder with system prompt and temperature
                let mut agent_builder = rig::agent::AgentBuilder::new(completion_model);
                agent_builder = agent_builder.preamble(config.effective_preamble());
                if let Some(temp) = temperature {
                    agent_builder = agent_builder.temperature(*temp);
                }
                if let Some(max) = config.llm.max_tokens() {
                    agent_builder = agent_builder.max_tokens(max);
                }
                if let Some(params) = additional_params {
                    agent_builder = agent_builder.additional_params(params.clone());
                }

                let builder_state = BuilderState::Initial(agent_builder);
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager).await?;
                let agent = builder_state.build();

                ProviderAgent::Anthropic(agent)
            }
            LlmConfig::Bedrock {
                model,
                region,
                profile,
                temperature,
                additional_params,
                ..
            } => {
                tracing::info!("Initializing AWS Bedrock provider");
                tracing::debug!("  Region: {}", region);
                tracing::debug!("  Profile: {:?}", profile);

                use aws_config::{BehaviorVersion, Region};

                let sdk_config = if let Some(profile_name) = profile {
                    tracing::info!("  Loading AWS config with profile: {}", profile_name);
                    aws_config::defaults(BehaviorVersion::latest())
                        .region(Region::new(region.to_string()))
                        .profile_name(profile_name)
                        .load()
                        .await
                } else {
                    tracing::info!("  Loading AWS config from environment");
                    aws_config::defaults(BehaviorVersion::latest())
                        .region(Region::new(region.to_string()))
                        .load()
                        .await
                };

                tracing::info!("  AWS SDK config loaded, region: {:?}", sdk_config.region());

                let aws_client = aws_sdk_bedrockruntime::Client::new(&sdk_config);
                let bedrock_client = rig_bedrock::client::Client::from(aws_client);
                tracing::info!("AWS Bedrock client initialized successfully");

                tracing::info!("Creating Bedrock completion model: {}", model);
                let completion_model = bedrock_client.completion_model(model);
                tracing::info!("Bedrock completion model created successfully");

                // Create agent builder with system prompt and temperature
                let mut agent_builder = rig::agent::AgentBuilder::new(completion_model);
                agent_builder = agent_builder.preamble(config.effective_preamble());
                if let Some(temp) = temperature {
                    agent_builder = agent_builder.temperature(*temp);
                }
                if let Some(max) = config.llm.max_tokens() {
                    agent_builder = agent_builder.max_tokens(max);
                }
                if let Some(params) = additional_params {
                    agent_builder = agent_builder.additional_params(params.clone());
                }

                let builder_state = BuilderState::Initial(agent_builder);
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager).await?;
                let agent = builder_state.build();

                ProviderAgent::Bedrock(agent)
            }
            LlmConfig::Gemini {
                api_key,
                model,
                base_url,
                temperature,
                additional_params,
                ..
            } => {
                tracing::info!("Initializing Gemini provider");
                tracing::debug!("  Model: {}", model);

                let mut client_builder =
                    rig::providers::gemini::Client::<reqwest::Client>::builder().api_key(api_key);

                if let Some(url) = base_url {
                    tracing::info!("  Using custom base URL: {}", url);
                    client_builder = client_builder.base_url(url);
                }

                let client = client_builder.build()?;
                tracing::info!("Gemini client initialized successfully");

                let completion_model = client.completion_model(model);

                let mut agent_builder = rig::agent::AgentBuilder::new(completion_model);
                agent_builder = agent_builder.preamble(config.effective_preamble());
                if let Some(temp) = temperature {
                    agent_builder = agent_builder.temperature(*temp);
                }
                if let Some(params) = additional_params {
                    agent_builder = agent_builder.additional_params(params.clone());
                }

                let builder_state = BuilderState::Initial(agent_builder);
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager).await?;
                let agent = builder_state.build();

                ProviderAgent::Gemini(agent)
            }
            LlmConfig::Ollama {
                model,
                base_url,
                temperature,
                additional_params,
                ..
            } => {
                tracing::info!("Initializing Ollama provider");
                tracing::debug!("  Model: {}", model);

                let url = base_url.as_deref().unwrap_or("http://localhost:11434");
                tracing::info!("  Base URL: {}", url);

                let client = rig::providers::ollama::Client::builder()
                    .api_key(rig::client::Nothing)
                    .base_url(url)
                    .build()?;
                tracing::info!("Ollama client initialized successfully");

                let completion_model = client.completion_model(model);

                // Create agent builder with system prompt and temperature
                let mut agent_builder = rig::agent::AgentBuilder::new(completion_model);
                agent_builder = agent_builder.preamble(config.effective_preamble());
                if let Some(temp) = temperature {
                    agent_builder = agent_builder.temperature(*temp);
                }

                if let Some(params) = additional_params {
                    agent_builder = agent_builder.additional_params(params.clone());
                }

                let builder_state = BuilderState::Initial(agent_builder);
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager).await?;
                let agent = builder_state.build();

                ProviderAgent::Ollama(agent)
            }
        };

        Ok(Agent {
            inner: provider_agent,
            model: model_name,
            max_depth,
            mcp_manager,
            fallback_tool_parsing,
            fallback_tool_names,
            context_window: config.llm.context_window(),
        })
    }

    /// Add all tools to a builder state (shared across all providers)
    ///
    /// When `config.tool_wrapper` is set, MCP tools are wrapped with the provided
    /// wrapper. The `config.tool_context_factory` is used to create context for each
    /// wrapped tool call.
    pub(crate) async fn add_all_tools<M>(
        mut builder_state: BuilderState<M>,
        config: &AgentConfig,
        mcp_manager: &Option<Arc<McpManager>>,
    ) -> Result<BuilderState<M>, Box<dyn std::error::Error + Send + Sync>>
    where
        M: rig::completion::CompletionModel + Send + Sync,
    {
        if config.tool_wrapper.is_some() {
            tracing::info!("Tool wrapper configured");
        }
        let effective_filter = config
            .mcp_filter
            .as_ref()
            .or(config.agent.mcp_filter.as_ref());
        if let Some(patterns) = effective_filter {
            tracing::info!("MCP filter: {} pattern(s): {:?}", patterns.len(), patterns);
        }

        // Add HTTP streamable tools using dynamic adaptors
        if let Some(mcp_manager) = mcp_manager.as_deref() {
            for (server_name, client) in &mcp_manager.streamable_clients {
                if let Some(server_tools) = mcp_manager.streamable_tools.get(server_name) {
                    let filtered_tools: Vec<_> = server_tools
                        .iter()
                        .filter(|t| config.tool_matches_filter(&t.name))
                        .collect();
                    log_filtered_tools(
                        "",
                        "HTTP streamable",
                        server_name,
                        filtered_tools.len(),
                        server_tools.len(),
                    );

                    let client_arc = Arc::new(client.clone());
                    for mcp_tool in filtered_tools {
                        tracing::info!("  Adding dynamic HTTP tool: {}", mcp_tool.name);

                        let tool_adaptor = crate::mcp_dynamic::HttpMcpToolAdaptor::new(
                            mcp_tool.clone(),
                            server_name.clone(),
                            client_arc.clone(),
                        );

                        // Wrap with tool_wrapper if configured
                        builder_state = Self::add_mcp_tool(builder_state, tool_adaptor, config);
                    }
                }
            }
        }

        // Add filesystem tools if configured
        if let Some(tools_config) = &config.tools
            && tools_config.filesystem
        {
            tracing::info!("Adding filesystem tools");
            let fs_tool = FilesystemTool::new()
                .with_write_access(false)
                .with_max_file_size(1_048_576);

            builder_state = builder_state.add_tool(ReadFileTool(fs_tool.clone()));
            builder_state = builder_state.add_tool(ListDirTool(fs_tool));
        }

        // Add vector store tools if configured
        if !config.vector_stores.is_empty() {
            tracing::info!("Adding {} vector store tool(s)", config.vector_stores.len());

            for vector_store_config in &config.vector_stores {
                tracing::info!("  Configuring vector store: {}", vector_store_config.name);

                let vector_store_manager =
                    Arc::new(VectorStoreManager::from_config(vector_store_config).await?);

                let vector_search_tool = DynamicVectorSearchTool::new(
                    vector_store_manager.clone(),
                    vector_store_config.name.clone(),
                );

                tracing::info!(
                    "  Created dynamic tool 'vector_search_{}'",
                    vector_store_config.name
                );

                builder_state = builder_state.add_tool(vector_search_tool);

                tracing::info!(
                    "  Vector store '{}' configured with search tool",
                    vector_store_config.name
                );
            }

            tracing::info!("All vector stores configured successfully");
        }

        // Add STDIO MCP tools using rmcp_tools
        if let Some(mcp_manager) = mcp_manager.as_deref()
            && !mcp_manager.tool_definitions.is_empty()
        {
            let filtered_definitions: Vec<_> = mcp_manager
                .tool_definitions
                .iter()
                .filter(|(tool, _)| config.tool_matches_filter(&tool.name))
                .collect();
            log_filtered_tools(
                "",
                "STDIO",
                "MCP servers",
                filtered_definitions.len(),
                mcp_manager.tool_definitions.len(),
            );

            // Group tools by client (rmcp_tools takes Vec<Tool> + one client)
            use std::collections::HashMap;
            let mut tools_by_client: HashMap<
                String,
                (Vec<rmcp::model::Tool>, rmcp::service::ServerSink),
            > = HashMap::new();

            for (tool, client) in filtered_definitions {
                let client_key = format!("{client:?}");
                tools_by_client
                    .entry(client_key)
                    .or_insert_with(|| (Vec::new(), client.clone()))
                    .0
                    .push(tool.clone());
            }

            for (_key, (tools, client)) in tools_by_client {
                tracing::info!("  Adding {} STDIO tools for client group", tools.len());
                builder_state = builder_state.add_rmcp_tools(tools, client);
            }
        }

        // Add read_artifact tool when orchestration persistence is available
        if let Some(ref persistence) = config.orchestration_persistence {
            let read_artifact = crate::orchestration::ReadArtifactTool::new(persistence.clone());
            builder_state = builder_state.add_tool(read_artifact);
        }

        // Add get_conversation_context tool when chat history is available
        if let Some(ref history) = config.orchestration_chat_history {
            let context_tool =
                crate::orchestration::GetConversationContextTool::new(history.clone());
            builder_state = builder_state.add_tool(context_tool);
        }

        Ok(builder_state)
    }

    /// Helper to add an MCP tool, optionally wrapping with config.tool_wrapper.
    ///
    /// If `config.tool_wrapper` is set, the tool is wrapped and a context is
    /// created using `config.tool_context_factory` (or a default context).
    fn add_mcp_tool<M, T>(
        builder_state: BuilderState<M>,
        tool: T,
        config: &AgentConfig,
    ) -> BuilderState<M>
    where
        M: rig::completion::CompletionModel + Send + Sync,
        T: rig::tool::Tool<Args = serde_json::Value, Output = String, Error = rig::tool::ToolError>
            + Send
            + Sync
            + Clone
            + 'static,
    {
        match (&config.tool_wrapper, &config.tool_context_factory) {
            (Some(wrapper), Some(ctx_factory)) => {
                // Wrap with both wrapper and context factory
                let tool_name = tool.name();
                let ctx_factory = ctx_factory.clone();
                let wrapped = WrappedTool::new(tool, wrapper.clone())
                    .with_context_factory(move |_| ctx_factory(&tool_name));
                builder_state.add_tool(wrapped)
            }
            (Some(wrapper), None) => {
                // Wrap with wrapper only (default context)
                let wrapped = WrappedTool::new(tool, wrapper.clone());
                builder_state.add_tool(wrapped)
            }
            _ => {
                // No wrapping
                builder_state.add_tool(tool)
            }
        }
    }

    /// Process a query with the agent (no chat history).
    ///
    /// Uses the streaming pipeline internally and collects the result.
    #[tracing::instrument(name = "agent.prompt", skip(self), fields(model = %self.model))]
    pub async fn prompt(
        &self,
        query: &str,
    ) -> Result<crate::provider_agent::CompletionResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        let span = tracing::Span::current();
        record_input_attributes(&span, self.inner.provider_name(), &self.model, query);

        let stream = self.stream_prompt(query).await;
        let result = self.collect_stream_response(stream).await;
        record_completion_result(&span, &result);
        result
    }

    /// Process a chat query with conversation history.
    ///
    /// Uses the streaming pipeline internally and collects the result.
    #[tracing::instrument(name = "agent.chat", skip(self, chat_history), fields(model = %self.model, history_len = chat_history.len()))]
    pub async fn chat(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
    ) -> Result<crate::provider_agent::CompletionResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        let span = tracing::Span::current();
        record_input_attributes(&span, self.inner.provider_name(), &self.model, query);

        let stream = self.stream_chat(query, chat_history).await;
        let result = self.collect_stream_response(stream).await;
        record_completion_result(&span, &result);
        result
    }

    /// Internal: Collect a stream into a CompletionResponse.
    ///
    /// Consumes stream items, accumulating text content until a `Final` or `FinalMarker`
    /// is received. The `Final` variant provides authoritative content and usage stats;
    /// accumulated text serves as fallback when only `FinalMarker` is received.
    async fn collect_stream_response(
        &self,
        mut stream: Pin<
            Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>,
        >,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        // Accumulate text as fallback; Final variant provides authoritative response
        let mut content = String::new();
        let mut usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
        };

        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                    content.push_str(&text);
                }
                Ok(StreamItem::Final(response)) => {
                    content = response.content;
                    usage = response.usage;
                    break;
                }
                Ok(StreamItem::FinalMarker) | Ok(StreamItem::TurnUsage(_)) => {
                    // Per-turn marker — not end-of-stream. Continue collecting.
                }
                Ok(_) => {
                    // Tool calls, deltas, reasoning - handled by fallback executor
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        tracing::info!("Turn complete - response ready");
        Ok(CompletionResponse { content, usage })
    }

    /// Stream a query with the agent (no chat history) - returns true streaming response with multi-turn tool support
    pub async fn stream_prompt(
        &self,
        query: &str,
    ) -> Pin<Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>> {
        let stream = self.inner.stream_prompt(query, self.max_depth).await;
        self.maybe_wrap_with_fallback(stream)
    }

    /// Stream a chat query with conversation history - returns true streaming response with multi-turn tool support
    pub async fn stream_chat(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
    ) -> Pin<Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>> {
        let stream = self
            .inner
            .stream_chat(query, chat_history, self.max_depth)
            .await;
        self.maybe_wrap_with_fallback(stream)
    }

    /// Stream a chat with explicit max_depth override.
    ///
    /// Unlike `stream_chat()` which uses `self.max_depth`, this allows callers
    /// to specify depth. Used by orchestration phases that need tighter bounds.
    #[tracing::instrument(name = "agent.stream_chat", skip(self, chat_history),
        fields(model = %self.model, history_len = chat_history.len(), max_depth))]
    pub async fn stream_chat_with_depth(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        max_depth: usize,
    ) -> Pin<Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>> {
        let stream = self.inner.stream_chat(query, chat_history, max_depth).await;
        self.maybe_wrap_with_fallback(stream)
    }

    /// Conditionally wrap stream for Ollama text-to-tool parsing.
    ///
    /// When `fallback_tool_parsing` is enabled (Ollama config), this wraps the stream
    /// with a `FallbackToolExecutor` that:
    /// 1. Buffers streamed text content
    /// 2. On stream end, parses text for tool call patterns (JSON, XML, etc.)
    /// 3. Executes detected tools via MCP and injects results into the stream
    ///
    /// This handles Ollama models (e.g., qwen3-coder) that output tool calls as text
    /// instead of using native tool_call structures.
    fn maybe_wrap_with_fallback(
        &self,
        stream: Pin<
            Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>,
        >,
    ) -> Pin<Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>> {
        // Early return if fallback not enabled or no tools available
        if !self.fallback_tool_parsing || self.fallback_tool_names.is_empty() {
            return stream;
        }

        // Wrap stream with fallback executor (requires MCP manager for tool execution)
        if let Some(mcp_manager) = self.mcp_manager.clone() {
            let executor = crate::fallback_tool_stream::FallbackToolExecutor::new(
                mcp_manager,
                self.fallback_tool_names.clone(),
            );
            return executor.wrap_stream(Box::pin(stream));
        }

        stream
    }

    /// Stream a query with timeout and cancellation support.
    ///
    /// Returns the stream and a sender to trigger external cancellation.
    ///
    /// # Arguments
    /// * `query` - The user query
    /// * `timeout` - Timeout duration for the request
    /// * `request_id` - Unique request ID for MCP tool cancellation context
    ///
    /// # Returns
    /// * Stream of multi-turn items
    /// * Sender to trigger external cancellation (e.g., on client disconnect)
    ///
    /// # Cancellation
    /// The StreamingRequestHook checks for cancellation at key points during streaming:
    /// - Before each LLM completion call
    /// - On each text delta (periodic)
    /// - Before each tool call (sets active request context for MCP cancellation)
    /// - After each tool result (clears active request context, adds to pending_tool_ids)
    /// - After each streaming completion (captures usage, emits aura.tool_usage)
    ///
    /// To cancel externally (e.g., on client disconnect), call `cancel_tx.send(true)`.
    ///
    /// # Returns
    /// * Stream of multi-turn items
    /// * Sender to trigger external cancellation
    /// * UsageState for reading final usage at stream end (shared with hook)
    pub async fn stream_prompt_with_timeout(
        &self,
        query: &str,
        timeout: Duration,
        request_id: &str,
    ) -> (
        Pin<Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>>,
        watch::Sender<bool>,
        crate::streaming_request_hook::UsageState,
    ) {
        let (stream, cancel_tx, usage_state) = self
            .inner
            .stream_prompt_with_timeout(query, self.max_depth, timeout, request_id)
            .await;
        (
            self.maybe_wrap_with_fallback(stream),
            cancel_tx,
            usage_state,
        )
    }

    /// Stream a chat query with timeout and cancellation support.
    ///
    /// # Arguments
    /// * `query` - The user query
    /// * `chat_history` - Previous conversation messages
    /// * `timeout` - Timeout duration for the request
    /// * `request_id` - Unique request ID for MCP tool cancellation context
    ///
    /// # Returns
    /// * Stream of multi-turn items
    /// * Sender to trigger external cancellation (e.g., on client disconnect)
    /// * UsageState for reading final usage at stream end (shared with hook)
    ///
    /// # Cancellation
    /// See `stream_prompt_with_timeout` for cancellation details.
    pub async fn stream_chat_with_timeout(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        timeout: Duration,
        request_id: &str,
    ) -> (
        Pin<Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>>,
        watch::Sender<bool>,
        crate::streaming_request_hook::UsageState,
    ) {
        let (stream, cancel_tx, usage_state) = self
            .inner
            .stream_chat_with_timeout(query, chat_history, self.max_depth, timeout, request_id)
            .await;
        (
            self.maybe_wrap_with_fallback(stream),
            cancel_tx,
            usage_state,
        )
    }

    /// Get provider information
    pub fn get_provider_info(&self) -> (&str, &str) {
        (self.inner.provider_name(), &self.model)
    }

    /// Cancel all in-flight MCP tool requests for an HTTP request.
    ///
    /// This sends `notifications/cancelled` to all MCP servers that have
    /// in-flight requests, allowing them to abort long-running operations.
    /// Call this when a client disconnects or request times out.
    ///
    /// # Arguments
    /// * `http_request_id` - The HTTP request ID whose MCP calls should be cancelled
    /// * `reason` - Reason for cancellation (e.g., "client disconnected", "timeout")
    ///
    /// # Returns
    /// Total number of cancellation notifications sent
    pub async fn cancel_mcp_requests(&self, http_request_id: &str, reason: &str) -> usize {
        if let Some(mcp_manager) = &self.mcp_manager {
            mcp_manager
                .cancel_all_for_request(http_request_id, reason)
                .await
        } else {
            0
        }
    }

    /// Cancel all in-flight MCP requests and forcefully close connections.
    ///
    /// This sends `notifications/cancelled` to all MCP servers and then
    /// terminates the connections. Use this when the server ignores
    /// cancellation requests and keeps sending progress notifications.
    ///
    /// # Warning
    /// After calling this, all MCP clients become unusable. The next request
    /// will need to reinitialize them.
    ///
    /// # Arguments
    /// * `http_request_id` - The HTTP request ID whose MCP calls should be cancelled
    /// * `reason` - Reason for cancellation
    ///
    /// # Returns
    /// Total number of cancellation notifications sent
    pub async fn cancel_and_close_mcp(&self, http_request_id: &str, reason: &str) -> usize {
        if let Some(mcp_manager) = &self.mcp_manager {
            mcp_manager
                .cancel_and_close_all(http_request_id, reason)
                .await
        } else {
            0
        }
    }

    /// Get all available tool names from MCP servers.
    ///
    /// Returns a list of tool names that can be used for fallback tool execution.
    pub fn get_available_tool_names(&self) -> Vec<String> {
        self.mcp_manager
            .as_ref()
            .map(|m| m.get_available_tool_names())
            .unwrap_or_default()
    }

    /// Get reference to the MCP manager (if configured).
    pub fn mcp_manager(&self) -> Option<&McpManager> {
        self.mcp_manager.as_deref()
    }
}

// ---------------------------------------------------------------------------
// OTel dual-path recording
// ---------------------------------------------------------------------------
//
// Non-streaming (prompt/chat):
//   The full LLM lifecycle completes inside `Agent::prompt`/`Agent::chat`,
//   so `record_input_attributes` and `record_completion_result` below own
//   the entire span: input attributes are set before the call, output
//   attributes are set after the call, all on the same span.
//
// Streaming (`Agent::stream_prompt` / `Agent::stream_chat`):
//   The stream is returned to the HTTP handler which sends it back as SSE.
//   The LLM output is not available until the stream is fully consumed
//   inside a `tokio::spawn` block, so these helpers are NOT used. Instead,
//   input/output attributes are recorded in the web server's spawned task
//   via `StreamOtelContext::record_input()` / `StreamOtelContext::record_output()`
//   on the `agent.stream` span (see `aura-web-server/src/handlers.rs`).
//
// If you change attribute names here, update the streaming path too.
// Canonical attribute name constants live in `logging.rs`.
// ---------------------------------------------------------------------------

/// Record input attributes on the span via the OTel API directly.
///
/// Uses `OpenTelemetrySpanExt::set_attribute` to bypass the tracing
/// `Filtered::on_record` path which doesn't propagate to the OTel layer.
fn record_input_attributes(span: &tracing::Span, provider_name: &str, model: &str, query: &str) {
    crate::logging::set_llm_identifiers(span, provider_name, model);
    crate::logging::set_input_attributes(span, query);
}

/// Record completion result fields (usage, content, status) on the span via OTel API.
fn record_completion_result(
    span: &tracing::Span,
    result: &Result<
        crate::provider_agent::CompletionResponse,
        Box<dyn std::error::Error + Send + Sync>,
    >,
) {
    match result {
        Ok(response) => {
            crate::logging::set_token_usage(
                span,
                response.usage.input_tokens,
                response.usage.output_tokens,
                response.usage.total_tokens,
                0, // no per-tool breakdown in non-streaming path
            );
            crate::logging::set_output_attributes(span, &response.content);
            crate::logging::set_span_ok(span);
        }
        Err(e) => {
            crate::logging::set_span_error(span, crate::logging::truncate_for_otel(&e.to_string()));
        }
    }
}

// Implement StreamingAgent trait for Agent
use crate::streaming::StreamingAgent;
use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

#[async_trait]
impl StreamingAgent for Agent {
    fn get_provider_info(&self) -> (&str, &str) {
        Agent::get_provider_info(self)
    }

    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        _cancel_token: CancellationToken,
        request_id: &str,
    ) -> Result<BoxStream<'static, Result<StreamItem, StreamError>>, StreamError> {
        if let Some(mcp_manager) = &self.mcp_manager {
            mcp_manager.set_current_request(request_id).await;
        }

        let stream = if chat_history.is_empty() {
            self.stream_prompt(query).await
        } else {
            self.stream_chat(query, chat_history).await
        };

        Ok(Box::pin(stream))
    }

    async fn stream_with_timeout(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        timeout: Duration,
        request_id: &str,
    ) -> (
        BoxStream<'static, Result<StreamItem, StreamError>>,
        watch::Sender<bool>,
        crate::UsageState,
    ) {
        // Production entry point — set MCP request ID before delegating
        if let Some(mcp_manager) = &self.mcp_manager {
            mcp_manager.set_current_request(request_id).await;
        }

        let (stream, cancel_tx, usage_state) = if chat_history.is_empty() {
            self.stream_prompt_with_timeout(query, timeout, request_id)
                .await
        } else {
            self.stream_chat_with_timeout(query, chat_history, timeout, request_id)
                .await
        };

        (Box::pin(stream), cancel_tx, usage_state)
    }

    async fn cancel_and_close_mcp(&self, request_id: &str, reason: &str) -> usize {
        Agent::cancel_and_close_mcp(self, request_id, reason).await
    }

    fn context_window(&self) -> Option<u64> {
        self.context_window
    }
}

/// Build the appropriate streaming agent based on configuration.
///
/// Returns either an orchestrated multi-agent workflow or a standard single
/// agent, depending on `orchestration.enabled`. Both implement `StreamingAgent`.
pub async fn build_streaming_agent(
    config: &crate::config::AgentConfig,
) -> Result<Arc<dyn StreamingAgent>, Box<dyn std::error::Error + Send + Sync>> {
    use crate::orchestration::OrchestratorFactory;

    if config.orchestration_enabled() {
        tracing::info!("Building OrchestratorFactory (orchestration.enabled = true)");
        let factory = OrchestratorFactory::new(config.clone());
        Ok(Arc::new(factory))
    } else {
        // Standard single-agent mode
        tracing::info!("Building Agent (orchestration.enabled = false)");
        let agent = Agent::new(config).await?;
        Ok(Arc::new(agent))
    }
}

/// Builder for constructing Rig agents from configuration
pub struct AgentBuilder {
    config: AgentConfig,
}

impl AgentBuilder {
    /// Create a new builder from configuration
    pub fn new(config: AgentConfig) -> Self {
        // Log the configuration for debugging
        tracing::info!("=== Agent Configuration ===");

        // Log provider and model based on enum variant
        match &config.llm {
            LlmConfig::OpenAI { model, .. } => {
                tracing::info!("LLM Provider: OpenAI");
                tracing::info!("LLM Model: {}", model);
            }
            LlmConfig::Anthropic { model, .. } => {
                tracing::info!("LLM Provider: Anthropic");
                tracing::info!("LLM Model: {}", model);
            }
            LlmConfig::Bedrock { model, region, .. } => {
                tracing::info!("LLM Provider: Bedrock");
                tracing::info!("LLM Model: {}", model);
                tracing::info!("AWS Region: {}", region);
            }
            LlmConfig::Gemini {
                model, base_url, ..
            } => {
                tracing::info!("LLM Provider: Gemini");
                tracing::info!("LLM Model: {}", model);
                if let Some(url) = base_url {
                    tracing::info!("Base URL: {}", url);
                }
            }
            LlmConfig::Ollama {
                model, base_url, ..
            } => {
                tracing::info!("LLM Provider: Ollama");
                tracing::info!("LLM Model: {}", model);
                if let Some(url) = base_url {
                    tracing::info!("Base URL: {}", url);
                }
            }
        }

        tracing::info!("Agent Name: {}", config.agent.name);

        if let Some(mcp_config) = &config.mcp {
            tracing::info!("=== MCP Servers Configuration ===");
            for (name, server) in &mcp_config.servers {
                match server {
                    McpServerConfig::Stdio {
                        cmd,
                        args,
                        env,
                        description,
                    } => {
                        tracing::info!("MCP Server '{}' (STDIO):", name);
                        tracing::info!("  Command: {:?}", cmd);
                        tracing::info!("  Args: {:?}", args);
                        tracing::info!("  Env vars: {} defined", env.len());
                        if let Some(desc) = description {
                            tracing::info!("  Description: {}", desc);
                        }
                    }
                    McpServerConfig::HttpStreamable {
                        url,
                        headers,
                        description,
                        headers_from_request,
                    } => {
                        tracing::info!("MCP Server '{}' (HTTP Streamable):", name);
                        tracing::info!("  URL: {}", url);
                        tracing::info!("  Headers: {} defined", headers.len());
                        if let Some(desc) = description {
                            tracing::info!("  Description: {}", desc);
                        }
                        tracing::info!(
                            "  Headers from requests: {} to forward",
                            headers_from_request.len()
                        );
                    }
                }
            }
        } else {
            tracing::info!("No MCP servers configured");
        }

        if !config.vector_stores.is_empty() {
            tracing::info!("=== Vector Stores Configuration ===");
            for vector_store in &config.vector_stores {
                tracing::info!("Store: {}", vector_store.name);
                tracing::info!("  Type: {}", vector_store.store_type);
                if let Some(em) = &vector_store.embedding_model {
                    tracing::info!("  Embedding Provider: {}", em.provider());
                    tracing::info!("  Embedding Model: {}", em.model());
                }
                if let Some(kb_id) = &vector_store.knowledge_base_id {
                    tracing::info!("  Knowledge Base ID: {}", kb_id);
                }
            }
        }

        if let Some(tools) = &config.tools {
            tracing::info!("=== Tools Configuration ===");
            tracing::info!("Filesystem tool enabled: {}", tools.filesystem);
            tracing::info!("Custom tools: {:?}", tools.custom_tools);
        }

        tracing::info!("=== Configuration Ready ===");

        Self { config }
    }

    /// Build an agent with all configured capabilities using Rig's DynClientBuilder
    pub async fn build_agent(&self) -> BuilderResult<Agent> {
        tracing::info!("=== Building Agent ===");

        let agent = Agent::new(&self.config)
            .await
            .map_err(|e| BuilderError::AgentError(e.to_string()))?;

        tracing::info!("Agent built successfully with full tool integration");

        Ok(agent)
    }
}
