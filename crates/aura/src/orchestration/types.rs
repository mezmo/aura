//! Types for orchestration mode task management.
//!
//! This module defines the core types used by the orchestrator to decompose
//! queries into tasks, track their execution, and manage dependencies.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::string_utils::safe_truncate;

/// Serde helper: skip serializing `current_phase_index` when it's 0 (non-phased plans).
fn is_zero(v: &usize) -> bool {
    *v == 0
}

/// Maximum nesting depth for step structures.
/// Depth 0 = top-level steps list, depth 1 = inside a parallel group,
/// depth 2 = sub-chain inside a parallel group. No deeper nesting allowed.
const MAX_STEP_NESTING: usize = 2;

/// A step in a sequential plan. Deserialized via untagged enum — serde tries
/// variants in declaration order: `ParallelGroup`, `SubChain`, then `LeafTask`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum StepInput {
    /// A group of steps that execute in parallel.
    ParallelGroup { parallel: Vec<StepInput> },
    /// A sequential sub-chain (only valid inside a `ParallelGroup`).
    SubChain { steps: Vec<StepInput> },
    /// A single task to execute.
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
            if let Some(w) = worker {
                t.worker = Some(w.clone());
            }
            tasks.push(t);
            Ok(vec![id])
        }
        StepInput::ParallelGroup { parallel } => {
            if parallel.is_empty() {
                return Err("Empty parallel group".to_string());
            }
            if depth >= MAX_STEP_NESTING {
                return Err(format!(
                    "Step nesting depth exceeds maximum of {}",
                    MAX_STEP_NESTING
                ));
            }
            if parallel.len() == 1 {
                tracing::warn!("Single-item parallel group is redundant, treating as sequential");
            }
            let mut exit_frontier = Vec::new();
            for branch in parallel {
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
    /// Original step structure from the coordinator (when using steps format).
    /// Present for `StepsPlan` responses, absent for legacy `Orchestrated` fallbacks.
    /// This is purely for persistence/observability — execution uses `tasks`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<StepInput>>,
    /// Ordered list of tasks to accomplish the goal.
    /// Auto-generated from `steps` via `flatten_steps()` when steps are present.
    pub tasks: Vec<Task>,
    /// Optional phase groupings for multi-phase execution.
    ///
    /// When `Some`, tasks are executed phase-by-phase with coordinator
    /// checkpoints between phases. When `None`, all tasks execute as
    /// a single flat plan (backward compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<Vec<Phase>>,
    /// Index of the currently active phase (0-indexed).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub current_phase_index: usize,
}

impl Plan {
    /// Create a new plan with the given goal.
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            steps: None,
            tasks: Vec::new(),
            phases: None,
            current_phase_index: 0,
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
            .filter(|t| t.status == TaskStatus::Pending)
            .count()
    }

    /// Get the count of completed tasks.
    pub fn completed_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Complete)
            .count()
    }

    /// Get the count of failed tasks.
    pub fn failed_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .count()
    }

    /// Check if all tasks are complete (or failed).
    pub fn is_finished(&self) -> bool {
        self.tasks
            .iter()
            .all(|t| t.status == TaskStatus::Complete || t.status == TaskStatus::Failed)
    }

    /// Get the next task that is ready to run (pending with all dependencies complete).
    pub fn next_ready_task(&self) -> Option<&Task> {
        self.tasks.iter().find(|task| {
            task.status == TaskStatus::Pending
                && task.dependencies.iter().all(|dep_id| {
                    self.tasks
                        .iter()
                        .find(|t| t.id == *dep_id)
                        .map(|t| t.status == TaskStatus::Complete)
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
    /// - Status is `Pending`
    /// - All dependencies have status `Complete`
    ///
    /// Tasks with failed dependencies are NOT returned (use `blocked_tasks()` to find them).
    pub fn ready_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| {
                if task.status != TaskStatus::Pending {
                    return false;
                }

                // Check each dependency
                for dep_id in &task.dependencies {
                    let dep = self.tasks.iter().find(|t| t.id == *dep_id);
                    match dep.map(|t| &t.status) {
                        Some(TaskStatus::Complete) => continue, // Dependency satisfied
                        Some(TaskStatus::Failed) => return false, // Blocked by failure
                        _ => return false, // Not ready yet (pending/running/missing)
                    }
                }
                true
            })
            .collect()
    }

    /// Whether this plan uses phased execution.
    pub fn is_phased(&self) -> bool {
        self.phases.is_some()
    }

    /// Get the currently active phase, if any.
    pub fn current_phase(&self) -> Option<&Phase> {
        self.phases
            .as_ref()
            .and_then(|phases| phases.get(self.current_phase_index))
    }

    /// Advance to the next phase. Returns `true` if there is a next phase.
    pub fn advance_phase(&mut self) -> bool {
        if let Some(phases) = &self.phases
            && self.current_phase_index + 1 < phases.len()
        {
            self.current_phase_index += 1;
            return true;
        }
        false
    }

    /// Get tasks belonging to a specific phase.
    pub fn phase_tasks(&self, phase_id: usize) -> Vec<&Task> {
        let Some(phases) = &self.phases else {
            return Vec::new();
        };
        let Some(phase) = phases.iter().find(|p| p.id == phase_id) else {
            return Vec::new();
        };
        self.tasks
            .iter()
            .filter(|t| phase.task_ids.contains(&t.id))
            .collect()
    }

    /// Get tasks that are ready to execute within the current phase only.
    ///
    /// Like `ready_tasks()` but filtered to the current phase's task set.
    /// Falls back to `ready_tasks()` for flat (non-phased) plans.
    pub fn current_phase_ready_tasks(&self) -> Vec<&Task> {
        let Some(phase) = self.current_phase() else {
            return self.ready_tasks();
        };
        let phase_task_ids = &phase.task_ids;
        self.tasks
            .iter()
            .filter(|task| {
                if !phase_task_ids.contains(&task.id) {
                    return false;
                }
                if task.status != TaskStatus::Pending {
                    return false;
                }
                for dep_id in &task.dependencies {
                    let dep = self.tasks.iter().find(|t| t.id == *dep_id);
                    match dep.map(|t| &t.status) {
                        Some(TaskStatus::Complete) => continue,
                        _ => return false,
                    }
                }
                true
            })
            .collect()
    }

    /// Check if all tasks in the current phase are finished.
    pub fn is_current_phase_finished(&self) -> bool {
        let Some(phase) = self.current_phase() else {
            return self.is_finished();
        };
        self.tasks
            .iter()
            .filter(|t| phase.task_ids.contains(&t.id))
            .all(|t| t.status == TaskStatus::Complete || t.status == TaskStatus::Failed)
    }

    /// Returns tasks that are blocked due to failed dependencies.
    ///
    /// These tasks cannot execute because at least one dependency has failed.
    /// Used to identify tasks that need replanning.
    pub fn blocked_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| {
                task.status == TaskStatus::Pending
                    && task.dependencies.iter().any(|dep_id| {
                        self.tasks
                            .iter()
                            .find(|t| t.id == *dep_id)
                            .map(|t| t.status == TaskStatus::Failed)
                            .unwrap_or(false)
                    })
            })
            .collect()
    }
}

