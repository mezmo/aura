use crate::{
    config::{AgentConfig, LlmConfig, McpServerConfig},
    error::{BuilderError, BuilderResult},
    mcp::McpManager,
    passthrough_tool::PassthroughTool,
    provider_agent::{
        BuilderState, CompletionResponse, ProviderAgent, StreamError, StreamItem,
        StreamedAssistantContent,
    },
    scratchpad,
    tool_wrapper::WrappedTool,
    tools::{FilesystemTool, ListDirTool, ReadFileTool, WriteFileTool},
    vector_dynamic::DynamicVectorSearchTool,
    vector_store::VectorStoreManager,
};
use futures::StreamExt;
use rig::client::CompletionClient;
use rig::completion::Usage;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// A client-side tool definition supplied with a request.
///
/// Used to register passthrough tools on the agent — the LLM sees them as
/// callable, but `call()` returns `PASSTHROUGH_MARKER` and the streaming layer
/// terminates the stream with `finish_reason: "tool_calls"` so the client can
/// execute the tool locally and submit results back in a follow-up request.
#[derive(Debug, Clone)]
pub struct ClientTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

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

/// Build a `StreamItem::ScratchpadUsage` for this agent's budget — `None` if
/// the budget had no activity (no interception, no extraction). Pure decision
/// helper shared by the single-agent stream tail and the orchestrator's
/// per-worker emission so both paths stay in lock-step on the gating rule.
pub(crate) fn scratchpad_usage_event(
    budget: &scratchpad::ContextBudget,
    agent_id: &str,
) -> Option<StreamItem> {
    let (intercepted, extracted) = budget.scratchpad_usage();
    (intercepted > 0 || extracted > 0).then(|| StreamItem::ScratchpadUsage {
        agent_id: agent_id.to_string(),
        tokens_intercepted: intercepted,
        tokens_extracted: extracted,
    })
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
    /// Per-agent scratchpad budget for context tracking.
    /// Set by orchestration workers (from resolved worker LLM + scratchpad config);
    /// `None` for coordinator agents and non-scratchpad use.
    pub(crate) scratchpad_budget: Option<scratchpad::ContextBudget>,
    /// Names of client-side (passthrough) tools registered for this agent.
    /// When the LLM calls one of these, the streaming layer terminates the
    /// stream with `finish_reason: "tool_calls"` so the caller can execute
    /// the tool and resume in a follow-up request.
    pub(crate) client_tool_names: HashSet<String>,
}

