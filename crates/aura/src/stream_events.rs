//! Aura-specific SSE streaming events for enhanced client UX.
//!
//! Emitted alongside OpenAI-compatible chunks for tool execution observability.
//! All events include `CorrelationContext` for Phoenix/OTEL trace correlation.

use rmcp::model::ProgressToken;
use serde::Serialize;

/// Context identifying which agent emitted an event.
///
/// For single-agent deployments, use `AgentContext::single_agent()` which sets
/// `agent_id: "main"`. For multi-agent orchestrators, populate `parent_agent_id`
/// to establish the agent hierarchy.
///
/// Note: `AgentContext::default()` gives an empty `agent_id` - use `single_agent()`
/// for the standard single-agent context.
#[derive(Clone, Debug, Serialize, Default)]
pub struct AgentContext {
    /// Unique identifier for this agent (e.g., "main", "log_worker", "rca_worker")
    pub agent_id: String,
    /// Human-readable name for display (e.g., "Aura Agent", "Log Analysis Worker")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Parent agent ID for hierarchical multi-agent setups
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
}

impl AgentContext {
    /// Create a default single-agent context with agent_id = "main"
    pub fn single_agent() -> Self {
        Self {
            agent_id: "main".to_string(),
            agent_name: None,
            parent_agent_id: None,
        }
    }

    /// Create a single-agent context with a custom name
    pub fn single_agent_with_name(name: impl Into<String>) -> Self {
        Self {
            agent_id: "main".to_string(),
            agent_name: Some(name.into()),
            parent_agent_id: None,
        }
    }

    /// Create a worker agent context with parent hierarchy
    pub fn worker(
        id: impl Into<String>,
        name: Option<String>,
        parent_id: impl Into<String>,
    ) -> Self {
        Self {
            agent_id: id.into(),
            agent_name: name,
            parent_agent_id: Some(parent_id.into()),
        }
    }
}

/// Correlation context for OTEL/Phoenix tracing and session grouping.
///
/// All Aura events include this context to enable:
/// - Session grouping: All events in a conversation share `session_id`
/// - Trace correlation: Link SSE events to Phoenix/OTEL traces via `trace_id`
#[derive(Clone, Debug, Serialize, Default)]
pub struct CorrelationContext {
    /// Chat session ID (from request handler or X-Chat-Session-Id header)
    pub session_id: String,
    /// OTEL trace ID (from tracing::Span) for Phoenix correlation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl CorrelationContext {
    /// Create correlation context from the current tracing span.
    ///
    /// Captures the trace ID from `tracing::Span::current()` for OTEL correlation.
    pub fn from_current_span(session_id: impl Into<String>) -> Self {
        let trace_id = tracing::Span::current()
            .id()
            .map(|id| format!("{:x}", id.into_u64()));
        Self {
            session_id: session_id.into(),
            trace_id,
        }
    }

    /// Create correlation context with explicit values (for testing or custom setups)
    pub fn new(session_id: impl Into<String>, trace_id: Option<String>) -> Self {
        Self {
            session_id: session_id.into(),
            trace_id,
        }
    }
}

/// Worker phase for multi-agent orchestration (future use).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    /// Worker is planning its approach
    Planning,
    /// Worker is executing tools
    Executing,
    /// Worker is analyzing results
    Analyzing,
}

