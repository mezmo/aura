use crate::{
    config::{AgentConfig, LlmConfig, McpServerConfig},
    error::{BuilderError, BuilderResult},
    mcp::McpManager,
    provider_agent::{
        BuilderState, CompletionResponse, ProviderAgent, StreamError, StreamItem,
        StreamedAssistantContent,
    },
    tools::{FilesystemTool, ListDirTool, ReadFileTool, WriteFileTool},
    vector_tools::DynamicVectorSearchTool,
};
use futures::StreamExt;
use rig::client::CompletionClient;
use rig::completion::Usage;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Default maximum depth for multi-turn conversations
pub const DEFAULT_MAX_DEPTH: usize = 8;

/// Check if model supports reasoning_effort parameter
fn is_reasoning_model(model: &str) -> bool {
    model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("gpt-5")
}

/// Build Ollama-specific parameters for the agent builder.
///
/// Combines `num_ctx`, `num_predict`, and any `additional_params` into a single
/// JSON object that gets passed to Rig's `additional_params()` method.
///
/// Returns `None` if no parameters are set.
fn build_ollama_params(
    num_ctx: Option<u32>,
    num_predict: Option<u32>,
    additional_params: Option<std::collections::HashMap<String, serde_json::Value>>,
) -> Option<serde_json::Value> {
    let mut params = serde_json::Map::new();

    if let Some(ctx) = num_ctx {
        params.insert("num_ctx".to_string(), serde_json::json!(ctx));
    }
    if let Some(predict) = num_predict {
        params.insert("num_predict".to_string(), serde_json::json!(predict));
    }
    if let Some(additional) = additional_params {
        for (key, value) in additional {
            params.insert(key, value);
        }
    }

    if params.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(params))
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
    inner: ProviderAgent,
    model: String,
    max_depth: usize,
    mcp_manager: Option<Arc<crate::mcp::McpManager>>,
    /// Ollama text-to-tool parsing: when enabled, intercepts text output containing
    /// tool calls (JSON/XML) and executes them. Only applies to Ollama provider.
    /// See `maybe_wrap_with_fallback()` for the wrapping logic.
    fallback_tool_parsing: bool,
    /// Cached tool names for fallback parsing (avoids recomputing on each stream).
    /// Only populated when `fallback_tool_parsing` is enabled.
    fallback_tool_names: Vec<String>,
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

        // Check if model has a known context limit (for usage tracking)
        if crate::model_limits::get_context_limit(&model_name).is_none() {
            tracing::warn!(
                model = %model_name,
                "Model not found in context limits registry - usage percentage will not be available. \
                 Consider adding this model to model_limits.rs"
            );
        }

        // Build provider-specific agent with tools
        // Each branch creates its own completion model and agent builder,
        // adds tools using the shared BuilderState helper, then wraps in ProviderAgent
        let provider_agent = match &config.llm {
            LlmConfig::OpenAI {
                api_key,
                model,
                base_url,
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
                agent_builder = agent_builder.preamble(&config.agent.system_prompt);
                if let Some(temp) = config.agent.temperature {
                    agent_builder = agent_builder.temperature(temp);
                }
                if let Some(effort) = config.agent.reasoning_effort {
                    if is_reasoning_model(model) {
                        let effort_str = match effort {
                            crate::config::ReasoningEffort::Minimal => "minimal",
                            crate::config::ReasoningEffort::Low => "low",
                            crate::config::ReasoningEffort::Medium => "medium",
                            crate::config::ReasoningEffort::High => "high",
                        };
                        agent_builder = agent_builder
                            .additional_params(serde_json::json!({"reasoning_effort": effort_str}));
                    } else {
                        tracing::warn!(
                            "reasoning_effort ignored for model '{}' (only supported on o1/o3/o4/gpt-5+)",
                            model
                        );
                    }
                }
                if let Some(max) = config.agent.max_tokens {
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
                agent_builder = agent_builder.preamble(&config.agent.system_prompt);
                if let Some(temp) = config.agent.temperature {
                    agent_builder = agent_builder.temperature(temp);
                }
                if let Some(max) = config.agent.max_tokens {
                    agent_builder = agent_builder.max_tokens(max);
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
                agent_builder = agent_builder.preamble(&config.agent.system_prompt);
                if let Some(temp) = config.agent.temperature {
                    agent_builder = agent_builder.temperature(temp);
                }
                if let Some(max) = config.agent.max_tokens {
                    agent_builder = agent_builder.max_tokens(max);
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
                agent_builder = agent_builder.preamble(&config.agent.system_prompt);
                if let Some(temp) = config.agent.temperature {
                    agent_builder = agent_builder.temperature(temp);
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
                num_ctx,
                num_predict,
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
                agent_builder = agent_builder.preamble(&config.agent.system_prompt);
                if let Some(temp) = config.agent.temperature {
                    agent_builder = agent_builder.temperature(temp);
                }

                // Build and apply Ollama-specific parameters (num_ctx, num_predict, etc.)
                if let Some(ollama_params) =
                    build_ollama_params(*num_ctx, *num_predict, additional_params.clone())
                {
                    tracing::info!("  Ollama params: {}", ollama_params);
                    agent_builder = agent_builder.additional_params(ollama_params);
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
        })
    }

    /// Add all tools to a builder state (shared across all providers)
    async fn add_all_tools<M>(
        mut builder_state: BuilderState<M>,
        config: &AgentConfig,
        mcp_manager: &Option<Arc<McpManager>>,
    ) -> Result<BuilderState<M>, Box<dyn std::error::Error + Send + Sync>>
    where
        M: rig::completion::CompletionModel + Send + Sync,
    {
        // Add HTTP streamable tools using dynamic adaptors
        if let Some(mcp_manager) = mcp_manager.as_deref() {
            for (server_name, client) in &mcp_manager.streamable_clients {
                if let Some(server_tools) = mcp_manager.streamable_tools.get(server_name) {
                    tracing::info!(
                        "Adding {} dynamic HTTP streamable tools from server: {}",
                        server_tools.len(),
                        server_name
                    );

                    let client_arc = Arc::new(client.clone());
                    for mcp_tool in server_tools {
                        tracing::info!("  Adding dynamic HTTP tool: {}", mcp_tool.name);

                        let tool_adaptor = crate::mcp_dynamic::HttpMcpToolAdaptor::new(
                            mcp_tool.clone(),
                            server_name.clone(),
                            client_arc.clone(),
                        );

                        builder_state = builder_state.add_tool(tool_adaptor);
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
                let tool = DynamicVectorSearchTool::from_config(vector_store_config).await?;
                builder_state = builder_state.add_tool(tool);
            }
        }

        // Add STDIO MCP tools using rmcp_tools
        if let Some(mcp_manager) = mcp_manager.as_deref()
            && !mcp_manager.tool_definitions.is_empty()
        {
            tracing::info!(
                "Adding {} STDIO MCP tools",
                mcp_manager.tool_definitions.len()
            );

            // Group tools by client (rmcp_tools takes Vec<Tool> + one client)
            use std::collections::HashMap;
            let mut tools_by_client: HashMap<
                String,
                (Vec<rmcp::model::Tool>, rmcp::service::ServerSink),
            > = HashMap::new();

            for (tool, client) in &mcp_manager.tool_definitions {
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

        Ok(builder_state)
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
                Ok(StreamItem::FinalMarker) => {
                    break;
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

    /// Set the current HTTP request ID on all MCP clients.
    ///
    /// Call this before starting a streaming request. All subsequent tool calls
    /// will automatically be tracked under this request ID for cancellation support.
    ///
    /// This is more reliable than thread-local storage because Rig spawns tool
    /// execution in separate tasks where thread-local doesn't propagate.
    pub async fn set_mcp_request_id(&self, http_request_id: &str) {
        if let Some(mcp_manager) = &self.mcp_manager {
            mcp_manager.set_current_request(http_request_id).await;
        }
    }

    /// Clear the current HTTP request ID on all MCP clients.
    ///
    /// Call this after a streaming request completes (successfully or otherwise).
    pub async fn clear_mcp_request_id(&self) {
        if let Some(mcp_manager) = &self.mcp_manager {
            mcp_manager.clear_current_request().await;
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
    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        _cancel_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamItem, StreamError>>, StreamError> {
        // Use the appropriate method based on whether we have chat history
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
    ) {
        // Use the appropriate method based on whether we have chat history
        // Note: We discard usage_state here as this trait method doesn't expose it.
        // Callers needing usage_state should use stream_prompt_with_timeout/stream_chat_with_timeout directly.
        let (stream, cancel_tx, _usage_state) = if chat_history.is_empty() {
            self.stream_prompt_with_timeout(query, timeout, request_id)
                .await
        } else {
            self.stream_chat_with_timeout(query, chat_history, timeout, request_id)
                .await
        };

        (Box::pin(stream), cancel_tx)
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
                tracing::info!("  URL: {}", vector_store.url);
                tracing::info!("  Collection: {}", vector_store.collection_name);
                tracing::info!(
                    "  Embedding Provider: {}",
                    vector_store.embedding_model.provider
                );
                tracing::info!("  Embedding Model: {}", vector_store.embedding_model.model);
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
