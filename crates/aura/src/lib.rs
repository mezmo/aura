//! # Rig Agent Builder
//!
//! A library for constructing Rig.rs agents from configuration structs.
//! This crate is independent of TOML parsing and can be used directly
//! in web services or other applications that need to build agents
//! programmatically.

pub mod approval_event_broker;
pub mod bedrock_embedding;
pub mod builder;
pub mod config;
pub mod env_flags;
pub mod error;
pub mod fallback_tool_parser;
pub mod fallback_tool_stream;
pub mod hitl;
pub mod logging;
pub mod mcp;
pub mod mcp_dynamic;
pub mod mcp_progress;
pub mod mcp_response;
pub mod mcp_sse;
pub mod mcp_streamable_http;
pub mod mcp_tool_execution;
#[cfg(feature = "otel")]
pub mod openinference_exporter;
pub mod orchestration;
pub mod passthrough_tool;
pub mod prompts;
mod provider_agent; // Private - internal implementation detail
pub mod rag_tools;
pub mod request_cancellation;
pub mod request_progress;
pub mod rig_builder;
mod schema_sanitize; // Private - MCP schema sanitization for OpenAI compatibility
pub mod scratchpad;
pub mod session_store;
pub mod skill_tool;
pub mod stream_events;
pub mod streaming;
pub mod streaming_request_hook;
pub(crate) mod string_utils;
pub mod tool_call_observer;
pub mod tool_error_detection;
pub mod tool_event_broker;
pub mod tool_wrapper;
pub mod tools;
pub mod turn_nudge;
pub mod vector_dynamic;
pub mod vector_store;

pub use builder::{Agent, AgentBuilder, FilesystemTools, build_streaming_agent};
pub use config::{AgentRuntimeConfig, SessionId, ToolContextFactory};
// Pure config types are owned by `aura-config` and re-exported here for
// ergonomic consumption (`aura::LlmConfig`, etc.).
pub use aura_config::{
    AgentConfig, AgentSettings, EmbeddingConfig, LlmConfig, McpConfig, McpServerConfig,
    ReasoningEffort, SkillConfig, TodoToolsConfig, ToolsConfig, VectorStoreConfig, VectorStoreType,
    glob_match, lenient_int,
};
pub use error::{BuilderError, BuilderResult};
pub use orchestration::tools::{
    CreatePlanTool, RequestClarificationTool, RespondDirectlyTool, RoutingDecision, RoutingToolSet,
};
pub use orchestration::{
    ArtifactsConfig, EventContext, OrchestrationConfig, OrchestrationStreamEvent, Orchestrator,
    OrchestratorEvent, OrchestratorFactory, Plan, PlanningResponse, RoutingMode, RunId, Task,
    TaskIdentity, TaskJson, TaskState, TaskStatus, TimeoutsConfig, agent_info, worker_overview,
};
pub use passthrough_tool::{PASSTHROUGH_MARKER, PassthroughTool};
pub use provider_agent::{
    FinalResponseInfo, StreamError, StreamItem, StreamedAssistantContent, StreamedUserContent,
    ToolCall, ToolResult,
};
pub use rig::completion::{Message, ToolDefinition as RigToolDefinition};
pub use rig::message::{AssistantContent, ToolCall as RigToolCall, ToolResultContent, UserContent};
pub use rig::one_or_many::OneOrMany;
pub use rig::tool::{Tool as RigTool, ToolDyn};
pub use rig_builder::RigBuilder;
pub use scratchpad::{ScratchpadConfig, ScratchpadToolEntry};
pub use streaming::StreamingAgent;

// Legacy aliases (deprecated)
#[deprecated(since = "1.2.0", note = "use StreamedAssistantContent instead")]
pub type AuraStreamedAssistantContent = provider_agent::StreamedAssistantContent;
#[deprecated(since = "1.2.0", note = "use StreamedUserContent instead")]
pub type AuraStreamedUserContent = provider_agent::StreamedUserContent;
#[deprecated(since = "1.2.0", note = "use ToolCall instead")]
pub type AuraToolCall = provider_agent::ToolCall;
#[deprecated(since = "1.2.0", note = "use ToolResult instead")]
pub type AuraToolResult = provider_agent::ToolResult;

pub use approval_event_broker::{
    ApprovalEventBroker, ApprovalLifecycleEvent, subscribe as approval_event_subscribe,
    unsubscribe as approval_event_unsubscribe,
};
pub use mcp::McpManager;
pub use mcp_progress::ProgressEnabledHandler;
pub use mcp_streamable_http::InFlightRequests;
pub use rag_tools::{AutoIngest, VectorIngestTool};
pub use request_cancellation::{RequestCancellation, RequestId};
pub use request_progress::{
    ProgressNotification, RequestProgressBroker, global as request_progress_global,
    subscribe as request_progress_subscribe, unsubscribe as request_progress_unsubscribe,
};
pub use rmcp::model::{NumberOrString, ProgressToken};
pub use skill_tool::{LoadSkillTool, ReadSkillFileTool, SkillToolset, render_skill_catalog};
pub use stream_events::{
    AgentContext, AuraStreamEvent, CorrelationContext, CorrelationContextExt, WorkerPhase,
    format_named_sse,
};
pub use streaming_request_hook::{ResponseContent, StreamingRequestHook, UsageState};
pub use tool_call_observer::{RetryHint, ToolCallObserver, ToolEvent, ToolOutcome};
pub use tool_error_detection::{DetectedToolError, ToolResultStatus, detect_tool_error};
pub use tool_event_broker::{
    ToolCallId, ToolEventBroker, ToolLifecycleEvent, ToolName, ToolUsageEvent,
    global as tool_event_global, peek_tool_call_id, pop_tool_call_id, publish_tool_start,
    publish_tool_usage, push_tool_call_id, subscribe as tool_event_subscribe, tool_usage_subscribe,
    tool_usage_unsubscribe, unsubscribe as tool_event_unsubscribe,
};
pub use tool_wrapper::{
    ComposedWrapper, ToolCallContext, ToolWrapper, TransformArgsResult, TransformOutputResult,
    WrappedTool,
};
pub use tools::{FilesystemTool, ListDirTool, ReadFileTool, WriteFileTool};
pub use vector_dynamic::DynamicVectorSearchTool;

// Fallback tool parser and stream wrapper for Ollama
pub use fallback_tool_parser::{ParsedToolCall, parse_fallback_tool_calls};
pub use fallback_tool_stream::FallbackToolExecutor;
