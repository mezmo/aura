//! Types for orchestration mode task management.
//!
//! This module defines the core types used by the orchestrator to decompose
//! queries into tasks, track their execution, and manage dependencies.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Maximum nesting depth for step structures.
/// Depth 0 = top-level steps list, depth 1 = inside a parallel group,
/// depth 2 = sub-chain inside a parallel group. No deeper nesting allowed.
const MAX_STEP_NESTING: usize = 2;

/// A step in a sequential plan. Tagged enum so models declare intent
/// explicitly via `"type"` before filling fields.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepInput {
    /// A group of steps that execute in parallel.
    #[serde(rename = "parallel")]
    ParallelGroup { items: Vec<StepInput> },
    /// A sequential sub-chain (only valid inside a `ParallelGroup`).
    #[serde(rename = "chain")]
    SubChain { steps: Vec<StepInput> },
    /// A single task to execute.
    #[serde(rename = "task")]
    LeafTask {
        task: String,
        #[serde(default)]
        worker: Option<String>,
    },
}

/// Convert a list of `StepInput` into a flat `Vec<Task>` with auto-assigned IDs
/// and frontier-based dependency tracking.
///
/// Top-level steps are sequential: each step depends on all tasks produced by
/// the previous step (the "frontier"). Parallel groups run their branches
/// concurrently, and the combined exit tasks form the new frontier.
pub fn flatten_steps(steps: &[StepInput]) -> Result<Vec<Task>, String> {
    if steps.is_empty() {
        return Err("Steps list is empty".to_string());
    }
    let mut tasks = Vec::new();
    let mut counter: usize = 0;
    let frontier = Vec::new(); // initial frontier is empty (no deps for first step)
    flatten_sequential(steps, &frontier, &mut counter, &mut tasks, 0)?;
    Ok(tasks)
}

/// Flatten a sequential list of steps. Each step depends on the current frontier,
/// and produces a new frontier for the next step.
fn flatten_sequential(
    steps: &[StepInput],
    initial_frontier: &[usize],
    counter: &mut usize,
    tasks: &mut Vec<Task>,
    depth: usize,
) -> Result<Vec<usize>, String> {
    let mut frontier = initial_frontier.to_vec();

    for step in steps {
        frontier = flatten_one(step, &frontier, counter, tasks, depth)?;
    }

    Ok(frontier)
}

/// Flatten a single step, returning the new frontier (task IDs produced).
fn flatten_one(
    step: &StepInput,
    frontier: &[usize],
    counter: &mut usize,
    tasks: &mut Vec<Task>,
    depth: usize,
) -> Result<Vec<usize>, String> {
    match step {
        StepInput::LeafTask { task, worker } => {
            let id = *counter;
            *counter += 1;
            let mut t = Task::new(id, task.clone(), String::new());
            t.dependencies = frontier.to_vec();
            t.worker = worker.clone();
            tasks.push(t);
            Ok(vec![id])
        }
        StepInput::ParallelGroup { items } => {
            if items.is_empty() {
                return Err("Empty parallel group".to_string());
            }
            if depth >= MAX_STEP_NESTING {
                return Err(format!(
                    "Step nesting depth exceeds maximum of {}",
                    MAX_STEP_NESTING
                ));
            }
            if items.len() == 1 {
                tracing::warn!("Single-item parallel group is redundant, treating as sequential");
            }
            let mut exit_frontier = Vec::new();
            for branch in items {
                let branch_exit = flatten_one(branch, frontier, counter, tasks, depth + 1)?;
                exit_frontier.extend(branch_exit);
            }
            Ok(exit_frontier)
        }
        StepInput::SubChain { steps } => {
            if steps.is_empty() {
                return Err("Empty sub-chain".to_string());
            }
            if depth >= MAX_STEP_NESTING {
                return Err(format!(
                    "Step nesting depth exceeds maximum of {}",
                    MAX_STEP_NESTING
                ));
            }
            flatten_sequential(steps, frontier, counter, tasks, depth + 1)
        }
    }
}

/// A plan representing a decomposed query.
///
/// The coordinator creates a plan by analyzing the user's query and
/// breaking it down into discrete, actionable tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    /// The original goal/query being addressed.
    pub goal: String,
    /// Original step structure from the coordinator. Always present for plans
    /// constructed from a `PlanningResponse::StepsPlan`; populated for persistence
    /// and observability — execution uses `tasks`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<StepInput>>,
    /// Ordered list of tasks to accomplish the goal.
    /// Auto-generated from `steps` via `flatten_steps()` when steps are present.
    pub tasks: Vec<Task>,
}

impl Plan {
    /// Create a new plan with the given goal.
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            steps: None,
            tasks: Vec::new(),
        }
    }

    /// Add a task to the plan.
    pub fn add_task(&mut self, task: Task) {
        self.tasks.push(task);
    }

    /// Get the count of pending tasks.
    pub fn pending_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| matches!(t.state, TaskState::Pending))
            .count()
    }

    /// Get the count of completed tasks.
    pub fn completed_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| matches!(t.state, TaskState::Complete { .. }))
            .count()
    }

    /// Get the count of failed tasks.
    pub fn failed_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| matches!(t.state, TaskState::Failed { .. }))
            .count()
    }

    /// Check if all tasks are complete (or failed).
    pub fn is_finished(&self) -> bool {
        self.tasks.iter().all(|t| {
            matches!(
                t.state,
                TaskState::Complete { .. } | TaskState::Failed { .. }
            )
        })
    }

    /// Get the next task that is ready to run (pending with all dependencies complete).
    pub fn next_ready_task(&self) -> Option<&Task> {
        self.tasks.iter().find(|task| {
            matches!(task.state, TaskState::Pending)
                && task.dependencies.iter().all(|dep_id| {
                    self.tasks
                        .iter()
                        .find(|t| t.id == *dep_id)
                        .map(|t| matches!(t.state, TaskState::Complete { .. }))
                        .unwrap_or(true)
                })
        })
    }

    /// Get a mutable reference to a task by ID.
    pub fn get_task_mut(&mut self, task_id: usize) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == task_id)
    }

    /// Returns tasks that are ready to execute.
    ///
    /// A task is ready when:
    /// - State is `Pending`
    /// - All dependencies have state `Complete`
    ///
    /// Tasks with failed dependencies are NOT returned (use `blocked_tasks()` to find them).
    pub fn ready_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| {
                if !matches!(task.state, TaskState::Pending) {
                    return false;
                }

                for dep_id in &task.dependencies {
                    let dep = self.tasks.iter().find(|t| t.id == *dep_id);
                    match dep.map(|t| &t.state) {
                        Some(TaskState::Complete { .. }) => continue,
                        Some(TaskState::Failed { .. }) => return false,
                        _ => return false,
                    }
                }
                true
            })
            .collect()
    }

    /// Returns tasks that are blocked due to failed dependencies.
    ///
    /// These tasks cannot execute because at least one dependency has failed.
    /// Used to identify tasks that need replanning.
    pub fn blocked_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| {
                matches!(task.state, TaskState::Pending)
                    && task.dependencies.iter().any(|dep_id| {
                        self.tasks
                            .iter()
                            .find(|t| t.id == *dep_id)
                            .map(|t| matches!(t.state, TaskState::Failed { .. }))
                            .unwrap_or(false)
                    })
            })
            .collect()
    }
}

/// A discrete task within a plan.
#[derive(Debug, Clone)]
pub struct Task {
    /// Unique identifier for this task.
    pub id: usize,
    /// Human-readable description of what this task accomplishes.
    pub description: String,
    /// IDs of tasks that must complete before this one can start.
    pub dependencies: Vec<usize>,
    /// Execution state — use pattern matching to access variant data.
    pub state: TaskState,
    /// Assigned worker name (when specialized workers are configured).
    pub worker: Option<String>,
    /// Why this task exists and how it advances the goal.
    pub rationale: String,
    /// Structured output from `submit_result` tool. When present, `summary`
    /// is used as the inline preview in continuation prompts and manifests.
    /// Top-level because it's orthogonal to pass/fail — workers can submit
    /// structured output regardless of task outcome.
    pub structured_output: Option<StructuredTaskOutput>,
}

impl Serialize for Task {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("description", &self.description)?;
        map.serialize_entry("dependencies", &self.dependencies)?;
        map.serialize_entry("status", &TaskStatus::from(&self.state))?;
        match &self.state {
            TaskState::Complete { result } => {
                map.serialize_entry("result", result)?;
            }
            TaskState::Failed { error, category } => {
                map.serialize_entry("error", error)?;
                map.serialize_entry("failure_category", category)?;
            }
            _ => {}
        }
        if let Some(ref w) = self.worker {
            map.serialize_entry("worker", w)?;
        }
        map.serialize_entry("rationale", &self.rationale)?;
        if let Some(ref so) = self.structured_output {
            map.serialize_entry("structured_output", so)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Task {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct TaskHelper {
            id: usize,
            description: String,
            #[serde(default)]
            dependencies: Vec<usize>,
            status: TaskStatus,
            result: Option<String>,
            error: Option<String>,
            #[serde(default)]
            failure_category: Option<FailureCategory>,
            #[serde(default)]
            worker: Option<String>,
            #[serde(default)]
            rationale: String,
            #[serde(default)]
            structured_output: Option<StructuredTaskOutput>,
        }
        let h = TaskHelper::deserialize(deserializer)?;
        let state = match h.status {
            TaskStatus::Pending => TaskState::Pending,
            TaskStatus::Running => TaskState::Running,
            TaskStatus::Complete => TaskState::Complete {
                result: h.result.unwrap_or_default(),
            },
            TaskStatus::Failed => TaskState::Failed {
                error: h.error.unwrap_or_else(|| "unknown".into()),
                category: h.failure_category.unwrap_or_default(),
            },
        };
        Ok(Task {
            id: h.id,
            description: h.description,
            dependencies: h.dependencies,
            state,
            worker: h.worker,
            rationale: h.rationale,
            structured_output: h.structured_output,
        })
    }
}

/// Structured metadata from `submit_result`, collapsed into a single optional
/// to eliminate invalid states (e.g., confidence without summary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredTaskOutput {
    pub summary: String,
    pub confidence: super::tools::submit_result::Confidence,
}

impl Task {
    /// Create a new pending task.
    pub fn new(id: usize, description: impl Into<String>, rationale: impl Into<String>) -> Self {
        Self {
            id,
            description: description.into(),
            dependencies: Vec::new(),
            state: TaskState::Pending,
            worker: None,
            rationale: rationale.into(),
            structured_output: None,
        }
    }

    /// Assign this task to a specific worker.
    pub fn with_worker(mut self, worker: impl Into<String>) -> Self {
        self.worker = Some(worker.into());
        self
    }

    /// Add a dependency on another task.
    pub fn with_dependency(mut self, task_id: usize) -> Self {
        self.dependencies.push(task_id);
        self
    }