/// Aura-specific streaming events sent via SSE `event:` field.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum AuraStreamEvent {
    /// Emitted at stream start with model and context limit information.
    /// UI can use this for context window percentage calculation.
    SessionInfo {
        /// The model name (e.g., "gpt-4o", "claude-3-opus")
        model: String,
        /// Context limit for this model (in tokens), if known
        #[serde(skip_serializing_if = "Option::is_none")]
        model_context_limit: Option<u32>,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
    /// Emitted when the LLM decides to call a tool (immediate UI feedback).
    /// This is sent as soon as we know a tool will be called, before MCP execution.
    ToolRequested {
        tool_id: String,
        tool_name: String,
        arguments: serde_json::Value,
        #[serde(flatten)]
        agent: AgentContext,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
    /// Emitted when MCP tool execution actually begins.
    /// Contains progress_token for correlating with aura.progress events.
    ToolStart {
        tool_id: String,
        tool_name: String,
        /// Progress token for correlating with aura.progress events from MCP server
        #[serde(skip_serializing_if = "Option::is_none")]
        progress_token: Option<ProgressToken>,
        #[serde(flatten)]
        agent: AgentContext,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
    /// Emitted when a tool call completes (success or failure).
    ToolComplete {
        tool_id: String,
        tool_name: String,
        duration_ms: u64,
        success: bool,
        /// The actual tool result content (for success) or error message (for failure)
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(flatten)]
        agent: AgentContext,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
    /// Emitted for LLM reasoning content (Anthropic extended thinking).
    Reasoning {
        content: String,
        #[serde(flatten)]
        agent: AgentContext,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
    /// Emitted for initialization/discovery progress updates.
    /// Contains progress_token for correlating with the aura.tool_start event.
    Progress {
        message: String,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<u8>,
        /// Token linking this progress to the tool that generated it (from aura.tool_start)
        #[serde(skip_serializing_if = "Option::is_none")]
        progress_token: Option<ProgressToken>,
        #[serde(flatten)]
        agent: AgentContext,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
    /// Emitted for multi-agent worker phase transitions (future).
    WorkerPhase {
        phase: WorkerPhase,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(flatten)]
        agent: AgentContext,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
    /// Emitted when usage information becomes available, associating tools with usage snapshot.
    /// This is emitted from the `on_stream_completion_response_finish` hook.
    ToolUsage {
        /// Tool IDs completed since the last usage event
        tool_ids: Vec<String>,
        /// Prompt tokens (context size at this point)
        prompt_tokens: u64,
        /// Completion tokens generated
        completion_tokens: u64,
        /// Total tokens used
        total_tokens: u64,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
    /// Emitted at stream end with final usage information.
    /// UI uses `prompt_tokens` to calculate context window percentage:
    /// `percentage = (prompt_tokens / model_context_limit) * 100`
    Usage {
        /// Prompt tokens (final context size for % calculation)
        prompt_tokens: u64,
        /// Completion tokens generated in final response
        completion_tokens: u64,
        /// Total tokens used
        total_tokens: u64,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
}

impl AuraStreamEvent {
    /// Get the SSE event name for this event type.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::SessionInfo { .. } => "aura.session_info",
            Self::ToolRequested { .. } => "aura.tool_requested",
            Self::ToolStart { .. } => "aura.tool_start",
            Self::ToolComplete { .. } => "aura.tool_complete",
            Self::Reasoning { .. } => "aura.reasoning",
            Self::Progress { .. } => "aura.progress",
            Self::WorkerPhase { .. } => "aura.worker_phase",
            Self::ToolUsage { .. } => "aura.tool_usage",
            Self::Usage { .. } => "aura.usage",
        }
    }

    /// Format this event as an SSE message with the event: field.
    ///
    /// Returns a string in the format:
    /// ```text
    /// event: aura.tool_start
    /// data: {"tool_id":"...","tool_name":"..."}
    ///
    /// ```
    pub fn format_sse(&self) -> String {
        let event_name = self.event_name();
        let data = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        format!("event: {event_name}\ndata: {data}\n\n")
    }

    /// Create a ToolRequested event (LLM decided to call a tool).
    /// This provides immediate UI feedback before MCP execution begins.
    pub fn tool_requested(
        tool_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
        agent: AgentContext,
        correlation: CorrelationContext,
    ) -> Self {
        Self::ToolRequested {
            tool_id: tool_id.into(),
            tool_name: tool_name.into(),
            arguments,
            agent,
            correlation,
        }
    }

    /// Create a ToolStart event (MCP execution actually beginning).
    /// This includes the progress_token for correlating with aura.progress events.
    pub fn tool_start(
        tool_id: impl Into<String>,
        tool_name: impl Into<String>,
        progress_token: Option<ProgressToken>,
        agent: AgentContext,
        correlation: CorrelationContext,
    ) -> Self {
        Self::ToolStart {
            tool_id: tool_id.into(),
            tool_name: tool_name.into(),
            progress_token,
            agent,
            correlation,
        }
    }

    /// Create a ToolComplete event for successful execution.
    pub fn tool_complete_success(
        tool_id: impl Into<String>,
        tool_name: impl Into<String>,
        duration_ms: u64,
        result: impl Into<String>,
        agent: AgentContext,
        correlation: CorrelationContext,
    ) -> Self {
        Self::ToolComplete {
            tool_id: tool_id.into(),
            tool_name: tool_name.into(),
            duration_ms,
            success: true,
            result: Some(result.into()),
            error: None,
            agent,
            correlation,
        }
    }

    /// Create a ToolComplete event for failed execution.
    pub fn tool_complete_failure(
        tool_id: impl Into<String>,
        tool_name: impl Into<String>,
        duration_ms: u64,
        error: impl Into<String>,
        agent: AgentContext,
        correlation: CorrelationContext,
    ) -> Self {
        Self::ToolComplete {
            tool_id: tool_id.into(),
            tool_name: tool_name.into(),
            duration_ms,
            success: false,
            result: None,
            error: Some(error.into()),
            agent,
            correlation,
        }
    }

    /// Create a Reasoning event.
    pub fn reasoning(
        content: impl Into<String>,
        agent: AgentContext,
        correlation: CorrelationContext,
    ) -> Self {
        Self::Reasoning {
            content: content.into(),
            agent,
            correlation,
        }
    }

    /// Create a Progress event with optional progress_token for tool correlation.
    pub fn progress(
        message: impl Into<String>,
        phase: impl Into<String>,
        percent: Option<u8>,
        progress_token: Option<ProgressToken>,
        agent: AgentContext,
        correlation: CorrelationContext,
    ) -> Self {
        Self::Progress {
            message: message.into(),
            phase: phase.into(),
            percent,
            progress_token,
            agent,
            correlation,
        }
    }

    /// Create a SessionInfo event (emitted at stream start).
    ///
    /// Contains model name and context limit for UI context window calculation.
    pub fn session_info(
        model: impl Into<String>,
        model_context_limit: Option<u32>,
        correlation: CorrelationContext,
    ) -> Self {
        Self::SessionInfo {
            model: model.into(),
            model_context_limit,
            correlation,
        }
    }

    /// Create a ToolUsage event (emitted when usage becomes available).
    ///
    /// Associates tool_ids with their usage snapshot. This is called from
    /// `on_stream_completion_response_finish` when the hook receives usage data.
    pub fn tool_usage(
        tool_ids: Vec<String>,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        correlation: CorrelationContext,
    ) -> Self {
        Self::ToolUsage {
            tool_ids,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            correlation,
        }
    }

    /// Create a Usage event (emitted at stream end).
    ///
    /// Contains final usage for context window percentage calculation:
    /// `percentage = (prompt_tokens / model_context_limit) * 100`
    pub fn usage(
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        correlation: CorrelationContext,
    ) -> Self {
        Self::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            correlation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_context_single_agent() {
        let ctx = AgentContext::single_agent();
        assert_eq!(ctx.agent_id, "main");
        assert!(ctx.agent_name.is_none());
        assert!(ctx.parent_agent_id.is_none());
    }

    #[test]
    fn test_agent_context_with_name() {
        let ctx = AgentContext::single_agent_with_name("My Agent");
        assert_eq!(ctx.agent_id, "main");
        assert_eq!(ctx.agent_name, Some("My Agent".to_string()));
    }

    #[test]
    fn test_agent_context_worker() {
        let ctx =
            AgentContext::worker("log_worker", Some("Log Worker".to_string()), "orchestrator");
        assert_eq!(ctx.agent_id, "log_worker");
        assert_eq!(ctx.agent_name, Some("Log Worker".to_string()));
        assert_eq!(ctx.parent_agent_id, Some("orchestrator".to_string()));
    }

    #[test]
    fn test_correlation_context_new() {
        let ctx = CorrelationContext::new("sess_123", Some("trace_abc".to_string()));
        assert_eq!(ctx.session_id, "sess_123");
        assert_eq!(ctx.trace_id, Some("trace_abc".to_string()));
    }

    #[test]
    fn test_tool_requested_event_name() {
        let event = AuraStreamEvent::tool_requested(
            "call_123",
            "list_pipelines",
            serde_json::json!({}),
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        assert_eq!(event.event_name(), "aura.tool_requested");
    }

    #[test]
    fn test_tool_start_event_name() {
        let event = AuraStreamEvent::tool_start(
            "call_123",
            "list_pipelines",
            None,
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        assert_eq!(event.event_name(), "aura.tool_start");
    }

    #[test]
    fn test_tool_complete_event_name() {
        let event = AuraStreamEvent::tool_complete_success(
            "call_123",
            "list_pipelines",
            100,
            "Tool result content",
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        assert_eq!(event.event_name(), "aura.tool_complete");
    }

    #[test]
    fn test_reasoning_event_name() {
        let event = AuraStreamEvent::reasoning(
            "Let me analyze...",
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        assert_eq!(event.event_name(), "aura.reasoning");
    }

    #[test]
    fn test_format_sse_tool_requested() {
        let event = AuraStreamEvent::tool_requested(
            "call_abc123",
            "list_pipelines",
            serde_json::json!({"filter": "error"}),
            AgentContext::single_agent(),
            CorrelationContext::new("sess_xyz", None),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.tool_requested\n"));
        assert!(sse.contains("data: "));
        assert!(sse.contains("\"tool_id\":\"call_abc123\""));
        assert!(sse.contains("\"tool_name\":\"list_pipelines\""));
        assert!(sse.contains("\"arguments\":{\"filter\":\"error\"}"));
        assert!(sse.contains("\"session_id\":\"sess_xyz\""));
        assert!(sse.ends_with("\n\n"));
    }

    #[test]
    fn test_format_sse_tool_start() {
        let event = AuraStreamEvent::tool_start(
            "call_abc123",
            "list_pipelines",
            Some(rmcp::model::ProgressToken(
                rmcp::model::NumberOrString::Number(42),
            )),
            AgentContext::single_agent(),
            CorrelationContext::new("sess_xyz", None),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.tool_start\n"));
        assert!(sse.contains("data: "));
        assert!(sse.contains("\"tool_id\":\"call_abc123\""));
        assert!(sse.contains("\"tool_name\":\"list_pipelines\""));
        assert!(sse.contains("\"progress_token\":42"));
        assert!(sse.contains("\"session_id\":\"sess_xyz\""));
        assert!(!sse.contains("\"arguments\"")); // ToolStart doesn't have arguments
        assert!(sse.ends_with("\n\n"));
    }

    #[test]
    fn test_format_sse_tool_complete() {
        let event = AuraStreamEvent::tool_complete_success(
            "call_abc123",
            "list_pipelines",
            1234,
            "Pipeline list result",
            AgentContext::single_agent(),
            CorrelationContext::new("sess_xyz", Some("trace_123".to_string())),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.tool_complete\n"));
        assert!(sse.contains("\"duration_ms\":1234"));
        assert!(sse.contains("\"success\":true"));
        assert!(sse.contains("\"result\":\"Pipeline list result\""));
        assert!(sse.contains("\"trace_id\":\"trace_123\""));
    }

    #[test]
    fn test_format_sse_tool_complete_failure() {
        let event = AuraStreamEvent::tool_complete_failure(
            "call_abc123",
            "list_pipelines",
            500,
            "Connection timeout",
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        let sse = event.format_sse();
        assert!(sse.contains("\"success\":false"));
        assert!(sse.contains("\"error\":\"Connection timeout\""));
        assert!(!sse.contains("\"result\":")); // result should be None for failures
    }

    #[test]
    fn test_skip_serializing_none_fields_tool_requested() {
        let event = AuraStreamEvent::tool_requested(
            "call_123",
            "test_tool",
            serde_json::json!({}),
            AgentContext::single_agent(), // No agent_name or parent_agent_id
            CorrelationContext::new("sess", None), // No trace_id
        );
        let sse = event.format_sse();
        // None fields should not appear in output
        assert!(!sse.contains("agent_name"));
        assert!(!sse.contains("parent_agent_id"));
        assert!(!sse.contains("trace_id"));
    }

    #[test]
    fn test_skip_serializing_none_fields_tool_start() {
        let event = AuraStreamEvent::tool_start(
            "call_123",
            "test_tool",
            None,                                  // No progress_token
            AgentContext::single_agent(),          // No agent_name or parent_agent_id
            CorrelationContext::new("sess", None), // No trace_id
        );
        let sse = event.format_sse();
        // None fields should not appear in output
        assert!(!sse.contains("agent_name"));
        assert!(!sse.contains("parent_agent_id"));
        assert!(!sse.contains("trace_id"));
        assert!(!sse.contains("progress_token"));
    }

    #[test]
    fn test_progress_event() {
        let event = AuraStreamEvent::progress(
            "Discovered 11 MCP tools",
            "discovery",
            Some(100),
            Some(rmcp::model::ProgressToken(
                rmcp::model::NumberOrString::Number(42),
            )),
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.progress\n"));
        assert!(sse.contains("\"message\":\"Discovered 11 MCP tools\""));
        assert!(sse.contains("\"phase\":\"discovery\""));
        assert!(sse.contains("\"percent\":100"));
        assert!(
            sse.contains("\"progress_token\":42"),
            "progress_token should be included for tool correlation"
        );
    }

    #[test]
    fn test_progress_event_without_token() {
        let event = AuraStreamEvent::progress(
            "Initializing...",
            "init",
            None,
            None, // No progress_token
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.progress\n"));
        assert!(!sse.contains("progress_token")); // Should be omitted when None
    }

    #[test]
    fn test_worker_phase_event() {
        let event = AuraStreamEvent::WorkerPhase {
            phase: WorkerPhase::Executing,
            task_id: Some("task_1".to_string()),
            agent: AgentContext::worker("log_worker", None, "orchestrator"),
            correlation: CorrelationContext::default(),
        };
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.worker_phase\n"));
        assert!(sse.contains("\"phase\":\"executing\""));
        assert!(sse.contains("\"task_id\":\"task_1\""));
        assert!(sse.contains("\"parent_agent_id\":\"orchestrator\""));
    }

    #[test]
    fn test_session_info_event() {
        let event = AuraStreamEvent::session_info(
            "gpt-4o",
            Some(128_000),
            CorrelationContext::new("sess_xyz", None),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.session_info\n"));
        assert!(sse.contains("\"model\":\"gpt-4o\""));
        assert!(sse.contains("\"model_context_limit\":128000"));
        assert!(sse.contains("\"session_id\":\"sess_xyz\""));
    }

    #[test]
    fn test_session_info_without_limit() {
        let event = AuraStreamEvent::session_info(
            "unknown-model",
            None,
            CorrelationContext::new("sess_abc", None),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.session_info\n"));
        assert!(sse.contains("\"model\":\"unknown-model\""));
        assert!(!sse.contains("model_context_limit")); // Should be omitted when None
    }

    #[test]
    fn test_tool_usage_event() {
        let event = AuraStreamEvent::tool_usage(
            vec!["call_abc".to_string(), "call_def".to_string()],
            18777,
            500,
            19277,
            CorrelationContext::new("sess_123", None),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.tool_usage\n"));
        assert!(sse.contains("\"tool_ids\":[\"call_abc\",\"call_def\"]"));
        assert!(sse.contains("\"prompt_tokens\":18777"));
        assert!(sse.contains("\"completion_tokens\":500"));
        assert!(sse.contains("\"total_tokens\":19277"));
        assert!(sse.contains("\"session_id\":\"sess_123\""));
    }

    #[test]
    fn test_usage_event() {
        let event = AuraStreamEvent::usage(
            21500,
            342,
            21842,
            CorrelationContext::new("sess_final", Some("trace_xyz".to_string())),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.usage\n"));
        assert!(sse.contains("\"prompt_tokens\":21500"));
        assert!(sse.contains("\"completion_tokens\":342"));
        assert!(sse.contains("\"total_tokens\":21842"));
        assert!(sse.contains("\"session_id\":\"sess_final\""));
        assert!(sse.contains("\"trace_id\":\"trace_xyz\""));
    }

    #[test]
    fn test_event_name_new_events() {
        let session_info =
            AuraStreamEvent::session_info("gpt-4", None, CorrelationContext::default());
        assert_eq!(session_info.event_name(), "aura.session_info");

        let tool_usage =
            AuraStreamEvent::tool_usage(vec![], 0, 0, 0, CorrelationContext::default());
        assert_eq!(tool_usage.event_name(), "aura.tool_usage");

        let usage = AuraStreamEvent::usage(0, 0, 0, CorrelationContext::default());
        assert_eq!(usage.event_name(), "aura.usage");
    }
}