/// A discrete task within a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique identifier for this task.
    pub id: usize,
    /// Human-readable description of what this task accomplishes.
    pub description: String,
    /// IDs of tasks that must complete before this one can start.
    pub dependencies: Vec<usize>,
    /// Current execution status.
    pub status: TaskStatus,
    /// Result of execution (if complete).
    pub result: Option<String>,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Assigned worker name (when specialized workers are configured).
    ///
    /// When set, this task will be executed by the named worker agent
    /// with its specific preamble and MCP tool filter. When `None`,
    /// the generic worker preamble is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// Why this task exists and how it advances the goal.
    pub rationale: String,
    /// When set, this task reuses the result from the specified task ID
    /// in the previous iteration's plan (set by `apply_result_reuse()`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_result_from: Option<usize>,
}

impl Task {
    /// Create a new pending task.
    pub fn new(id: usize, description: impl Into<String>, rationale: impl Into<String>) -> Self {
        Self {
            id,
            description: description.into(),
            dependencies: Vec::new(),
            status: TaskStatus::Pending,
            result: None,
            error: None,
            worker: None,
            rationale: rationale.into(),
            reuse_result_from: None,
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
        self.status = TaskStatus::Running;
    }

    /// Mark this task as complete with a result.
    pub fn complete(&mut self, result: impl Into<String>) {
        self.status = TaskStatus::Complete;
        self.result = Some(result.into());
    }

    /// Mark this task as failed with an error.
    pub fn fail(&mut self, error: impl Into<String>) {
        self.status = TaskStatus::Failed;
        self.error = Some(error.into());
    }
}

/// What should happen after a phase completes.
///
/// The coordinator inspects phase results and decides whether to proceed
/// with the next phase or replan based on what was discovered.
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
            PhaseContinuation::Continue => write!(f, "continue"),
            PhaseContinuation::Replan => write!(f, "replan"),
        }
    }
}

