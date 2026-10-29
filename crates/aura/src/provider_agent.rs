//! Provider-specific agent implementations.
//!
//! This module contains the `ProviderAgent` enum which wraps concrete rig agent types
//! for each supported LLM provider. This is an internal implementation detail of
//! `aura::Agent` and should not be exposed publicly.
//!
//! The enum pattern allows us to:
//! 1. Use concrete types (no deprecated `CompletionModelHandle`)
//! 2. Keep provider-specific code isolated
//! 3. Present a unified API through `aura::Agent`

use futures::stream::StreamExt;
use rig::agent::{AgentBuilder, AgentBuilderSimple, MultiTurnStreamItem};
use rig::completion::{CompletionModel, Usage};
use rig::message::ToolResultContent;
use rig::streaming::{StreamingChat, StreamingPrompt};
use std::collections::HashSet;
use std::pin::Pin;

use crate::scratchpad::ContextBudget;
use crate::streaming_request_hook::StreamingRequestHook;
use aura_events::agent::AgentEvent;

// Type aliases for provider-specific completion models
pub type OpenAICompletionModel =
    rig::providers::openai::completion::CompletionModel<reqwest::Client>;
pub type AnthropicCompletionModel =
    rig::providers::anthropic::completion::CompletionModel<reqwest::Client>;
pub type BedrockCompletionModel = rig_bedrock::completion::CompletionModel;
pub type OllamaCompletionModel = rig::providers::ollama::CompletionModel<reqwest::Client>;
pub type GeminiCompletionModel =
    rig::providers::gemini::completion::CompletionModel<reqwest::Client>;
pub type OpenRouterCompletionModel = rig::providers::openrouter::CompletionModel<reqwest::Client>;

// Type aliases for provider-specific agents
pub type OpenAIAgent = rig::agent::Agent<OpenAICompletionModel>;
pub type AnthropicAgent = rig::agent::Agent<AnthropicCompletionModel>;
pub type BedrockAgent = rig::agent::Agent<BedrockCompletionModel>;
pub type OllamaAgent = rig::agent::Agent<OllamaCompletionModel>;
pub type GeminiAgent = rig::agent::Agent<GeminiCompletionModel>;
pub type OpenRouterAgent = rig::agent::Agent<OpenRouterCompletionModel>;

/// Provider-specific agent wrapper.
///
/// This enum is internal to the `aura` crate and wraps concrete rig agent types
/// for each LLM provider. The calling code should never match on this directly -
/// instead use the methods which delegate appropriately.
pub(crate) enum ProviderAgent {
    OpenAI(OpenAIAgent),
    Anthropic(AnthropicAgent),
    Bedrock(BedrockAgent),
    Gemini(GeminiAgent),
    Ollama(OllamaAgent),
    OpenRouter(OpenRouterAgent),
    /// Scripted-model agent (test-only): the worker-model injection seam the
    /// park/reify rig queues through `install_worker_overrides` and the
    /// orchestrator's cfg(test) prelude consumes. Never constructed outside
    /// `cfg(test)`.
    #[cfg(test)]
    Scripted(crate::orchestration::ScriptedAgent),
}