    /// Add multiple dependencies.
    pub fn with_dependencies(mut self, task_ids: impl IntoIterator<Item = usize>) -> Self {
        self.dependencies.extend(task_ids);
        self
    }

    /// Mark this task as running.
    pub fn start(&mut self) {
        self.state = TaskState::Running;
    }

    /// Mark this task as complete with a result.
    pub fn complete(&mut self, result: impl Into<String>) {
        self.state = TaskState::Complete {
            result: result.into(),
        };
    }

    /// Mark this task as failed with an error and structured category.
    pub fn fail(&mut self, error: impl Into<String>, category: FailureCategory) {
        self.state = TaskState::Failed {
            error: error.into(),
            category,
        };
    }
}

/// Status of a task in the execution plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Task is waiting to be executed.
    #[default]
    Pending,
    /// Task is currently being executed.
    Running,
    /// Task completed successfully.
    Complete,
    /// Task failed.
    Failed,
}

/// Structured classification of why a task failed, surfaced in the
/// continuation prompt so the coordinator can make informed replan decisions.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default, strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum FailureCategory {
    /// Worker timed out before completing.
    AgentTimeout,
    /// Worker hit its context window limit.
    ContextOverflow,
    /// Worker exhausted its maximum tool call depth.
    DepthExhausted,
    /// Duplicate call guard fired — worker stuck in a loop.
    LoopDetected,
    /// LLM temporarily unavailable (429/503).
    ProviderOverloaded,
    /// LLM credentials or auth failed (401/403).
    ProviderAuthError,
    /// LLM model not found or invalid model identifier (404).
    ProviderNotFound,
    /// Upstream dependency task failed.
    DependencyFailed,
    /// Worker completed but reported unable to produce a result.
    SoftFailure,
    /// Unclassified worker failure.
    #[default]
    AgentError,
}

/// Rich state of a task, making invalid states unrepresentable.
///
/// Use pattern matching or `matches!()` for boolean checks.
#[derive(Debug, Clone)]
pub enum TaskState {
    Pending,
    Running,
    Complete {
        result: String,
    },
    Failed {
        error: String,
        category: FailureCategory,
    },
}

impl From<&TaskState> for TaskStatus {
    fn from(state: &TaskState) -> Self {
        match state {
            TaskState::Pending => TaskStatus::Pending,
            TaskState::Running => TaskStatus::Running,
            TaskState::Complete { .. } => TaskStatus::Complete,
            TaskState::Failed { .. } => TaskStatus::Failed,
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Pending => write!(f, "pending"),
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Complete => write!(f, "complete"),
            TaskStatus::Failed => write!(f, "failed"),
        }
    }
}

// ============================================================================
// Planning Response (routing decision from coordinator)
// ============================================================================

/// The coordinator's routing decision for a user query.
///
/// The coordinator calls one of three routing tools to indicate how to handle
/// the query. This enum captures that decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "response_type")]
pub enum PlanningResponse {
    /// The query can be answered directly without orchestration.
    #[serde(rename = "direct")]
    Direct {
        response: String,
        routing_rationale: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_summary: Option<String>,
    },
    /// The query requires orchestration, expressed as ordered steps.
    ///
    /// Steps are sequential by default; parallel execution is opt-in via
    /// `ParallelGroup`. Converted to `Plan` via `flatten_steps()` in `into_plan()`.
    #[serde(rename = "steps_plan")]
    StepsPlan {
        goal: String,
        steps: Vec<StepInput>,
        routing_rationale: String,
        #[serde(default)]
        planning_summary: String,
    },
    /// The query is ambiguous and needs clarification from the user.
    #[serde(rename = "clarification")]
    Clarification {
        question: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        routing_rationale: String,
    },
}

impl PlanningResponse {
    /// Convert a `StepsPlan` response into a `Plan`.
    ///
    /// Returns `None` for `Direct` and `Clarification` variants.
    pub fn into_plan(self) -> Option<Plan> {
        match self {
            PlanningResponse::StepsPlan { goal, steps, .. } => match flatten_steps(&steps) {
                Ok(tasks) => {
                    let mut plan = Plan::new(goal);
                    plan.steps = Some(steps);
                    for task in tasks {
                        plan.add_task(task);
                    }
                    Some(plan)
                }
                Err(e) => {
                    tracing::error!("Failed to flatten steps plan: {}", e);
                    None
                }
            },
            _ => None,
        }
    }

    /// Human-readable variant name for logging.
    pub fn variant_name(&self) -> &'static str {
        match self {
            PlanningResponse::Direct { .. } => "Direct",
            PlanningResponse::StepsPlan { .. } => "StepsPlan",
            PlanningResponse::Clarification { .. } => "Clarification",
        }
    }

    /// Get the routing rationale regardless of variant.
    pub fn routing_rationale(&self) -> &str {
        match self {
            PlanningResponse::Direct {
                routing_rationale, ..
            }
            | PlanningResponse::StepsPlan {
                routing_rationale, ..
            }
            | PlanningResponse::Clarification {
                routing_rationale, ..
            } => routing_rationale,
        }
    }

    /// Get the planning summary (only present on StepsPlan variant).
    pub fn planning_summary(&self) -> Option<&str> {
        match self {
            PlanningResponse::StepsPlan {
                planning_summary, ..
            } => Some(planning_summary),
            _ => None,
        }
    }
}

/// JSON representation of a task in a planning response.
///
/// This is the shape the LLM produces when calling `create_plan`.
/// Converted to `Task` via `PlanningResponse::into_plan()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskJson {
    pub id: usize,
    pub description: String,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub dependencies: Option<Vec<usize>>,
    #[serde(default)]
    pub worker: Option<String>,
}

/// Summary of task failures during an iteration.
///
/// Populated on the failure-replan path so the coordinator can see what
/// went wrong when it renders the continuation prompt. Intentionally
/// smaller than the deleted `EvaluationResult` — no score, no evaluator
/// vocabulary; just a reasoning string and a list of specific gaps.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FailureSummary {
    /// Short explanation of the failure state for this iteration.
    pub reasoning: String,
    /// Specific areas needing attention (e.g., failed task descriptions).
    #[serde(default)]
    pub gaps: Vec<String>,
}

impl FailureSummary {
    /// Format gaps as bullet points for the continuation prompt.
    pub fn gaps_as_bullets(&self) -> String {
        if self.gaps.is_empty() {
            "- No specific gaps identified".to_string()
        } else {
            self.gaps
                .iter()
                .map(|g| format!("- {}", g))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// Record of a task that failed during an iteration.
///
/// Accumulated across iterations so the coordinator can see which tasks
/// have repeatedly failed and avoid repeating the same approach.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedTaskRecord {
    /// Description of the failed task.
    pub description: String,
    /// Error message from the failure.
    pub error: String,
    /// Which iteration this failure occurred in.
    pub iteration: usize,
    /// Worker that was assigned (if any).
    pub worker: Option<String>,
    /// Structured failure classification.
    #[serde(default)]
    pub category: FailureCategory,
}

/// Context from a previous iteration, used for the post-execute
/// Context from a previous iteration, used for the post-execute
/// coordinator decision (continuation prompt).
///
/// Every iteration's end-of-execute state is rendered via this context
/// — clean success (`failure_summary = None`), failure, partial, or
/// dependency-blocked cases all share the same template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationContext {
    /// Which iteration just completed (1-indexed).
    pub iteration: usize,
    /// The plan from the previous iteration.
    pub previous_plan: Plan,
    /// The verbatim original user query, pinned for the continuation goal
    /// line (`docs/redesign/ARCHITECTURE.md` section 1.2). The orchestrator
    /// pins it at the post-execute construction site; contexts that never
    /// render a continuation (plan carry-forward) leave it `None`, and the
    /// renderer falls back to `previous_plan.goal`.
    #[serde(default)]
    pub pinned_goal: Option<super::context::PinnedGoal>,
    /// Failure summary populated only when the iteration had failures or
    /// blocked tasks. `None` on the clean-success path.
    pub failure_summary: Option<FailureSummary>,
    /// Accumulated failure history across all iterations.
    pub failure_history: Vec<FailedTaskRecord>,
    /// Pre-loaded tool traces per task, keyed by task ID.
    /// Populated when `show_tool_reasoning_in_continuation` is enabled;
    /// empty HashMap otherwise.
    #[serde(default)]
    pub tool_traces: HashMap<usize, Vec<super::persistence::ToolTraceEntry>>,
}

impl IterationContext {
    /// Create a new iteration context.
    pub fn new(
        iteration: usize,
        previous_plan: Plan,
        failure_summary: Option<FailureSummary>,
        failure_history: Vec<FailedTaskRecord>,
        tool_traces: HashMap<usize, Vec<super::persistence::ToolTraceEntry>>,
    ) -> Self {
        Self {
            iteration,
            previous_plan,
            pinned_goal: None,
            failure_summary,
            failure_history,
            tool_traces,
        }
    }

    /// Pin the continuation goal line to the verbatim original user query,
    /// replacing the coordinator's own drifting `previous_plan.goal`
    /// (`docs/redesign/ARCHITECTURE.md` section 1.2).
    #[must_use]
    pub fn with_pinned_goal(mut self, goal: super::context::PinnedGoal) -> Self {
        self.pinned_goal = Some(goal);
        self
    }

