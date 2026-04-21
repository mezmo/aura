//! Orchestration-specific SSE streaming events.
//!
//! These events are emitted during orchestrated multi-agent execution to provide
//! visibility into plan creation, task execution, and synthesis phases.
//!
//! # Event Types
//!
//! - `aura.orchestrator.plan_created` - Plan decomposed from user query
//! - `aura.orchestrator.task_started` - Worker began task execution
//! - `aura.orchestrator.task_completed` - Worker finished task (success/failure)
//! - `aura.orchestrator.iteration_complete` - Plan-execute-synthesize cycle done
//! - `aura.orchestrator.synthesizing` - Combining results into final response
//! - `aura.orchestrator.tool_call_started` - Worker tool execution began
//! - `aura.orchestrator.tool_call_completed` - Worker tool execution finished
//!
//! # Separation from Base Events
//!
//! These events are intentionally separate from `AuraStreamEvent` to:
//! 1. Keep orchestration evolution isolated from base aura streaming
//! 2. Allow different serialization or handling if needed

use crate::orchestration::events::RoutingMode;
use crate::stream_events::{AgentContext, CorrelationContext};
use serde::Serialize;

/// Shared context included in every orchestration SSE event.
///
/// Bundles agent identity and correlation IDs (session, trace) to avoid
/// repeating the same two `#[serde(flatten)]` fields on every variant.
#[derive(Clone, Debug, Serialize)]
pub struct EventContext {
    #[serde(flatten)]
    pub agent: AgentContext,
    #[serde(flatten)]
    pub correlation: CorrelationContext,
}

impl EventContext {
    pub fn new(agent: AgentContext, correlation: CorrelationContext) -> Self {
        Self { agent, correlation }
    }
}

/// Shared identity fields for task events (TaskStarted, TaskCompleted).
#[derive(Clone, Debug, Serialize)]
pub struct TaskContext {
    pub task_id: usize,
    pub orchestrator_id: String,
    pub worker_id: String,
}

