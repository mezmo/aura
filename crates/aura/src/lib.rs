//! # Rig Agent Builder
//!
//! A library for constructing Rig.rs agents from configuration structs.
//! This crate is independent of TOML parsing and can be used directly
//! in web services or other applications that need to build agents
//! programmatically.

pub mod builder;
pub mod config;
pub mod error;
pub mod fallback_tool_parser;
pub mod fallback_tool_stream;
pub mod logging;
pub mod mcp;
pub mod mcp_dynamic;
pub mod mcp_progress;
pub mod mcp_response;
pub mod mcp_streamable_http;
pub mod mcp_tool_execution;
pub mod model_limits;
#[cfg(feature = "otel")]
pub mod openinference_exporter;
pub mod orchestration;
pub mod prompts;
mod provider_agent; // Private - internal implementation detail
pub mod rag_tools;
pub mod request_cancellation;
pub mod request_progress;
mod schema_sanitize; // Private - MCP schema sanitization for OpenAI compatibility
pub mod stream_events;
pub mod streaming;
pub mod streaming_request_hook;
pub(crate) mod string_utils;
pub mod todo_tool;
pub mod tool_call_observer;
pub mod tool_error_detection;
pub mod tool_event_broker;
pub mod tool_wrapper;
pub mod tools;
pub mod vector_dynamic;
pub mod vector_store;

pub use builder::{build_streaming_agent, Agent, AgentBuilder, FilesystemTools};
pub use config::{
    AgentConfig, AgentSettings, EmbeddingModelConfig, LlmConfig, McpConfig, McpServerConfig,
    ReasoningEffort, TodoToolsConfig, ToolsConfig, VectorStoreConfig,
};
pub use error::{BuilderError, BuilderResult};
pub use orchestration::tools::{
    CreatePlanTool, RequestClarificationTool, RespondDirectlyTool, RoutingDecision, RoutingToolSet,
};
pub use orchestration::{
    ArtifactsConfig, OrchestrationConfig, OrchestrationStreamEvent, Orchestrator,
    OrchestratorEvent, Plan, PlanAttemptFailure, PlanningResponse, Task, TaskJson, TaskStatus,
    TimeoutsConfig,
};
pub use provider_agent::{
    FinalResponseInfo, StreamError, StreamItem, StreamedAssistantContent, StreamedUserContent,
    ToolCall, ToolResult,
};
pub use rig::completion::Message;
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

pub use mcp::McpManager;
pub use mcp_progress::ProgressEnabledHandler;
pub use mcp_streamable_http::InFlightRequests;
pub use model_limits::get_context_limit;
pub use rag_tools::{AutoIngest, VectorIngestTool};
pub use request_cancellation::{RequestCancellation, RequestId};
pub use request_progress::{
    ProgressNotification, RequestProgressBroker, global as request_progress_global,
    subscribe as request_progress_subscribe, unsubscribe as request_progress_unsubscribe,
};
pub use rmcp::model::{NumberOrString, ProgressToken};
pub use stream_events::{
    format_named_sse, AgentContext, AuraStreamEvent, CorrelationContext, WorkerPhase,
};
pub use streaming_request_hook::{ResponseContent, StreamingRequestHook, UsageState};
pub use todo_tool::{
    PlanIteration, PlanState, ReadTodosArgs, ReadTodosTool, Todo, TodoError, TodoState, TodoStatus,
    TodoWriteTool, WriteTodosArgs, TODO_SYSTEM_PROMPT, TODO_TOOL_DESCRIPTION,
};
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