    /// Build the continuation section for the post-execute coordinator call.
    ///
    /// Renders the previous iteration's per-task state (completed, failed,
    /// blocked) as evidence-framed entries — correlation label plus the
    /// worker's own reported evidence, never the coordinator's task
    /// description (`docs/redesign/ARCHITECTURE.md` section 1.3) — plus the
    /// optional failure summary, accumulated failure history with
    /// repeated-failure detection keyed by truncated handle (section 1.5),
    /// and a conditional reuse hint. Uses the `.md` template in
    /// `crates/aura/src/prompts/continuation_prompt.md`.
    ///
    /// The goal line renders the pinned original user query when one is set
    /// (section 1.2), falling back to `previous_plan.goal` when no goal was
    /// pinned. Completed results are inlined fully when
    /// no artifact was created (result at or under the threshold, R2 gate
    /// decision 2); a spilled result shows its stand-in plus the artifact
    /// pointer instead.
    ///
    /// `_content_max_length` is no longer read: error previews own their
    /// 2000-character bound (`ErrorPreview::MAX_CHARS`, R2 gate decision 6).
    /// The parameter stays until the orchestrator call site is updated by a
    /// later card.
    pub fn build_continuation_prompt(
        &self,
        max_iterations: usize,
        show_tool_chain: bool,
        _content_max_length: usize,
    ) -> String {
        use super::context::{
            BlockedEntry, CompletedEntry, CorrelationLabel, ErrorPreview, EvidenceEntry,
            FailedEntry, FailureHandle, FailureRecord, FailureReport, IterationNumber, PinnedGoal,
            SpilledArtifact, TaskId, WorkerClaim, WorkerRole,
        };
        use super::templates::{ContinuationVars, render_continuation_prompt};

        // Correlation labels carry task id and worker role only; a blank
        // worker name is an unassigned task, which the label renders bare.
        let correlation_label = |t: &Task| CorrelationLabel {
            task: TaskId::new(t.id),
            worker: t.worker.as_deref().and_then(|w| WorkerRole::new(w).ok()),
        };

        // Categorize tasks
        let mut completed_lines = Vec::new();
        let mut blocked_lines = Vec::new();
        let mut redesign_lines = Vec::new();
        let mut has_failed_tasks = false;

        for t in &self.previous_plan.tasks {
            // An empty submit_result summary carries no claim.
            let claim = t
                .structured_output
                .as_ref()
                .and_then(|so| WorkerClaim::try_from(so).ok());
            match &t.state {
                TaskState::Complete { result } => {
                    let artifacts = artifact_refs(self.tool_traces.get(&t.id));
                    let entry = match EvidenceEntry::from_completed_result(result, claim) {
                        Ok(evidence) => String::from(
                            CompletedEntry {
                                label: correlation_label(t),
                                evidence,
                                artifacts,
                            }
                            .render(),
                        ),
                        // A whitespace-only result has no evidence to show;
                        // the label still records that the task ran, and the
                        // artifact inventory stays visible.
                        Err(_) => {
                            let mut line = format!("- Task {}", t.id);
                            if let Some(worker) = correlation_label(t).worker {
                                line.push_str(&format!(" ({worker})"));
                            }
                            for artifact in &artifacts {
                                line.push_str(&format!("\n    {artifact}"));
                            }
                            line
                        }
                    };
                    completed_lines.push(entry);
                    if show_tool_chain {
                        for line in render_tool_chain_lines(self.tool_traces.get(&t.id)) {
                            completed_lines.push(format!("    {}", line));
                        }
                    }
                }
                TaskState::Failed { error, category } => {
                    has_failed_tasks = true;
                    let report = match (category, claim) {
                        // Soft failures keep today's rendering: the worker's
                        // own claim plus any artifact footer from the spill
                        // path.
                        (FailureCategory::SoftFailure, Some(claim)) => FailureReport::Soft {
                            claim,
                            artifact: SpilledArtifact::parse_trailing(error),
                        },
                        (category, _) => FailureReport::Hard {
                            category: *category,
                            error: ErrorPreview::new(error),
                        },
                    };
                    redesign_lines.push(String::from(
                        FailedEntry {
                            label: correlation_label(t),
                            report,
                        }
                        .render(),
                    ));
                    for line in render_tool_chain_lines(self.tool_traces.get(&t.id)) {
                        redesign_lines.push(format!("    {}", line));
                    }
                }
                TaskState::Pending | TaskState::Running => {
                    blocked_lines.push(String::from(
                        BlockedEntry {
                            label: correlation_label(t),
                        }
                        .render(),
                    ));
                }
            }
        }

        use super::prompt_constants::continuation as hdr;

        let completed_section = if completed_lines.is_empty() {
            String::new()
        } else {
            format!(
                "{}\n{}\n\n",
                hdr::COMPLETED_TASKS,
                completed_lines.join("\n")
            )
        };

        let blocked_section = if blocked_lines.is_empty() {
            String::new()
        } else {
            format!("{}\n{}\n\n", hdr::BLOCKED_TASKS, blocked_lines.join("\n"))
        };

        let redesign_section = if redesign_lines.is_empty() {
            String::new()
        } else {
            format!("{}\n{}\n\n", hdr::FAILED_TASKS, redesign_lines.join("\n"))
        };

        let failure_section = match &self.failure_summary {
            Some(fs) => format!(
                "{}\n{}\n\n{}\n{}\n\n",
                hdr::FAILURE_SUMMARY,
                fs.reasoning,
                hdr::AREAS_NEEDING_ATTENTION,
                fs.gaps_as_bullets(),
            ),
            None => String::new(),
        };

        // Build failure history. Each record's display identity and its
        // repeat-detection grouping key are the same truncated handle
        // (`docs/redesign/ARCHITECTURE.md` section 1.5); records with no
        // identity to render (empty description, iteration zero) are
        // dropped rather than rendered blank.
        let records: Vec<FailureRecord> = self
            .failure_history
            .iter()
            .filter_map(|record| {
                Some(FailureRecord {
                    iteration: IterationNumber::new(record.iteration).ok()?,
                    handle: FailureHandle::from_description(&record.description).ok()?,
                    worker: record
                        .worker
                        .as_deref()
                        .and_then(|w| WorkerRole::new(w).ok()),
                    category: record.category,
                    error: ErrorPreview::new(&record.error),
                })
            })
            .collect();
        let failure_history = if records.is_empty() {
            String::new()
        } else {
            let mut fh = String::from(hdr::FAILURE_HISTORY);
            for record in &records {
                fh.push('\n');
                fh.push_str(record.render().as_str());
            }

            // Identify repeated failures — group by (handle, category) so
            // the same task failing with different categories is not
            // flagged.
            let mut handle_counts: std::collections::HashMap<_, usize> =
                std::collections::HashMap::new();
            for record in &records {
                *handle_counts.entry(record.repeat_key()).or_insert(0) += 1;
            }
            let repeated: Vec<_> = handle_counts
                .into_iter()
                .filter(|(_, count)| *count > 1)
                .collect();
            if !repeated.is_empty() {
                fh.push_str(&format!("\n\n{}", hdr::OBSERVED_PATTERNS));
                for ((handle, category), count) in &repeated {
                    fh.push_str(&format!(
                        "\n- \"{}\" has failed {} times with [{}]{}",
                        handle.as_str(),
                        count,
                        category,
                        hdr::REPEATED_FAILURE_SUFFIX,
                    ));
                }
            }
            fh.push_str("\n\n");
            fh
        };

        // Urgency header
        let urgency = if self.iteration + 1 >= max_iterations {
            hdr::FINAL_ATTEMPT.to_string()
        } else {
            String::new()
        };

        let succeeded = self.previous_plan.completed_count();
        let total = self.previous_plan.tasks.len();
        let iteration_str = self.iteration.to_string();
        let max_iter_str = max_iterations.to_string();
        let succeeded_str = succeeded.to_string();
        let total_str = total.to_string();

        let reuse_guidance = if has_failed_tasks && succeeded > 0 {
            super::prompt_constants::guidance::RESULT_FORWARDING
        } else {
            ""
        };

        // The goal line is the pinned original user query (section 1.2); a
        // context with no pinned goal falls back to the plan goal.
        let goal = self
            .pinned_goal
            .as_ref()
            .map(PinnedGoal::as_str)
            .unwrap_or(&self.previous_plan.goal);

        render_continuation_prompt(&ContinuationVars {
            iteration: &iteration_str,
            max_iterations: &max_iter_str,
            urgency: &urgency,
            succeeded: &succeeded_str,
            total: &total_str,
            goal,
            completed_section: &completed_section,
            blocked_section: &blocked_section,
            redesign_section: &redesign_section,
            failure_section: &failure_section,
            failure_history: &failure_history,
            reuse_guidance,
        })
    }
}

/// Collect artifact inventory refs from pre-loaded traces.
///
/// Always rendered in the continuation prompt so the coordinator has a
/// deterministic list of filenames available via `read_artifact`
/// (`docs/redesign/ARCHITECTURE.md` section 1.4).
fn artifact_refs(
    traces: Option<&Vec<super::persistence::ToolTraceEntry>>,
) -> Vec<super::context::ArtifactRef> {
    use super::persistence::ToolOutcome;

    traces
        .into_iter()
        .flatten()
        .filter_map(|t| match (&t.artifact_filename, &t.outcome) {
            (Some(filename), ToolOutcome::Success { output_bytes }) => {
                super::context::ArtifactRef::new(filename, *output_bytes).ok()
            }
            (_, ToolOutcome::Success { .. } | ToolOutcome::Error { .. }) => None,
        })
        .collect()
}

/// Render verbose tool chain + artifact ref lines from pre-loaded traces.
///
/// Gated by `show_tool_reasoning_in_continuation`. Includes tool names,
/// durations, reasoning snippets, and error details.
fn render_tool_chain_lines(
    traces: Option<&Vec<super::persistence::ToolTraceEntry>>,
) -> Vec<String> {
    use super::persistence::ToolOutcome;

    let traces = match traces.filter(|v| !v.is_empty()) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let parts: Vec<String> = traces
        .iter()
        .map(|t| {
            let duration = format!("{:.1}s", t.duration_ms as f64 / 1000.0);
            match &t.outcome {
                ToolOutcome::Success { .. } => {
                    if t.reasoning.is_empty() {
                        format!("{} ({})", t.tool, duration)
                    } else {
                        let r = truncate_reasoning(&t.reasoning, 100);
                        format!("{} ({}, \"{}\")", t.tool, duration, r)
                    }
                }
                ToolOutcome::Error { message } => {
                    format!("{} ({}, FAILED: {})", t.tool, duration, message)
                }
            }
        })
        .collect();

    vec![format!("Tool chain: {}", parts.join(" → "))]
}

fn truncate_reasoning(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}…", truncated)
    }
}

/// Outcome returned by `run_iteration` to drive the orchestration loop.
///
/// Errors bubble via `Result` — this enum carries only the success variants.
#[allow(clippy::large_enum_variant)]
pub(crate) enum IterationOutcome {
    /// The iteration produced a final answer; the loop should break.
    FinalResult(String),
    /// The iteration requires another pass; swap in the new plan and continue.
    Continue {
        new_plan: Plan,
        previous_context: Option<IterationContext>,
    },
}

#[cfg(test)]
mod tests {
    use super::super::context::{ErrorPreview, FailureHandle, PinnedGoal};
    use super::*;