/// A group of tasks executed together within a phased plan.
///
/// Phases provide deliberate coordinator checkpoints between execution groups.
/// Between phases, the coordinator inspects results and decides whether to
/// continue with the next phase or replan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    /// Phase identifier (0-indexed).
    pub id: usize,
    /// Human-readable label for this phase (e.g., "Gather data").
    pub label: String,
    /// IDs of tasks belonging to this phase.
    pub task_ids: Vec<usize>,
    /// Default continuation strategy after this phase completes.
    #[serde(default)]
    pub continuation: PhaseContinuation,
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
    },
    /// The query requires multi-agent orchestration.
    #[serde(rename = "orchestrated")]
    Orchestrated {
        goal: String,
        tasks: Vec<TaskJson>,
        routing_rationale: String,
        /// Natural-language summary of the plan from the coordinator.
        #[serde(default)]
        planning_summary: String,
        /// Optional phase groupings for multi-phase execution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phases: Option<Vec<PhaseJson>>,
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
    /// Convert an `Orchestrated` or `StepsPlan` response into a `Plan`.
    ///
    /// Returns `None` for `Direct` and `Clarification` variants.
    pub fn into_plan(self) -> Option<Plan> {
        match self {
            PlanningResponse::Orchestrated {
                goal,
                tasks,
                phases,
                ..
            } => {
                let mut plan = Plan::new(goal);
                for task_json in tasks {
                    let mut task = Task::new(
                        task_json.id,
                        task_json.description,
                        task_json.rationale.unwrap_or_default(),
                    );
                    task.dependencies = task_json.dependencies.unwrap_or_default();
                    if let Some(w) = task_json.worker {
                        task.worker = Some(w);
                    }
                    task.reuse_result_from = task_json.reuse_result_from;
                    plan.add_task(task);
                }
                plan.phases = phases.map(|phase_jsons: Vec<PhaseJson>| {
                    phase_jsons
                        .into_iter()
                        .map(|pj: PhaseJson| Phase {
                            id: pj.id,
                            label: pj.label,
                            task_ids: pj.task_ids,
                            continuation: pj
                                .continuation
                                .as_deref()
                                .map(|c| match c {
                                    "replan" => PhaseContinuation::Replan,
                                    _ => PhaseContinuation::Continue,
                                })
                                .unwrap_or_default(),
                        })
                        .collect()
                });
                Some(plan)
            }
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
            PlanningResponse::Orchestrated { .. } => "Orchestrated",
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
            | PlanningResponse::Orchestrated {
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

    /// Get the planning summary (only present on Orchestrated/StepsPlan variants).
    pub fn planning_summary(&self) -> Option<&str> {
        match self {
            PlanningResponse::Orchestrated {
                planning_summary, ..
            }
            | PlanningResponse::StepsPlan {
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
    /// When set, this task reuses the result from the specified task ID
    /// in the previous iteration's plan. The coordinator sets this to
    /// carry forward results that don't need re-execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_result_from: Option<usize>,
}

/// JSON representation of a phase in a planning response.
///
/// This is the shape the LLM produces when calling `create_plan` with phases.
/// Converted to `Phase` via `PlanningResponse::into_plan()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseJson {
    pub id: usize,
    pub label: String,
    pub task_ids: Vec<usize>,
    #[serde(default)]
    pub continuation: Option<String>,
}

/// Result of semantic evaluation by the coordinator LLM.
///
/// The evaluation assesses how well the synthesized response
/// answers the user's original query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    /// Quality score between 0.0 and 1.0.
    pub score: f32,
    /// Brief explanation of the evaluation.
    pub reasoning: String,
    /// Identified gaps or missing elements in the response.
    #[serde(default)]
    pub gaps: Vec<String>,
}

impl EvaluationResult {
    /// Create a new evaluation result.
    pub fn new(score: f32, reasoning: impl Into<String>) -> Self {
        Self {
            score: score.clamp(0.0, 1.0),
            reasoning: reasoning.into(),
            gaps: Vec::new(),
        }
    }

    /// Format gaps as bullet points for reflection prompts.
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

    /// Add identified gaps.
    pub fn with_gaps(mut self, gaps: Vec<String>) -> Self {
        self.gaps = gaps;
        self
    }

    /// Create a fallback evaluation when LLM fails.
    ///
    /// Uses the simple heuristic of completed/total task ratio.
    pub fn fallback(completed: usize, total: usize) -> Self {
        let score = if total == 0 {
            0.0
        } else {
            completed as f32 / total as f32
        };
        Self {
            score,
            reasoning: format!(
                "Fallback heuristic: {} of {} tasks completed",
                completed, total
            ),
            gaps: Vec::new(),
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
}

/// A categorized planning attempt failure with timing information.
///
/// Replaces stringly-typed error collection in the planning retry loop,
/// enabling accurate summary logs (e.g. "3 timeouts" vs "3 parse failures").
#[derive(Debug, Clone)]
pub enum PlanAttemptFailure {
    /// The LLM call timed out before producing a response.
    Timeout {
        attempt: usize,
        timeout_secs: u64,
        elapsed: Duration,
    },
    /// The coordinator exhausted its turn depth without calling a routing tool.
    DepthExhausted {
        attempt: usize,
        detail: String,
        elapsed: Duration,
    },
    /// The coordinator responded but the response could not be parsed as a plan.
    ParseFailure {
        attempt: usize,
        detail: String,
        response_preview: String,
        elapsed: Duration,
    },
    /// An LLM provider error (rate limit, auth, network, etc.).
    LlmError {
        attempt: usize,
        detail: String,
        elapsed: Duration,
    },
}

impl PlanAttemptFailure {
    /// Failure category label for summary logs.
    pub fn category(&self) -> &'static str {
        match self {
            PlanAttemptFailure::Timeout { .. } => "timeout",
            PlanAttemptFailure::DepthExhausted { .. } => "depth exhaustion",
            PlanAttemptFailure::ParseFailure { .. } => "parse failure",
            PlanAttemptFailure::LlmError { .. } => "LLM error",
        }
    }

    /// Wall-clock time this attempt consumed.
    pub fn elapsed(&self) -> Duration {
        match self {
            PlanAttemptFailure::Timeout { elapsed, .. }
            | PlanAttemptFailure::DepthExhausted { elapsed, .. }
            | PlanAttemptFailure::ParseFailure { elapsed, .. }
            | PlanAttemptFailure::LlmError { elapsed, .. } => *elapsed,
        }
    }
}

impl std::fmt::Display for PlanAttemptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanAttemptFailure::Timeout {
                attempt,
                timeout_secs,
                elapsed,
            } => write!(
                f,
                "Attempt {} timed out after {:.1}s (limit: {}s)",
                attempt,
                elapsed.as_secs_f64(),
                timeout_secs,
            ),
            PlanAttemptFailure::DepthExhausted {
                attempt,
                detail,
                elapsed,
            } => write!(
                f,
                "Attempt {} exhausted turn depth after {:.1}s: {}",
                attempt,
                elapsed.as_secs_f64(),
                detail,
            ),
            PlanAttemptFailure::ParseFailure {
                attempt,
                detail,
                response_preview,
                elapsed,
            } => write!(
                f,
                "Attempt {} parse failed after {:.1}s: {}. Response: {}",
                attempt,
                elapsed.as_secs_f64(),
                detail,
                response_preview,
            ),
            PlanAttemptFailure::LlmError {
                attempt,
                detail,
                elapsed,
            } => write!(
                f,
                "Attempt {} LLM error after {:.1}s: {}",
                attempt,
                elapsed.as_secs_f64(),
                detail,
            ),
        }
    }
}

/// Context from a previous iteration, used for informed reflection.
///
/// When an iteration fails to meet the quality threshold, this context
/// is passed to the next planning phase so the coordinator can learn
/// from what went wrong and adjust the plan accordingly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationContext {
    /// Which iteration just completed (1-indexed).
    pub iteration: usize,
    /// The plan from the previous iteration.
    pub previous_plan: Plan,
    /// The evaluation of the previous iteration.
    pub evaluation: EvaluationResult,
    /// Accumulated failure history across all iterations.
    pub failure_history: Vec<FailedTaskRecord>,
    /// Synthesis summary from the previous iteration (learnings, not final answer).
    /// Present for multi-task iterations; absent for single-task or failure paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis_summary: Option<String>,
}

impl IterationContext {
    /// Create a new iteration context.
    pub fn new(
        iteration: usize,
        previous_plan: Plan,
        evaluation: EvaluationResult,
        failure_history: Vec<FailedTaskRecord>,
    ) -> Self {
        Self {
            iteration,
            previous_plan,
            evaluation,
            failure_history,
            synthesis_summary: None,
        }
    }

    /// Set the synthesis summary from the previous iteration.
    pub fn with_synthesis_summary(mut self, summary: String) -> Self {
        self.synthesis_summary = Some(summary);
        self
    }

    /// Build the reflection section for the planning prompt.
    ///
    /// Formats the previous iteration's results categorized into completed,
    /// blocked, and redesign sections. Uses the `.md` template in
    /// `crates/aura/src/prompts/reflection_prompt.md`.
    ///
    /// By default, completed task results are included inline (truncated to 500 bytes)
    /// so the coordinator can replan with full context. Gate: `AURA_ENRICH_REPLAN`
    /// env var (default=true).
    pub fn build_reflection_prompt(&self, max_iterations: usize) -> String {
        use super::templates::{ReflectionVars, render_reflection_prompt};

        let enrich = std::env::var("AURA_ENRICH_REPLAN")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);

        // Categorize tasks
        let mut completed_lines = Vec::new();
        let mut blocked_lines = Vec::new();
        let mut redesign_lines = Vec::new();

        for t in &self.previous_plan.tasks {
            match t.status {
                TaskStatus::Complete => {
                    let result_text = t.result.as_deref().unwrap_or("(no result)");

                    // Check worker self-assessment for non-achieved objectives
                    let has_negative_assessment = result_text.contains("Objective: not achieved")
                        || result_text.contains("Objective: partial");

                    let detail = if enrich {
                        let (truncated, was_truncated) = safe_truncate(result_text, 500);
                        if was_truncated {
                            format!("{truncated}...")
                        } else {
                            truncated.to_string()
                        }
                    } else {
                        let len = t.result.as_ref().map(|r| r.len()).unwrap_or(0);
                        format!("({len} chars)")
                    };

                    if has_negative_assessment {
                        // Worker self-reported failure — move to redesign
                        redesign_lines.push(format!(
                            "- Task {}: {} → self-assessed incomplete: {}",
                            t.id, t.description, detail
                        ));
                    } else {
                        completed_lines
                            .push(format!("- Task {}: {} → {}", t.id, t.description, detail));
                    }
                }
                TaskStatus::Failed => {
                    let err = t.error.as_deref().unwrap_or("unknown");
                    redesign_lines.push(format!(
                        "- Task {}: {} → failed: {}",
                        t.id, t.description, err
                    ));
                }
                TaskStatus::Pending | TaskStatus::Running => {
                    // Tasks blocked by failed dependencies
                    blocked_lines.push(format!(
                        "- Task {}: {} → blocked (dependency failed)",
                        t.id, t.description
                    ));
                }
            }
        }

        let completed_section = if completed_lines.is_empty() {
            String::new()
        } else {
            format!("COMPLETED TASKS:\n{}\n\n", completed_lines.join("\n"))
        };

        let blocked_section = if blocked_lines.is_empty() {
            String::new()
        } else {
            format!(
                "BLOCKED TASKS (dependencies failed):\n{}\n\n",
                blocked_lines.join("\n")
            )
        };

        let redesign_section = if redesign_lines.is_empty() {
            String::new()
        } else {
            format!("TASKS TO REDESIGN:\n{}\n\n", redesign_lines.join("\n"))
        };

        // Build failure history
        let failure_history = if self.failure_history.is_empty() {
            String::new()
        } else {
            let mut fh = String::from("\nFAILURE HISTORY:");
            for record in &self.failure_history {
                let worker_info = record
                    .worker
                    .as_deref()
                    .map(|w| format!(" (worker: {w})"))
                    .unwrap_or_default();
                fh.push_str(&format!(
                    "\n- Iteration {}: \"{}\"{} — {}",
                    record.iteration, record.description, worker_info, record.error,
                ));
            }

            // Identify repeated failures
            let mut desc_counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for record in &self.failure_history {
                *desc_counts.entry(&record.description).or_insert(0) += 1;
            }
            let repeated: Vec<_> = desc_counts
                .into_iter()
                .filter(|(_, count)| *count > 1)
                .collect();
            if !repeated.is_empty() {
                fh.push_str("\n\nREPEATED FAILURES:");
                for (desc, count) in &repeated {
                    fh.push_str(&format!(
                        "\n- \"{}\" has failed {} times — consider a fundamentally different approach",
                        desc, count,
                    ));
                }
            }
            fh.push('\n');
            fh
        };

        // Urgency header
        let urgency = if self.iteration + 1 >= max_iterations {
            " (FINAL ATTEMPT)".to_string()
        } else {
            String::new()
        };

        let succeeded = self.previous_plan.completed_count();
        let total = self.previous_plan.tasks.len();
        let iteration_str = self.iteration.to_string();
        let max_iter_str = max_iterations.to_string();
        let succeeded_str = succeeded.to_string();
        let total_str = total.to_string();
        let score_str = format!("{:.2}", self.evaluation.score);

        // Add reuse guidance when there are completed tasks
        let reuse_guidance = if succeeded > 0 {
            "\nTo carry forward a completed task's result without re-executing it, set \"reuse_result_from\" to the original task ID. Only reuse tasks reporting Objective: achieved — if a task self-assessed as not achieved or partial, redesign it with a different approach instead of reusing.\nCompleted tasks with actionable results should be carried forward using reuse_result_from, not re-planned from scratch."
        } else {
            ""
        };

        // Build synthesis section from previous iteration summary
        let synthesis_section = match &self.synthesis_summary {
            Some(summary) => {
                let (truncated, was_truncated) = safe_truncate(summary, 1000);
                let suffix = if was_truncated { "..." } else { "" };
                format!("PREVIOUS ITERATION FINDINGS:\n{}{}\n\n", truncated, suffix)
            }
            None => String::new(),
        };

        render_reflection_prompt(&ReflectionVars {
            iteration: &iteration_str,
            max_iterations: &max_iter_str,
            urgency: &urgency,
            succeeded: &succeeded_str,
            total: &total_str,
            goal: &self.previous_plan.goal,
            score: &score_str,
            completed_section: &completed_section,
            blocked_section: &blocked_section,
            redesign_section: &redesign_section,
            synthesis_section: &synthesis_section,
            reasoning: &self.evaluation.reasoning,
            gaps: &self.evaluation.gaps_as_bullets(),
            failure_history: &failure_history,
            reuse_guidance,
        })
    }
}

