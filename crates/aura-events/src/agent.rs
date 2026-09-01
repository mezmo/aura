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
    AgentContext, ApprovalCompleted, ApprovalPending, ApprovalRequested, McpServerStatus,
    ProgressToken, WorkerPhase,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEventPayload {
    SessionInfo {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_context_limit: Option<u64>,
    },

    McpStatus {
        servers: Vec<McpServerStatus>,
    },

    TextDelta {
        content: String,
    },

    Reasoning {
        content: String,
    },

    /// The model's decision to call a tool, ahead of any execution.
    ToolRequested {
        tool_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },

    ToolStart {
        tool_id: String,
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress_token: Option<ProgressToken>,
    },

    ToolComplete {
        tool_id: String,
        tool_name: String,
        duration_ms: u64,
        #[serde(flatten)]
        outcome: ToolOutcome,
    },

    ToolProgress {
        progress_token: ProgressToken,
        progress: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    WorkerPhase {
        phase: WorkerPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },

    /// Provider-billed tokens for one turn.
    ToolUsage {
        tool_ids: Vec<String>,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    },

    /// Provider-billed tokens, cumulative across turns.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    },

    /// Context-window occupancy, not billing.
    ContextUsage {
        context_tokens: u64,
        response_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
    },

    ScratchpadUsage {
        tokens_intercepted: usize,
        tokens_extracted: usize,
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
        worker_id: String,
    },

    TaskCompleted {
        task_id: usize,
        success: bool,
        duration_ms: u64,
        orchestrator_id: String,
        worker_id: String,
        result: String,
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

    WorkerReasoning {
        task_id: usize,
        worker_id: String,
        content: String,
    },

    ToolCallStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<usize>,
        tool_call_id: String,
        tool_name: String,
        worker_id: String,
        arguments: serde_json::Value,
    },

    ToolCallCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<usize>,
        tool_call_id: String,
        success: bool,
        duration_ms: u64,
        result: String,
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
            tool_id: "call_1".to_string(),
            tool_name: "list_files".to_string(),
            progress_token: None,
        });
        assert!(matches!(start, AgentEventPayload::ToolStart { .. }));

        let usage = roundtrip(AgentEventPayload::Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        });
        assert!(matches!(usage, AgentEventPayload::Usage { .. }));
    }

    #[test]
    fn tool_outcome_flattens_onto_tool_complete() {
        let json = serde_json::to_value(AgentEventPayload::ToolComplete {
            tool_id: "call_1".to_string(),
            tool_name: "list_files".to_string(),
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

    #[test]
    fn arguments_survive_a_roundtrip() {
        let payload = roundtrip(AgentEventPayload::ToolRequested {
            tool_id: "call_1".to_string(),
            tool_name: "list_files".to_string(),
            arguments: json!({ "path": "/mock", "depth": 2 }),
        });

        let AgentEventPayload::ToolRequested { arguments, .. } = payload else {
            panic!("expected ToolRequested");
        };
        assert_eq!(arguments, json!({ "path": "/mock", "depth": 2 }));
    }
}