/// Outcome fields shared by completion events (TaskCompleted, ToolCallCompleted).
#[derive(Clone, Debug, Serialize)]
pub struct CompletionOutcome {
    pub success: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// Constants for SSE event names. Import in tests for compile-time linkage.
pub mod event_names {
    pub const PLAN_CREATED: &str = "aura.orchestrator.plan_created";
    pub const DIRECT_ANSWER: &str = "aura.orchestrator.direct_answer";
    pub const CLARIFICATION_NEEDED: &str = "aura.orchestrator.clarification_needed";
    pub const TASK_STARTED: &str = "aura.orchestrator.task_started";
    pub const TASK_COMPLETED: &str = "aura.orchestrator.task_completed";
    pub const ITERATION_COMPLETE: &str = "aura.orchestrator.iteration_complete";
    pub const REPLAN_STARTED: &str = "aura.orchestrator.replan_started";
    pub const SYNTHESIZING: &str = "aura.orchestrator.synthesizing";
    pub const WORKER_REASONING: &str = "aura.orchestrator.worker_reasoning";
    pub const TOOL_CALL_STARTED: &str = "aura.orchestrator.tool_call_started";
    pub const TOOL_CALL_COMPLETED: &str = "aura.orchestrator.tool_call_completed";
}

/// SSE events specific to orchestration mode.
///
/// These events provide real-time visibility into multi-agent execution
/// and are emitted alongside standard `AuraStreamEvent`s.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum OrchestrationStreamEvent {
    /// Emitted when orchestrator creates a plan from user query.
    PlanCreated {
        goal: String,
        tasks: Vec<String>,
        routing_mode: RoutingMode,
        routing_rationale: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        planning_response: Option<String>,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator answers directly without orchestration.
    DirectAnswer {
        response: String,
        routing_rationale: String,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator needs clarification from the user.
    ClarificationNeeded {
        question: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        routing_rationale: String,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator starts a task.
    TaskStarted {
        description: String,
        #[serde(flatten)]
        task: TaskContext,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator completes a task.
    TaskCompleted {
        #[serde(flatten)]
        task: TaskContext,
        #[serde(flatten)]
        outcome: CompletionOutcome,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator completes an iteration.
    IterationComplete {
        iteration: usize,
        will_replan: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        gaps: Vec<String>,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator starts a replan cycle.
    ReplanStarted {
        iteration: usize,
        trigger: String,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator starts synthesizing results.
    Synthesizing {
        iteration: usize,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when a worker produces reasoning content.
    WorkerReasoning {
        task_id: usize,
        worker_id: String,
        content: String,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when a tool call starts within a worker task.
    ToolCallStarted {
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<usize>,
        tool_call_id: String,
        tool_name: String,
        worker_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when a tool call completes within a worker task.
    ToolCallCompleted {
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<usize>,
        tool_call_id: String,
        #[serde(flatten)]
        outcome: CompletionOutcome,
        #[serde(flatten)]
        context: EventContext,
    },
}

impl OrchestrationStreamEvent {
    /// Get the SSE event name for this event type.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::PlanCreated { .. } => event_names::PLAN_CREATED,
            Self::DirectAnswer { .. } => event_names::DIRECT_ANSWER,
            Self::ClarificationNeeded { .. } => event_names::CLARIFICATION_NEEDED,
            Self::TaskStarted { .. } => event_names::TASK_STARTED,
            Self::TaskCompleted { .. } => event_names::TASK_COMPLETED,
            Self::IterationComplete { .. } => event_names::ITERATION_COMPLETE,
            Self::ReplanStarted { .. } => event_names::REPLAN_STARTED,
            Self::Synthesizing { .. } => event_names::SYNTHESIZING,
            Self::WorkerReasoning { .. } => event_names::WORKER_REASONING,
            Self::ToolCallStarted { .. } => event_names::TOOL_CALL_STARTED,
            Self::ToolCallCompleted { .. } => event_names::TOOL_CALL_COMPLETED,
        }
    }

    /// Format this event as an SSE message with the event: field.
    pub fn format_sse(&self) -> String {
        crate::stream_events::format_named_sse(self.event_name(), self)
    }

    // ========================================================================
    // Constructors
    // ========================================================================

    /// Create a PlanCreated event.
    pub fn plan_created(
        goal: impl Into<String>,
        tasks: Vec<String>,
        routing_mode: RoutingMode,
        routing_rationale: impl Into<String>,
        planning_response: Option<String>,
        context: EventContext,
    ) -> Self {
        Self::PlanCreated {
            goal: goal.into(),
            tasks,
            routing_mode,
            routing_rationale: routing_rationale.into(),
            planning_response,
            context,
        }
    }

    /// Create a DirectAnswer event.
    pub fn direct_answer(
        response: impl Into<String>,
        routing_rationale: impl Into<String>,
        context: EventContext,
    ) -> Self {
        Self::DirectAnswer {
            response: response.into(),
            routing_rationale: routing_rationale.into(),
            context,
        }
    }

    /// Create a ClarificationNeeded event.
    pub fn clarification_needed(
        question: impl Into<String>,
        options: Option<Vec<String>>,
        routing_rationale: impl Into<String>,
        context: EventContext,
    ) -> Self {
        Self::ClarificationNeeded {
            question: question.into(),
            options,
            routing_rationale: routing_rationale.into(),
            context,
        }
    }

    /// Create a TaskStarted event.
    pub fn task_started(
        task_id: usize,
        description: impl Into<String>,
        orchestrator_id: impl Into<String>,
        worker_id: impl Into<String>,
        context: EventContext,
    ) -> Self {
        Self::TaskStarted {
            description: description.into(),
            task: TaskContext {
                task_id,
                orchestrator_id: orchestrator_id.into(),
                worker_id: worker_id.into(),
            },
            context,
        }
    }

    /// Create a TaskCompleted event.
    pub fn task_completed(
        task_id: usize,
        success: bool,
        duration_ms: u64,
        orchestrator_id: impl Into<String>,
        worker_id: impl Into<String>,
        result: Option<String>,
        context: EventContext,
    ) -> Self {
        Self::TaskCompleted {
            task: TaskContext {
                task_id,
                orchestrator_id: orchestrator_id.into(),
                worker_id: worker_id.into(),
            },
            outcome: CompletionOutcome {
                success,
                duration_ms,
                result,
            },
            context,
        }
    }

    /// Create an IterationComplete event.
    pub fn iteration_complete(
        iteration: usize,
        will_replan: bool,
        reasoning: Option<String>,
        gaps: Vec<String>,
        context: EventContext,
    ) -> Self {
        Self::IterationComplete {
            iteration,
            will_replan,
            reasoning,
            gaps,
            context,
        }
    }

    /// Create a ReplanStarted event.
    pub fn replan_started(
        iteration: usize,
        trigger: impl Into<String>,
        context: EventContext,
    ) -> Self {
        Self::ReplanStarted {
            iteration,
            trigger: trigger.into(),
            context,
        }
    }

    /// Create a Synthesizing event.
    pub fn synthesizing(iteration: usize, context: EventContext) -> Self {
        Self::Synthesizing { iteration, context }
    }

    /// Create a WorkerReasoning event.
    pub fn worker_reasoning(
        task_id: usize,
        worker_id: impl Into<String>,
        content: impl Into<String>,
        context: EventContext,
    ) -> Self {
        Self::WorkerReasoning {
            task_id,
            worker_id: worker_id.into(),
            content: content.into(),
            context,
        }
    }

    /// Create a ToolCallStarted event.
    pub fn tool_call_started(
        task_id: Option<usize>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        worker_id: impl Into<String>,
        arguments: Option<serde_json::Value>,
        context: EventContext,
    ) -> Self {
        Self::ToolCallStarted {
            task_id,
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            worker_id: worker_id.into(),
            arguments,
            context,
        }
    }

    /// Create a ToolCallCompleted event.
    pub fn tool_call_completed(
        task_id: Option<usize>,
        tool_call_id: impl Into<String>,
        success: bool,
        duration_ms: u64,
        result: Option<String>,
        context: EventContext,
    ) -> Self {
        Self::ToolCallCompleted {
            task_id,
            tool_call_id: tool_call_id.into(),
            outcome: CompletionOutcome {
                success,
                duration_ms,
                result,
            },
            context,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> EventContext {
        EventContext::new(
            AgentContext::single_agent(),
            CorrelationContext::new("test-session", None),
        )
    }

    #[test]
    fn test_event_names() {
        let ctx = test_ctx();

        assert_eq!(
            OrchestrationStreamEvent::plan_created(
                "goal",
                Vec::from([
                    "Task 1 description".to_string(),
                    "Task 2 description".to_string(),
                    "Task 3 description".to_string(),
                ]),
                RoutingMode::Orchestrated,
                "test rationale",
                None,
                ctx.clone()
            )
            .event_name(),
            event_names::PLAN_CREATED
        );

        assert_eq!(
            OrchestrationStreamEvent::direct_answer("answer", "simple query", ctx.clone())
                .event_name(),
            event_names::DIRECT_ANSWER
        );

        assert_eq!(
            OrchestrationStreamEvent::clarification_needed(
                "which one?",
                None,
                "ambiguous",
                ctx.clone()
            )
            .event_name(),
            event_names::CLARIFICATION_NEEDED
        );

        assert_eq!(
            OrchestrationStreamEvent::task_started(0, "desc", "orch-id", "worker-id", ctx.clone())
                .event_name(),
            event_names::TASK_STARTED
        );

        assert_eq!(
            OrchestrationStreamEvent::synthesizing(1, ctx).event_name(),
            event_names::SYNTHESIZING
        );
    }

    #[test]
    fn test_format_sse() {
        let event = OrchestrationStreamEvent::plan_created(
            "test goal",
            Vec::from([
                "Task 1 description".to_string(),
                "Task 2 description".to_string(),
            ]),
            RoutingMode::Orchestrated,
            "test rationale",
            Some("coordinator response text".to_string()),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::PLAN_CREATED)));
        assert!(sse.contains("\"goal\":\"test goal\""));
        assert!(sse.contains("\"tasks\":[\"Task 1 description\",\"Task 2 description\"]"));
        assert!(sse.contains("\"routing_mode\":\"orchestrated\""));
        assert!(sse.contains("\"routing_rationale\":\"test rationale\""));
        assert!(sse.contains("\"planning_response\":\"coordinator response text\""));
    }

    #[test]
    fn test_format_sse_plan_created_routed() {
        let event = OrchestrationStreamEvent::plan_created(
            "simple math",
            Vec::from(["Calculate the mean of [10, 20, 30]".to_string()]),
            RoutingMode::Routed,
            "single worker",
            None,
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.contains("\"routing_mode\":\"routed\""));
        assert!(!sse.contains("planning_response"));
    }

    #[test]
    fn test_format_sse_plan_created_without_response() {
        let event = OrchestrationStreamEvent::plan_created(
            "goal",
            Vec::from(["Task 1".to_string()]),
            RoutingMode::Routed,
            "rationale",
            None,
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(!sse.contains("planning_response"));
    }

    #[test]
    fn test_format_sse_iteration_complete() {
        let event = OrchestrationStreamEvent::iteration_complete(
            1,
            false,
            Some("Single-task plan completed successfully".to_string()),
            vec![],
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.contains("\"will_replan\":false"));
        assert!(sse.contains("\"iteration\":1"));
    }

    #[test]
    fn test_format_sse_task_completed_with_result() {
        let event = OrchestrationStreamEvent::task_completed(
            0,
            true,
            1500,
            "orch-1",
            "worker-1",
            Some("The mean is 30.0".to_string()),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::TASK_COMPLETED)));
        assert!(sse.contains("\"result\":\"The mean is 30.0\""));
        assert!(sse.contains("\"success\":true"));
    }

    #[test]
    fn test_format_sse_tool_call_started_with_arguments() {
        let args = serde_json::json!({"numbers": [10, 20, 30]});
        let event = OrchestrationStreamEvent::tool_call_started(
            Some(0),
            "call_1",
            "mean",
            "statistics",
            Some(args),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::TOOL_CALL_STARTED)));
        assert!(sse.contains("\"arguments\":{\"numbers\":[10,20,30]}"));
    }

    #[test]
    fn test_format_sse_tool_call_completed_with_result() {
        let event = OrchestrationStreamEvent::tool_call_completed(
            Some(0),
            "call_1",
            true,
            42,
            Some("30.0".to_string()),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::TOOL_CALL_COMPLETED)));
        assert!(sse.contains("\"result\":\"30.0\""));
        assert!(sse.contains("\"success\":true"));
    }
}
