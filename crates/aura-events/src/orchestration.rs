//! Orchestration-specific SSE streaming events.
//!
//! These events are emitted during orchestrated multi-agent execution to provide
//! visibility into plan creation, task execution, and synthesis phases.

use crate::{format_named_sse, AgentContext, CorrelationContext};
use serde::{Deserialize, Serialize};

/// Shared context included in every orchestration SSE event.
///
/// Bundles agent identity and correlation IDs (session, trace) to avoid
/// repeating the same two `#[serde(flatten)]` fields on every variant.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskContext {
    pub task_id: usize,
    pub orchestrator_id: String,
    pub worker_id: String,
}

/// Outcome fields shared by completion events (TaskCompleted, ToolCallCompleted).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletionOutcome {
    pub success: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// One task's edges in the plan DAG, emitted on `plan_created`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskDagNode {
    pub id: usize,
    pub dependencies: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

/// How the coordinator routed a query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    /// Coordinator answered directly without task execution.
    DirectAnswer,
    /// Coordinator created a plan and executed it via the orchestrator.
    ///
    /// Both single-task and multi-task plans use this variant. There is no
    /// separate single-worker routing path in the coordinator's tool set.
    Orchestrated,
}

/// The continuation decision after a phase completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PhaseContinuation {
    /// Proceed to the next phase as planned.
    #[default]
    Continue,
    /// Discard remaining phases and replan based on results so far.
    Replan,
}

impl std::fmt::Display for PhaseContinuation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Continue => write!(f, "continue"),
            Self::Replan => write!(f, "replan"),
        }
    }
}

/// Constants for SSE event names.
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
    pub const PHASE_STARTED: &str = "aura.orchestrator.phase_started";
    pub const PHASE_COMPLETED: &str = "aura.orchestrator.phase_completed";
}

