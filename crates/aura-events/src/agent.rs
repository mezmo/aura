//! The agent-produced event vocabulary.
//!
//! [`AgentEvent`] is what a running agent emits. Observers — an SSE producer, an
//! A2A status bridge, an OTel exporter — consume this stream and project it into
//! whatever shape they serve. Contrast [`crate::AuraStreamEvent`], which is the
//! HTTP *wire* form of one such projection.
//!
//! Two properties distinguish this schema from the wire schema:
//!
//! - **Internally tagged.** [`crate::AuraStreamEvent`] is `#[serde(untagged)]`,
//!   so its variant order is load-bearing during deserialization. This enum
//!   carries a `type` discriminator instead, making variant order irrelevant.
//! - **No correlation context.** The wire events flatten a
//!   [`CorrelationContext`](crate::CorrelationContext) (session id, trace id)
//!   into every payload. That is ambient request state, not something an agent
//!   knows, so it is applied by the observer rather than carried here.

use serde::{Deserialize, Serialize};

use crate::orchestration::{IterationTimings, RoutingMode};
use crate::{
    AgentContext, ApprovalCompleted, ApprovalPending, ApprovalRequested, McpServerStatus, Progress,
    ProgressToken, TokenCount, TokenUsage, ToolCallId, ToolName, WorkerPhase,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentEvent {
    pub agent: AgentContext,
    pub payload: AgentEventPayload,
}

impl AgentEvent {
    pub fn new(agent: AgentContext, payload: AgentEventPayload) -> Self {
        Self { agent, payload }
    }

    /// Attributes the payload to `agent_id: "main"`.
    pub fn single_agent(payload: AgentEventPayload) -> Self {
        Self::new(AgentContext::single_agent(), payload)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ToolOutcome {
    Success { result: String },
    Failure { error: String },
}

/// What an agent has to say about its own execution.
///
/// `#[non_exhaustive]` because the vocabulary grows as producers move onto this
/// schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEventPayload {
    SessionInfo {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_context_limit: Option<TokenCount>,
    },

    McpStatus {
        servers: Vec<McpServerStatus>,
    },

    TextDelta {
        content: String,
    },

    Reasoning {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<usize>,
    },

    /// The model's decision to call a tool, ahead of any execution.
    ToolRequested {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        arguments: serde_json::Value,
    },

    ToolStart {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress_token: Option<ProgressToken>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<usize>,
    },

    ToolComplete {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        duration_ms: u64,
        #[serde(flatten)]
        outcome: ToolOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<usize>,
    },

    ToolProgress {
        progress_token: ProgressToken,
        progress: Progress,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    WorkerPhase {
        phase: WorkerPhase,
        /// Plan-relative: unique within its iteration, not across a run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<usize>,
    },

    /// One turn's tokens, attributed to the tool calls that turn covered.
    ToolUsage {
        tool_call_ids: Vec<ToolCallId>,
        #[serde(flatten)]
        usage: TokenUsage,
    },

    /// Cumulative across every turn.
    Usage {
        #[serde(flatten)]
        usage: TokenUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_input_tokens: Option<TokenCount>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_creation_input_tokens: Option<TokenCount>,
    },

    /// Context-window occupancy, not billing.
    ContextUsage {
        context_tokens: TokenCount,
        response_tokens: TokenCount,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<TokenCount>,
    },

    ScratchpadUsage {
        tokens_intercepted: TokenCount,
        tokens_extracted: TokenCount,
    },

    ApprovalRequested(ApprovalRequested),

    ApprovalPending(ApprovalPending),

    ApprovalCompleted(ApprovalCompleted),

    PlanCreated {
        goal: String,
        tasks: Vec<String>,
        routing_mode: RoutingMode,
        routing_rationale: String,
        planning_response: String,
    },

    DirectAnswer {
        response: String,
        routing_rationale: String,
    },

    ClarificationNeeded {
        question: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        routing_rationale: String,
    },

    TaskStarted {
        task_id: usize,
        description: String,
        orchestrator_id: String,
    },

    TaskCompleted {
        task_id: usize,
        duration_ms: u64,
        orchestrator_id: String,
        #[serde(flatten)]
        outcome: ToolOutcome,
    },

    /// A worker's gated call is waiting on an approval the run will not block
    /// for; one per parked call.
    TaskBlocked {
        task_id: usize,
        tool_call_id: ToolCallId,
        decision_id: String,
        tool_name: ToolName,
        orchestrator_id: String,
    },

    /// The run stopped to await its parked approvals. Terminal.
    RunParked {
        run_id: String,
        decision_ids: Vec<String>,
        expires_at: String,
        iteration: usize,
    },

    IterationComplete {
        iteration: usize,
        will_replan: bool,
        reasoning: String,
        gaps: Vec<String>,
        timings: IterationTimings,
    },

    ReplanStarted {
        iteration: usize,
        /// `"coordinator"` or `"failure"`.
        trigger: String,
    },

    Synthesizing {
        iteration: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(payload: AgentEventPayload) -> AgentEventPayload {
        let json = serde_json::to_string(&payload).expect("payload should serialize");
        serde_json::from_str(&json).expect("payload should deserialize")
    }

    #[test]
    fn the_tag_names_the_variant() {
        let json = serde_json::to_value(AgentEventPayload::TextDelta {
            content: "hi".to_string(),
        })
        .expect("should serialize");

        assert_eq!(json["type"], "text_delta");
        assert_eq!(json["content"], "hi");
    }

    /// The wire enum needs `ToolComplete` declared before `ToolStart` and
    /// `ToolUsage` before `Usage` to deserialize correctly. The tag makes the
    /// same shapes unambiguous here regardless of declaration order.
    #[test]
    fn variants_the_wire_enum_must_order_are_unambiguous_here() {
        let start = roundtrip(AgentEventPayload::ToolStart {
            tool_call_id: ToolCallId::new("call_1"),
            tool_name: ToolName::new("list_files"),
            progress_token: None,
            arguments: None,
            task_id: None,
        });
        assert!(matches!(start, AgentEventPayload::ToolStart { .. }));

        let usage = roundtrip(AgentEventPayload::Usage {
            usage: TokenUsage {
                prompt_tokens: TokenCount::new(1),
                completion_tokens: TokenCount::new(2),
                total_tokens: TokenCount::new(3),
            },
            cache_read_input_tokens: Some(TokenCount::new(80)),
            cache_creation_input_tokens: Some(TokenCount::new(4)),
        });
        assert!(matches!(
            usage,
            AgentEventPayload::Usage {
                cache_read_input_tokens: Some(_),
                cache_creation_input_tokens: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn tool_outcome_flattens_onto_tool_complete() {
        let json = serde_json::to_value(AgentEventPayload::ToolComplete {
            task_id: None,
            tool_call_id: ToolCallId::new("call_1"),
            tool_name: ToolName::new("list_files"),
            duration_ms: 12,
            outcome: ToolOutcome::Failure {
                error: "boom".to_string(),
            },
        })
        .expect("should serialize");

        assert_eq!(json["type"], "tool_complete");
        assert_eq!(json["outcome"], "failure");
        assert_eq!(json["error"], "boom");
    }

    /// `ToolStart` is emitted by a lone agent and by an orchestration worker,
    /// so nothing about the payload says which frames it becomes — only the
    /// envelope does.
    #[test]
    fn a_shared_variant_is_told_apart_by_its_agent_not_its_payload() {
        let payload = || AgentEventPayload::ToolStart {
            tool_call_id: ToolCallId::new("call_1"),
            tool_name: ToolName::new("list_files"),
            progress_token: None,
            arguments: None,
            task_id: None,
        };

        let alone = AgentEvent::single_agent(payload());
        let worker = AgentEvent::new(
            AgentContext::worker("log_worker", None, "coordinator"),
            payload(),
        );

        assert!(alone.agent.is_single_agent());
        assert!(!worker.agent.is_single_agent());
    }

    /// An orchestration run names its coordinator-owned tool calls `"main"`,
    /// the same id a lone agent uses, so only the parent tells them apart.
    #[test]
    fn a_coordinator_owned_call_named_main_is_not_the_lone_agent() {
        let coordinator_owned = AgentContext::worker("main", None, "coordinator");

        assert!(!coordinator_owned.is_single_agent());
        assert!(AgentContext::single_agent().is_single_agent());
    }

    #[test]
    fn an_event_carries_its_emitting_agent() {
        let event = AgentEvent::new(
            AgentContext::worker("log_worker", None, "orchestrator"),
            AgentEventPayload::TextDelta {
                content: "scanning".to_string(),
            },
        );
        let json = serde_json::to_value(&event).expect("should serialize");

        assert_eq!(json["agent"]["agent_id"], "log_worker");
        assert_eq!(json["agent"]["parent_agent_id"], "orchestrator");
        assert_eq!(json["payload"]["type"], "text_delta");
    }

    /// `ToolUsage` is the one variant that mixes a flattened struct with a
    /// field of its own, so it exercises `flatten` under the `type` tag.
    #[test]
    fn a_flattened_usage_survives_a_roundtrip_beside_its_tool_ids() {
        let payload = roundtrip(AgentEventPayload::ToolUsage {
            tool_call_ids: vec![ToolCallId::new("call_1")],
            usage: TokenUsage {
                prompt_tokens: TokenCount::new(10),
                completion_tokens: TokenCount::new(5),
                total_tokens: TokenCount::new(15),
            },
        });

        let AgentEventPayload::ToolUsage {
            tool_call_ids,
            usage,
        } = payload
        else {
            panic!("expected ToolUsage");
        };
        assert_eq!(tool_call_ids, vec![ToolCallId::new("call_1")]);
        assert_eq!(usage.total_tokens.get(), 15);
    }

    #[test]
    fn arguments_survive_a_roundtrip() {
        let payload = roundtrip(AgentEventPayload::ToolRequested {
            tool_call_id: ToolCallId::new("call_1"),
            tool_name: ToolName::new("list_files"),
            arguments: json!({ "path": "/mock", "depth": 2 }),
        });

        let AgentEventPayload::ToolRequested { arguments, .. } = payload else {
            panic!("expected ToolRequested");
        };
        assert_eq!(arguments, json!({ "path": "/mock", "depth": 2 }));
    }
}