impl ProviderAgent {
    /// Get the provider name as a static string.
    pub fn provider_name(&self) -> &'static str {
        match self {
            Self::OpenAI(_) => "openai",
            Self::Anthropic(_) => "anthropic",
            Self::Bedrock(_) => "bedrock",
            Self::Gemini(_) => "gemini",
            Self::Ollama(_) => "ollama",
            Self::OpenRouter(_) => "openrouter",
            #[cfg(test)]
            Self::Scripted(_) => "scripted",
        }
    }

    /// Invoke a registered tool by name through the agent's tool server —
    /// the same dynamic-tool path the multi-turn loop uses, so the tool's
    /// full wrapper chain (persistence, observer, HITL gate) runs. The
    /// returned string is the tool output as the loop delivers it to the
    /// model: rig JSON-serializes tool outputs, so a plain string arrives
    /// JSON-quoted.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: &str,
    ) -> Result<String, rig::tool::server::ToolServerError> {
        match self {
            Self::OpenAI(agent) => agent.tool_server_handle.call_tool(tool_name, args).await,
            Self::Anthropic(agent) => agent.tool_server_handle.call_tool(tool_name, args).await,
            Self::Bedrock(agent) => agent.tool_server_handle.call_tool(tool_name, args).await,
            Self::Gemini(agent) => agent.tool_server_handle.call_tool(tool_name, args).await,
            Self::Ollama(agent) => agent.tool_server_handle.call_tool(tool_name, args).await,
            Self::OpenRouter(agent) => agent.tool_server_handle.call_tool(tool_name, args).await,
            #[cfg(test)]
            Self::Scripted(agent) => agent.tool_server_handle.call_tool(tool_name, args).await,
        }
    }

    /// Stream a chat whose prompt is a full message — the continuation
    /// resume submits the checkpointed tool-result prompt, not a plain
    /// string. Carries the park-aware hook like [`Self::stream_chat_with_timeout`].
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat_message_with_timeout(
        &self,
        prompt: rig::completion::Message,
        chat_history: Vec<rig::completion::Message>,
        max_depth: usize,
        options: crate::streaming::RunOptions,
        request_id: &str,
        scratchpad_budget: Option<ContextBudget>,
        client_tool_names: HashSet<String>,
    ) -> crate::streaming::AgentRun {
        let (hook, cancel_tx, usage_state) = StreamingRequestHook::with_scratchpad_budget(
            options,
            request_id,
            scratchpad_budget,
            client_tool_names,
        );

        match self {
            Self::OpenAI(agent) => {
                let stream = agent
                    .stream_chat(prompt, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Anthropic(agent) => {
                let stream = agent
                    .stream_chat(prompt, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Bedrock(agent) => {
                let stream = agent
                    .stream_chat(prompt, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Gemini(agent) => {
                let stream = agent
                    .stream_chat(prompt, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Ollama(agent) => {
                let stream = agent
                    .stream_chat(prompt, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::OpenRouter(agent) => {
                let stream = agent
                    .stream_chat(prompt, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            #[cfg(test)]
            Self::Scripted(agent) => {
                let stream = agent
                    .stream_chat(prompt, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
        }
    }

    /// Stream a prompt with multi-turn support.
    ///
    /// Returns a boxed stream that yields `MultiTurnStreamItem` wrapped in our error type.
    /// The stream items are type-erased using serde_json for the response content.
    pub async fn stream_prompt(
        &self,
        query: &str,
        max_depth: usize,
    ) -> Pin<Box<dyn futures::Stream<Item = Result<StreamItem, StreamError>> + Send>> {
        match self {
            Self::OpenAI(agent) => {
                let stream = agent.stream_prompt(query).multi_turn(max_depth).await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::Anthropic(agent) => {
                let stream = agent.stream_prompt(query).multi_turn(max_depth).await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::Bedrock(agent) => {
                let stream = agent.stream_prompt(query).multi_turn(max_depth).await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::Gemini(agent) => {
                let stream = agent.stream_prompt(query).multi_turn(max_depth).await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::Ollama(agent) => {
                let stream = agent.stream_prompt(query).multi_turn(max_depth).await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::OpenRouter(agent) => {
                let stream = agent.stream_prompt(query).multi_turn(max_depth).await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            #[cfg(test)]
            Self::Scripted(agent) => {
                let stream = agent.stream_prompt(query).multi_turn(max_depth).await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
        }
    }

    /// Stream a chat with history and multi-turn support.
    pub async fn stream_chat(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        max_depth: usize,
    ) -> Pin<Box<dyn futures::Stream<Item = Result<StreamItem, StreamError>> + Send>> {
        match self {
            Self::OpenAI(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .multi_turn(max_depth)
                    .await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::Anthropic(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .multi_turn(max_depth)
                    .await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::Bedrock(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .multi_turn(max_depth)
                    .await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::Gemini(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .multi_turn(max_depth)
                    .await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::Ollama(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .multi_turn(max_depth)
                    .await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            Self::OpenRouter(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .multi_turn(max_depth)
                    .await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
            #[cfg(test)]
            Self::Scripted(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .multi_turn(max_depth)
                    .await;
                Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item))
            }
        }
    }

    /// Stream a prompt with timeout and cancellation support.
    ///
    /// Returns the run: its stream, the token that cancels it, and its usage.
    pub async fn stream_prompt_with_timeout(
        &self,
        query: &str,
        max_depth: usize,
        options: crate::streaming::RunOptions,
        request_id: &str,
        scratchpad_budget: Option<ContextBudget>,
        client_tool_names: HashSet<String>,
    ) -> crate::streaming::AgentRun {
        let (hook, cancel_tx, usage_state) = StreamingRequestHook::with_scratchpad_budget(
            options,
            request_id,
            scratchpad_budget,
            client_tool_names,
        );

        match self {
            Self::OpenAI(agent) => {
                let stream = agent
                    .stream_prompt(query)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Anthropic(agent) => {
                let stream = agent
                    .stream_prompt(query)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Bedrock(agent) => {
                let stream = agent
                    .stream_prompt(query)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Gemini(agent) => {
                let stream = agent
                    .stream_prompt(query)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Ollama(agent) => {
                let stream = agent
                    .stream_prompt(query)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::OpenRouter(agent) => {
                let stream = agent
                    .stream_prompt(query)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            #[cfg(test)]
            Self::Scripted(agent) => {
                let stream = agent
                    .stream_prompt(query)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
        }
    }

    /// Stream a chat with timeout and cancellation support.
    ///
    /// Returns the run: its stream, the token that cancels it, and its usage.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat_with_timeout(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        max_depth: usize,
        options: crate::streaming::RunOptions,
        request_id: &str,
        scratchpad_budget: Option<ContextBudget>,
        client_tool_names: HashSet<String>,
    ) -> crate::streaming::AgentRun {
        let (hook, cancel_tx, usage_state) = StreamingRequestHook::with_scratchpad_budget(
            options,
            request_id,
            scratchpad_budget,
            client_tool_names,
        );

        match self {
            Self::OpenAI(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Anthropic(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Bedrock(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Gemini(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::Ollama(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            Self::OpenRouter(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
            #[cfg(test)]
            Self::Scripted(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                crate::streaming::AgentRun::new(
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: String,
    pub usage: Usage,
}

/// Unified stream item type that erases provider-specific response types.
///
/// This allows callers to work with a single type regardless of which
/// LLM provider is being used.
#[derive(Debug, Clone)]
pub enum StreamItem {
    /// Assistant is streaming content
    StreamAssistantItem(StreamedAssistantContent),
    /// User content (e.g., tool results)
    StreamUserItem(StreamedUserContent),
    /// Final response with accumulated content
    Final(FinalResponseInfo),
    /// Internal marker for final response (filtered out before returning to caller)
    FinalMarker,
    /// Per-turn token usage from intermediate turns (not end-of-stream),
    /// with the turn's provider-reported prompt-cache split.
    /// Emitted on every Rig `Final` chunk so callers can capture usage
    /// even when they short-circuit before the terminal `FinalResponse`.
    TurnUsage(Usage, Option<rig::completion::CacheUsage>),
    /// An event emitted by a running agent, carrying the agent it came from.
    AgentEvent(Box<AgentEvent>),
    /// Per-agent scratchpad usage report.
    ///
    /// Emitted after an agent (single-agent or orchestration worker) finishes
    /// with scratchpad activity. The web server handler converts this to an
    /// `aura.scratchpad_usage` SSE event, filling in correlation context from
    /// the request.
    ScratchpadUsage {
        /// The agent that produced this usage (worker name, "main", etc.).
        agent_id: String,
        /// Tokens of raw tool output diverted to scratchpad.
        tokens_intercepted: usize,
        /// Tokens extracted from scratchpad back into context.
        tokens_extracted: usize,
    },
    /// Per-agent context-window occupancy report.
    ///
    /// Emitted after an orchestration agent finishes, carrying the provider's
    /// final-turn input/output. The web server handler converts this to an
    /// `aura.context_usage` SSE event, filling in correlation context from the
    /// request.
    ContextUsage {
        /// The agent that produced this usage (worker name, coordinator id, etc.).
        agent_id: String,
        /// Provider-reported input tokens of the final turn (context size).
        context_tokens: u64,
        /// Provider-reported output tokens of the final turn.
        response_tokens: u64,
        /// Model context-window limit, when known.
        context_window: Option<u64>,
    },
    /// MCP server connection status snapshot.
    ///
    /// Emitted once near the start of an orchestration run (after the shared
    /// `McpManager` is initialized) so clients can see degraded/unavailable
    /// servers. The web server handler converts this to an `aura.mcp_status`
    /// SSE event — the same event single-agent mode emits at stream start —
    /// filling in correlation context from the request.
    McpStatus(Vec<aura_events::McpServerStatus>),
}

/// Final response information.
#[derive(Debug, Clone)]
pub struct FinalResponseInfo {
    pub content: String,
    pub usage: Usage,
    /// Prompt-cache split matching `usage`'s turn population.
    pub cache_usage: Option<rig::completion::CacheUsage>,
}

/// Streamed assistant content (provider-agnostic).
#[derive(Debug, Clone)]
pub enum StreamedAssistantContent {
    /// Text chunk from the assistant
    Text(String),
    /// Complete tool call with all data (name + arguments).
    /// This is what we use for tool execution.
    ToolCall(ToolCall),
    /// Incremental tool call data for streaming UX.
    /// The LLM sends `name` first, then `delta` chunks with argument data.
    /// We don't use this - we wait for the complete `ToolCall` instead.
    ToolCallDelta {
        id: String,
        name: Option<String>,
        delta: Option<String>,
    },
    /// Complete reasoning content from models with thinking capabilities
    /// (e.g., Claude's extended thinking, OpenAI o1, Gemini, Cohere).
    Reasoning(String),
    /// Incremental reasoning content for streaming UX.
    /// We don't use this - we wait for the complete `Reasoning` instead.
    ReasoningDelta { id: Option<String>, delta: String },
}

/// Streamed user content (provider-agnostic).
#[derive(Debug, Clone)]
pub enum StreamedUserContent {
    /// Tool result from execution
    ToolResult(ToolResult),
}

/// Tool call information.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Tool result information.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub id: String,
    pub call_id: Option<String>,
    pub result: String,
    pub kind: ToolResultKind,
}

/// What a tool result's text holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultKind {
    /// The tool's output, as text.
    Text,
    /// The tool's output, as JSON text.
    Json,
    /// The call's error.
    Error,
}

impl ToolResult {
    /// The call's failure as text to show people, or `None` when the call
    /// produced output. A call fails with an error, or with output that
    /// reports one (an MCP error result or a HITL denial).
    pub fn failure(&self) -> Option<&str> {
        match self.kind {
            ToolResultKind::Error => Some(crate::tool_error_detection::tool_error_message(
                &self.result,
            )),
            ToolResultKind::Text | ToolResultKind::Json
                if crate::tool_error_detection::is_reported_error(&self.result) =>
            {
                Some(&self.result)
            }
            ToolResultKind::Text | ToolResultKind::Json => None,
        }
    }
}

/// Stream error type.
pub type StreamError = Box<dyn std::error::Error + Send + Sync>;

/// Map a provider-specific stream item to our unified type.
///
/// This function handles the conversion from rig's generic streaming types
/// to our provider-agnostic types. The generic parameter R is the provider's
/// streaming response type, which we don't need to inspect.
fn map_stream_item<R: rig::completion::GetTokenUsage>(
    item: Result<MultiTurnStreamItem<R>, impl std::error::Error + Send + Sync + 'static>,
) -> Result<StreamItem, StreamError> {
    use rig::streaming::{
        StreamedAssistantContent as RigAssistant, StreamedUserContent as RigUser,
    };

    match item {
        Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => {
            let mapped = match content {
                RigAssistant::Text(text) => StreamedAssistantContent::Text(text.text),
                RigAssistant::ToolCall(tc) => StreamedAssistantContent::ToolCall(ToolCall {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.to_string(),
                }),
                RigAssistant::ToolCallDelta { id, content } => {
                    let (name, delta) = match content {
                        rig::streaming::ToolCallDeltaContent::Name(n) => (Some(n), None),
                        rig::streaming::ToolCallDeltaContent::Delta(d) => (None, Some(d)),
                    };
                    StreamedAssistantContent::ToolCallDelta { id, name, delta }
                }
                RigAssistant::Reasoning(r) => {
                    StreamedAssistantContent::Reasoning(r.reasoning.join("\n"))
                }
                RigAssistant::ReasoningDelta { id, reasoning } => {
                    StreamedAssistantContent::ReasoningDelta {
                        id,
                        delta: reasoning,
                    }
                }
                RigAssistant::Final(ref resp) => {
                    // Per-turn usage — not end-of-stream. Callers that
                    // short-circuit (e.g. stream_and_collect) can capture
                    // token counts from this without waiting for FinalResponse.
                    let usage = resp.token_usage().unwrap_or(Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                        total_tokens: 0,
                    });
                    return Ok(StreamItem::TurnUsage(usage, resp.cache_token_usage()));
                }
            };
            Ok(StreamItem::StreamAssistantItem(mapped))
        }
        Ok(MultiTurnStreamItem::StreamUserItem(content)) => {
            let mapped = match content {
                RigUser::ToolResult(tr) => {
                    let (result, kind) = read_tool_result(&tr.content);
                    StreamedUserContent::ToolResult(ToolResult {
                        call_id: tr.call_id.clone(),
                        id: tr.id.clone(),
                        result,
                        kind,
                    })
                }
            };
            Ok(StreamItem::StreamUserItem(mapped))
        }
        Ok(MultiTurnStreamItem::FinalResponse(final_resp)) => {
            let usage = final_resp.usage();
            Ok(StreamItem::Final(FinalResponseInfo {
                content: final_resp.response().to_string(),
                usage,
                cache_usage: final_resp.cache_usage(),
            }))
        }
        // Handle any future variants added to the non-exhaustive enum
        Ok(_) => Ok(StreamItem::FinalMarker),
        Err(e) => Err(Box::new(e) as StreamError),
    }
}

/// A rig tool result's text and what it holds.
///
/// Rig JSON-encodes a tool's output and passes a call's error as its plain
/// display text, which is never JSON. A string output is decoded to its text;
/// any other JSON output is kept as it is.
fn read_tool_result(content: &rig::OneOrMany<ToolResultContent>) -> (String, ToolResultKind) {
    let mut kind = ToolResultKind::Text;
    let text = content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(text) => {
                if let Ok(output) = serde_json::from_str::<String>(&text.text) {
                    output
                } else {
                    if serde_json::from_str::<serde::de::IgnoredAny>(&text.text).is_err() {
                        kind = ToolResultKind::Error;
                    } else if kind == ToolResultKind::Text {
                        kind = ToolResultKind::Json;
                    }
                    text.text.clone()
                }
            }
            ToolResultContent::Image(_) => "[Image content]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Joined items are no longer one JSON value.
    if kind == ToolResultKind::Json && content.len() > 1 {
        kind = ToolResultKind::Text;
    }
    (text, kind)
}

/// Builder state enum for handling AgentBuilder type transitions.
///
/// After the first `.tool()` call, `AgentBuilder<M>` transitions to
/// `AgentBuilderSimple<M>`. This enum handles both states uniformly.
pub(crate) enum BuilderState<M: CompletionModel> {
    Initial(AgentBuilder<M>),
    WithTools(AgentBuilderSimple<M>),
}

impl<M> BuilderState<M>
where
    M: CompletionModel + Send + Sync,
{
    /// Add a tool to the agent builder.
    pub fn add_tool<T>(self, tool: T) -> BuilderState<M>
    where
        T: rig::tool::Tool + Send + Sync + 'static,
    {
        match self {
            BuilderState::Initial(builder) => BuilderState::WithTools(builder.tool(tool)),
            BuilderState::WithTools(builder) => BuilderState::WithTools(builder.tool(tool)),
        }
    }

    /// Add multiple dynamic tools to the agent builder.
    ///
    /// This accepts pre-boxed `ToolDyn` objects, which is useful for adding
    /// tools from external crates that implement `rig::tool::Tool`.
    pub fn add_tools_dyn(self, tools: Vec<Box<dyn rig::tool::ToolDyn>>) -> BuilderState<M> {
        if tools.is_empty() {
            return self;
        }
        match self {
            BuilderState::Initial(builder) => BuilderState::WithTools(builder.tools(tools)),
            BuilderState::WithTools(builder) => BuilderState::WithTools(builder.tools(tools)),
        }
    }

    /// Build the final agent.
    pub fn build(self) -> rig::agent::Agent<M> {
        match self {
            BuilderState::Initial(builder) => builder.build(),
            BuilderState::WithTools(builder) => builder.build(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &str) -> (String, ToolResultKind) {
        read_tool_result(&rig::OneOrMany::one(ToolResultContent::text(raw)))
    }

    fn result(text: &str, kind: ToolResultKind) -> ToolResult {
        ToolResult {
            id: "call_1".into(),
            call_id: None,
            result: text.into(),
            kind,
        }
    }

    #[test]
    fn a_string_output_is_decoded_from_rigs_json_encoding() {
        assert_eq!(
            read(r#""line one\nline two""#),
            ("line one\nline two".into(), ToolResultKind::Text)
        );
    }

    #[test]
    fn other_json_output_is_kept_as_it_is() {
        assert_eq!(
            read(r#"{"files": []}"#),
            (r#"{"files": []}"#.into(), ToolResultKind::Json)
        );
    }

    #[test]
    fn text_that_is_not_json_is_the_calls_error() {
        assert_eq!(
            read("Toolset error: ToolCallError: boom"),
            (
                "Toolset error: ToolCallError: boom".into(),
                ToolResultKind::Error
            )
        );
    }

    /// Reading a result's kind depends on how rig carries a tool's output
    /// and a call's error, so run real tools through rig's tool server.
    #[tokio::test]
    async fn rig_carries_output_as_json_and_an_error_as_plain_text() {
        use rig::tool::server::ToolServer;
        use rig::tool::{Tool, ToolError};

        #[derive(Clone)]
        struct Echo;

        impl Tool for Echo {
            const NAME: &'static str = "echo";
            type Error = ToolError;
            type Args = serde_json::Value;
            type Output = String;

            async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
                rig::completion::ToolDefinition {
                    name: Self::NAME.into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }

            async fn call(&self, _args: Self::Args) -> Result<String, ToolError> {
                Ok("Tool execution failed: disk full on node-3".into())
            }
        }

        #[derive(Clone)]
        struct Listing;

        impl Tool for Listing {
            const NAME: &'static str = "listing";
            type Error = ToolError;
            type Args = serde_json::Value;
            type Output = serde_json::Value;

            async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
                rig::completion::ToolDefinition {
                    name: Self::NAME.into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }

            async fn call(&self, _args: Self::Args) -> Result<serde_json::Value, ToolError> {
                Ok(serde_json::json!({"files": ["a.txt"]}))
            }
        }

        #[derive(Clone)]
        struct Failing;

        impl Tool for Failing {
            const NAME: &'static str = "failing";
            type Error = ToolError;
            type Args = serde_json::Value;
            type Output = String;

            async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
                rig::completion::ToolDefinition {
                    name: Self::NAME.into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }

            async fn call(&self, _args: Self::Args) -> Result<String, ToolError> {
                Err(ToolError::ToolCallError("42".into()))
            }
        }

        let handle = ToolServer::new()
            .tool(Echo)
            .tool(Listing)
            .tool(Failing)
            .run();
        let text_of = |raw: String| read(&raw);

        let echoed = handle.call_tool("echo", "{}").await.unwrap();
        assert_eq!(
            text_of(echoed),
            (
                "Tool execution failed: disk full on node-3".into(),
                ToolResultKind::Text
            )
        );

        let listed = handle.call_tool("listing", "{}").await.unwrap();
        assert_eq!(text_of(listed).1, ToolResultKind::Json);

        let failed = handle.call_tool("failing", "{}").await.unwrap_err();
        assert_eq!(text_of(failed.to_string()).1, ToolResultKind::Error);
    }

    #[test]
    fn a_failure_is_an_error_or_output_that_reports_one() {
        assert_eq!(
            result("Toolset error: ToolCallError: boom", ToolResultKind::Error).failure(),
            Some("boom")
        );
        assert_eq!(
            result("Tool returned an error: 401", ToolResultKind::Text).failure(),
            Some("Tool returned an error: 401")
        );
        assert_eq!(
            result("Tool execution failed: disk full", ToolResultKind::Text).failure(),
            None
        );
        assert_eq!(
            result(r#"{"ok": true}"#, ToolResultKind::Json).failure(),
            None
        );
    }
}
