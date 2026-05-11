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

/// How the coordinator routed a query that produced a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    /// Coordinator classified query to a single worker.
    Routed,
    /// Full orchestration — multi-task DAG with synthesis + evaluation.
    Orchestrated,
}

impl RoutingMode {
    /// Derive routing mode from the number of tasks in a plan.
    pub fn for_plan(task_count: usize) -> Self {
        if task_count == 1 {
            Self::Routed
        } else {
            Self::Orchestrated
        }
    }
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
        task_count: usize,
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
        task_id: usize,
        description: String,
        worker_id: String,
        orchestrator_id: String,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator completes a task.
    TaskCompleted {
        task_id: usize,
        success: bool,
        duration_ms: u64,
        orchestrator_id: String,
        worker_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(flatten)]
        context: EventContext,
    },
    /// Emitted when orchestrator completes an iteration.
    IterationComplete {
        iteration: usize,
        quality_score: f32,
        quality_threshold: f32,
        will_replan: bool,
        evaluation_skipped: bool,
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
        success: bool,
        duration_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
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
        task_count: usize,
        routing_mode: RoutingMode,
        routing_rationale: impl Into<String>,
        planning_response: Option<String>,
        context: EventContext,
    ) -> Self {
        Self::PlanCreated {
            goal: goal.into(),
            task_count,
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
            task_id,
            description: description.into(),
            orchestrator_id: orchestrator_id.into(),
            worker_id: worker_id.into(),
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
            task_id,
            success,
            duration_ms,
            orchestrator_id: orchestrator_id.into(),
            worker_id: worker_id.into(),
            result,
            context,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn iteration_complete(
        iteration: usize,
        quality_score: f32,
        quality_threshold: f32,
        will_replan: bool,
        evaluation_skipped: bool,
        reasoning: Option<String>,
        gaps: Vec<String>,
        context: EventContext,
    ) -> Self {
        Self::IterationComplete {
            iteration,
            quality_score,
            quality_threshold,
            will_replan,
            evaluation_skipped,
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
            success,
            duration_ms,
            result,
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
