//! Model Context Protocol (MCP) client integration: connecting to MCP
//! servers over HTTP-streamable, SSE, and STDIO transports; discovering and
//! sanitizing tools; and executing tool calls with unified logging,
//! cancellation, and progress-notification support.
//!
//! # Layout
//!
//! - [`McpManager`] owns per-server connections and discovered tools across
//!   all three transports, and is the entry point built from `[mcp]` config
//!   (`initialize_from_config`).
//! - [`McpClient`] (`client`) is the transport-agnostic client each
//!   connection uses to call tools, track in-flight requests, and support
//!   cancellation/progress.
//! - [`McpToolAdaptor`] (`dynamic`) is the `RigTool` wrapper the connection
//!   paths build for every discovered tool, regardless of transport.
//! - `execution` and `response` hold the shared tool-call execution and
//!   result-extraction logic used by every transport.
//! - `sse` implements the legacy SSE transport; `progress` forwards MCP
//!   progress notifications to request-scoped SSE subscribers.
//! - `tools` holds legacy, unused `RigTool` wrappers kept only for source
//!   compatibility — see its module doc.

pub(crate) mod client;
mod dynamic;
mod execution;
mod manager;
mod progress;
mod response;
mod sse;
mod tools;

pub use client::{InFlightRequests, McpClient};
pub use dynamic::McpToolAdaptor;
pub use execution::execute_mcp_tool;
pub use manager::{ConnectionStatus, McpManager, ServerInfo};
pub use progress::ProgressEnabledHandler;
pub use response::{CallOutcome, MAX_TOOL_ERROR_BYTES, bound_error_content, extract_tool_result};
pub use sse::SseTransport;

#[allow(deprecated)]
pub use tools::{
    AnalyzeLogsRelativeTimeTool, AnalyzeLogsTimeRangeTool, ExportLogsRelativeTimeTool,
    ExportLogsTimeRangeTool, FallbackHttpMcpTool, GetCurrentTimeTool, GetPipelineTool,
    ListPipelinesTool, StreamableHttpMcpTool, ToolExecutionError, create_fallback_http_tool,
};
