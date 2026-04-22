//! Types for orchestration mode task management.
//!
//! This module defines the core types used by the orchestrator to decompose
//! queries into tasks, track their execution, and manage dependencies.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::string_utils::safe_truncate;

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
    /// Reuse a result from a previous iteration without re-executing.
    #[serde(rename = "reuse")]
    ReuseTask { reuse_result_from: usize },
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
        StepInput::ReuseTask { reuse_result_from } => {
            let id = *counter;
            *counter += 1;
            let mut t = Task::new(id, String::new(), String::new());
            t.dependencies = frontier.to_vec();
            t.reuse_result_from = Some(*reuse_result_from);
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
    /// When set, this task reuses the result from the specified task ID
    /// in the previous iteration's plan. The coordinator sets this to
    /// carry forward results that don't need re-execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_result_from: Option<usize>,
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
    /// Failure summary populated only when the iteration had failures or
    /// blocked tasks. `None` on the clean-success path.
    pub failure_summary: Option<FailureSummary>,
    /// Accumulated failure history across all iterations.
    pub failure_history: Vec<FailedTaskRecord>,
}

impl IterationContext {
    /// Create a new iteration context.
    pub fn new(
        iteration: usize,
        previous_plan: Plan,
        failure_summary: Option<FailureSummary>,
        failure_history: Vec<FailedTaskRecord>,
    ) -> Self {
        Self {
            iteration,
            previous_plan,
            failure_summary,
            failure_history,
        }
    }

