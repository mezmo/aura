//! Aura-specific SSE streaming events for enhanced client UX.
//!
//! Type definitions live in the [`aura_events`] crate (lightweight, no agent deps).
//! This module re-exports them and adds the `from_current_span` constructor
//! which depends on `tracing` (only available in the full aura crate).

// Re-export everything from aura-events so existing consumers don't break
pub use aura_events::{
    AgentContext, AuraStreamEvent, CorrelationContext, NumberOrString, ProgressToken, WorkerPhase,
    format_named_sse,
};

// Re-export orchestration event types
pub use aura_events::orchestration;

/// Extension trait for `CorrelationContext` that adds tracing integration.
///
/// This is defined as a trait (not inherent impl) because `CorrelationContext`
/// lives in the `aura-events` crate. Only available in the full `aura` crate.
pub trait CorrelationContextExt {
    /// Create correlation context from the current tracing span.
    ///
    /// Captures the trace ID from `tracing::Span::current()` for OTEL correlation.
    fn from_current_span(session_id: impl Into<String>) -> CorrelationContext;
}

impl CorrelationContextExt for CorrelationContext {
    fn from_current_span(session_id: impl Into<String>) -> CorrelationContext {
        let trace_id = tracing::Span::current()
            .id()
            .map(|id| format!("{:x}", id.into_u64()));
        CorrelationContext {
            session_id: session_id.into(),
            trace_id,
        }
    }
}

/// Constants for SSE event names on the base `aura.*` namespace.
///
/// Import these in tests or downstream consumers instead of hard-coding the
/// string literals. Orchestration-specific events live in
/// `orchestration::stream_events::event_names` under `aura.orchestrator.*`.
pub mod event_names {
    pub const SESSION_INFO: &str = "aura.session_info";
    pub const TOOL_REQUESTED: &str = "aura.tool_requested";
    pub const TOOL_START: &str = "aura.tool_start";
    pub const TOOL_COMPLETE: &str = "aura.tool_complete";
    pub const REASONING: &str = "aura.reasoning";
    pub const PROGRESS: &str = "aura.progress";
    pub const WORKER_PHASE: &str = "aura.worker_phase";
    pub const TOOL_USAGE: &str = "aura.tool_usage";
    pub const USAGE: &str = "aura.usage";
    pub const SCRATCHPAD_USAGE: &str = "aura.scratchpad_usage";
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
        assert_eq!(event.event_name(), event_names::TOOL_REQUESTED);
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
        assert_eq!(event.event_name(), event_names::TOOL_START);
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
        assert_eq!(event.event_name(), event_names::TOOL_COMPLETE);
    }

    #[test]
    fn test_reasoning_event_name() {
        let event = AuraStreamEvent::reasoning(
            "Let me analyze...",
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        assert_eq!(event.event_name(), event_names::REASONING);
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::TOOL_REQUESTED)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::TOOL_START)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::TOOL_COMPLETE)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::PROGRESS)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::PROGRESS)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::WORKER_PHASE)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::SESSION_INFO)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::SESSION_INFO)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::TOOL_USAGE)));
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
        assert!(sse.starts_with(&format!("event: {}\n", event_names::USAGE)));
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
        assert_eq!(session_info.event_name(), event_names::SESSION_INFO);

        let tool_usage =
            AuraStreamEvent::tool_usage(vec![], 0, 0, 0, CorrelationContext::default());
        assert_eq!(tool_usage.event_name(), event_names::TOOL_USAGE);

        let usage = AuraStreamEvent::usage(0, 0, 0, CorrelationContext::default());
        assert_eq!(usage.event_name(), event_names::USAGE);
    }

    #[test]
    fn test_scratchpad_usage_event_name() {
        let event = AuraStreamEvent::scratchpad_usage(
            15_840,
            1_200,
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        assert_eq!(event.event_name(), "aura.scratchpad_usage");
    }

    #[test]
    fn test_scratchpad_usage_format_sse() {
        let event = AuraStreamEvent::scratchpad_usage(
            15_840,
            1_200,
            AgentContext::worker("data-explorer", None, "orchestrator"),
            CorrelationContext::new("sess_xyz", Some("trace_123".to_string())),
        );
        let sse = event.format_sse();
        assert!(sse.starts_with(&format!("event: {}\n", event_names::SCRATCHPAD_USAGE)));
        assert!(sse.contains("\"tokens_intercepted\":15840"));
        assert!(sse.contains("\"tokens_extracted\":1200"));
        assert!(sse.contains("\"agent_id\":\"data-explorer\""));
        assert!(sse.contains("\"parent_agent_id\":\"orchestrator\""));
        assert!(sse.contains("\"session_id\":\"sess_xyz\""));
        assert!(sse.contains("\"trace_id\":\"trace_123\""));
        // Event is agent-scoped, not task-scoped.
        assert!(!sse.contains("task_id"));
        assert!(sse.ends_with("\n\n"));
    }

    #[test]
    fn test_scratchpad_usage_single_agent_id_is_main() {
        let event = AuraStreamEvent::scratchpad_usage(
            0,
            0,
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        let sse = event.format_sse();
        assert!(sse.contains("\"agent_id\":\"main\""));
        assert!(!sse.contains("parent_agent_id"));
    }

    #[test]
    fn test_scratchpad_usage_zero_values_still_serialize() {
        let event = AuraStreamEvent::scratchpad_usage(
            0,
            0,
            AgentContext::single_agent(),
            CorrelationContext::default(),
        );
        let sse = event.format_sse();
        assert!(sse.contains("\"tokens_intercepted\":0"));
        assert!(sse.contains("\"tokens_extracted\":0"));
    }
}