#[cfg(test)]
mod tests {
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
        plan.get_task_mut(0).unwrap().fail("Something went wrong");

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
        plan.get_task_mut(0).unwrap().fail("Something went wrong");

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
        plan.get_task_mut(0).unwrap().fail("Error");

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

        plan.get_task_mut(1).unwrap().fail("Error");
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
    // EvaluationResult tests
    // ========================================================================

    #[test]
    fn test_evaluation_gaps_as_bullets_empty() {
        let eval = EvaluationResult::new(0.8, "Good response");
        assert_eq!(eval.gaps_as_bullets(), "- No specific gaps identified");
    }

    #[test]
    fn test_evaluation_gaps_as_bullets_with_gaps() {
        let eval = EvaluationResult::new(0.5, "Missing details").with_gaps(vec![
            "Missing API details".into(),
            "No error handling".into(),
        ]);

        let bullets = eval.gaps_as_bullets();
        assert!(bullets.contains("- Missing API details"));
        assert!(bullets.contains("- No error handling"));
    }

    #[test]
    fn test_evaluation_fallback() {
        let eval = EvaluationResult::fallback(2, 4);
        assert!((eval.score - 0.5).abs() < 0.001);
        assert!(eval.reasoning.contains("2 of 4 tasks completed"));
    }

    // ========================================================================
    // IterationContext tests
    // ========================================================================

    #[test]
    fn test_iteration_context_creation() {
        let mut plan = Plan::new("Test goal");
        plan.add_task(Task::new(0, "Task 1", "First task"));

        let eval = EvaluationResult::new(0.4, "Incomplete response")
            .with_gaps(vec!["Missing context".into()]);

        let ctx = IterationContext::new(1, plan.clone(), eval, vec![]);

        assert_eq!(ctx.iteration, 1);
        assert_eq!(ctx.previous_plan.goal, "Test goal");
        assert!((ctx.evaluation.score - 0.4).abs() < 0.001);
        assert!(ctx.failure_history.is_empty());
    }

