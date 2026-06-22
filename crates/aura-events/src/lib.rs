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

pub mod event_names;
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

/// Marker the `aura` crate prepends to an HTTP status when an MCP transport
/// fails (e.g. `"server returned HTTP 404 Not Found"`), surfaced in
/// [`McpServerStatus::reason`].
///
/// Defined here, in the shared wire crate, so the producer (`aura`'s transport
/// layer) and consumers (the CLI, which condenses the reason for display) match
/// on a single source of truth rather than duplicated literals. Note the
/// trailing space — callers format `"{HTTP_STATUS_MARKER}{status}"`.
pub const HTTP_STATUS_MARKER: &str = "server returned HTTP ";

/// Connection status of a single configured MCP server.
///
/// Wire projection of the aura crate's `ConnectionStatus`/`ServerInfo`. Kept as
/// plain primitives so this lightweight crate stays free of agent/MCP deps.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerStatus {
    /// Configured server name (e.g. "pagerduty").
    pub server_name: String,
    /// Transport kind: "http_streamable", "sse", or "stdio".
    pub transport: String,
    /// Connection outcome: "connected", "failed", or "not_attempted".
    pub status: String,
    /// Number of tools discovered (0 for failed or genuinely empty servers).
    pub tools_count: usize,
    /// Failure reason when `status == "failed"`; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    /// Emitted when a HITL approval request is raised.
    ApprovalRequested(ApprovalRequested),
    /// Emitted when a HITL approval is awaiting an attended decision.
    ApprovalPending(ApprovalPending),
    /// Emitted when a HITL approval reaches a terminal outcome.
    ApprovalCompleted(ApprovalCompleted),
    /// Emitted at stream start with the connection status of every configured
    /// MCP server. Lets clients distinguish between "server connected", "server has no tools"
    /// "server is configured but unavailable", (auth failure, connection refused), etc...
    ///
    /// Placed last in the enum: its unique required `servers` field keeps
    /// `#[serde(untagged)]` deserialization unambiguous regardless of order.
    McpStatus {
        servers: Vec<McpServerStatus>,
        #[serde(flatten)]
        correlation: CorrelationContext,
    },
}

impl AuraStreamEvent {
    /// Get the SSE event name for this event type.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::SessionInfo { .. } => event_names::SESSION_INFO,
            Self::ToolRequested { .. } => event_names::TOOL_REQUESTED,
            Self::ToolStart { .. } => event_names::TOOL_START,
            Self::ToolComplete { .. } => event_names::TOOL_COMPLETE,
            Self::Reasoning { .. } => event_names::REASONING,
            Self::Progress { .. } => event_names::PROGRESS,
            Self::WorkerPhase { .. } => event_names::WORKER_PHASE,
            Self::ToolUsage { .. } => event_names::TOOL_USAGE,
            Self::Usage { .. } => event_names::USAGE,
            Self::ScratchpadUsage { .. } => event_names::SCRATCHPAD_USAGE,
            Self::ApprovalRequested(_) => event_names::APPROVAL_REQUESTED,
            Self::ApprovalPending(_) => event_names::APPROVAL_PENDING,
            Self::ApprovalCompleted(_) => event_names::APPROVAL_COMPLETED,
            Self::McpStatus { .. } => event_names::MCP_STATUS,
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

