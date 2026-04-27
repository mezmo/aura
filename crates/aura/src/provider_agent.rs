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
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::watch;

use crate::orchestration::OrchestratorEvent;
use crate::scratchpad::ContextBudget;
use crate::streaming_request_hook::StreamingRequestHook;

// Type aliases for provider-specific completion models
pub type OpenAICompletionModel =
    rig::providers::openai::completion::CompletionModel<reqwest::Client>;
pub type AnthropicCompletionModel =
    rig::providers::anthropic::completion::CompletionModel<reqwest::Client>;
pub type BedrockCompletionModel = rig_bedrock::completion::CompletionModel;
pub type OllamaCompletionModel = rig::providers::ollama::CompletionModel<reqwest::Client>;
pub type GeminiCompletionModel =
    rig::providers::gemini::completion::CompletionModel<reqwest::Client>;

// Type aliases for provider-specific agents
pub type OpenAIAgent = rig::agent::Agent<OpenAICompletionModel>;
pub type AnthropicAgent = rig::agent::Agent<AnthropicCompletionModel>;
pub type BedrockAgent = rig::agent::Agent<BedrockCompletionModel>;
pub type OllamaAgent = rig::agent::Agent<OllamaCompletionModel>;
pub type GeminiAgent = rig::agent::Agent<GeminiCompletionModel>;

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
        }
    }

    /// Stream a prompt with timeout and cancellation support.
    ///
    /// Returns (stream, cancel_sender, usage_state):
    /// - stream: The actual stream of completion items
    /// - cancel_sender: Send `true` to cancel the stream
    /// - usage_state: Shared state for reading final usage at stream end
    pub async fn stream_prompt_with_timeout(
        &self,
        query: &str,
        max_depth: usize,
        timeout: Duration,
        request_id: &str,
        scratchpad_budget: Option<ContextBudget>,
    ) -> (
        Pin<Box<dyn futures::Stream<Item = Result<StreamItem, StreamError>> + Send>>,
        watch::Sender<bool>,
        crate::streaming_request_hook::UsageState,
    ) {
        let (hook, cancel_tx, usage_state) =
            StreamingRequestHook::with_scratchpad_budget(timeout, request_id, scratchpad_budget);

        match self {
            Self::OpenAI(agent) => {
                let stream = agent
                    .stream_prompt(query)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                (
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
                (
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
                (
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
                (
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
                (
                    Box::pin(stream.map::<Result<StreamItem, StreamError>, _>(map_stream_item)),
                    cancel_tx,
                    usage_state,
                )
            }
        }
    }

    /// Stream a chat with timeout and cancellation support.
    ///
    /// Returns (stream, cancel_sender, usage_state):
    /// - stream: The actual stream of completion items
    /// - cancel_sender: Send `true` to cancel the stream
    /// - usage_state: Shared state for reading final usage at stream end
    pub async fn stream_chat_with_timeout(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        max_depth: usize,
        timeout: Duration,
        request_id: &str,
        scratchpad_budget: Option<ContextBudget>,
    ) -> (
        Pin<Box<dyn futures::Stream<Item = Result<StreamItem, StreamError>> + Send>>,
        watch::Sender<bool>,
        crate::streaming_request_hook::UsageState,
    ) {
        let (hook, cancel_tx, usage_state) =
            StreamingRequestHook::with_scratchpad_budget(timeout, request_id, scratchpad_budget);

        match self {
            Self::OpenAI(agent) => {
                let stream = agent
                    .stream_chat(query, chat_history)
                    .with_hook(hook)
                    .multi_turn(max_depth)
                    .await;
                (
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
                (
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
                (
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
                (
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
                (
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
    /// Per-turn token usage from intermediate turns (not end-of-stream).
    /// Emitted on every Rig `Final` chunk so callers can capture usage
    /// even when they short-circuit before the terminal `FinalResponse`.
    TurnUsage(Usage),
    /// Orchestrator status event (plan progress, task status, etc.)
    OrchestratorEvent(OrchestratorEvent),
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
}

/// Final response information.
#[derive(Debug, Clone)]
pub struct FinalResponseInfo {
    pub content: String,
    pub usage: Usage,
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
                    return Ok(StreamItem::TurnUsage(usage));
                }
            };
            Ok(StreamItem::StreamAssistantItem(mapped))
        }
        Ok(MultiTurnStreamItem::StreamUserItem(content)) => {
            let mapped = match content {
                RigUser::ToolResult(tr) => StreamedUserContent::ToolResult(ToolResult {
                    call_id: tr.call_id.clone(),
                    id: tr.id.clone(),
                    result: format_tool_result_content(&tr.content),
                }),
            };
            Ok(StreamItem::StreamUserItem(mapped))
        }
        Ok(MultiTurnStreamItem::FinalResponse(final_resp)) => {
            let usage = final_resp.usage();
            Ok(StreamItem::Final(FinalResponseInfo {
                content: final_resp.response().to_string(),
                usage,
            }))
        }
        // Handle any future variants added to the non-exhaustive enum
        Ok(_) => Ok(StreamItem::FinalMarker),
        Err(e) => Err(Box::new(e) as StreamError),
    }
}

/// Format tool result content to a string.
fn format_tool_result_content(content: &rig::OneOrMany<ToolResultContent>) -> String {
    content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(text) => text.text.clone(),
            ToolResultContent::Image(_) => "[Image content]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
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

    /// Add RMCP tools to the agent builder.
    pub fn add_rmcp_tools(
        self,
        tools: Vec<rmcp::model::Tool>,
        client: rmcp::service::ServerSink,
    ) -> BuilderState<M> {
        match self {
            BuilderState::Initial(builder) => {
                BuilderState::WithTools(builder.rmcp_tools(tools, client))
            }
            BuilderState::WithTools(builder) => {
                BuilderState::WithTools(builder.rmcp_tools(tools, client))
            }
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