    #[test]
    fn test_iteration_context_reflection_prompt() {
        // Ensure enriched mode is active (env may leak from other tests in parallel)
        unsafe { std::env::remove_var("AURA_ENRICH_REPLAN") };

        let mut plan = Plan::new("Investigate the issue");
        let mut task = Task::new(0, "Gather logs", "Get system logs");
        task.complete("Here are the logs...".to_string());
        plan.add_task(task);

        let eval = EvaluationResult::new(0.3, "Response lacks detail").with_gaps(vec![
            "Missing root cause".into(),
            "No remediation steps".into(),
        ]);

        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        // Verify key sections are present
        assert!(prompt.contains("REPLAN CYCLE 1 of 3"));
        assert!(prompt.contains("Goal: Investigate the issue"));
        assert!(prompt.contains("Quality Score: 0.30"));
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("Task 0: Gather logs"));
        // Default enriched mode includes truncated results
        assert!(prompt.contains("Here are the logs..."));
        assert!(prompt.contains("EVALUATION:"));
        assert!(prompt.contains("Response lacks detail"));
        assert!(prompt.contains("GAPS TO ADDRESS:"));
        assert!(prompt.contains("- Missing root cause"));
        assert!(prompt.contains("- No remediation steps"));
        assert!(prompt.contains("TASKS TO REDESIGN"));
    }

    #[test]
    fn test_iteration_context_reflection_prompt_no_gaps() {
        let mut plan = Plan::new("Simple query");
        let mut task = Task::new(0, "Execute", "Run query");
        task.complete("Done".to_string());
        plan.add_task(task);

        let eval = EvaluationResult::new(0.6, "Partially complete");

        let ctx = IterationContext::new(2, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        // Should show "No specific gaps identified" when gaps is empty
        assert!(prompt.contains("- No specific gaps identified"));
        assert!(prompt.contains("REPLAN CYCLE 2 of 3"));
        assert!(prompt.contains("COMPLETED TASKS"));
    }

    #[test]
    fn test_reflection_prompt_with_failure_history() {
        let mut plan = Plan::new("Debug the issue");
        let mut task = Task::new(0, "Gather logs", "Collect logs");
        task.fail("Timeout contacting service");
        plan.add_task(task);

        let eval = EvaluationResult::new(0.0, "Task failed");
        let failures = vec![FailedTaskRecord {
            description: "Gather logs".to_string(),
            error: "Timeout contacting service".to_string(),
            iteration: 1,
            worker: Some("operations".to_string()),
        }];

        let ctx = IterationContext::new(1, plan, eval, failures);
        let prompt = ctx.build_reflection_prompt(3);

        assert!(prompt.contains("FAILURE HISTORY:"));
        assert!(prompt.contains("Iteration 1: \"Gather logs\""));
        assert!(prompt.contains("(worker: operations)"));
        assert!(prompt.contains("Timeout contacting service"));
        // No repeated failures section (only 1 occurrence)
        assert!(!prompt.contains("REPEATED FAILURES:"));
    }

    #[test]
    fn test_reflection_prompt_with_repeated_failures() {
        let mut plan = Plan::new("Debug the issue");
        let mut task = Task::new(0, "Fetch data", "Get data");
        task.fail("Connection refused");
        plan.add_task(task);

        let eval = EvaluationResult::new(0.0, "Tasks failed");
        let failures = vec![
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "Timeout".to_string(),
                iteration: 1,
                worker: None,
            },
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "Connection refused".to_string(),
                iteration: 2,
                worker: None,
            },
        ];

        let ctx = IterationContext::new(2, plan, eval, failures);
        let prompt = ctx.build_reflection_prompt(3);

        assert!(prompt.contains("FAILURE HISTORY:"));
        assert!(prompt.contains("REPEATED FAILURES:"));
        assert!(prompt.contains("\"Fetch data\" has failed 2 times"));
        assert!(prompt.contains("fundamentally different approach"));
    }

    #[test]
    fn test_reflection_prompt_enriched_truncation() {
        // Ensure enriched mode is active (env may leak from other tests in parallel)
        unsafe { std::env::set_var("AURA_ENRICH_REPLAN", "true") };

        let mut plan = Plan::new("Test truncation");
        let mut task = Task::new(0, "Big result", "Produce output");
        // 600-char result exceeds 500-byte truncation limit
        let long_result = "x".repeat(600);
        task.complete(long_result);
        plan.add_task(task);

        let eval = EvaluationResult::new(0.5, "Needs work");
        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        // Should contain truncated result with "..." suffix
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("..."));
        // Should NOT contain the full 600-char string
        assert!(!prompt.contains(&"x".repeat(600)));
        // But should contain 500 chars worth
        assert!(prompt.contains(&"x".repeat(500)));

        unsafe { std::env::remove_var("AURA_ENRICH_REPLAN") };
    }

    #[test]
    fn test_reflection_prompt_legacy_mode() {
        // SAFETY: tests run with --test-threads=1 per project convention
        unsafe { std::env::set_var("AURA_ENRICH_REPLAN", "false") };

        let mut plan = Plan::new("Test legacy");
        let mut task = Task::new(0, "Some task", "Do something");
        task.complete("Result content here".to_string());
        plan.add_task(task);

        let eval = EvaluationResult::new(0.5, "OK");
        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        // Legacy mode shows char count, not content
        assert!(prompt.contains("(19 chars)"));
        assert!(!prompt.contains("Result content here"));

        // Clean up
        unsafe { std::env::remove_var("AURA_ENRICH_REPLAN") };
    }

    #[test]
    fn test_reflection_prompt_urgency_final_attempt() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Task", "Do it");
        task.fail("error");
        plan.add_task(task);

        let eval = EvaluationResult::new(0.0, "Failed");
        // iteration=2, max=3 → next would be iteration 3 = max, so FINAL ATTEMPT
        let ctx = IterationContext::new(2, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        assert!(prompt.contains("(FINAL ATTEMPT)"));
    }

    #[test]
    fn test_reflection_prompt_no_urgency_early_iteration() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Task", "Do it");
        task.fail("error");
        plan.add_task(task);

        let eval = EvaluationResult::new(0.0, "Failed");
        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        assert!(!prompt.contains("FINAL ATTEMPT"));
    }

    #[test]
    fn test_reflection_prompt_includes_reuse_guidance() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Completed task", "Done");
        task.complete("Some result");
        plan.add_task(task);

        let eval = EvaluationResult::new(0.4, "Needs improvement");
        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        assert!(prompt.contains("reuse_result_from"));
    }

    #[test]
    fn test_reflection_prompt_no_reuse_guidance_when_all_failed() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Failed task", "Tried");
        task.fail("error");
        plan.add_task(task);

        let eval = EvaluationResult::new(0.0, "All failed");
        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        assert!(!prompt.contains("reuse_result_from"));
    }

    #[test]
    fn test_reflection_prompt_self_assessment_categorization() {
        unsafe { std::env::remove_var("AURA_ENRICH_REPLAN") };

        let mut plan = Plan::new("Analyze data");

        // Task with positive self-assessment → COMPLETED
        let mut achieved = Task::new(0, "Fetch data", "Get data");
        achieved.complete("Objective: achieved\nResult: Got 100 rows\nProcess: Used query tool");
        plan.add_task(achieved);

        // Task with negative self-assessment → REDESIGN
        let mut not_achieved = Task::new(1, "Transform data", "Process data");
        not_achieved.complete(
            "Objective: not achieved\nResult: Could not parse format\nProcess: Attempted CSV parse",
        );
        plan.add_task(not_achieved);

        // Task with partial self-assessment → REDESIGN
        let mut partial = Task::new(2, "Summarize findings", "Summarize");
        partial.complete(
            "Objective: partial\nResult: Only 2 of 5 metrics computed\nProcess: Missing source data",
        );
        plan.add_task(partial);

        let eval = EvaluationResult::new(0.4, "Incomplete");
        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        // Task 0 should be in COMPLETED (achieved)
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("Task 0: Fetch data"));

        // Tasks 1 and 2 should be in TASKS TO REDESIGN (not achieved / partial)
        assert!(prompt.contains("TASKS TO REDESIGN"));
        assert!(prompt.contains("Task 1: Transform data"));
        assert!(prompt.contains("self-assessed incomplete"));
        assert!(prompt.contains("Task 2: Summarize findings"));

        // Reuse guidance should mention Objective: achieved
        assert!(prompt.contains("Objective: achieved"));
    }

    #[test]
    fn test_reflection_prompt_mixed_categories() {
        unsafe { std::env::remove_var("AURA_ENRICH_REPLAN") };

        let mut plan = Plan::new("Mixed results");

        let mut completed = Task::new(0, "Completed task", "Worked");
        completed.complete("Good result");
        plan.add_task(completed);

        let mut failed = Task::new(1, "Failed task", "Broken");
        failed.fail("Connection refused");
        plan.add_task(failed);

        // Task 2 depends on failed task 1, so it stays Pending (blocked)
        let mut blocked = Task::new(2, "Blocked task", "Waiting");
        blocked.dependencies = vec![1];
        plan.add_task(blocked);

        let eval =
            EvaluationResult::new(0.3, "Partial success").with_gaps(vec!["Task 1 failed".into()]);
        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt(3);

        // All three sections should be present
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("Task 0: Completed task"));
        assert!(prompt.contains("TASKS TO REDESIGN"));
        assert!(prompt.contains("Task 1: Failed task"));
        assert!(prompt.contains("BLOCKED TASKS"));
        assert!(prompt.contains("Task 2: Blocked task"));

        // Verify ordering: completed before blocked before redesign
        let completed_pos = prompt.find("COMPLETED TASKS").unwrap();
        let blocked_pos = prompt.find("BLOCKED TASKS").unwrap();
        let redesign_pos = prompt.find("TASKS TO REDESIGN").unwrap();
        assert!(completed_pos < blocked_pos);
        assert!(blocked_pos < redesign_pos);
    }

    // ========================================================================
    // PlanningResponse tests
    // ========================================================================

    #[test]
    fn test_planning_response_direct_serde() {
        let response = PlanningResponse::Direct {
            response: "The answer is 42.".to_string(),
            routing_rationale: "Simple arithmetic".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"response_type\":\"direct\""));
        assert!(json.contains("\"routing_rationale\":\"Simple arithmetic\""));

        let deserialized: PlanningResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlanningResponse::Direct {
                response,
                routing_rationale,
            } => {
                assert_eq!(response, "The answer is 42.");
                assert_eq!(routing_rationale, "Simple arithmetic");
            }
            other => panic!("Expected Direct, got {:?}", other),
        }
    }

    #[test]
    fn test_planning_response_orchestrated_serde() {
        let response = PlanningResponse::Orchestrated {
            goal: "Check logs".to_string(),
            tasks: vec![
                TaskJson {
                    id: 0,
                    description: "Fetch logs".to_string(),
                    rationale: Some("Need data".to_string()),
                    dependencies: None,
                    worker: Some("operations".to_string()),
                    reuse_result_from: None,
                },
                TaskJson {
                    id: 1,
                    description: "Analyze".to_string(),
                    rationale: None,
                    dependencies: Some(vec![0]),
                    worker: None,
                    reuse_result_from: None,
                },
            ],
            routing_rationale: "Requires tool execution".to_string(),
            planning_summary: "Fetch and analyze logs".to_string(),
            phases: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"response_type\":\"orchestrated\""));

        let deserialized: PlanningResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlanningResponse::Orchestrated { goal, tasks, .. } => {
                assert_eq!(goal, "Check logs");
                assert_eq!(tasks.len(), 2);
                assert_eq!(tasks[0].worker, Some("operations".to_string()));
                assert_eq!(tasks[1].dependencies, Some(vec![0]));
            }
            other => panic!("Expected Orchestrated, got {:?}", other),
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
    fn test_into_plan_from_orchestrated() {
        let response = PlanningResponse::Orchestrated {
            goal: "Test goal".to_string(),
            tasks: vec![
                TaskJson {
                    id: 0,
                    description: "Task A".to_string(),
                    rationale: Some("Reason A".to_string()),
                    dependencies: None,
                    worker: None,
                    reuse_result_from: None,
                },
                TaskJson {
                    id: 1,
                    description: "Task B".to_string(),
                    rationale: Some("Reason B".to_string()),
                    dependencies: Some(vec![0]),
                    worker: Some("ops".to_string()),
                    reuse_result_from: None,
                },
            ],
            routing_rationale: "Needs orchestration".to_string(),
            planning_summary: "Execute tasks A and B".to_string(),
            phases: None,
        };

        let plan = response.into_plan().unwrap();
        assert_eq!(plan.goal, "Test goal");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].description, "Task A");
        assert_eq!(plan.tasks[0].rationale, "Reason A");
        assert!(plan.tasks[0].dependencies.is_empty());
        assert!(plan.tasks[0].worker.is_none());
        assert_eq!(plan.tasks[1].dependencies, vec![0]);
        assert_eq!(plan.tasks[1].worker, Some("ops".to_string()));
    }

    #[test]
    fn test_into_plan_from_direct_returns_none() {
        let response = PlanningResponse::Direct {
            response: "42".to_string(),
            routing_rationale: "Simple".to_string(),
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
        };
        assert_eq!(direct.routing_rationale(), "reason_d");

        let orch = PlanningResponse::Orchestrated {
            goal: "g".to_string(),
            tasks: vec![],
            routing_rationale: "reason_o".to_string(),
            planning_summary: "summary".to_string(),
            phases: None,
        };
        assert_eq!(orch.routing_rationale(), "reason_o");

        let clar = PlanningResponse::Clarification {
            question: "q".to_string(),
            options: None,
            routing_rationale: "reason_c".to_string(),
        };
        assert_eq!(clar.routing_rationale(), "reason_c");
    }

    // ========================================================================
    // PlanAttemptFailure tests
    // ========================================================================

    #[test]
    fn test_plan_attempt_failure_display_timeout() {
        let f = PlanAttemptFailure::Timeout {
            attempt: 1,
            timeout_secs: 60,
            elapsed: Duration::from_secs_f64(60.1),
        };
        let s = f.to_string();
        assert!(s.contains("Attempt 1 timed out"), "got: {}", s);
        assert!(s.contains("60.1s"), "got: {}", s);
        assert!(s.contains("limit: 60s"), "got: {}", s);
        assert_eq!(f.category(), "timeout");
    }

    #[test]
    fn test_plan_attempt_failure_display_depth_exhausted() {
        let f = PlanAttemptFailure::DepthExhausted {
            attempt: 2,
            detail: "inspect_tool_params consumed the budget".to_string(),
            elapsed: Duration::from_secs_f64(35.3),
        };
        let s = f.to_string();
        assert!(s.contains("Attempt 2 exhausted turn depth"), "got: {}", s);
        assert!(s.contains("35.3s"), "got: {}", s);
        assert!(s.contains("inspect_tool_params"), "got: {}", s);
        assert_eq!(f.category(), "depth exhaustion");
    }

    #[test]
    fn test_plan_attempt_failure_display_parse_failure() {
        let f = PlanAttemptFailure::ParseFailure {
            attempt: 3,
            detail: "expected JSON object".to_string(),
            response_preview: "I'll help you with...".to_string(),
            elapsed: Duration::from_secs_f64(12.7),
        };
        let s = f.to_string();
        assert!(s.contains("Attempt 3 parse failed"), "got: {}", s);
        assert!(s.contains("12.7s"), "got: {}", s);
        assert!(s.contains("expected JSON object"), "got: {}", s);
        assert!(s.contains("I'll help you with"), "got: {}", s);
        assert_eq!(f.category(), "parse failure");
    }

    #[test]
    fn test_plan_attempt_failure_display_llm_error() {
        let f = PlanAttemptFailure::LlmError {
            attempt: 1,
            detail: "rate limit exceeded".to_string(),
            elapsed: Duration::from_secs_f64(0.5),
        };
        let s = f.to_string();
        assert!(s.contains("Attempt 1 LLM error"), "got: {}", s);
        assert!(s.contains("0.5s"), "got: {}", s);
        assert!(s.contains("rate limit exceeded"), "got: {}", s);
        assert_eq!(f.category(), "LLM error");
    }

    #[test]
    fn test_plan_attempt_failure_elapsed_accessor() {
        let cases = [
            PlanAttemptFailure::Timeout {
                attempt: 1,
                timeout_secs: 60,
                elapsed: Duration::from_secs(60),
            },
            PlanAttemptFailure::DepthExhausted {
                attempt: 1,
                detail: String::new(),
                elapsed: Duration::from_secs(30),
            },
            PlanAttemptFailure::ParseFailure {
                attempt: 1,
                detail: String::new(),
                response_preview: String::new(),
                elapsed: Duration::from_secs(10),
            },
            PlanAttemptFailure::LlmError {
                attempt: 1,
                detail: String::new(),
                elapsed: Duration::from_millis(500),
            },
        ];
        let expected = [
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::from_secs(10),
            Duration::from_millis(500),
        ];
        for (f, exp) in cases.iter().zip(expected.iter()) {
            assert_eq!(f.elapsed(), *exp, "elapsed mismatch for {:?}", f);
        }
    }

    // ========================================================================
    // Phase and phased plan tests
    // ========================================================================

    #[test]
    fn test_plan_phased_creation() {
        let mut plan = Plan::new("Multi-phase goal");
        plan.add_task(Task::new(0, "Discover services", "Phase 1 discovery"));
        plan.add_task(Task::new(1, "Gather metrics", "Phase 1 data collection"));
        plan.add_task(Task::new(2, "Analyze results", "Phase 2 analysis").with_dependency(0));

        plan.phases = Some(vec![
            Phase {
                id: 0,
                label: "Discovery".to_string(),
                task_ids: vec![0, 1],
                continuation: PhaseContinuation::Continue,
            },
            Phase {
                id: 1,
                label: "Analysis".to_string(),
                task_ids: vec![2],
                continuation: PhaseContinuation::Replan,
            },
        ]);

        assert!(plan.is_phased());
        let phase = plan.current_phase().unwrap();
        assert_eq!(phase.id, 0);
        assert_eq!(phase.label, "Discovery");
    }

    #[test]
    fn test_plan_advance_phase() {
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "A", "first"));
        plan.add_task(Task::new(1, "B", "second"));
        plan.add_task(Task::new(2, "C", "third"));

        plan.phases = Some(vec![
            Phase {
                id: 0,
                label: "Phase 1".to_string(),
                task_ids: vec![0],
                continuation: PhaseContinuation::Continue,
            },
            Phase {
                id: 1,
                label: "Phase 2".to_string(),
                task_ids: vec![1],
                continuation: PhaseContinuation::Continue,
            },
            Phase {
                id: 2,
                label: "Phase 3".to_string(),
                task_ids: vec![2],
                continuation: PhaseContinuation::Replan,
            },
        ]);

        assert_eq!(plan.current_phase().unwrap().id, 0);
        assert!(plan.advance_phase());
        assert_eq!(plan.current_phase().unwrap().id, 1);
        assert!(plan.advance_phase());
        assert_eq!(plan.current_phase().unwrap().id, 2);
        // No more phases
        assert!(!plan.advance_phase());
        assert_eq!(plan.current_phase().unwrap().id, 2);
    }

    #[test]
    fn test_plan_flat_is_not_phased() {
        let mut plan = Plan::new("Flat plan");
        plan.add_task(Task::new(0, "Task A", "reason"));
        plan.add_task(Task::new(1, "Task B", "reason"));

        assert!(!plan.is_phased());
        assert!(plan.current_phase().is_none());
        assert!(!plan.advance_phase());
    }

    #[test]
    fn test_phase_tasks() {
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "A", "r"));
        plan.add_task(Task::new(1, "B", "r"));
        plan.add_task(Task::new(2, "C", "r"));

        plan.phases = Some(vec![
            Phase {
                id: 0,
                label: "Phase 1".to_string(),
                task_ids: vec![0, 1],
                continuation: PhaseContinuation::Continue,
            },
            Phase {
                id: 1,
                label: "Phase 2".to_string(),
                task_ids: vec![2],
                continuation: PhaseContinuation::Continue,
            },
        ]);

        let p0_tasks = plan.phase_tasks(0);
        assert_eq!(p0_tasks.len(), 2);
        assert_eq!(p0_tasks[0].id, 0);
        assert_eq!(p0_tasks[1].id, 1);

        let p1_tasks = plan.phase_tasks(1);
        assert_eq!(p1_tasks.len(), 1);
        assert_eq!(p1_tasks[0].id, 2);

        // Non-existent phase returns empty
        assert!(plan.phase_tasks(99).is_empty());
    }

    #[test]
    fn test_current_phase_ready_tasks() {
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "A", "r"));
        plan.add_task(Task::new(1, "B", "r").with_dependency(0));
        plan.add_task(Task::new(2, "C", "r")); // Phase 2

        plan.phases = Some(vec![
            Phase {
                id: 0,
                label: "Phase 1".to_string(),
                task_ids: vec![0, 1],
                continuation: PhaseContinuation::Continue,
            },
            Phase {
                id: 1,
                label: "Phase 2".to_string(),
                task_ids: vec![2],
                continuation: PhaseContinuation::Continue,
            },
        ]);

        // Phase 1: only task 0 is ready (task 1 depends on 0)
        let ready = plan.current_phase_ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, 0);

        // Task 2 is NOT ready (wrong phase)
        assert!(ready.iter().all(|t| t.id != 2));

        // Complete task 0 → task 1 becomes ready
        plan.get_task_mut(0).unwrap().complete("done");
        let ready = plan.current_phase_ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, 1);
    }

    #[test]
    fn test_is_current_phase_finished() {
        let mut plan = Plan::new("Test");
        plan.add_task(Task::new(0, "A", "r"));
        plan.add_task(Task::new(1, "B", "r"));

        plan.phases = Some(vec![Phase {
            id: 0,
            label: "Phase 1".to_string(),
            task_ids: vec![0, 1],
            continuation: PhaseContinuation::Continue,
        }]);

        assert!(!plan.is_current_phase_finished());
        plan.get_task_mut(0).unwrap().complete("done");
        assert!(!plan.is_current_phase_finished());
        plan.get_task_mut(1).unwrap().complete("done");
        assert!(plan.is_current_phase_finished());
    }

    #[test]
    fn test_phased_plan_parsing() {
        let response = PlanningResponse::Orchestrated {
            goal: "Investigate system".to_string(),
            tasks: vec![
                TaskJson {
                    id: 0,
                    description: "Discover services".to_string(),
                    rationale: Some("Need to find what's running".to_string()),
                    dependencies: None,
                    worker: None,
                    reuse_result_from: None,
                },
                TaskJson {
                    id: 1,
                    description: "Analyze health".to_string(),
                    rationale: Some("Check discovered services".to_string()),
                    dependencies: Some(vec![0]),
                    worker: None,
                    reuse_result_from: None,
                },
            ],
            routing_rationale: "Complex investigation".to_string(),
            planning_summary: "Discover then analyze".to_string(),
            phases: Some(vec![
                PhaseJson {
                    id: 0,
                    label: "Discovery".to_string(),
                    task_ids: vec![0],
                    continuation: Some("continue".to_string()),
                },
                PhaseJson {
                    id: 1,
                    label: "Analysis".to_string(),
                    task_ids: vec![1],
                    continuation: Some("replan".to_string()),
                },
            ]),
        };

        let plan = response.into_plan().unwrap();
        assert!(plan.is_phased());
        let phases = plan.phases.as_ref().unwrap();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].label, "Discovery");
        assert_eq!(phases[0].continuation, PhaseContinuation::Continue);
        assert_eq!(phases[1].label, "Analysis");
        assert_eq!(phases[1].continuation, PhaseContinuation::Replan);
        assert_eq!(phases[1].task_ids, vec![1]);
    }

    #[test]
    fn test_flat_plan_parsing_unchanged() {
        // Existing flat plan (no phases) should parse identically
        let response = PlanningResponse::Orchestrated {
            goal: "Simple query".to_string(),
            tasks: vec![TaskJson {
                id: 0,
                description: "Do thing".to_string(),
                rationale: Some("Reason".to_string()),
                dependencies: None,
                worker: None,
                reuse_result_from: None,
            }],
            routing_rationale: "Needs tool".to_string(),
            planning_summary: "Just do it".to_string(),
            phases: None,
        };

        let plan = response.into_plan().unwrap();
        assert!(!plan.is_phased());
        assert!(plan.phases.is_none());
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].description, "Do thing");
    }

    #[test]
    fn test_phase_continuation_display() {
        assert_eq!(PhaseContinuation::Continue.to_string(), "continue");
        assert_eq!(PhaseContinuation::Replan.to_string(), "replan");
    }

    #[test]
    fn test_phase_continuation_default() {
        let default: PhaseContinuation = Default::default();
        assert_eq!(default, PhaseContinuation::Continue);
    }

    #[test]
    fn test_phased_plan_serde_roundtrip() {
        let response = PlanningResponse::Orchestrated {
            goal: "Test".to_string(),
            tasks: vec![TaskJson {
                id: 0,
                description: "Task".to_string(),
                rationale: None,
                dependencies: None,
                worker: None,
                reuse_result_from: None,
            }],
            routing_rationale: "reason".to_string(),
            planning_summary: "summary".to_string(),
            phases: Some(vec![PhaseJson {
                id: 0,
                label: "Only phase".to_string(),
                task_ids: vec![0],
                continuation: Some("continue".to_string()),
            }]),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"phases\""));
        assert!(json.contains("\"Only phase\""));

        let deserialized: PlanningResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlanningResponse::Orchestrated { phases, .. } => {
                let phases = phases.unwrap();
                assert_eq!(phases.len(), 1);
                assert_eq!(phases[0].label, "Only phase");
            }
            other => panic!("Expected Orchestrated, got {:?}", other),
        }
    }

    #[test]
    fn test_flat_plan_serde_no_phases_field() {
        let response = PlanningResponse::Orchestrated {
            goal: "Test".to_string(),
            tasks: vec![],
            routing_rationale: "reason".to_string(),
            planning_summary: "summary".to_string(),
            phases: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        // phases: None with skip_serializing_if should omit the field
        assert!(!json.contains("\"phases\""));

        // Deserializing JSON without phases field should produce None
        let deserialized: PlanningResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlanningResponse::Orchestrated { phases, .. } => {
                assert!(phases.is_none());
            }
            other => panic!("Expected Orchestrated, got {:?}", other),
        }
    }

    #[test]
    fn test_phase_continuation_missing_defaults_to_continue() {
        // When continuation is omitted from JSON, it should default to Continue
        let response = PlanningResponse::Orchestrated {
            goal: "Test".to_string(),
            tasks: vec![TaskJson {
                id: 0,
                description: "Task".to_string(),
                rationale: None,
                dependencies: None,
                worker: None,
                reuse_result_from: None,
            }],
            routing_rationale: "reason".to_string(),
            planning_summary: "summary".to_string(),
            phases: Some(vec![PhaseJson {
                id: 0,
                label: "Phase".to_string(),
                task_ids: vec![0],
                continuation: None, // omitted
            }]),
        };

        let plan = response.into_plan().unwrap();
        let phases = plan.phases.as_ref().unwrap();
        assert_eq!(phases[0].continuation, PhaseContinuation::Continue);
    }

    #[test]
    fn test_current_phase_ready_tasks_flat_plan_fallback() {
        // For a flat plan, current_phase_ready_tasks should behave like ready_tasks
        let mut plan = Plan::new("Flat");
        plan.add_task(Task::new(0, "A", "r"));
        plan.add_task(Task::new(1, "B", "r").with_dependency(0));

        let ready = plan.current_phase_ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, 0);
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
                parallel: vec![
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
                parallel: vec![
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
        let steps = vec![StepInput::ParallelGroup { parallel: vec![] }];
        let result = flatten_steps(&steps);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty parallel"));
    }

    #[test]
    fn test_flatten_depth_exceeded() {
        // Depth 0: top-level, depth 1: parallel, depth 2: sub-chain,
        // depth 3: nested parallel inside sub-chain -> should fail
        let steps = vec![StepInput::ParallelGroup {
            parallel: vec![StepInput::SubChain {
                steps: vec![StepInput::ParallelGroup {
                    parallel: vec![StepInput::LeafTask {
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
            parallel: vec![StepInput::SubChain { steps: vec![] }],
        }];
        let result = flatten_steps(&steps);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty sub-chain"));
    }

    #[test]
    fn test_step_input_deserialize_sequential() {
        let json = r#"[
            {"task": "Compute mean", "worker": "stats"},
            {"task": "Multiply result", "worker": "math"}
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
            {"parallel": [
                {"task": "A", "worker": "w1"},
                {"task": "B", "worker": "w2"}
            ]},
            {"task": "C"}
        ]"#;
        let steps: Vec<StepInput> = serde_json::from_str(json).unwrap();
        assert_eq!(steps.len(), 2);
        match &steps[0] {
            StepInput::ParallelGroup { parallel } => {
                assert_eq!(parallel.len(), 2);
            }
            other => panic!("Expected ParallelGroup, got {:?}", other),
        }
    }

    #[test]
    fn test_step_input_deserialize_recursive() {
        let json = r#"[
            {"parallel": [
                {"steps": [
                    {"task": "Get A"},
                    {"task": "Transform A"}
                ]},
                {"task": "Get B"}
            ]},
            {"task": "Combine"}
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
        assert!(plan.phases.is_none());
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
    fn test_task_json_reuse_result_from_serde_roundtrip() {
        let task = TaskJson {
            id: 1,
            description: "Compute sum".into(),
            rationale: Some("Needed for total".into()),
            dependencies: Some(vec![0]),
            worker: Some("math".into()),
            reuse_result_from: Some(3),
        };
        let json = serde_json::to_string(&task).unwrap();
        let deserialized: TaskJson = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, 1);
        assert_eq!(deserialized.reuse_result_from, Some(3));
        assert_eq!(deserialized.worker, Some("math".into()));
    }

    #[test]
    fn test_task_json_reuse_result_from_absent_defaults_none() {
        let json = r#"{"id": 0, "description": "Do thing"}"#;
        let task: TaskJson = serde_json::from_str(json).unwrap();
        assert_eq!(task.reuse_result_from, None);
        assert_eq!(task.dependencies, None);
        assert_eq!(task.worker, None);
    }

    #[test]
    fn test_into_plan_preserves_reuse_result_from() {
        let response = PlanningResponse::Orchestrated {
            goal: "Test reuse".into(),
            tasks: vec![
                TaskJson {
                    id: 0,
                    description: "Fresh task".into(),
                    rationale: None,
                    dependencies: None,
                    worker: None,
                    reuse_result_from: None,
                },
                TaskJson {
                    id: 1,
                    description: "Reused task".into(),
                    rationale: Some("Carry forward".into()),
                    dependencies: Some(vec![0]),
                    worker: None,
                    reuse_result_from: Some(5),
                },
            ],
            routing_rationale: "test".into(),
            planning_summary: "test".into(),
            phases: None,
        };
        let plan = response.into_plan().unwrap();
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].reuse_result_from, None);
        assert_eq!(plan.tasks[1].reuse_result_from, Some(5));
    }

    #[test]
    fn test_into_plan_then_apply_reuse_pipeline() {
        use crate::orchestration::orchestrator::Orchestrator;

        // Build a "previous" plan with a completed task at id=0
        let mut previous = Plan::new("Previous goal");
        let mut prev_task = Task::new(0, "Compute mean", "stats");
        prev_task.complete("42".to_string());
        previous.add_task(prev_task);

        // Build a new plan via PlanningResponse with reuse_result_from
        let response = PlanningResponse::Orchestrated {
            goal: "New goal".into(),
            tasks: vec![TaskJson {
                id: 0,
                description: "Reuse mean".into(),
                rationale: None,
                dependencies: None,
                worker: None,
                reuse_result_from: Some(0),
            }],
            routing_rationale: "test".into(),
            planning_summary: "test".into(),
            phases: None,
        };
        let mut plan = response.into_plan().unwrap();
        assert_eq!(plan.tasks[0].status, TaskStatus::Pending);

        Orchestrator::apply_result_reuse(&mut plan, Some(&previous));

        assert_eq!(plan.tasks[0].status, TaskStatus::Complete);
        assert_eq!(plan.tasks[0].result.as_deref(), Some("42"));
    }

    #[test]
    fn test_failed_task_record_serde_roundtrip() {
        let record = FailedTaskRecord {
            description: "Divide numbers".into(),
            error: "Division by zero".into(),
            iteration: 2,
            worker: Some("math".into()),
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
            EvaluationResult {
                score: 0.7,
                reasoning: "Decent".into(),
                gaps: vec!["Missing detail".into()],
            },
            vec![FailedTaskRecord {
                description: "bad task".into(),
                error: "oops".into(),
                iteration: 1,
                worker: None,
            }],
        );
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: IterationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.iteration, 1);
        assert_eq!(deserialized.evaluation.score, 0.7);
        assert_eq!(deserialized.previous_plan.tasks.len(), 1);
        assert_eq!(deserialized.failure_history.len(), 1);
    }
}