impl Agent {
    /// Wire up scratchpad for a single-agent config. Skipped in orchestration
    /// mode (workers build their own per-worker budget in `create_worker()`)
    /// and when no accessible MCP tool matches a scratchpad threshold.
    ///
    /// Per-request lifecycle: this runs inside `Agent::new`, which is called
    /// fresh per chat request from
    /// `aura-web-server::handlers::build_agent_for_request`. The `Agent` (and
    /// this `ContextBudget`) is dropped when the request stream ends — there
    /// is no cross-request budget state to manage. Request-specific data
    /// (user query + chat history) is seeded into the budget at stream-start
    /// in `Agent::stream_*_with_timeout`, not here, so this constructor
    /// stays free of request-shape parameters.
    async fn setup_single_agent_scratchpad(
        config: &mut AgentConfig,
        mcp_manager: Option<&Arc<McpManager>>,
    ) -> Result<Option<scratchpad::ContextBudget>, Box<dyn std::error::Error + Send + Sync>> {
        if config.orchestration_enabled() {
            return Ok(None);
        }

        let sp_cfg = match config.agent.scratchpad.as_ref() {
            Some(sp) if sp.enabled => sp.clone(),
            _ => return Ok(None),
        };

        let tools_per_server = mcp_manager
            .map(|mgr| mgr.tool_names_per_server())
            .unwrap_or_default();
        let scratchpad_tool_map =
            scratchpad::scratchpad_tool_map(config.mcp.as_ref(), &tools_per_server);
        let accessible_tools = mcp_manager
            .map(|mgr| mgr.get_available_tool_names())
            .unwrap_or_default();
        let filter = config
            .mcp_filter
            .as_deref()
            .or(config.agent.mcp_filter.as_deref())
            .unwrap_or(&[]);
        if !scratchpad::has_accessible_scratchpad_tool(
            &accessible_tools,
            filter,
            &scratchpad_tool_map,
        ) {
            tracing::info!(
                "Single-agent scratchpad enabled but no MCP tool matches a scratchpad threshold; skipping"
            );
            return Ok(None);
        }

        // Validation enforces these upstream; re-check here so runtime
        // misconfiguration fails loudly instead of silently degrading.
        let context_window = config.llm.context_window().ok_or_else(
            || -> Box<dyn std::error::Error + Send + Sync> {
                "Scratchpad enabled but [agent.llm].context_window is not set".into()
            },
        )? as usize;
        let memory_dir = config
            .effective_memory_dir()
            .map(str::to_owned)
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                "Scratchpad enabled but `memory_dir` is not set".into()
            })?;

        let (provider, model) = config.llm.model_info();
        let token_counter = scratchpad::token_counter_for_provider(provider, model);

        // Exact MCP tool-schema tokens (BPE on the JSON each tool serializes to).
        // The user query and chat history aren't counted here; they're seeded into
        // `estimated_used` at stream-start in `Agent::stream_*_with_timeout`
        // (see `seed_scratchpad_request_input`), where they're naturally
        // available without dirtying constructors with request data.
        let mcp_tool_tokens = mcp_manager
            .map(|m| {
                scratchpad::count_mcp_tool_schema_tokens(
                    &*token_counter,
                    m.tool_definitions_iter(),
                    filter,
                )
            })
            .unwrap_or(0);

        let initial_used = scratchpad::estimate_scratchpad_overhead(
            &*token_counter,
            &[config.effective_preamble()],
        ) + mcp_tool_tokens;

        let build = scratchpad::build_scratchpad(scratchpad::ScratchpadBuildInputs {
            sp_cfg: &sp_cfg,
            storage_dir: std::path::Path::new(&memory_dir),
            scratchpad_tool_map,
            context_window,
            initial_used,
            token_counter,
        })
        .await?;

        // Order in vec controls reverse-iter `transform_output`: the LAST
        // entry in the vec runs FIRST on raw output. Place `existing` after
        // scratchpad so a caller-supplied wrapper observes the raw tool
        // output (matches the orchestration convention where persistence-
        // class wrappers go after scratchpad — see `create_worker`).
        //
        // Asymmetry callers should know about: `wrap_schema`, `transform_args`,
        // and `validate_args` walk the vec forward, so `existing` sees a
        // scratchpad-wrapped schema (extra fields injected) and scratchpad-
        // stripped args (scratchpad fields removed) — not the raw MCP
        // versions. Audit / logging wrappers are unaffected; a wrapper that
        // introspects schema or transforms args needs to account for this.
        // See `ComposedWrapper` docs and the lockdown test at the bottom of
        // this module (`test_composed_scratchpad_then_existing_observes_raw_output`).
        config.tool_wrapper = Some(match config.tool_wrapper.take() {
            Some(existing) => Arc::new(crate::tool_wrapper::ComposedWrapper::new(vec![
                build.wrapper,
                existing,
            ])),
            None => build.wrapper,
        });
        config.preamble_override = Some(format!(
            "{}{}",
            config.effective_preamble(),
            scratchpad::SCRATCHPAD_PREAMBLE
        ));
        config.scratchpad_tools_config = Some(build.tools_config);

        Ok(Some(build.budget))
    }

    /// Create a new agent from configuration with optional additional tools.
    ///
    /// `additional_tools` registers extra rig tools the agent will execute itself
    /// (e.g. tools other applications using Aura as a library want to expose).
    /// Pass `vec![]` when no extra tools are needed.
    ///
    /// `client_tools` registers passthrough tools — the LLM sees them as callable,
    /// but the streaming layer terminates the stream when one is invoked so the
    /// client can execute the tool locally. Pass `None` to disable.
    pub async fn new(
        config: &AgentConfig,
        additional_tools: Vec<Box<dyn rig::tool::ToolDyn>>,
        client_tools: Option<Vec<ClientTool>>,
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

        // Clone so setup_single_agent_scratchpad can mutate extension fields
        // (tool_wrapper, preamble_override, scratchpad_tools_config).
        let mut config_owned = config.clone();
        let agent_scratchpad_budget =
            Self::setup_single_agent_scratchpad(&mut config_owned, mcp_manager.as_ref()).await?;
        let config = &config_owned;

        // Scratchpad bonus only applies when scratchpad was actually wired up
        // (enabled + context_window + a matching MCP tool).
        let base_depth = config.agent.turn_depth.unwrap_or(DEFAULT_MAX_DEPTH);
        let scratchpad_bonus = config
            .scratchpad_tools_config
            .as_ref()
            .and(config.agent.scratchpad.as_ref())
            .map(|sp| sp.turn_depth_bonus)
            .unwrap_or(0);
        let max_depth = base_depth + scratchpad_bonus;
        if scratchpad_bonus > 0 {
            tracing::info!(
                "  Max turn depth: {} (base={}, scratchpad_bonus={})",
                max_depth,
                base_depth,
                scratchpad_bonus
            );
        } else {
            tracing::info!("  Max turn depth: {} (tool calls per turn)", max_depth);
        }

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
                let builder_state = if let Some(ref tools) = client_tools {
                    Self::add_passthrough_tools(builder_state, tools)
                } else {
                    builder_state
                };
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager, additional_tools)
                        .await?;
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
                let builder_state = if let Some(ref tools) = client_tools {
                    Self::add_passthrough_tools(builder_state, tools)
                } else {
                    builder_state
                };
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager, additional_tools)
                        .await?;
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
                let builder_state = if let Some(ref tools) = client_tools {
                    Self::add_passthrough_tools(builder_state, tools)
                } else {
                    builder_state
                };
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager, additional_tools)
                        .await?;
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
                let builder_state = if let Some(ref tools) = client_tools {
                    Self::add_passthrough_tools(builder_state, tools)
                } else {
                    builder_state
                };
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager, additional_tools)
                        .await?;
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
                let builder_state = if let Some(ref tools) = client_tools {
                    Self::add_passthrough_tools(builder_state, tools)
                } else {
                    builder_state
                };
                let builder_state =
                    Self::add_all_tools(builder_state, config, &mcp_manager, additional_tools)
                        .await?;
                let agent = builder_state.build();

                ProviderAgent::Ollama(agent)
            }
        };

        let client_tool_names = client_tools
            .as_ref()
            .map(|tools| tools.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default();

        Ok(Agent {
            inner: provider_agent,
            model: model_name,
            max_depth,
            mcp_manager,
            fallback_tool_parsing,
            fallback_tool_names,
            context_window: config.llm.context_window(),
            scratchpad_budget: agent_scratchpad_budget,
            client_tool_names,
        })
    }

    /// Register passthrough tools so the LLM can request them, but execution
    /// is deferred to the client.
    ///
    /// Each tool is wrapped in a `PassthroughTool` that returns
    /// `PASSTHROUGH_MARKER` from `call()`; the streaming layer detects the
    /// marker, suppresses the result, and emits `finish_reason: "tool_calls"`.
    fn add_passthrough_tools<M>(
        builder_state: BuilderState<M>,
        client_tools: &[ClientTool],
    ) -> BuilderState<M>
    where
        M: rig::completion::CompletionModel + Send + Sync,
    {
        let mut state = builder_state;
        for tool in client_tools {
            tracing::info!("  Adding passthrough tool: {}", tool.name);
            let passthrough = PassthroughTool::new(
                tool.name.clone(),
                tool.description.clone(),
                tool.parameters.clone(),
            );
            state = state.add_tool(passthrough);
        }
        state
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
        additional_tools: Vec<Box<dyn rig::tool::ToolDyn>>,
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

        if let Some(ref scratchpad) = config.scratchpad_tools_config {
            use crate::scratchpad::{
                GetInTool, GrepTool, HeadTool, ItemSchemaTool, IterateOverTool, ReadTool,
                SchemaTool, SliceTool,
            };
            tracing::info!(
                "Adding scratchpad tools (head, slice, grep, schema, item_schema, get_in, iterate_over, read)"
            );
            let s = &scratchpad.storage;
            let b = &scratchpad.budget;
            builder_state = builder_state
                .add_tool(HeadTool::new(s.clone(), b.clone()))
                .add_tool(SliceTool::new(s.clone(), b.clone()))
                .add_tool(GrepTool::new(s.clone(), b.clone()))
                .add_tool(SchemaTool::new(s.clone(), b.clone()))
                .add_tool(ItemSchemaTool::new(s.clone(), b.clone()))
                .add_tool(GetInTool::new(s.clone(), b.clone()))
                .add_tool(IterateOverTool::new(s.clone(), b.clone()))
                .add_tool(ReadTool::new(s.clone(), b.clone()));
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

        // Add additional custom tools (e.g., CLI local tools in standalone mode)
        if !additional_tools.is_empty() {
            tracing::info!("Adding {} additional custom tools", additional_tools.len());
            builder_state = builder_state.add_tools_dyn(additional_tools);
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

    /// Append a final `StreamItem::ScratchpadUsage` if this agent has a
    /// scratchpad budget with non-zero activity. Mirrors the per-worker event
    /// the orchestrator emits after each task — the web server handler
    /// converts this into the `aura.scratchpad_usage` SSE event for the UI.
    fn append_scratchpad_usage(
        &self,
        stream: Pin<
            Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>,
        >,
    ) -> Pin<Box<dyn futures::stream::Stream<Item = Result<StreamItem, StreamError>> + Send>> {
        let Some(budget) = self.scratchpad_budget.clone() else {
            return stream;
        };
        let tail = futures::stream::once(async move { scratchpad_usage_event(&budget, "main") })
            .filter_map(|opt| async move { opt.map(Ok) });
        Box::pin(stream.chain(tail))
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
        self.seed_scratchpad_request_input(query, &[]);
        let (stream, cancel_tx, usage_state) = self
            .inner
            .stream_prompt_with_timeout(
                query,
                self.max_depth,
                timeout,
                request_id,
                self.scratchpad_budget.clone(),
                self.client_tool_names.clone(),
            )
            .await;
        (
            self.append_scratchpad_usage(self.maybe_wrap_with_fallback(stream)),
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
        self.seed_scratchpad_request_input(query, &chat_history);
        let (stream, cancel_tx, usage_state) = self
            .inner
            .stream_chat_with_timeout(
                query,
                chat_history,
                self.max_depth,
                timeout,
                request_id,
                self.scratchpad_budget.clone(),
                self.client_tool_names.clone(),
            )
            .await;
        (
            self.append_scratchpad_usage(self.maybe_wrap_with_fallback(stream)),
            cancel_tx,
            usage_state,
        )
    }

    /// Seed the scratchpad budget's running estimate with the user query +
    /// chat history at stream-start so early extraction budget checks see
    /// the request shape before turn-1 LLM-reported `input_tokens` arrives.
    /// No-op when scratchpad isn't wired up. `Debug` formatting on history
    /// over-counts vs. per-provider serialization — conservative direction
    /// for budget gating, and `set_estimated_used` corrects from LLM ground
    /// truth after each turn anyway.
    fn seed_scratchpad_request_input(
        &self,
        query: &str,
        chat_history: &[rig::completion::Message],
    ) {
        let Some(budget) = &self.scratchpad_budget else {
            return;
        };
        let query_tokens = budget.count_tokens(query);
        let history_tokens: usize = chat_history
            .iter()
            .map(|m| budget.count_tokens(&format!("{m:?}")))
            .sum();
        budget.record_usage(query_tokens + history_tokens);
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
///
/// `client_tools` is the request-supplied passthrough tool definitions.
/// **Client-side tools are only supported in single-agent mode** — when
/// `orchestration.enabled = true`, any supplied `client_tools` are dropped
/// with a warning. In single-agent mode, they are attached to the agent only
/// when `[agent].enable_client_tools = true` (filtered by `client_tool_filter`).
pub async fn build_streaming_agent(
    config: &crate::config::AgentConfig,
    client_tools: Option<Vec<ClientTool>>,
) -> Result<Arc<dyn StreamingAgent>, Box<dyn std::error::Error + Send + Sync>> {
    use crate::orchestration::OrchestratorFactory;

    if config.orchestration_enabled() {
        tracing::info!("Building OrchestratorFactory (orchestration.enabled = true)");
        if client_tools.as_ref().is_some_and(|t| !t.is_empty()) {
            tracing::warn!(
                "Client-side tools were supplied but orchestration is enabled — \
                 client tools are only supported in single-agent configurations and \
                 will be ignored. Use a non-orchestrated agent config to enable them."
            );
        }
        let factory = OrchestratorFactory::new(config.clone());
        Ok(Arc::new(factory))
    } else {
        // Standard single-agent mode: gate client tools on the agent's TOML opt-in
        // and apply its client_tool_filter.
        tracing::info!("Building Agent (orchestration.enabled = false)");
        let attached = if config.agent.enable_client_tools {
            client_tools.map(|tools| {
                tools
                    .into_iter()
                    .filter(|t| config.client_tool_matches_filter(&t.name))
                    .collect::<Vec<_>>()
            })
        } else {
            None
        };
        let agent = Agent::new(config, vec![], attached).await?;
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
                        ..
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
                        ..
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

        let agent = Agent::new(&self.config, vec![], None)
            .await
            .map_err(|e| BuilderError::AgentError(e.to_string()))?;

        tracing::info!("Agent built successfully with full tool integration");

        Ok(agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratchpad::TiktokenCounter;

    fn budget() -> scratchpad::ContextBudget {
        let counter = Arc::new(TiktokenCounter::default_counter());
        scratchpad::ContextBudget::new(128_000, 0.20, 0, counter)
    }

    #[test]
    fn scratchpad_usage_event_returns_none_when_no_activity() {
        let b = budget();
        assert!(scratchpad_usage_event(&b, "main").is_none());
    }

    #[test]
    fn scratchpad_usage_event_emits_when_only_intercepted_is_set() {
        let b = budget();
        b.record_intercepted(500);
        let event = scratchpad_usage_event(&b, "main").expect("must emit when intercepted > 0");
        match event {
            StreamItem::ScratchpadUsage {
                agent_id,
                tokens_intercepted,
                tokens_extracted,
            } => {
                assert_eq!(agent_id, "main");
                assert_eq!(tokens_intercepted, 500);
                assert_eq!(tokens_extracted, 0);
            }
            other => panic!("expected ScratchpadUsage, got {other:?}"),
        }
    }

    #[test]
    fn scratchpad_usage_event_emits_when_only_extracted_is_set() {
        let b = budget();
        b.record_extracted(200);
        let event = scratchpad_usage_event(&b, "main").expect("must emit when extracted > 0");
        match event {
            StreamItem::ScratchpadUsage {
                tokens_intercepted,
                tokens_extracted,
                ..
            } => {
                assert_eq!(tokens_intercepted, 0);
                assert_eq!(tokens_extracted, 200);
            }
            other => panic!("expected ScratchpadUsage, got {other:?}"),
        }
    }

    #[test]
    fn scratchpad_usage_event_uses_provided_agent_id() {
        let b = budget();
        b.record_intercepted(1);
        let event = scratchpad_usage_event(&b, "data-explorer").expect("must emit");
        match event {
            StreamItem::ScratchpadUsage { agent_id, .. } => {
                assert_eq!(agent_id, "data-explorer");
            }
            other => panic!("expected ScratchpadUsage, got {other:?}"),
        }
    }

    /// End-to-end ordering test for the single-agent wrapper composition at
    /// `setup_single_agent_scratchpad` (this file). Mirrors the orchestration
    /// lockdown in `persistence_wrapper.rs::test_composed_persistence_after_scratchpad_captures_raw`.
    ///
    /// Locks down two related contracts that builder.rs:230 relies on:
    /// 1. **Output runs reverse**: a caller-supplied `existing` wrapper placed
    ///    after `ScratchpadWrapper` in the vec sees the **raw** tool output;
    ///    scratchpad runs second and rewrites the LLM-facing output to a
    ///    pointer.
    /// 2. **Schema/args run forward**: `existing` sees whatever scratchpad
    ///    emitted from `wrap_schema`/`transform_args`. Today scratchpad is a
    ///    pass-through there, so this asserts identity. If scratchpad ever
    ///    starts modifying schema or args, this test fails and forces a
    ///    review of the asymmetry doc (see `ComposedWrapper` and the comment
    ///    above the `Some(existing) => ...` arm in `setup_single_agent_scratchpad`).
    ///
    /// A reorder of the vec, a flip in `ComposedWrapper`'s iteration
    /// direction, or a new transform inside `ScratchpadWrapper::wrap_schema`/
    /// `transform_args` will all surface here.
    #[tokio::test]
    async fn test_composed_scratchpad_then_existing_observes_raw_output() {
        use crate::mcp_response::CallOutcome;
        use crate::scratchpad::{ScratchpadStorage, ScratchpadWrapper};
        use crate::tool_wrapper::{
            ComposedWrapper, ToolCallContext, ToolWrapper, TransformArgsResult,
            TransformOutputResult,
        };
        use async_trait::async_trait;
        use serde_json::Value;
        use std::collections::HashMap;
        use std::sync::Mutex;

        /// Fake "caller-supplied" wrapper that records what each composition
        /// step hands it. Lets the test assert exactly what the existing
        /// wrapper observes under the documented composition order.
        #[derive(Default)]
        struct RecordingWrapper {
            schema_seen: Mutex<Option<Value>>,
            args_seen: Mutex<Option<Value>>,
            output_seen: Mutex<Option<String>>,
        }

        #[async_trait]
        impl ToolWrapper for RecordingWrapper {
            fn wrap_schema(&self, schema: Value) -> Value {
                *self.schema_seen.lock().unwrap() = Some(schema.clone());
                schema
            }

            fn transform_args(&self, args: Value, _ctx: &ToolCallContext) -> TransformArgsResult {
                *self.args_seen.lock().unwrap() = Some(args.clone());
                TransformArgsResult::new(args)
            }

            fn transform_output(
                &self,
                output: String,
                _outcome: &CallOutcome,
                _ctx: &ToolCallContext,
                _extracted: Option<&Value>,
            ) -> TransformOutputResult {
                *self.output_seen.lock().unwrap() = Some(output.clone());
                TransformOutputResult::new(output)
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-single-compose")
                .await
                .unwrap(),
        );
        let counter = TiktokenCounter::default_counter();
        let sp_budget = scratchpad::ContextBudget::new(128_000, 0.20, 0, Arc::new(counter));

        let scratchpad_tools = HashMap::from([("big_tool".to_string(), 10_usize)]);
        let scratchpad: Arc<dyn ToolWrapper> =
            Arc::new(ScratchpadWrapper::new(scratchpad_tools, storage, sp_budget));

        let recording = Arc::new(RecordingWrapper::default());
        let recording_dyn: Arc<dyn ToolWrapper> = recording.clone();

        // Same ordering as `setup_single_agent_scratchpad`: scratchpad first,
        // existing (recording) last → reverse iter on transform_output runs
        // recording first → recording sees raw → scratchpad rewrites to pointer.
        let composed = ComposedWrapper::new(vec![scratchpad, recording_dyn]);

        // Forward iter: scratchpad's wrap_schema runs first, then recording.
        let original_schema = serde_json::json!({"type": "object", "properties": {}});
        let _ = composed.wrap_schema(original_schema.clone());
        assert_eq!(
            recording.schema_seen.lock().unwrap().as_ref(),
            Some(&original_schema),
            "ScratchpadWrapper should be a passthrough for wrap_schema; \
             if this fails, the asymmetry doc on builder.rs:230 + ComposedWrapper \
             needs a re-read because existing wrappers will now see a modified schema",
        );

        // Forward iter: scratchpad's transform_args runs first, then recording.
        let ctx = ToolCallContext::new("big_tool").with_task_context(7, "single_agent".into(), 0);
        let original_args = serde_json::json!({"input": "hello"});
        let _ = composed.transform_args(original_args.clone(), &ctx);
        assert_eq!(
            recording.args_seen.lock().unwrap().as_ref(),
            Some(&original_args),
            "ScratchpadWrapper should be a passthrough for transform_args; \
             if this fails, the asymmetry doc on builder.rs:230 + ComposedWrapper \
             needs a re-read because existing wrappers will now see modified args",
        );

        // Reverse iter: recording's transform_output runs first on RAW output,
        // then scratchpad runs on recording's pass-through output and rewrites
        // it to a pointer.
        let raw: String = (0..500).map(|i| format!("entry_{} ", i)).collect();
        let outcome = CallOutcome::Success(raw.clone());
        let result = composed.transform_output(raw.clone(), &outcome, &ctx, None);

        assert_eq!(
            recording.output_seen.lock().unwrap().as_deref(),
            Some(raw.as_str()),
            "existing wrapper must see RAW output, not the scratchpad pointer",
        );
        assert!(
            result.output.contains("[scratchpad:"),
            "scratchpad must rewrite the LLM-facing output to a pointer, got: {}",
            &result.output[..result.output.len().min(120)]
        );
    }
}
