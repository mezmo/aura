//! Types for orchestration mode task management.
//!
//! This module defines the core types used by the orchestrator to decompose
//! queries into tasks, track their execution, and manage dependencies.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A plan representing a decomposed query.
///
/// The coordinator creates a plan by analyzing the user's query and
/// breaking it down into discrete, actionable tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    /// The original goal/query being addressed.
    pub goal: String,
    /// Ordered list of tasks to accomplish the goal.
    pub tasks: Vec<Task>,
}

impl Plan {
    /// Create a new plan with the given goal.
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
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
    /// The query requires multi-agent orchestration.
    #[serde(rename = "orchestrated")]
    Orchestrated {
        goal: String,
        tasks: Vec<TaskJson>,
        routing_rationale: String,
        /// Natural-language summary of the plan from the coordinator.
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
    /// Convert an `Orchestrated` response into a `Plan`.
    ///
    /// Returns `None` for `Direct` and `Clarification` variants.
    pub fn into_plan(self) -> Option<Plan> {
        match self {
            PlanningResponse::Orchestrated { goal, tasks, .. } => {
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
                    plan.add_task(task);
                }
                Some(plan)
            }
            _ => None,
        }
    }

    /// Human-readable variant name for logging.
    pub fn variant_name(&self) -> &'static str {
        match self {
            PlanningResponse::Direct { .. } => "Direct",
            PlanningResponse::Orchestrated { .. } => "Orchestrated",
            PlanningResponse::Clarification { .. } => "Clarification",
        }
    }

    /// Get the routing rationale regardless of variant.
    pub fn routing_rationale(&self) -> &str {
        match self {
            PlanningResponse::Direct {
                routing_rationale, ..
            } => routing_rationale,
            PlanningResponse::Orchestrated {
                routing_rationale, ..
            } => routing_rationale,
            PlanningResponse::Clarification {
                routing_rationale, ..
            } => routing_rationale,
        }
    }

    /// Get the planning summary (only present on Orchestrated variant).
    pub fn planning_summary(&self) -> Option<&str> {
        match self {
            PlanningResponse::Orchestrated {
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct IterationContext {
    /// Which iteration just completed (1-indexed).
    pub iteration: usize,
    /// The plan from the previous iteration.
    pub previous_plan: Plan,
    /// The evaluation of the previous iteration.
    pub evaluation: EvaluationResult,
    /// Accumulated failure history across all iterations.
    pub failure_history: Vec<FailedTaskRecord>,
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
        }
    }

    /// Build the reflection section for the planning prompt.
    ///
    /// This formats the previous iteration's results and evaluation
    /// in a way that helps the coordinator understand what to fix.
    ///
    /// Key design choice: We include task summaries (status + result length) rather
    /// than truncated results. The evaluation gaps are the primary signal for what
    /// needs improvement. If the coordinator needs full task results, they can
    /// request them via tools.
    pub fn build_reflection_prompt(&self) -> String {
        // Build task summaries: status and result size (not content)
        let task_summaries = self
            .previous_plan
            .tasks
            .iter()
            .map(|t| {
                let status_detail = match t.status {
                    TaskStatus::Complete => {
                        let len = t.result.as_ref().map(|r| r.len()).unwrap_or(0);
                        format!("✓ complete ({} chars)", len)
                    }
                    TaskStatus::Failed => {
                        let err = t.error.as_deref().unwrap_or("unknown");
                        format!("✗ failed: {}", err)
                    }
                    TaskStatus::Pending => "⏳ pending".to_string(),
                    TaskStatus::Running => "▶ running".to_string(),
                };
                format!("- Task {}: {} [{}]", t.id, t.description, status_detail)
            })
            .collect::<Vec<_>>()
            .join("\n");

        let mut prompt = format!(
            r#"
PREVIOUS ATTEMPT (Iteration {iteration}):
Goal: {goal}
Quality Score: {score:.2}

TASKS EXECUTED:
{task_summaries}

EVALUATION:
{reasoning}

GAPS TO ADDRESS:
{gaps}

Create a new plan that addresses these gaps. Consider different approaches or more targeted tasks."#,
            iteration = self.iteration,
            goal = self.previous_plan.goal,
            score = self.evaluation.score,
            task_summaries = task_summaries,
            reasoning = self.evaluation.reasoning,
            gaps = self.evaluation.gaps_as_bullets(),
        );

        // Append failure history if present
        if !self.failure_history.is_empty() {
            prompt.push_str("\n\nFAILURE HISTORY:");
            for record in &self.failure_history {
                let worker_info = record
                    .worker
                    .as_deref()
                    .map(|w| format!(" (worker: {})", w))
                    .unwrap_or_default();
                prompt.push_str(&format!(
                    "\n- Iteration {}: \"{}\"{} — {}",
                    record.iteration, record.description, worker_info, record.error,
                ));
            }

            // Identify repeated failures (same description appearing multiple times)
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
                prompt.push_str("\n\nREPEATED FAILURES:");
                for (desc, count) in &repeated {
                    prompt.push_str(&format!(
                        "\n- \"{}\" has failed {} times — consider a fundamentally different approach",
                        desc, count,
                    ));
                }
            }
        }

        prompt
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
        let mut plan = Plan::new("Investigate the issue");
        let mut task = Task::new(0, "Gather logs", "Get system logs");
        task.complete("Here are the logs...".to_string());
        plan.add_task(task);

        let eval = EvaluationResult::new(0.3, "Response lacks detail").with_gaps(vec![
            "Missing root cause".into(),
            "No remediation steps".into(),
        ]);

        let ctx = IterationContext::new(1, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt();

        // Verify key sections are present
        assert!(prompt.contains("PREVIOUS ATTEMPT (Iteration 1)"));
        assert!(prompt.contains("Goal: Investigate the issue"));
        assert!(prompt.contains("Quality Score: 0.30"));
        assert!(prompt.contains("TASKS EXECUTED:"));
        assert!(prompt.contains("Task 0: Gather logs"));
        assert!(prompt.contains("✓ complete"));
        assert!(prompt.contains("EVALUATION:"));
        assert!(prompt.contains("Response lacks detail"));
        assert!(prompt.contains("GAPS TO ADDRESS:"));
        assert!(prompt.contains("- Missing root cause"));
        assert!(prompt.contains("- No remediation steps"));
        assert!(prompt.contains("addresses these gaps"));
    }

    #[test]
    fn test_iteration_context_reflection_prompt_no_gaps() {
        let mut plan = Plan::new("Simple query");
        let mut task = Task::new(0, "Execute", "Run query");
        task.complete("Done".to_string());
        plan.add_task(task);

        let eval = EvaluationResult::new(0.6, "Partially complete");

        let ctx = IterationContext::new(2, plan, eval, vec![]);
        let prompt = ctx.build_reflection_prompt();

        // Should show "No specific gaps identified" when gaps is empty
        assert!(prompt.contains("- No specific gaps identified"));
        assert!(prompt.contains("Iteration 2"));
        assert!(prompt.contains("TASKS EXECUTED:"));
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
        let prompt = ctx.build_reflection_prompt();

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
        let prompt = ctx.build_reflection_prompt();

        assert!(prompt.contains("FAILURE HISTORY:"));
        assert!(prompt.contains("REPEATED FAILURES:"));
        assert!(prompt.contains("\"Fetch data\" has failed 2 times"));
        assert!(prompt.contains("fundamentally different approach"));
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
                },
                TaskJson {
                    id: 1,
                    description: "Analyze".to_string(),
                    rationale: None,
                    dependencies: Some(vec![0]),
                    worker: None,
                },
            ],
            routing_rationale: "Requires tool execution".to_string(),
            planning_summary: "Fetch and analyze logs".to_string(),
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
                },
                TaskJson {
                    id: 1,
                    description: "Task B".to_string(),
                    rationale: Some("Reason B".to_string()),
                    dependencies: Some(vec![0]),
                    worker: Some("ops".to_string()),
                },
            ],
            routing_rationale: "Needs orchestration".to_string(),
            planning_summary: "Execute tasks A and B".to_string(),
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
}