    #[test]
    fn test_plan_creation() {
        let mut plan = Plan::new("Test goal");
        assert_eq!(plan.goal, "Test goal");
        assert!(plan.tasks.is_empty());

        plan.add_task(Task::new(0, "First task", "Test the first functionality"));
        plan.add_task(Task::new(1, "Second task", "Build on first task").with_dependency(0));

        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.pending_count(), 2);
    }

    #[test]
    fn test_task_dependencies() {
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "Task A", "Initial task"));
        plan.add_task(Task::new(1, "Task B", "Depends on A").with_dependency(0));
        plan.add_task(Task::new(2, "Task C", "Depends on A and B").with_dependencies([0, 1]));

        // Only task 0 should be ready initially
        let ready = plan.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, 0);

        // Complete task 0
        plan.get_task_mut(0).unwrap().complete("Done");

        // Now task 1 should be ready
        let ready = plan.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, 1);

        // Complete task 1
        plan.get_task_mut(1).unwrap().complete("Done");

        // Now task 2 should be ready
        let ready = plan.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, 2);
    }

    #[test]
    fn test_ready_tasks_excludes_blocked_by_failure() {
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "Task A", "Initial task"));
        plan.add_task(Task::new(1, "Task B", "Depends on A").with_dependency(0));
        plan.add_task(Task::new(2, "Task C", "Independent task"));

        // Fail task 0
        plan.get_task_mut(0)
            .unwrap()
            .fail("Something went wrong", FailureCategory::AgentError);

        // Task 1 should NOT be ready (dependency failed)
        // Task 2 should be ready (no dependencies)
        let ready = plan.ready_tasks();
        assert_eq!(ready.len(), 1, "Only independent task should be ready");
        assert_eq!(ready[0].id, 2);
    }

    #[test]
    fn test_blocked_tasks_with_failed_dependency() {
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "Task A", "Initial task"));
        plan.add_task(Task::new(1, "Task B", "Depends on A").with_dependency(0));
        plan.add_task(Task::new(2, "Task C", "Independent task"));

        // Initially no blocked tasks
        assert!(
            plan.blocked_tasks().is_empty(),
            "No tasks should be blocked initially"
        );

        // Fail task 0
        plan.get_task_mut(0)
            .unwrap()
            .fail("Something went wrong", FailureCategory::AgentError);

        // Task 1 should now be blocked
        let blocked = plan.blocked_tasks();
        assert_eq!(blocked.len(), 1, "One task should be blocked");
        assert_eq!(
            blocked[0].id, 1,
            "Task 1 should be blocked by failed task 0"
        );
    }

    #[test]
    fn test_blocked_tasks_transitive_dependency() {
        // Test: A -> B -> C, where A fails
        // Both B and C depend (transitively) on A
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "Task A", "Initial task"));
        plan.add_task(Task::new(1, "Task B", "Depends on A").with_dependency(0));
        plan.add_task(Task::new(2, "Task C", "Depends on B").with_dependency(1));

        // Fail task 0
        plan.get_task_mut(0)
            .unwrap()
            .fail("Error", FailureCategory::AgentError);

        // Only task 1 is immediately blocked (direct dependency)
        // Task 2 is not blocked yet because its direct dependency (task 1) hasn't failed
        let blocked = plan.blocked_tasks();
        assert_eq!(blocked.len(), 1, "Only direct dependents are blocked");
        assert_eq!(blocked[0].id, 1);

        // No tasks should be ready
        assert!(
            plan.ready_tasks().is_empty(),
            "No tasks ready when chain is broken"
        );
    }

    #[test]
    fn test_plan_finished() {
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "Task A", "First task"));
        plan.add_task(Task::new(1, "Task B", "Second task"));

        assert!(!plan.is_finished());

        plan.get_task_mut(0).unwrap().complete("Done");
        assert!(!plan.is_finished());

        plan.get_task_mut(1)
            .unwrap()
            .fail("Error", FailureCategory::AgentError);
        assert!(plan.is_finished()); // All tasks either complete or failed
    }

    #[test]
    fn test_task_status_display() {
        assert_eq!(TaskStatus::Pending.to_string(), "pending");
        assert_eq!(TaskStatus::Running.to_string(), "running");
        assert_eq!(TaskStatus::Complete.to_string(), "complete");
        assert_eq!(TaskStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn test_task_worker_assignment() {
        // Task without worker
        let task = Task::new(0, "Generic task", "Test rationale");
        assert!(task.worker.is_none());

        // Task with worker via builder
        let task = Task::new(1, "Operations task", "Ops rationale").with_worker("operations");
        assert_eq!(task.worker, Some("operations".to_string()));

        // Chained builders
        let task = Task::new(2, "Dependent ops task", "Depends on previous")
            .with_dependency(1)
            .with_worker("operations");
        assert_eq!(task.dependencies, vec![1]);
        assert_eq!(task.worker, Some("operations".to_string()));
    }

    #[test]
    fn test_task_serialization_with_worker() {
        // Task with worker should serialize the field
        let task = Task::new(0, "Test", "Test rationale").with_worker("operations");
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"worker\":\"operations\""));
        assert!(json.contains("\"rationale\":\"Test rationale\""));

        // Task without worker should omit the worker field but include rationale
        let task = Task::new(0, "Test", "Another rationale");
        let json = serde_json::to_string(&task).unwrap();
        assert!(!json.contains("worker"));
        assert!(json.contains("\"rationale\":\"Another rationale\""));
    }

    #[test]
    fn test_task_deserialization_with_worker() {
        // JSON with worker and rationale
        let json = r#"{"id":0,"description":"Test","dependencies":[],"status":"pending","worker":"operations","rationale":"Test rationale"}"#;
        let task: Task = serde_json::from_str(json).unwrap();
        assert_eq!(task.worker, Some("operations".to_string()));
        assert_eq!(task.rationale, "Test rationale");

        // JSON without worker but with rationale
        let json = r#"{"id":0,"description":"Test","dependencies":[],"status":"pending","rationale":"Required rationale"}"#;
        let task: Task = serde_json::from_str(json).unwrap();
        assert!(task.worker.is_none());
        assert_eq!(task.rationale, "Required rationale");
    }

    #[test]
    fn test_task_rationale_required() {
        // Task must have a rationale
        let task = Task::new(0, "Test task", "This explains why the task exists");
        assert_eq!(task.rationale, "This explains why the task exists");
    }

    // ========================================================================
    // TaskState serde backward-compat tests
    // ========================================================================

    #[test]
    fn test_task_serde_roundtrip_pending() {
        let task = Task::new(0, "Fetch data", "test rationale");
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains(r#""status":"pending"#));
        assert!(!json.contains("result"));
        assert!(!json.contains("error"));
        let roundtripped: Task = serde_json::from_str(&json).unwrap();
        assert!(matches!(roundtripped.state, TaskState::Pending));
    }

    #[test]
    fn test_task_serde_roundtrip_complete() {
        let mut task = Task::new(0, "Fetch data", "test rationale");
        task.complete("42");
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains(r#""status":"complete"#));
        assert!(json.contains(r#""result":"42"#));
        assert!(!json.contains("error"));
        let roundtripped: Task = serde_json::from_str(&json).unwrap();
        let TaskState::Complete { ref result } = roundtripped.state else {
            panic!("expected Complete");
        };
        assert_eq!(result, "42");
    }

    #[test]
    fn test_task_serde_roundtrip_failed() {
        let mut task = Task::new(0, "Fetch data", "test rationale");
        task.fail("timed out", FailureCategory::AgentTimeout);
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains(r#""status":"failed"#));
        assert!(json.contains(r#""error":"timed out"#));
        assert!(json.contains(r#""failure_category":"agent_timeout"#));
        let roundtripped: Task = serde_json::from_str(&json).unwrap();
        let TaskState::Failed {
            ref error,
            category,
        } = roundtripped.state
        else {
            panic!("expected Failed");
        };
        assert_eq!(error, "timed out");
        assert_eq!(category, FailureCategory::AgentTimeout);
    }

    #[test]
    fn test_task_deserialize_legacy_null_fields() {
        let json = r#"{"id":0,"description":"Test","dependencies":[],"status":"pending","result":null,"error":null,"rationale":"test"}"#;
        let task: Task = serde_json::from_str(json).unwrap();
        assert!(matches!(task.state, TaskState::Pending));
    }

    #[test]
    fn test_task_deserialize_legacy_complete_with_null_error() {
        let json = r#"{"id":0,"description":"Test","dependencies":[],"status":"complete","result":"data","error":null,"rationale":"test"}"#;
        let task: Task = serde_json::from_str(json).unwrap();
        let TaskState::Complete { ref result } = task.state else {
            panic!("expected Complete");
        };
        assert_eq!(result, "data");
    }

    #[test]
    fn test_task_deserialize_legacy_failed_without_category() {
        let json = r#"{"id":0,"description":"Test","dependencies":[],"status":"failed","result":null,"error":"boom","rationale":"test"}"#;
        let task: Task = serde_json::from_str(json).unwrap();
        let TaskState::Failed {
            ref error,
            category,
        } = task.state
        else {
            panic!("expected Failed");
        };
        assert_eq!(error, "boom");
        assert_eq!(category, FailureCategory::AgentError);
    }

    // ========================================================================
    // FailureSummary tests
    // ========================================================================

    #[test]
    fn test_failure_summary_gaps_as_bullets_empty() {
        let fs = FailureSummary::default();
        assert_eq!(fs.gaps_as_bullets(), "- No specific gaps identified");
    }

    #[test]
    fn test_failure_summary_gaps_as_bullets_with_gaps() {
        let fs = FailureSummary {
            reasoning: "Missing details".into(),
            gaps: vec!["Missing API details".into(), "No error handling".into()],
        };

        let bullets = fs.gaps_as_bullets();
        assert!(bullets.contains("- Missing API details"));
        assert!(bullets.contains("- No error handling"));
    }

    // ========================================================================
    // IterationContext tests
    // ========================================================================

    #[test]
    fn test_iteration_context_creation() {
        let mut plan = Plan::new("Test goal");
        plan.add_task(Task::new(0, "Task 1", "First task"));

        let fs = FailureSummary {
            reasoning: "Incomplete response".into(),
            gaps: vec!["Missing context".into()],
        };

        let ctx = IterationContext::new(1, plan.clone(), Some(fs), vec![], HashMap::new());

        assert_eq!(ctx.iteration, 1);
        assert_eq!(ctx.previous_plan.goal, "Test goal");
        assert_eq!(
            ctx.failure_summary.as_ref().unwrap().reasoning,
            "Incomplete response"
        );
        assert!(ctx.failure_history.is_empty());
    }

    #[test]
    fn test_iteration_context_continuation_prompt() {
        let mut plan = Plan::new("Investigate the issue");
        let mut task = Task::new(0, "Gather logs", "Get system logs");
        task.complete("Here are the logs...".to_string());
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Response lacks detail".into(),
            gaps: vec!["Missing root cause".into(), "No remediation steps".into()],
        };

        let ctx = IterationContext::new(1, plan, Some(fs), vec![], HashMap::new())
            .with_pinned_goal(PinnedGoal::new("Investigate the issue").expect("non-empty query"));
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        // Verify key sections are present
        assert!(prompt.contains("ITERATION 1 of 3"));
        assert!(
            prompt.contains("Goal (verbatim from the original request): Investigate the issue")
        );
        // No evaluator vocabulary — no "Quality Score"
        assert!(!prompt.contains("Quality Score"));
        assert!(prompt.contains("COMPLETED TASKS"));
        // Evidence-framed entry: correlation label plus the worker's own
        // result; the coordinator task description is not replayed.
        assert!(prompt.contains("- Task 0\n    Here are the logs..."));
        assert!(!prompt.contains("Gather logs"));
        assert!(prompt.contains("FAILURE SUMMARY:"));
        assert!(prompt.contains("Response lacks detail"));
        assert!(prompt.contains("AREAS NEEDING ATTENTION:"));
        assert!(prompt.contains("- Missing root cause"));
        assert!(prompt.contains("- No remediation steps"));
        // Routing tool block must be present
        assert!(prompt.contains("respond_directly"));
        assert!(prompt.contains("create_plan"));
        assert!(prompt.contains("request_clarification"));
        // The artifact-read directive is integral to the template
        assert!(prompt.contains("read_artifact"));
    }

    #[test]
    fn test_iteration_context_continuation_prompt_clean_success() {
        // Clean-success path: failure_summary = None means no FAILURE SUMMARY
        // section is rendered.
        let mut plan = Plan::new("Simple query");
        let mut task = Task::new(0, "Execute", "Run query");
        task.complete("Done".to_string());
        plan.add_task(task);

        let ctx = IterationContext::new(2, plan, None, vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(prompt.contains("ITERATION 2 of 3"));
        assert!(prompt.contains("COMPLETED TASKS"));
        // No failure section on the success path
        assert!(!prompt.contains("FAILURE SUMMARY:"));
        assert!(!prompt.contains("AREAS NEEDING ATTENTION:"));
    }

    #[test]
    fn test_continuation_prompt_with_failure_history() {
        let mut plan = Plan::new("Debug the issue");
        let mut task = Task::new(0, "Gather logs", "Collect logs");
        task.fail("Timeout contacting service", FailureCategory::AgentTimeout);
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Task failed".into(),
            gaps: vec![],
        };
        let failures = vec![FailedTaskRecord {
            description: "Gather logs".to_string(),
            error: "Timeout contacting service".to_string(),
            iteration: 1,
            worker: Some("operations".to_string()),
            category: FailureCategory::AgentTimeout,
        }];

        let ctx = IterationContext::new(1, plan, Some(fs), failures, HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(prompt.contains("FAILURE HISTORY:"));
        assert!(prompt.contains("Iteration 1: \"Gather logs\""));
        assert!(prompt.contains("(worker: operations)"));
        assert!(prompt.contains("Timeout contacting service"));
        // Single occurrence — no observed-patterns block
        assert!(!prompt.contains("OBSERVED PATTERNS:"));
    }

    #[test]
    fn test_continuation_prompt_with_repeated_failures() {
        let mut plan = Plan::new("Debug the issue");
        let mut task = Task::new(0, "Fetch data", "Get data");
        task.fail("Connection refused", FailureCategory::AgentError);
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Tasks failed".into(),
            gaps: vec![],
        };
        let failures = vec![
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "Timeout".to_string(),
                iteration: 1,
                worker: None,
                category: FailureCategory::AgentError,
            },
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "Connection refused".to_string(),
                iteration: 2,
                worker: None,
                category: FailureCategory::AgentError,
            },
        ];

        let ctx = IterationContext::new(2, plan, Some(fs), failures, HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(prompt.contains("FAILURE HISTORY:"));
        assert!(prompt.contains("OBSERVED PATTERNS:"));
        assert!(prompt.contains("\"Fetch data\" has failed 2 times"));
        assert!(prompt.contains("fundamentally different approach"));
    }

    #[test]
    fn test_continuation_prompt_inlines_small_result() {
        let mut plan = Plan::new("Test inline");
        let mut task = Task::new(0, "Big result", "Produce output");
        // 600-char result has no artifact footer → inlined fully
        let long_result = "x".repeat(600);
        task.complete(long_result.clone());
        plan.add_task(task);

        let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(prompt.contains("COMPLETED TASKS"));
        // Full result inlined — no truncation when no artifact exists
        assert!(prompt.contains(&"x".repeat(600)));
    }

    #[test]
    fn test_continuation_prompt_preserves_artifact_footer() {
        // Regression guard: when a result contains the artifact footer
        // appended by maybe_create_artifact, the truncation must preserve
        // the footer so the coordinator can call read_artifact.
        let mut plan = Plan::new("Test artifact footer");
        let mut task = Task::new(0, "Big result", "Produce output");
        // 600 'x' chars + the artifact footer (past 500-byte budget)
        let body = "x".repeat(600);
        let long_result = format!(
            "{body}\n\n[Full result (12345 chars) saved to artifact: task-0-sre-iter-1-result.txt]"
        );
        task.complete(long_result);
        plan.add_task(task);

        let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        // The body is truncated but the artifact footer survives.
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("saved to artifact: task-0-sre-iter-1-result.txt"));
        assert!(prompt.contains("12345 chars"));
    }

    #[test]
    fn test_continuation_prompt_summary_preserves_artifact_footer() {
        use super::super::tools::submit_result::Confidence;

        let mut plan = Plan::new("Test summary + artifact footer");
        let mut task = Task::new(0, "Big result", "Produce output");
        let body = "x".repeat(600);
        let long_result = format!(
            "{body}\n\n[Full result (12345 chars) saved to artifact: task-0-sre-iter-1-result.txt]"
        );
        task.complete(long_result);
        task.structured_output = Some(StructuredTaskOutput {
            summary: "Found 47 error groups across 3 services".to_string(),
            confidence: Confidence::High,
        });
        plan.add_task(task);

        let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(prompt.contains("Found 47 error groups"));
        assert!(prompt.contains("saved to artifact: task-0-sre-iter-1-result.txt"));
        assert!(prompt.contains("(confidence: high)"));
    }

    #[test]
    fn test_continuation_prompt_urgency_final_attempt() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Task", "Do it");
        task.fail("error", FailureCategory::AgentError);
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        // iteration=2, max=3 → next would be iteration 3 = max, so FINAL ATTEMPT
        let ctx = IterationContext::new(2, plan, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(prompt.contains("(FINAL ATTEMPT)"));
    }

    #[test]
    fn test_continuation_prompt_no_urgency_early_iteration() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Task", "Do it");
        task.fail("error", FailureCategory::AgentError);
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(!prompt.contains("FINAL ATTEMPT"));
    }

    #[test]
    fn test_continuation_prompt_result_forwarding_guidance() {
        let guidance_marker = "Workers will receive relevant prior-iteration worker evidence";

        // Mixed (completed + failed): guidance present
        let mut mixed_plan = Plan::new("Goal");
        let mut completed = Task::new(0, "Completed task", "Done");
        completed.complete("Some result");
        mixed_plan.add_task(completed);
        let mut failed = Task::new(1, "Failed task", "Broken");
        failed.fail("boom", FailureCategory::AgentError);
        mixed_plan.add_task(failed);
        let fs = FailureSummary {
            reasoning: "Partial".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, mixed_plan, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);
        assert!(
            prompt.contains(guidance_marker),
            "result forwarding guidance should appear on mixed success/failure"
        );

        // All completed: guidance absent
        let mut all_ok = Plan::new("Goal");
        let mut t = Task::new(0, "Completed task", "Done");
        t.complete("Some result");
        all_ok.add_task(t);
        let ctx = IterationContext::new(1, all_ok, None, vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);
        assert!(
            !prompt.contains(guidance_marker),
            "result forwarding guidance should NOT appear on clean success"
        );

        // All failed: guidance absent
        let mut all_fail = Plan::new("Goal");
        let mut t = Task::new(0, "Failed task", "Tried");
        t.fail("error", FailureCategory::AgentError);
        all_fail.add_task(t);
        let fs = FailureSummary {
            reasoning: "All failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, all_fail, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);
        assert!(
            !prompt.contains(guidance_marker),
            "result forwarding guidance should NOT appear when all failed"
        );
    }

    #[test]
    fn test_continuation_prompt_mixed_categories() {
        let mut plan = Plan::new("Mixed results");

        let mut completed = Task::new(0, "Completed task", "Worked");
        completed.complete("Good result");
        plan.add_task(completed);

        let mut failed = Task::new(1, "Failed task", "Broken");
        failed.fail("Connection refused", FailureCategory::AgentError);
        plan.add_task(failed);

        // Task 2 depends on failed task 1, so it stays Pending (blocked)
        let mut blocked = Task::new(2, "Blocked task", "Waiting");
        blocked.dependencies = vec![1];
        plan.add_task(blocked);

        let fs = FailureSummary {
            reasoning: "Partial success".into(),
            gaps: vec!["Task 1 failed".into()],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        // All three sections should be present, each entry a correlation
        // label plus evidence — no coordinator task-description replay.
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("- Task 0\n    Good result"));
        assert!(prompt.contains("FAILED TASKS"));
        assert!(prompt.contains("- Task 1 -> failed [agent_error]: Connection refused"));
        assert!(prompt.contains("BLOCKED TASKS"));
        assert!(prompt.contains("- Task 2 -> blocked (dependency failed)"));
        assert!(!prompt.contains("Completed task"));
        assert!(!prompt.contains("Failed task"));
        assert!(!prompt.contains("Blocked task"));

        // Verify ordering: completed before blocked before failed (redesign)
        let completed_pos = prompt.find("COMPLETED TASKS").unwrap();
        let blocked_pos = prompt.find("BLOCKED TASKS").unwrap();
        let redesign_pos = prompt.find("FAILED TASKS").unwrap();
        assert!(completed_pos < blocked_pos);
        assert!(blocked_pos < redesign_pos);
    }

    // ========================================================================
    // R3a acceptance: evidence-framed continuation rendering
    // ========================================================================

    // ARCHITECTURE.md section 1.2: the goal line is the verbatim original
    // user query on every iteration, not the coordinator's drifting
    // plan.goal.
    #[test]
    fn continuation_goal_line_is_original_query_across_iterations() {
        let query = "Run Windows 3.11 for Workgroups in a virtual machine using qemu";
        for iteration in 1..=3 {
            let mut plan = Plan::new(format!("Drifted iteration-{iteration} plan goal"));
            let mut task = Task::new(0, "Launch the VM", "advance the goal");
            task.complete("VM launched; VNC on 5901");
            plan.add_task(task);

            let ctx = IterationContext::new(iteration, plan, None, vec![], HashMap::new())
                .with_pinned_goal(PinnedGoal::new(query).expect("non-empty query"));
            let prompt = ctx.build_continuation_prompt(4, false, 2000);

            let goal_line = prompt
                .lines()
                .find(|line| line.starts_with("Goal"))
                .expect("goal line rendered");
            assert_eq!(
                goal_line,
                format!("Goal (verbatim from the original request): {query}"),
                "iteration {iteration}: goal line equals the original user query"
            );
            assert!(
                !prompt.contains("Drifted iteration"),
                "the coordinator's own plan goal must not render"
            );
        }
    }

    // ARCHITECTURE.md section 1.3: per-task entries carry a correlation
    // label and worker evidence only; replaying the coordinator's task
    // description next to that evidence is the confirmed blur mechanism.
    #[test]
    fn completed_entries_carry_no_task_description() {
        let completed_description = "Install QEMU and launch Windows 3.11 with full configuration.";
        let mut plan = Plan::new("goal");
        let mut completed =
            Task::new(0, completed_description, "set up the VM").with_worker("operator");
        completed.complete("Installed qemu-system-i386. VM launched in tmux window 'qemu'.");
        completed.structured_output = Some(StructuredTaskOutput {
            summary: "QEMU running with VNC".into(),
            confidence: super::super::tools::submit_result::Confidence::High,
        });
        plan.add_task(completed);
        let mut failed =
            Task::new(1, "Verify the desktop booted", "confirm boot").with_worker("verifier");
        failed.fail("boom", FailureCategory::AgentError);
        plan.add_task(failed);
        let blocked = Task::new(2, "Correlate results", "correlate")
            .with_dependency(1)
            .with_worker("operator");
        plan.add_task(blocked);

        let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(prompt.contains(
            "- Task 0 (operator, confidence: high)\n    Installed qemu-system-i386. VM launched in tmux window 'qemu'."
        ));
        assert!(prompt.contains("- Task 1 (verifier) -> failed [agent_error]: boom"));
        assert!(prompt.contains("- Task 2 (operator) -> blocked (dependency failed)"));
        for description in [
            completed_description,
            "Verify the desktop booted",
            "Correlate results",
        ] {
            assert!(
                !prompt.contains(description),
                "task description must not render: {description}"
            );
        }
        // R2 gate decision 2: a claim tags a result; it does not replace a
        // result that fits inline.
        assert!(!prompt.contains("QEMU running with VNC"));
    }

    // ARCHITECTURE.md sections 1.4 and 1.5: the artifact inventory and the
    // accumulated failure history survive the evidence reframing.
    #[test]
    fn artifact_inventory_and_failure_history_are_preserved() {
        let mut plan = Plan::new("goal");
        let mut task = Task::new(0, "task", "produce data").with_worker("operator");
        task.complete(format!(
            "{}\n\n[Full result (8123 chars) saved to artifact: task-0-operator-iter-1-result.txt]",
            "x".repeat(100)
        ));
        task.structured_output = Some(StructuredTaskOutput {
            summary: "worker summary of the spilled result".into(),
            confidence: super::super::tools::submit_result::Confidence::High,
        });
        plan.add_task(task);

        let mut traces = HashMap::new();
        let mut entry = make_trace("log_search", "searching", 1000, None);
        entry.artifact_filename = Some("task-0-operator-iter-1-log_search-0-output.txt".into());
        traces.insert(0, vec![entry]);

        let failures = vec![FailedTaskRecord {
            description: "Gather logs".to_string(),
            error: "Timeout contacting service".to_string(),
            iteration: 1,
            worker: Some("operations".to_string()),
            category: FailureCategory::AgentTimeout,
        }];

        let ctx = IterationContext::new(2, plan, None, failures, traces);
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        // Spilled result: attested summary + footer, raw body not replayed.
        assert!(prompt.contains(
            "    worker summary of the spilled result\n    [Full result (8123 chars) saved to artifact: task-0-operator-iter-1-result.txt]"
        ));
        assert!(!prompt.contains(&"x".repeat(100)));
        // Artifact inventory line preserved verbatim.
        assert!(
            prompt.contains(
                "[Artifact: task-0-operator-iter-1-log_search-0-output.txt (1024 bytes)]"
            )
        );
        // Failure history preserved, keyed by handle.
        assert!(prompt.contains("FAILURE HISTORY:"));
        assert!(prompt.contains(
            "- Iteration 1: \"Gather logs\" (worker: operations) - [agent_timeout] Timeout contacting service"
        ));
    }

    // R2 gate decision 4 through the full render: repeat detection groups
    // by the marker-after-cap handle, and the full description never
    // renders.
    #[test]
    fn repeated_failures_group_by_truncated_handle() {
        let long_description = format!(
            "Install QEMU and launch Windows 3.11 with full configuration. {}",
            "Then run the next step. ".repeat(10)
        );
        let record = |iteration| FailedTaskRecord {
            description: long_description.clone(),
            error: "boom".to_string(),
            iteration,
            worker: None,
            category: FailureCategory::AgentError,
        };
        let mut plan = Plan::new("goal");
        let mut task = Task::new(0, "task", "retry");
        task.fail("boom", FailureCategory::AgentError);
        plan.add_task(task);

        let ctx = IterationContext::new(2, plan, None, vec![record(1), record(2)], HashMap::new());
        let prompt = ctx.build_continuation_prompt(4, false, 2000);

        let handle = FailureHandle::from_description(&long_description).expect("non-empty");
        assert_eq!(
            handle.as_str().chars().count(),
            FailureHandle::MAX_CHARS + FailureHandle::TRUNCATION_MARKER.chars().count(),
            "cut handle is the cap plus the marker"
        );
        assert!(prompt.contains("OBSERVED PATTERNS:"));
        assert!(prompt.contains(&format!(
            "- \"{}\" has failed 2 times with [agent_error]",
            handle.as_str()
        )));
        assert!(
            !prompt.contains(&long_description),
            "the full task description never renders"
        );
    }

    // ========================================================================
    // PlanningResponse tests
    // ========================================================================

    #[test]
    fn test_planning_response_direct_serde() {
        let response = PlanningResponse::Direct {
            response: "The answer is 42.".to_string(),
            routing_rationale: "Simple arithmetic".to_string(),
            response_summary: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"response_type\":\"direct\""));
        assert!(json.contains("\"routing_rationale\":\"Simple arithmetic\""));

        let deserialized: PlanningResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlanningResponse::Direct {
                response,
                routing_rationale,
                ..
            } => {
                assert_eq!(response, "The answer is 42.");
                assert_eq!(routing_rationale, "Simple arithmetic");
            }
            other => panic!("Expected Direct, got {:?}", other),
        }
    }

    #[test]
    fn test_planning_response_clarification_serde() {
        let response = PlanningResponse::Clarification {
            question: "Which service?".to_string(),
            options: Some(vec!["API".to_string(), "Worker".to_string()]),
            routing_rationale: "Ambiguous".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"response_type\":\"clarification\""));

        let deserialized: PlanningResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlanningResponse::Clarification {
                question, options, ..
            } => {
                assert_eq!(question, "Which service?");
                assert_eq!(options.unwrap(), vec!["API", "Worker"]);
            }
            other => panic!("Expected Clarification, got {:?}", other),
        }
    }

    #[test]
    fn test_planning_response_clarification_no_options() {
        let response = PlanningResponse::Clarification {
            question: "What do you mean?".to_string(),
            options: None,
            routing_rationale: "Vague".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        // options should be absent (skip_serializing_if)
        assert!(!json.contains("\"options\""));

        let deserialized: PlanningResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlanningResponse::Clarification { options, .. } => {
                assert!(options.is_none());
            }
            other => panic!("Expected Clarification, got {:?}", other),
        }
    }

    #[test]
    fn test_into_plan_from_direct_returns_none() {
        let response = PlanningResponse::Direct {
            response: "42".to_string(),
            routing_rationale: "Simple".to_string(),
            response_summary: None,
        };
        assert!(response.into_plan().is_none());
    }

    #[test]
    fn test_into_plan_from_clarification_returns_none() {
        let response = PlanningResponse::Clarification {
            question: "What?".to_string(),
            options: None,
            routing_rationale: "Unclear".to_string(),
        };
        assert!(response.into_plan().is_none());
    }

    #[test]
    fn test_routing_rationale_accessor() {
        let direct = PlanningResponse::Direct {
            response: "x".to_string(),
            routing_rationale: "reason_d".to_string(),
            response_summary: None,
        };
        assert_eq!(direct.routing_rationale(), "reason_d");

        let orch = PlanningResponse::StepsPlan {
            goal: "g".to_string(),
            steps: vec![StepInput::LeafTask {
                task: "t".to_string(),
                worker: None,
            }],
            routing_rationale: "reason_o".to_string(),
            planning_summary: "summary".to_string(),
        };
        assert_eq!(orch.routing_rationale(), "reason_o");

        let clar = PlanningResponse::Clarification {
            question: "q".to_string(),
            options: None,
            routing_rationale: "reason_c".to_string(),
        };
        assert_eq!(clar.routing_rationale(), "reason_c");
    }

    #[test]
    fn test_flat_plan_parsing_unchanged() {
        let response = PlanningResponse::StepsPlan {
            goal: "Simple query".to_string(),
            steps: vec![StepInput::LeafTask {
                task: "Do thing".to_string(),
                worker: None,
            }],
            routing_rationale: "Needs tool".to_string(),
            planning_summary: "Just do it".to_string(),
        };

        let plan = response.into_plan().unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].description, "Do thing");
    }

    // ========================================================================
    // StepInput / flatten_steps tests
    // ========================================================================

    #[test]
    fn test_flatten_sequential_two_steps() {
        let steps = vec![
            StepInput::LeafTask {
                task: "Compute mean of [10,20,30]".into(),
                worker: Some("statistics".into()),
            },
            StepInput::LeafTask {
                task: "Multiply the result by 3".into(),
                worker: Some("arithmetic".into()),
            },
        ];
        let tasks = flatten_steps(&steps).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, 0);
        assert!(tasks[0].dependencies.is_empty());
        assert_eq!(tasks[0].worker.as_deref(), Some("statistics"));
        assert_eq!(tasks[1].id, 1);
        assert_eq!(tasks[1].dependencies, vec![0]);
        assert_eq!(tasks[1].worker.as_deref(), Some("arithmetic"));
    }

    #[test]
    fn test_flatten_parallel_then_sequential() {
        let steps = vec![
            StepInput::ParallelGroup {
                items: vec![
                    StepInput::LeafTask {
                        task: "Compute median".into(),
                        worker: Some("statistics".into()),
                    },
                    StepInput::LeafTask {
                        task: "Compute sin(45)".into(),
                        worker: Some("trigonometry".into()),
                    },
                ],
            },
            StepInput::LeafTask {
                task: "Multiply the two results".into(),
                worker: Some("arithmetic".into()),
            },
        ];
        let tasks = flatten_steps(&steps).unwrap();
        assert_eq!(tasks.len(), 3);
        // Both parallel tasks have no deps
        assert!(tasks[0].dependencies.is_empty());
        assert!(tasks[1].dependencies.is_empty());
        // Third task depends on both parallel exits
        assert_eq!(tasks[2].dependencies, vec![0, 1]);
    }

    #[test]
    fn test_flatten_recursive_parallel_with_subchain() {
        // parallel { [Get A -> Transform A], Get B } -> Combine
        let steps = vec![
            StepInput::ParallelGroup {
                items: vec![
                    StepInput::SubChain {
                        steps: vec![
                            StepInput::LeafTask {
                                task: "Get A".into(),
                                worker: Some("ops".into()),
                            },
                            StepInput::LeafTask {
                                task: "Transform A".into(),
                                worker: Some("ops".into()),
                            },
                        ],
                    },
                    StepInput::LeafTask {
                        task: "Get B".into(),
                        worker: Some("ops".into()),
                    },
                ],
            },
            StepInput::LeafTask {
                task: "Combine".into(),
                worker: Some("ops".into()),
            },
        ];
        let tasks = flatten_steps(&steps).unwrap();
        assert_eq!(tasks.len(), 4);
        // Get A: id=0, deps=[]
        assert_eq!(tasks[0].id, 0);
        assert!(tasks[0].dependencies.is_empty());
        // Transform A: id=1, deps=[0]
        assert_eq!(tasks[1].id, 1);
        assert_eq!(tasks[1].dependencies, vec![0]);
        // Get B: id=2, deps=[]
        assert_eq!(tasks[2].id, 2);
        assert!(tasks[2].dependencies.is_empty());
        // Combine: id=3, deps=[1, 2] (exits of both branches)
        assert_eq!(tasks[3].id, 3);
        assert_eq!(tasks[3].dependencies, vec![1, 2]);
    }

    #[test]
    fn test_flatten_empty_steps_error() {
        let result = flatten_steps(&[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn test_flatten_empty_parallel_error() {
        let steps = vec![StepInput::ParallelGroup { items: vec![] }];
        let result = flatten_steps(&steps);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty parallel"));
    }

    #[test]
    fn test_flatten_depth_exceeded() {
        // Depth 0: top-level, depth 1: parallel, depth 2: sub-chain,
        // depth 3: nested parallel inside sub-chain -> should fail
        let steps = vec![StepInput::ParallelGroup {
            items: vec![StepInput::SubChain {
                steps: vec![StepInput::ParallelGroup {
                    items: vec![StepInput::LeafTask {
                        task: "too deep".into(),
                        worker: None,
                    }],
                }],
            }],
        }];
        let result = flatten_steps(&steps);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nesting depth"));
    }

    #[test]
    fn test_flatten_single_step() {
        let steps = vec![StepInput::LeafTask {
            task: "Just one thing".into(),
            worker: None,
        }];
        let tasks = flatten_steps(&steps).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 0);
        assert!(tasks[0].dependencies.is_empty());
        assert!(tasks[0].worker.is_none());
    }

    #[test]
    fn test_flatten_empty_subchain_error() {
        let steps = vec![StepInput::ParallelGroup {
            items: vec![StepInput::SubChain { steps: vec![] }],
        }];
        let result = flatten_steps(&steps);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty sub-chain"));
    }

    #[test]
    fn test_step_input_deserialize_sequential() {
        let json = r#"[
            {"type": "task", "task": "Compute mean", "worker": "stats"},
            {"type": "task", "task": "Multiply result", "worker": "math"}
        ]"#;
        let steps: Vec<StepInput> = serde_json::from_str(json).unwrap();
        assert_eq!(steps.len(), 2);
        match &steps[0] {
            StepInput::LeafTask { task, worker } => {
                assert_eq!(task, "Compute mean");
                assert_eq!(worker.as_deref(), Some("stats"));
            }
            other => panic!("Expected LeafTask, got {:?}", other),
        }
    }

    #[test]
    fn test_step_input_deserialize_parallel() {
        let json = r#"[
            {"type": "parallel", "items": [
                {"type": "task", "task": "A", "worker": "w1"},
                {"type": "task", "task": "B", "worker": "w2"}
            ]},
            {"type": "task", "task": "C"}
        ]"#;
        let steps: Vec<StepInput> = serde_json::from_str(json).unwrap();
        assert_eq!(steps.len(), 2);
        match &steps[0] {
            StepInput::ParallelGroup { items } => {
                assert_eq!(items.len(), 2);
            }
            other => panic!("Expected ParallelGroup, got {:?}", other),
        }
    }

    #[test]
    fn test_step_input_deserialize_recursive() {
        let json = r#"[
            {"type": "parallel", "items": [
                {"type": "chain", "steps": [
                    {"type": "task", "task": "Get A"},
                    {"type": "task", "task": "Transform A"}
                ]},
                {"type": "task", "task": "Get B"}
            ]},
            {"type": "task", "task": "Combine"}
        ]"#;
        let steps: Vec<StepInput> = serde_json::from_str(json).unwrap();
        let tasks = flatten_steps(&steps).unwrap();
        assert_eq!(tasks.len(), 4);
        assert_eq!(tasks[3].dependencies, vec![1, 2]);
    }

    #[test]
    fn test_steps_plan_into_plan() {
        let response = PlanningResponse::StepsPlan {
            goal: "Test goal".into(),
            steps: vec![
                StepInput::LeafTask {
                    task: "Step 1".into(),
                    worker: Some("w1".into()),
                },
                StepInput::LeafTask {
                    task: "Step 2".into(),
                    worker: Some("w2".into()),
                },
            ],
            routing_rationale: "Needs orchestration".into(),
            planning_summary: "Two sequential steps".into(),
        };
        let plan = response.into_plan().unwrap();
        assert_eq!(plan.goal, "Test goal");
        assert_eq!(plan.tasks.len(), 2);
        assert!(plan.tasks[0].dependencies.is_empty());
        assert_eq!(plan.tasks[1].dependencies, vec![0]);
    }

    #[test]
    fn test_steps_plan_variant_name() {
        let response = PlanningResponse::StepsPlan {
            goal: "g".into(),
            steps: vec![],
            routing_rationale: "r".into(),
            planning_summary: "s".into(),
        };
        assert_eq!(response.variant_name(), "StepsPlan");
    }

    // -----------------------------------------------------------------------
    // Serde roundtrip tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_leaf_task_serde_roundtrip() {
        let json = r#"{"type": "task", "task": "Fresh work", "worker": "ops"}"#;
        let step: StepInput = serde_json::from_str(json).unwrap();
        match &step {
            StepInput::LeafTask { task, worker } => {
                assert_eq!(task, "Fresh work");
                assert_eq!(worker.as_deref(), Some("ops"));
            }
            other => panic!("Expected LeafTask, got {:?}", other),
        }
        let serialized = serde_json::to_string(&step).unwrap();
        let deserialized: StepInput = serde_json::from_str(&serialized).unwrap();
        assert_eq!(step, deserialized);
    }

    #[test]
    fn test_failed_task_record_serde_roundtrip() {
        let record = FailedTaskRecord {
            description: "Divide numbers".into(),
            error: "Division by zero".into(),
            iteration: 2,
            worker: Some("math".into()),
            category: FailureCategory::AgentError,
        };
        let json = serde_json::to_string(&record).unwrap();
        let deserialized: FailedTaskRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.description, "Divide numbers");
        assert_eq!(deserialized.error, "Division by zero");
        assert_eq!(deserialized.iteration, 2);
        assert_eq!(deserialized.worker, Some("math".into()));
    }

    #[test]
    fn test_iteration_context_serde_roundtrip() {
        let mut plan = Plan::new("Test");
        let mut t = Task::new(0, "Task 0", "reason");
        t.complete("result".to_string());
        plan.add_task(t);

        let ctx = IterationContext::new(
            1,
            plan,
            Some(FailureSummary {
                reasoning: "Decent".into(),
                gaps: vec!["Missing detail".into()],
            }),
            vec![FailedTaskRecord {
                description: "bad task".into(),
                error: "oops".into(),
                iteration: 1,
                worker: None,
                category: FailureCategory::AgentError,
            }],
            HashMap::new(),
        )
        .with_pinned_goal(PinnedGoal::new("original user query").unwrap());
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: IterationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.iteration, 1);
        assert_eq!(
            deserialized.pinned_goal.as_ref().map(|g| g.as_str()),
            Some("original user query"),
            "pinned goal survives the round trip"
        );
        let fs = deserialized
            .failure_summary
            .as_ref()
            .expect("failure_summary present");
        assert_eq!(fs.reasoning, "Decent");
        assert_eq!(fs.gaps, vec!["Missing detail".to_string()]);
        assert_eq!(deserialized.previous_plan.tasks.len(), 1);
        assert_eq!(deserialized.failure_history.len(), 1);
    }

    #[test]
    fn test_iteration_context_serde_roundtrip_clean_success() {
        // failure_summary=None on the clean-success path
        let mut plan = Plan::new("Test");
        let mut t = Task::new(0, "Task 0", "reason");
        t.complete("result".to_string());
        plan.add_task(t);

        let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: IterationContext = serde_json::from_str(&json).unwrap();
        assert!(deserialized.failure_summary.is_none());
    }

    #[test]
    fn test_continuation_prompt_renders_failure_category() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Gather logs", "Collect logs");
        task.fail("Worker timed out after 30s", FailureCategory::AgentTimeout);
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(
            prompt.contains("[agent_timeout]"),
            "prompt should contain category label: {}",
            prompt
        );
        assert!(prompt.contains("Worker timed out after 30s"));
    }

    #[test]
    fn test_failure_history_includes_category() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Fetch data", "Get data");
        task.fail("Connection refused", FailureCategory::AgentError);
        plan.add_task(task);

        let failures = vec![FailedTaskRecord {
            description: "Fetch data".to_string(),
            error: "Connection refused".to_string(),
            iteration: 1,
            worker: None,
            category: FailureCategory::AgentError,
        }];
        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(2, plan, Some(fs), failures, HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(
            prompt.contains("[agent_error]"),
            "failure history should contain category label: {}",
            prompt
        );
    }

    #[test]
    fn test_long_error_strings_truncated_in_continuation_prompt() {
        let long_error = "x".repeat(5000);
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Fetch data", "Get data");
        task.fail(long_error.clone(), FailureCategory::ContextOverflow);
        plan.add_task(task);

        let failures = vec![FailedTaskRecord {
            description: "Fetch data".to_string(),
            error: long_error,
            iteration: 1,
            worker: None,
            category: FailureCategory::ContextOverflow,
        }];
        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(2, plan, Some(fs), failures, HashMap::new());
        // The legacy width argument is ignored: error previews own their
        // bound (R2 gate decision 6).
        let prompt = ctx.build_continuation_prompt(3, false, 200);

        assert!(
            prompt.contains("[truncated]"),
            "prompt should contain truncation marker: {}",
            prompt
        );
        assert!(
            !prompt.contains(&"x".repeat(ErrorPreview::MAX_CHARS + 1)),
            "errors are cut at the ErrorPreview bound"
        );
        assert!(
            prompt.contains(&"x".repeat(ErrorPreview::MAX_CHARS)),
            "the bounded preview itself renders"
        );
        assert!(
            prompt.contains("[context_overflow]"),
            "category should still be present: {}",
            prompt
        );
    }

    #[test]
    fn test_observed_patterns_group_by_description_and_category() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Fetch data", "Get data");
        task.fail("timeout", FailureCategory::AgentTimeout);
        plan.add_task(task);

        // Same task description, different categories — should NOT trigger pattern
        let failures = vec![
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "timeout".to_string(),
                iteration: 1,
                worker: None,
                category: FailureCategory::AgentTimeout,
            },
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "connection refused".to_string(),
                iteration: 2,
                worker: None,
                category: FailureCategory::AgentError,
            },
        ];
        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(3, plan, Some(fs), failures, HashMap::new());
        let prompt = ctx.build_continuation_prompt(4, false, 2000);

        assert!(
            !prompt.contains("OBSERVED PATTERNS"),
            "different categories for same task should not trigger pattern: {}",
            prompt
        );
    }

    #[test]
    fn test_failure_category_serde_roundtrip() {
        let record = FailedTaskRecord {
            description: "task".into(),
            error: "boom".into(),
            iteration: 1,
            worker: None,
            category: FailureCategory::ContextOverflow,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains("\"context_overflow\""),
            "should serialize as snake_case: {}",
            json
        );
        let deserialized: FailedTaskRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.category, FailureCategory::ContextOverflow);
    }

    #[test]
    fn test_soft_failure_renders_worker_summary() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Analyze logs", "Check logs");
        task.fail(
            "Worker did not call submit_result",
            FailureCategory::SoftFailure,
        );
        task.structured_output = Some(StructuredTaskOutput {
            summary: "Found partial matches but could not correlate across services".into(),
            confidence: super::super::tools::submit_result::Confidence::Low,
        });
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Inconclusive".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(
            prompt.contains("soft_failure"),
            "should contain soft_failure label: {}",
            prompt
        );
        assert!(
            prompt.contains("low confidence"),
            "should show confidence level: {}",
            prompt
        );
        assert!(
            prompt.contains("partial matches"),
            "should include worker summary: {}",
            prompt
        );
    }

    #[test]
    fn test_soft_failure_empty_summary_falls_back_to_bracket_format() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Analyze logs", "Check logs");
        task.fail("inconclusive", FailureCategory::SoftFailure);
        task.structured_output = Some(StructuredTaskOutput {
            summary: "".into(),
            confidence: super::super::tools::submit_result::Confidence::High,
        });
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Inconclusive".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(
            prompt.contains("[soft_failure]"),
            "empty summary should fall back to bracket format: {}",
            prompt
        );
    }

    #[test]
    fn test_failure_category_all_variants_serde_roundtrip() {
        let variants = [
            (FailureCategory::AgentTimeout, "agent_timeout"),
            (FailureCategory::ContextOverflow, "context_overflow"),
            (FailureCategory::DepthExhausted, "depth_exhausted"),
            (FailureCategory::LoopDetected, "loop_detected"),
            (FailureCategory::ProviderOverloaded, "provider_overloaded"),
            (FailureCategory::ProviderAuthError, "provider_auth_error"),
            (FailureCategory::DependencyFailed, "dependency_failed"),
            (FailureCategory::SoftFailure, "soft_failure"),
            (FailureCategory::AgentError, "agent_error"),
        ];
        for (variant, expected_str) in &variants {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(
                json,
                format!("\"{}\"", expected_str),
                "serde mismatch for {:?}",
                variant
            );
            let deserialized: FailureCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(&deserialized, variant);
        }
    }

    #[test]
    fn test_failed_task_record_legacy_json_defaults_category() {
        let json = r#"{"description":"test","error":"boom","iteration":1,"worker":null}"#;
        let record: FailedTaskRecord = serde_json::from_str(json).unwrap();
        assert_eq!(
            record.category,
            FailureCategory::AgentError,
            "missing category should default to AgentError"
        );
    }

    #[test]
    fn test_observed_patterns_same_category_triggers_warning() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Fetch data", "Get data");
        task.fail("timeout", FailureCategory::AgentTimeout);
        plan.add_task(task);

        let failures = vec![
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "timeout".to_string(),
                iteration: 1,
                worker: None,
                category: FailureCategory::AgentTimeout,
            },
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "timeout again".to_string(),
                iteration: 2,
                worker: None,
                category: FailureCategory::AgentTimeout,
            },
        ];
        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(3, plan, Some(fs), failures, HashMap::new());
        let prompt = ctx.build_continuation_prompt(4, false, 2000);

        assert!(
            prompt.contains("OBSERVED PATTERNS"),
            "same category should trigger pattern: {}",
            prompt
        );
        assert!(prompt.contains("[agent_timeout]"));
    }

    #[test]
    fn test_soft_failure_without_structured_output() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Analyze logs", "Check logs");
        task.fail(
            "Worker did not call submit_result",
            FailureCategory::SoftFailure,
        );
        // No structured_output set
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Inconclusive".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, false, 2000);

        assert!(
            prompt.contains("[soft_failure]"),
            "should fall back to bracket format: {}",
            prompt
        );
        assert!(prompt.contains("Worker did not call submit_result"));
    }

    // ====================================================================
    // Tool reasoning in continuation prompt
    // ====================================================================

    use super::super::persistence::{ToolOutcome, ToolTraceEntry};

    fn make_trace(
        tool: &str,
        reasoning: &str,
        duration_ms: u64,
        error: Option<&str>,
    ) -> ToolTraceEntry {
        ToolTraceEntry {
            tool: tool.to_string(),
            reasoning: reasoning.to_string(),
            duration_ms,
            outcome: match error {
                Some(msg) => ToolOutcome::Error {
                    message: msg.to_string(),
                },
                None => ToolOutcome::Success { output_bytes: 1024 },
            },
            artifact_filename: None,
        }
    }

    #[test]
    fn continuation_with_tool_reasoning_completed_task() {
        let mut plan = Plan::new("Test goal");
        let mut t = Task::new(0, "Search logs", "Search prod logs for errors");
        t.complete("Found 47 error groups".to_string());
        plan.add_task(t);

        let mut traces = HashMap::new();
        traces.insert(
            0,
            vec![
                make_trace("log_search", "searching for error patterns", 8200, None),
                make_trace("get_metrics", "checking pool utilization", 3100, None),
            ],
        );

        let ctx = IterationContext::new(1, plan, None, vec![], traces);
        let prompt = ctx.build_continuation_prompt(3, true, 2000);

        assert!(
            prompt.contains("Tool chain:"),
            "should contain tool chain: {}",
            prompt
        );
        assert!(
            prompt.contains("log_search (8.2s"),
            "should contain log_search duration"
        );
        assert!(
            prompt.contains("searching for error patterns"),
            "should contain reasoning"
        );
        assert!(
            prompt.contains("get_metrics (3.1s"),
            "should contain get_metrics duration"
        );
        assert!(prompt.contains(" → "), "should use arrow separator");
    }

    #[test]
    fn continuation_with_tool_reasoning_failed_task() {
        let mut plan = Plan::new("Test goal");
        let mut t = Task::new(0, "Query deployments", "Query deployment history");
        t.fail("403 Forbidden".to_string(), FailureCategory::AgentError);
        plan.add_task(t);

        let mut traces = HashMap::new();
        traces.insert(
            0,
            vec![
                make_trace("get_deployments", "checking staging", 1200, None),
                make_trace(
                    "get_deployments",
                    "querying prod",
                    30200,
                    Some("403 Forbidden"),
                ),
            ],
        );

        let ctx = IterationContext::new(1, plan, None, vec![], traces);
        let prompt = ctx.build_continuation_prompt(3, true, 2000);

        assert!(
            prompt.contains("Tool chain:"),
            "should contain tool chain: {}",
            prompt
        );
        assert!(
            prompt.contains("get_deployments (1.2s"),
            "should show first call"
        );
        assert!(
            prompt.contains("FAILED: 403 Forbidden"),
            "should show failure"
        );
    }

    #[test]
    fn continuation_without_tool_reasoning_unchanged() {
        let mut plan = Plan::new("Test goal");
        let mut t = Task::new(0, "Search logs", "Search prod logs");
        t.complete("Found errors".to_string());
        plan.add_task(t);

        let ctx_without = IterationContext::new(1, plan.clone(), None, vec![], HashMap::new());
        let prompt_without = ctx_without.build_continuation_prompt(3, true, 2000);

        assert!(
            !prompt_without.contains("Tool chain:"),
            "should not contain tool chain without traces"
        );
    }

    #[test]
    fn tool_reasoning_truncation() {
        let long_reasoning = "a".repeat(150);
        let mut plan = Plan::new("Test goal");
        let mut t = Task::new(0, "Task", "Some task");
        t.complete("Done".to_string());
        plan.add_task(t);

        let mut traces = HashMap::new();
        traces.insert(0, vec![make_trace("tool_a", &long_reasoning, 1000, None)]);

        let ctx = IterationContext::new(1, plan, None, vec![], traces);
        let prompt = ctx.build_continuation_prompt(3, true, 2000);

        assert!(
            prompt.contains("…"),
            "should contain ellipsis for truncated reasoning: {}",
            prompt
        );
        assert!(
            !prompt.contains(&long_reasoning),
            "should not contain full 150-char reasoning"
        );
    }

    #[test]
    fn mixed_tasks_some_with_some_without_traces() {
        let mut plan = Plan::new("Test goal");
        let mut t0 = Task::new(0, "With traces", "Has tool records");
        t0.complete("Done".to_string());
        let mut t1 = Task::new(1, "No traces", "No tool records");
        t1.complete("Also done".to_string());
        plan.add_task(t0);
        plan.add_task(t1);

        let mut traces = HashMap::new();
        traces.insert(0, vec![make_trace("log_search", "searching", 5000, None)]);

        let ctx = IterationContext::new(1, plan, None, vec![], traces);
        let prompt = ctx.build_continuation_prompt(3, true, 2000);

        let chain_count = prompt.matches("Tool chain:").count();
        assert_eq!(
            chain_count, 1,
            "only task 0 should have a tool chain: {}",
            prompt
        );
    }

    #[test]
    fn empty_reasoning_omits_quotes() {
        let mut plan = Plan::new("Test goal");
        let mut t = Task::new(0, "Task", "Some task");
        t.complete("Done".to_string());
        plan.add_task(t);

        let mut traces = HashMap::new();
        traces.insert(0, vec![make_trace("tool_a", "", 1000, None)]);

        let ctx = IterationContext::new(1, plan, None, vec![], traces);
        let prompt = ctx.build_continuation_prompt(3, true, 2000);

        assert!(
            prompt.contains("tool_a (1.0s)"),
            "should show tool without quotes: {}",
            prompt
        );
        assert!(!prompt.contains("\"\""), "should not contain empty quotes");
    }

    #[test]
    fn single_tool_chain_no_stray_arrow() {
        let mut plan = Plan::new("Test goal");
        let mut t = Task::new(0, "Task", "Some task");
        t.complete("Done".to_string());
        plan.add_task(t);

        let mut traces = HashMap::new();
        traces.insert(0, vec![make_trace("only_tool", "single call", 2000, None)]);

        let ctx = IterationContext::new(1, plan, None, vec![], traces);
        let prompt = ctx.build_continuation_prompt(3, true, 2000);

        assert!(
            prompt.contains("Tool chain: only_tool (2.0s"),
            "should contain single tool"
        );
        let chain_line = prompt.lines().find(|l| l.contains("Tool chain:")).unwrap();
        assert!(
            !chain_line.contains("→"),
            "single tool should have no arrow separator: {}",
            chain_line
        );
    }
}
