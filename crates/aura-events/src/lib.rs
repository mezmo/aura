//! Shared SSE event types for the Aura ecosystem.
//!
//! This crate defines the event types emitted by the aura web server and
//! consumed by the aura CLI. It is intentionally lightweight — no agent,
//! MCP, or provider dependencies — so both producer and consumer crates
//! can depend on it without pulling in the full aura stack.
//!
//! # Event Types
//!
//! - [`AuraStreamEvent`] — Base aura events (tool lifecycle, usage, reasoning, progress)
//! - [`OrchestrationStreamEvent`] — Multi-agent orchestration events
//!
//! Both enums derive `Serialize + Deserialize` so they can be used for
//! producing SSE (server) and parsing SSE (client) with the same types.

pub mod orchestration;

use serde::{Deserialize, Serialize};

// When the `rmcp-types` feature is enabled, use rmcp's ProgressToken directly
// for zero-cost interop with the aura crate. Otherwise, define a compatible
// local type so aura-cli doesn't need the heavy rmcp dependency.
#[cfg(feature = "rmcp-types")]
pub use rmcp::model::{NumberOrString, ProgressToken};

#[cfg(not(feature = "rmcp-types"))]
pub use self::progress_token::{NumberOrString, ProgressToken};

#[cfg(not(feature = "rmcp-types"))]
mod progress_token {
    use serde::{Deserialize, Serialize};

    /// A flexible identifier type that can be either a number or a string.
    /// Wire-compatible with `rmcp::model::NumberOrString`.
    #[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum NumberOrString {
        Number(i64),
        String(String),
    }

    /// MCP progress token. Compatible with `rmcp::model::ProgressToken`.
    #[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ProgressToken(pub NumberOrString);
}

/// Format any serializable type as an SSE event with a named event field.
///
/// Returns a string in the format:
/// ```text
/// event: {event_name}
/// data: {json}
///
/// ```
pub fn format_named_sse(event_name: &str, data: &impl Serialize) -> String {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    format!("event: {event_name}\ndata: {json}\n\n")
}

/// Context identifying which agent emitted an event.
///
/// For single-agent deployments, use `AgentContext::single_agent()` which sets
/// `agent_id: "main"`. For multi-agent orchestrators, populate `parent_agent_id`
/// to establish the agent hierarchy.
///
/// Note: `AgentContext::default()` gives an empty `agent_id` - use `single_agent()`
/// for the standard single-agent context.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
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
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CorrelationContext {
    /// Chat session ID (from request handler or X-Chat-Session-Id header)
    pub session_id: String,
    /// OTEL trace ID (from tracing::Span) for Phoenix correlation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl CorrelationContext {
    /// Create correlation context with explicit values (for testing or custom setups)
    pub fn new(session_id: impl Into<String>, trace_id: Option<String>) -> Self {
        Self {
            session_id: session_id.into(),
            trace_id,
        }
    }
}

/// Worker phase for multi-agent orchestration.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
///
/// Derives both `Serialize` (for producing SSE on the server) and
/// `Deserialize` (for parsing SSE on the client).
///
/// The `#[serde(untagged)]` attribute means the event type is determined
/// by the SSE `event:` field, not by a JSON discriminator. When deserializing,
/// serde tries each variant in declaration order — variant ordering matters.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AuraStreamEvent {
    /// Emitted at stream start with model and context limit information.
    /// UI can use this for context window percentage calculation.
    SessionInfo {
        /// The model name (e.g., "gpt-4o", "claude-3-opus")
        model: String,
        /// Context limit for this model (in tokens), if known
        #[serde(skip_serializing_if = "Option::is_none")]
        model_context_limit: Option<u64>,
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
    /// Emitted when a tool call completes (success or failure).
    ///
    /// Note: Must be ordered BEFORE `ToolStart` in this enum because
    /// `#[serde(untagged)]` tries variants in declaration order. `ToolStart`
    /// has fewer required fields and would match `ToolComplete` JSON prematurely.
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
    /// Emitted for multi-agent worker phase transitions.
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
    ///
    /// Note: Must be ordered BEFORE `Usage` in the enum because `#[serde(untagged)]`
    /// tries variants in order — `Usage` would match prematurely since it has a
    /// subset of `ToolUsage`'s fields. `tool_ids` acts as the discriminator.
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
    /// Emitted after an agent (single-agent or orchestration worker) completes
    /// with scratchpad activity. Reports how much raw tool output was diverted
    /// to disk vs extracted back into context.
    ScratchpadUsage {
        /// Tokens of raw tool output diverted to scratchpad by this agent.
        tokens_intercepted: usize,
        /// Tokens extracted from scratchpad back into context by this agent.
        tokens_extracted: usize,
        #[serde(flatten)]
        agent: AgentContext,
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
            Self::ScratchpadUsage { .. } => "aura.scratchpad_usage",
        }
    }

    /// Format this event as an SSE message with the event: field.
    pub fn format_sse(&self) -> String {
        format_named_sse(self.event_name(), self)
    }

    /// Create a ToolRequested event.
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

    /// Create a ToolStart event.
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
    pub fn session_info(
        model: impl Into<String>,
        model_context_limit: Option<u64>,
        correlation: CorrelationContext,
    ) -> Self {
        Self::SessionInfo {
            model: model.into(),
            model_context_limit,
            correlation,
        }
    }

    /// Create a ToolUsage event (emitted when usage becomes available).
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

    /// Create a ScratchpadUsage event.
    pub fn scratchpad_usage(
        tokens_intercepted: usize,
        tokens_extracted: usize,
        agent: AgentContext,
        correlation: CorrelationContext,
    ) -> Self {
        Self::ScratchpadUsage {
            tokens_intercepted,
            tokens_extracted,
            agent,
            correlation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_complete_roundtrip() {
        let event = AuraStreamEvent::tool_complete_success(
            "call_1",
            "Shell",
            1500,
            "output",
            AgentContext::single_agent(),
            CorrelationContext::new("s1", None),
        );
        let json = serde_json::to_string(&event).unwrap();
        eprintln!("JSON: {}", json);
        let parsed: AuraStreamEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            AuraStreamEvent::ToolComplete {
                tool_name,
                duration_ms,
                ..
            } => {
                assert_eq!(tool_name, "Shell");
                assert_eq!(duration_ms, 1500);
            }
            other => panic!("expected ToolComplete, got {:?}", other),
        }
    }

    #[test]
    fn usage_roundtrip() {
        let event = AuraStreamEvent::usage(100, 50, 150, CorrelationContext::new("s1", None));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuraStreamEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            AuraStreamEvent::Usage {
                prompt_tokens,
                completion_tokens,
                ..
            } => {
                assert_eq!(prompt_tokens, 100);
                assert_eq!(completion_tokens, 50);
            }
            other => panic!("expected Usage, got {:?}", other),
        }
    }

    #[test]
    fn tool_requested_roundtrip() {
        let event = AuraStreamEvent::tool_requested(
            "call_1",
            "Shell",
            serde_json::json!({"cmd": "ls"}),
            AgentContext::single_agent(),
            CorrelationContext::new("s1", None),
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuraStreamEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            AuraStreamEvent::ToolRequested { tool_name, .. } => assert_eq!(tool_name, "Shell"),
            other => panic!("expected ToolRequested, got {:?}", other),
        }
    }
}
