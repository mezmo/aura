//! Internal orchestration events.
//!
//! These events flow through `StreamItem::OrchestratorEvent` and are converted
//! to `OrchestrationStreamEvent` for SSE output by the web server handlers.
//!
//! This separation keeps orchestration-specific types isolated from the base
//! aura streaming infrastructure.

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
        task_count: usize,
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
    /// An iteration of the plan-execute-synthesize loop has completed.
    ///
    /// Emitted after synthesis and evaluation. The quality score determines
    /// whether the orchestrator will replan or accept the result.
    IterationComplete {
        /// Which iteration just completed (1-indexed)
        iteration: usize,
        /// Quality score from evaluation (0.0-1.0)
        quality_score: f32,
        /// The configured quality threshold
        quality_threshold: f32,
        /// Whether the orchestrator will replan after this iteration
        will_replan: bool,
        /// Evaluator's reasoning about quality
        reasoning: String,
        /// Identified gaps or missing elements
        gaps: Vec<String>,
    },
    /// The orchestrator is starting a replan cycle.
    ///
    /// Emitted when the orchestrator decides to create a new plan,
    /// either due to quality evaluation, task failures, or phase continuation.
    ReplanStarted {
        /// Which iteration is about to start (1-indexed)
        iteration: usize,
        /// What triggered the replan: "quality", "failure", or "phase_continuation"
        trigger: String,
    },
    /// The orchestrator is synthesizing results from completed tasks.
    ///
    /// Emitted before the synthesizer LLM call that combines worker
    /// outputs into a coherent final response.
    Synthesizing {
        /// Which iteration is being synthesized (1-indexed)
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
    /// A phase has started execution.
    PhaseStarted {
        /// Phase identifier
        phase_id: usize,
        /// Human-readable phase label
        label: String,
        /// The ID of the orchestrator
        orchestrator_id: String,
    },
    /// A phase has completed execution.
    PhaseCompleted {
        /// Phase identifier
        phase_id: usize,
        /// Human-readable phase label
        label: String,
        /// The continuation decision
        continuation: super::types::PhaseContinuation,
        /// The ID of the orchestrator
        orchestrator_id: String,
    },
}