/// SSE events specific to orchestration mode.
///
/// These events provide real-time visibility into multi-agent execution
/// and are emitted alongside standard `AuraStreamEvent`s.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OrchestrationStreamEvent {
    /// Emitted when orchestrator creates a plan from user query.
    PlanCreated {
        goal: String,
        /// Flat list of task descriptions, in task-ID order.
        #[serde(default)]
        tasks: Vec<String>,
        /// Dependency edges per task; `dag[i].id` pairs with `tasks[i]`.
        #[serde(default)]
        dag: Vec<TaskDagNode>,
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
    /// Emitted when a phase starts execution.
    PhaseStarted {
        phase_id: usize,
        label: String,
        orchestrator_id: String,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when a phase completes execution.
    PhaseCompleted {
        phase_id: usize,
        label: String,
        continuation: PhaseContinuation,
        orchestrator_id: String,
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
            Self::PhaseStarted { .. } => event_names::PHASE_STARTED,
            Self::PhaseCompleted { .. } => event_names::PHASE_COMPLETED,
        }
    }

    /// Format this event as an SSE message with the event: field.
    pub fn format_sse(&self) -> String {
        format_named_sse(self.event_name(), self)
    }

    pub fn plan_created(
        goal: impl Into<String>,
        tasks: Vec<String>,
        dag: Vec<TaskDagNode>,
        routing_mode: RoutingMode,
        routing_rationale: impl Into<String>,
        planning_response: Option<String>,
        context: EventContext,
    ) -> Self {
        Self::PlanCreated {
            goal: goal.into(),
            tasks,
            dag,
            routing_mode,
            routing_rationale: routing_rationale.into(),
            planning_response,
            context,
        }
    }

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

    pub fn synthesizing(iteration: usize, context: EventContext) -> Self {
        Self::Synthesizing { iteration, context }
    }

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

    pub fn phase_started(
        phase_id: usize,
        label: impl Into<String>,
        orchestrator_id: impl Into<String>,
        context: EventContext,
    ) -> Self {
        Self::PhaseStarted {
            phase_id,
            label: label.into(),
            orchestrator_id: orchestrator_id.into(),
            context,
        }
    }

    pub fn phase_completed(
        phase_id: usize,
        label: impl Into<String>,
        continuation: PhaseContinuation,
        orchestrator_id: impl Into<String>,
        context: EventContext,
    ) -> Self {
        Self::PhaseCompleted {
            phase_id,
            label: label.into(),
            continuation,
            orchestrator_id: orchestrator_id.into(),
            context,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentContext, CorrelationContext};

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
                vec![],
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
    fn test_format_sse_task_started_omits_dependencies() {
        // DAG structure lives on plan_created; task_started only carries
        // runtime identity and description.
        let event = OrchestrationStreamEvent::task_started(
            2,
            "Summarize root cause",
            "orch-1",
            "writer",
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::TASK_STARTED)));
        assert!(!sse.contains("dependencies"));
        assert!(sse.contains("\"task_id\":2"));
    }

    #[test]
    fn test_old_task_started_without_dependencies_still_deserializes() {
        // Pre-#221 task_started payloads had a dependencies field; modern
        // payloads omit it because plan_created carries the authoritative DAG.
        // Serde ignores unknown fields by default, so both old and new payloads
        // deserialize successfully.
        let old_payload = serde_json::json!({
            "task_id": 1,
            "description": "d",
            "dependencies": [0],
            "worker_id": "w",
            "orchestrator_id": "o",
            "agent_id": "coordinator",
            "session_id": "s"
        });
        let event: OrchestrationStreamEvent =
            serde_json::from_value(old_payload).expect("old payload deserializes");
        assert!(matches!(
            event,
            OrchestrationStreamEvent::TaskStarted { ref task, .. } if task.task_id == 1
        ));
    }

    // These wire-format assertions are the equivalence proof for the
    // local-enum -> aura-events consolidation: byte-for-byte JSON fragments
    // the server emitted before the swap must still be emitted after it.

    #[test]
    fn test_format_sse() {
        let event = OrchestrationStreamEvent::plan_created(
            "test goal",
            Vec::from([
                "Task 1 description".to_string(),
                "Task 2 description".to_string(),
            ]),
            vec![
                TaskDagNode {
                    id: 0,
                    dependencies: vec![],
                    worker: Some("sre".to_string()),
                },
                TaskDagNode {
                    id: 1,
                    dependencies: vec![0],
                    worker: None,
                },
            ],
            RoutingMode::Orchestrated,
            "test rationale",
            Some("coordinator response text".to_string()),
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.starts_with(&format!("event: {}\n", event_names::PLAN_CREATED)));
        assert!(sse.contains("\"goal\":\"test goal\""));
        assert!(sse.contains("\"tasks\":[\"Task 1 description\",\"Task 2 description\"]"));
        assert!(sse.contains(
            "\"dag\":[{\"id\":0,\"dependencies\":[],\"worker\":\"sre\"},{\"id\":1,\"dependencies\":[0]}]"
        ));
        assert!(sse.contains("\"routing_mode\":\"orchestrated\""));
        assert!(sse.contains("\"routing_rationale\":\"test rationale\""));
        assert!(sse.contains("\"planning_response\":\"coordinator response text\""));
    }

    #[test]
    fn test_format_sse_plan_created_single_task_is_orchestrated() {
        // Single-task plans execute through the orchestrator and report the
        // same routing_mode as multi-task plans.
        let event = OrchestrationStreamEvent::plan_created(
            "simple math",
            Vec::from(["Calculate the mean of [10, 20, 30]".to_string()]),
            vec![TaskDagNode {
                id: 0,
                dependencies: vec![],
                worker: None,
            }],
            RoutingMode::Orchestrated,
            "single task still goes through orchestrator",
            None,
            test_ctx(),
        );
        let sse = event.format_sse();

        assert!(sse.contains("\"routing_mode\":\"orchestrated\""));
        assert!(!sse.contains("planning_response"));
    }

    #[test]
    fn test_format_sse_plan_created_without_response() {
        let event = OrchestrationStreamEvent::plan_created(
            "goal",
            Vec::from(["Task 1".to_string()]),
            vec![],
            RoutingMode::Orchestrated,
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
        // Evaluation-era fields pruned during consolidation must not reappear.
        assert!(!sse.contains("quality_score"));
        assert!(!sse.contains("evaluation_skipped"));
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
        assert!(sse.contains("\"task_id\":0"));
        assert!(sse.contains("\"orchestrator_id\":\"orch-1\""));
        assert!(sse.contains("\"worker_id\":\"worker-1\""));
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

    #[test]
    fn test_old_stream_without_tasks_field_still_deserializes() {
        // Consumer tolerance: plan_created payloads from servers older than
        // the tasks field (which carried task_count instead) must still
        // match the PlanCreated variant via #[serde(default)].
        let old_payload = serde_json::json!({
            "goal": "g",
            "task_count": 2,
            "routing_mode": "orchestrated",
            "routing_rationale": "r",
            "agent_id": "coordinator",
            "session_id": "s"
        });
        let event: OrchestrationStreamEvent =
            serde_json::from_value(old_payload).expect("old payload deserializes");
        assert!(matches!(
            event,
            OrchestrationStreamEvent::PlanCreated { ref tasks, .. } if tasks.is_empty()
        ));
    }

    #[test]
    fn test_old_task_completed_flat_fields_still_deserialize() {
        // Pre-flatten TaskCompleted carried task_id/success/duration_ms/result
        // and the orchestrator/worker ids at the top level. The TaskContext +
        // CompletionOutcome flatten keeps the wire shape identical, so old
        // payloads must still deserialize into the flattened variant.
        let old_payload = serde_json::json!({
            "task_id": 2,
            "success": true,
            "duration_ms": 1234,
            "orchestrator_id": "o",
            "worker_id": "w",
            "result": "done",
            "agent_id": "coordinator",
            "session_id": "s"
        });
        let event: OrchestrationStreamEvent =
            serde_json::from_value(old_payload).expect("old TaskCompleted deserializes");
        assert!(matches!(
            event,
            OrchestrationStreamEvent::TaskCompleted { ref task, ref outcome, .. }
                if task.task_id == 2 && outcome.success && outcome.duration_ms == 1234
        ));
    }

    #[test]
    fn test_old_tool_call_completed_flat_fields_still_deserialize() {
        // Pre-flatten ToolCallCompleted carried success/duration_ms/result at
        // the top level alongside tool_call_id. The CompletionOutcome flatten
        // keeps the wire shape, so old payloads still deserialize into the
        // flattened variant.
        let old_payload = serde_json::json!({
            "task_id": 0,
            "tool_call_id": "call_1",
            "success": true,
            "duration_ms": 42,
            "result": "30.0",
            "agent_id": "coordinator",
            "session_id": "s"
        });
        let event: OrchestrationStreamEvent =
            serde_json::from_value(old_payload).expect("old ToolCallCompleted deserializes");
        assert!(matches!(
            event,
            OrchestrationStreamEvent::ToolCallCompleted { ref outcome, ref tool_call_id, .. }
                if outcome.duration_ms == 42 && tool_call_id == "call_1"
        ));
    }
}