    /// Create an McpStatus event (emitted at stream start).
    pub fn mcp_status(servers: Vec<McpServerStatus>, correlation: CorrelationContext) -> Self {
        Self::McpStatus {
            servers,
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
    fn mcp_status_roundtrip_and_event_name() {
        let event = AuraStreamEvent::mcp_status(
            vec![
                McpServerStatus {
                    server_name: "pagerduty".to_string(),
                    transport: "http_streamable".to_string(),
                    status: "failed".to_string(),
                    tools_count: 0,
                    reason: Some("authentication failed (401 Unauthorized)".to_string()),
                },
                McpServerStatus {
                    server_name: "mezmo".to_string(),
                    transport: "http_streamable".to_string(),
                    status: "connected".to_string(),
                    tools_count: 7,
                    reason: None,
                },
            ],
            CorrelationContext::new("s1", None),
        );
        assert_eq!(event.event_name(), "aura.mcp_status");

        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.mcp_status\n"));
        // A connected server omits the reason field entirely.
        assert!(sse.contains("\"status\":\"connected\""));
        assert!(sse.contains("\"reason\":\"authentication failed (401 Unauthorized)\""));

        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuraStreamEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            AuraStreamEvent::McpStatus { servers, .. } => {
                assert_eq!(servers.len(), 2);
                assert_eq!(servers[0].server_name, "pagerduty");
                assert_eq!(servers[0].status, "failed");
                assert_eq!(servers[1].tools_count, 7);
                assert_eq!(servers[1].reason, None);
            }
            other => panic!("expected McpStatus, got {:?}", other),
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

    #[test]
    fn approval_completed_error_roundtrip_and_event_name() {
        let event = AuraStreamEvent::ApprovalCompleted(ApprovalCompleted {
            decision_id: "019edead-beef-7000-8000-000000000001".to_string(),
            outcome: ApprovalOutcomeWire::Errored {
                message: "approval webhook returned status 500".to_string(),
            },
            duration_ms: 42,
            scope: AgentScopeWire::Worker {
                run_id: "019edead-beef-7000-8000-000000000002".to_string(),
                task_id: 3,
                worker: Some("ops".to_string()),
                session_id: None,
            },
        });

        assert_eq!(event.event_name(), "aura.approval_completed");
        let sse = event.format_sse();
        assert!(sse.starts_with("event: aura.approval_completed\n"));
        assert!(sse.contains("\"kind\":\"errored\""));

        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuraStreamEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            AuraStreamEvent::ApprovalCompleted(completed) => {
                assert_eq!(
                    completed.decision_id,
                    "019edead-beef-7000-8000-000000000001"
                );
                assert_eq!(completed.duration_ms, 42);
                assert!(matches!(
                    completed.outcome,
                    ApprovalOutcomeWire::Errored { .. }
                ));
            }
            other => panic!("expected ApprovalCompleted, got {:?}", other),
        }
    }
}

// ---------------------------------------------------------------------------
// HITL approval SSE DTOs
// ---------------------------------------------------------------------------
//
// Serde-only wire mirrors for the HITL approval lifecycle. No behavior; the
// `aura` crate's `hitl::events` module is the single boundary that converts the
// domain types into these.

/// Why this approval was raised.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalOriginWire {
    ConfigGate { matched_pattern: String },
    AgentRequested { reason: String },
}

/// Which agent surface is asking for approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentScopeWire {
    Single {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Worker {
        run_id: String,
        task_id: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        worker: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Coordinator {
        run_id: String,
    },
}

/// The terminal outcome of an approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalOutcomeWire {
    Approved,
    Denied {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    TimedOut {
        waited_ms: u64,
    },
    Cancelled {
        reason: CancelReasonWire,
    },
    Errored {
        message: String,
    },
}

/// Why a pending approval was cancelled rather than decided or timed out.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReasonWire {
    ClientDisconnected,
    Shutdown,
    SenderDropped,
}

/// `approval_requested`: an approval was raised (emitted on both routes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequested {
    pub decision_id: String,
    pub tool_name: String,
    pub origin: ApprovalOriginWire,
    pub scope: AgentScopeWire,
}

/// `approval_pending`: the attended prompt; conversational route only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPending {
    pub decision_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub origin: ApprovalOriginWire,
    pub scope: AgentScopeWire,
    /// RFC3339 instant after which the pending approval expires.
    pub expires_at: String,
}

/// `approval_completed`: terminal outcome (both routes; the outcome enum
/// includes timeout / cancelled, not just approve/deny).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalCompleted {
    pub decision_id: String,
    pub outcome: ApprovalOutcomeWire,
    pub duration_ms: u64,
    pub scope: AgentScopeWire,
}