    /// Build the continuation section for the post-execute coordinator call.
    ///
    /// Renders the previous iteration's per-task state (completed, failed,
    /// blocked), optional failure summary, accumulated failure history with
    /// repeated-failure detection, and a conditional reuse hint. Uses the
    /// `.md` template in `crates/aura/src/prompts/continuation_prompt.md`.
    ///
    /// Per-task completed results are included inline, truncated to 500 bytes
    /// so the coordinator can route with full context. When a completed result
    /// was spilled to an artifact, the
    /// `[Full result (N chars) saved to artifact: task-N-result.txt]` footer
    /// is re-appended after truncation so the coordinator can discover the
    /// artifact via `read_artifact`.
    pub fn build_continuation_prompt(&self, max_iterations: usize) -> String {
        use super::templates::{ContinuationVars, render_continuation_prompt};

        // Categorize tasks
        let mut completed_lines = Vec::new();
        let mut blocked_lines = Vec::new();
        let mut redesign_lines = Vec::new();
        let mut has_failed_tasks = false;

        for t in &self.previous_plan.tasks {
            match t.status {
                TaskStatus::Complete => {
                    let result = t.result.as_deref().unwrap_or("(no result)");
                    let detail = preserve_artifact_footer(result, 500);
                    completed_lines
                        .push(format!("- Task {}: {} → {}", t.id, t.description, detail));
                }
                TaskStatus::Failed => {
                    has_failed_tasks = true;
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
            format!("FAILED TASKS:\n{}\n\n", redesign_lines.join("\n"))
        };

        // Failure summary — only when there were failures in this iteration.
        let failure_section = match &self.failure_summary {
            Some(fs) => format!(
                "FAILURE SUMMARY:\n{}\n\nAREAS NEEDING ATTENTION:\n{}\n\n",
                fs.reasoning,
                fs.gaps_as_bullets(),
            ),
            None => String::new(),
        };

        // Build failure history
        let failure_history = if self.failure_history.is_empty() {
            String::new()
        } else {
            let mut fh = String::from("FAILURE HISTORY:");
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
                fh.push_str("\n\nOBSERVED PATTERNS:");
                for (desc, count) in &repeated {
                    fh.push_str(&format!(
                        "\n- \"{}\" has failed {} times — consider a fundamentally different approach",
                        desc, count,
                    ));
                }
            }
            fh.push_str("\n\n");
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

        // Only surface reuse guidance when there are failed tasks to
        // selectively retry; surfacing on clean success teaches the model
        // the no-op reuse-only plan pattern.
        let reuse_guidance = if has_failed_tasks && succeeded > 0 {
            "To carry forward a completed task's result when retrying failed tasks, set \"reuse_result_from\" to the original task ID.\n\n"
        } else {
            ""
        };

        render_continuation_prompt(&ContinuationVars {
            iteration: &iteration_str,
            max_iterations: &max_iter_str,
            urgency: &urgency,
            succeeded: &succeeded_str,
            total: &total_str,
            goal: &self.previous_plan.goal,
            completed_section: &completed_section,
            blocked_section: &blocked_section,
            redesign_section: &redesign_section,
            failure_section: &failure_section,
            failure_history: &failure_history,
            reuse_guidance,
        })
    }
}

/// Truncate `result` to `budget` bytes while preserving any trailing
/// artifact footer (`[Full result (N chars) saved to artifact: FILE]`)
/// that `maybe_create_artifact` appended past the budget. Without this,
/// a naive truncation would slice off the artifact pointer and the
/// coordinator would lose the ability to `read_artifact` the full content.
fn preserve_artifact_footer(result: &str, budget: usize) -> String {
    // Detect the artifact-footer marker appended by maybe_create_artifact.
    const FOOTER_PREFIX: &str = "[Full result (";
    let footer_start = result.rfind(FOOTER_PREFIX);

    match footer_start {
        Some(idx) => {
            // Everything before the footer is the body; footer keeps its full text.
            let body = &result[..idx];
            let footer = &result[idx..];
            let (truncated_body, was_truncated) = safe_truncate(body, budget);
            let body_str = if was_truncated {
                format!("{truncated_body}...")
            } else {
                truncated_body.to_string()
            };
            // Re-join truncated body with full footer. Artifact footer is
            // what lets the coordinator discover the artifact via read_artifact.
            if body_str.is_empty() {
                footer.to_string()
            } else {
                format!("{body_str} {footer}")
            }
        }
        None => {
            let (truncated, was_truncated) = safe_truncate(result, budget);
            if was_truncated {
                format!("{truncated}...")
            } else {
                truncated.to_string()
            }
        }
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

        let ctx = IterationContext::new(1, plan.clone(), Some(fs), vec![]);

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

        let ctx = IterationContext::new(1, plan, Some(fs), vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        // Verify key sections are present
        assert!(prompt.contains("ITERATION 1 of 3"));
        assert!(prompt.contains("Goal: Investigate the issue"));
        // No evaluator vocabulary — no "Quality Score"
        assert!(!prompt.contains("Quality Score"));
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("Task 0: Gather logs"));
        // Completed task results appear inline, truncated to the inline budget
        assert!(prompt.contains("Here are the logs..."));
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

        let ctx = IterationContext::new(2, plan, None, vec![]);
        let prompt = ctx.build_continuation_prompt(3);

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
        task.fail("Timeout contacting service");
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
        }];

        let ctx = IterationContext::new(1, plan, Some(fs), failures);
        let prompt = ctx.build_continuation_prompt(3);

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
        task.fail("Connection refused");
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
            },
            FailedTaskRecord {
                description: "Fetch data".to_string(),
                error: "Connection refused".to_string(),
                iteration: 2,
                worker: None,
            },
        ];

        let ctx = IterationContext::new(2, plan, Some(fs), failures);
        let prompt = ctx.build_continuation_prompt(3);

        assert!(prompt.contains("FAILURE HISTORY:"));
        assert!(prompt.contains("OBSERVED PATTERNS:"));
        assert!(prompt.contains("\"Fetch data\" has failed 2 times"));
        assert!(prompt.contains("fundamentally different approach"));
    }

    #[test]
    fn test_continuation_prompt_truncation() {
        let mut plan = Plan::new("Test truncation");
        let mut task = Task::new(0, "Big result", "Produce output");
        // 600-char result exceeds 500-byte truncation limit
        let long_result = "x".repeat(600);
        task.complete(long_result);
        plan.add_task(task);

        let ctx = IterationContext::new(1, plan, None, vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        // Should contain truncated result with "..." suffix
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("..."));
        // Should NOT contain the full 600-char string
        assert!(!prompt.contains(&"x".repeat(600)));
        // But should contain 500 chars worth
        assert!(prompt.contains(&"x".repeat(500)));
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
        let long_result =
            format!("{body}\n\n[Full result (12345 chars) saved to artifact: task-0-result.txt]");
        task.complete(long_result);
        plan.add_task(task);

        let ctx = IterationContext::new(1, plan, None, vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        // The body is truncated but the artifact footer survives.
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("saved to artifact: task-0-result.txt"));
        assert!(prompt.contains("12345 chars"));
    }

    #[test]
    fn test_continuation_prompt_urgency_final_attempt() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Task", "Do it");
        task.fail("error");
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        // iteration=2, max=3 → next would be iteration 3 = max, so FINAL ATTEMPT
        let ctx = IterationContext::new(2, plan, Some(fs), vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        assert!(prompt.contains("(FINAL ATTEMPT)"));
    }

    #[test]
    fn test_continuation_prompt_no_urgency_early_iteration() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Task", "Do it");
        task.fail("error");
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "Failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        assert!(!prompt.contains("FINAL ATTEMPT"));
    }

    #[test]
    fn test_continuation_prompt_reuse_guidance_only_with_failed_tasks() {
        // Reuse guidance must only appear when there are failed tasks to
        // selectively retry. Surfacing on succeeded > 0 alone teaches the
        // model the no-op reuse-only plan pattern.
        //
        // This test exercises the "completed tasks but no failures" path:
        // reuse_guidance must NOT appear.
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Completed task", "Done");
        task.complete("Some result");
        plan.add_task(task);

        let ctx = IterationContext::new(1, plan, None, vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        assert!(!prompt.contains("reuse_result_from"));
    }

    #[test]
    fn test_continuation_prompt_reuse_guidance_when_mixed() {
        // Mixed path: some completed + some failed → reuse guidance present
        // to let the coordinator retry failures while carrying completed
        // results forward.
        let mut plan = Plan::new("Goal");
        let mut completed = Task::new(0, "Completed task", "Done");
        completed.complete("Some result");
        plan.add_task(completed);

        let mut failed = Task::new(1, "Failed task", "Broken");
        failed.fail("boom");
        plan.add_task(failed);

        let fs = FailureSummary {
            reasoning: "Partial".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        assert!(prompt.contains("reuse_result_from"));
    }

    #[test]
    fn test_continuation_prompt_no_reuse_guidance_when_all_failed() {
        let mut plan = Plan::new("Goal");
        let mut task = Task::new(0, "Failed task", "Tried");
        task.fail("error");
        plan.add_task(task);

        let fs = FailureSummary {
            reasoning: "All failed".into(),
            gaps: vec![],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        assert!(!prompt.contains("reuse_result_from"));
    }

    #[test]
    fn test_continuation_prompt_mixed_categories() {
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

        let fs = FailureSummary {
            reasoning: "Partial success".into(),
            gaps: vec!["Task 1 failed".into()],
        };
        let ctx = IterationContext::new(1, plan, Some(fs), vec![]);
        let prompt = ctx.build_continuation_prompt(3);

        // All three sections should be present
        assert!(prompt.contains("COMPLETED TASKS"));
        assert!(prompt.contains("Task 0: Completed task"));
        assert!(prompt.contains("FAILED TASKS"));
        assert!(prompt.contains("Task 1: Failed task"));
        assert!(prompt.contains("BLOCKED TASKS"));
        assert!(prompt.contains("Task 2: Blocked task"));

        // Verify ordering: completed before blocked before failed (redesign)
        let completed_pos = prompt.find("COMPLETED TASKS").unwrap();
        let blocked_pos = prompt.find("BLOCKED TASKS").unwrap();
        let redesign_pos = prompt.find("FAILED TASKS").unwrap();
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
        let response = PlanningResponse::StepsPlan {
            goal: "Test reuse".into(),
            steps: vec![
                StepInput::LeafTask {
                    task: "Fresh task".into(),
                    worker: None,
                },
                StepInput::ReuseTask {
                    reuse_result_from: 5,
                },
            ],
            routing_rationale: "test".into(),
            planning_summary: "test".into(),
        };
        let plan = response.into_plan().unwrap();
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].reuse_result_from, None);
        assert_eq!(plan.tasks[1].reuse_result_from, Some(5));
    }

    #[test]
    fn test_into_plan_then_apply_reuse_pipeline() {
        use crate::orchestration::orchestrator::Orchestrator;

        let mut previous = Plan::new("Previous goal");
        let mut prev_task = Task::new(0, "Compute mean", "stats");
        prev_task.complete("42".to_string());
        previous.add_task(prev_task);

        let response = PlanningResponse::StepsPlan {
            goal: "New goal".into(),
            steps: vec![StepInput::ReuseTask {
                reuse_result_from: 0,
            }],
            routing_rationale: "test".into(),
            planning_summary: "test".into(),
        };
        let mut plan = response.into_plan().unwrap();
        assert_eq!(plan.tasks[0].status, TaskStatus::Pending);

        Orchestrator::apply_result_reuse(&mut plan, Some(&previous));

        assert_eq!(plan.tasks[0].status, TaskStatus::Complete);
        assert_eq!(plan.tasks[0].result.as_deref(), Some("42"));
    }

    #[test]
    fn test_flatten_steps_propagates_reuse_result_from() {
        let steps = vec![
            StepInput::ReuseTask {
                reuse_result_from: 2,
            },
            StepInput::LeafTask {
                task: "Fresh analysis".into(),
                worker: Some("analytics".into()),
            },
        ];
        let tasks = flatten_steps(&steps).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].reuse_result_from, Some(2));
        assert_eq!(tasks[1].reuse_result_from, None);
    }

    #[test]
    fn test_reuse_task_serde_roundtrip() {
        let json = r#"{"type": "reuse", "reuse_result_from": 3}"#;
        let step: StepInput = serde_json::from_str(json).unwrap();
        match &step {
            StepInput::ReuseTask { reuse_result_from } => {
                assert_eq!(*reuse_result_from, 3);
            }
            other => panic!("Expected ReuseTask, got {:?}", other),
        }
        let serialized = serde_json::to_string(&step).unwrap();
        let deserialized: StepInput = serde_json::from_str(&serialized).unwrap();
        assert_eq!(step, deserialized);
    }

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
        assert!(!serialized.contains("reuse_result_from"));
    }

    #[test]
    fn test_steps_plan_reuse_into_plan_pipeline() {
        use crate::orchestration::orchestrator::Orchestrator;

        let mut previous = Plan::new("Previous goal");
        let mut prev_task = Task::new(0, "Fetch logs", "observability");
        prev_task.complete("log data here".to_string());
        previous.add_task(prev_task);

        let response = PlanningResponse::StepsPlan {
            goal: "Analyze logs".into(),
            steps: vec![
                StepInput::ReuseTask {
                    reuse_result_from: 0,
                },
                StepInput::LeafTask {
                    task: "Analyze fetched logs".into(),
                    worker: Some("analytics".into()),
                },
            ],
            routing_rationale: "replan".into(),
            planning_summary: "Reuse fetch, fresh analysis".into(),
        };
        let mut plan = response.into_plan().unwrap();
        assert_eq!(plan.tasks[0].reuse_result_from, Some(0));
        assert_eq!(plan.tasks[0].status, TaskStatus::Pending);

        Orchestrator::apply_result_reuse(&mut plan, Some(&previous));

        assert_eq!(plan.tasks[0].status, TaskStatus::Complete);
        assert_eq!(plan.tasks[0].result.as_deref(), Some("log data here"));
        assert_eq!(plan.tasks[1].status, TaskStatus::Pending);
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
            Some(FailureSummary {
                reasoning: "Decent".into(),
                gaps: vec!["Missing detail".into()],
            }),
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

        let ctx = IterationContext::new(1, plan, None, vec![]);
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: IterationContext = serde_json::from_str(&json).unwrap();
        assert!(deserialized.failure_summary.is_none());
    }
}
