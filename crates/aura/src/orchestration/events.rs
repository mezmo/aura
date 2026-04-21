//! Internal orchestration events.
//!
//! These events flow through `StreamItem::OrchestratorEvent` and are converted
//! to `OrchestrationStreamEvent` for SSE output by the web server handlers.
//!
//! This separation keeps orchestration-specific types isolated from the base
//! aura streaming infrastructure.

use serde::{Deserialize, Serialize};

/// How the coordinator routed a query that produced a plan.
///
/// Provides a machine-readable signal in `plan_created` SSE events and
/// `RunManifest` persistence so clients can distinguish single-worker
/// classification from full orchestration without parsing text fields.
///
/// Direct answers and clarifications have their own event types
/// (`aura.orchestrator.direct_answer`, `aura.orchestrator.clarification_needed`)
/// and never produce a `PlanCreated` event, so they are not represented here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    /// Coordinator classified query to a single worker.
    Routed,
    /// Full orchestration — multi-task DAG execution.
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

/// Events emitted by the orchestrator during execution.
///
/// These are internal events that flow through the stream and are converted
/// to SSE events (`OrchestrationStreamEvent`) by the web server handlers.
#[derive(Debug, Clone)]
pub enum OrchestratorEvent {
    /// A plan has been created from the user's query.
    PlanCreated {
        /// The goal being addressed
        goal: String,
        /// Number of tasks in the plan
        tasks: Vec<String>,
        /// How the coordinator routed this query
        routing_mode: RoutingMode,
        /// Why the coordinator chose orchestration
        routing_rationale: String,
        /// The coordinator's planning response text (truncated to Option in SSE)
        planning_response: String,
    },
    /// The coordinator answered the query directly without orchestration.
    DirectAnswer {
        /// The direct response
        response: String,
        /// Why the coordinator chose to answer directly
        routing_rationale: String,
    },
    /// The coordinator needs clarification from the user.
    ClarificationNeeded {
        /// The clarification question
        question: String,
        /// Optional suggested options
        options: Option<Vec<String>>,
        /// Why clarification was needed
        routing_rationale: String,
    },
    /// A task has started execution.
    TaskStarted {
        /// Task identifier
        task_id: usize,
        /// Human-readable task description
        description: String,
        /// The ID of the orchestrator
        orchestrator_id: String,
        /// The ID of the Worker who is handling the task
        worker_id: String,
    },
    /// A task has completed (success or failure).
    TaskCompleted {
        /// Task identifier
        task_id: usize,
        /// Whether the task succeeded
        success: bool,
        /// How long the task took in milliseconds
        duration_ms: u64,
        /// The ID of the orchestrator
        orchestrator_id: String,
        /// The ID of the Worker who is handling the task
        worker_id: String,
        /// The task result (output string or error message; truncated to Option in SSE)
        result: String,
    },
    /// An iteration of the plan-execute loop has completed.
    ///
    /// Emitted after execution completes. Indicates whether the orchestrator
    /// will replan based on task failures.
    IterationComplete {
        /// Which iteration just completed (1-indexed)
        iteration: usize,
        /// Whether the orchestrator will replan after this iteration
        will_replan: bool,
        /// Reasoning about why replan was triggered (empty if not replanning)
        reasoning: String,
        /// Identified gaps or issues (empty if not replanning)
        gaps: Vec<String>,
    },
    /// The orchestrator is starting a replan cycle.
    ///
    /// Emitted when the orchestrator decides to create a new plan,
    /// either because the coordinator routed back to `create_plan` or because
    /// task failures forced a replan.
    ReplanStarted {
        /// Which iteration is about to start (1-indexed)
        iteration: usize,
        /// What triggered the replan: "coordinator" or "failure"
        trigger: String,
    },
    /// Task results are being consolidated for the coordinator.
    ///
    /// Emitted before the post-execute coordinator call that presents
    /// worker outputs and chooses a routing decision (respond, replan,
    /// or clarify).
    Synthesizing {
        /// Which iteration's results are being consolidated (1-indexed)
        iteration: usize,
    },
    /// Reasoning content from a worker agent.
    ///
    /// Wraps raw `ReasoningDelta`/`Reasoning` stream items from workers
    /// with task and worker identity for proper SSE attribution.
    WorkerReasoning {
        /// Task identifier
        task_id: usize,
        /// The worker that produced this reasoning (e.g., "statistics")
        worker_id: String,
        /// The reasoning text content
        content: String,
    },
    /// A tool call has started within a worker task.
    ToolCallStarted {
        /// Task ID the tool call belongs to (None if ID couldn't be parsed)
        task_id: Option<usize>,
        /// Unique identifier for this tool call
        tool_call_id: String,
        /// Name of the tool being called
        tool_name: String,
        /// ID of the worker or orchestrator that called the tool
        worker_id: String,
        /// Arguments passed to the tool
        arguments: serde_json::Value,
    },
    /// A tool call has completed within a worker task.
    ToolCallCompleted {
        /// Task ID the tool call belongs to (None if ID couldn't be parsed)
        task_id: Option<usize>,
        /// The tool call ID this result corresponds to
        tool_call_id: String,
        /// Whether the tool call succeeded
        success: bool,
        /// How long the call took in milliseconds
        duration_ms: u64,
        /// The tool result (output string or error message; truncated to Option in SSE)
        result: String,
    },
}
