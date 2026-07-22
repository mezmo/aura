//! Frame-by-frame validation of the orchestration prompt/context flow.
//!
//! Each test constructs realistic orchestration state and verifies that
//! the rendered output for a specific "frame" (preamble, continuation,
//! session history, worker task) contains the expected artifact-repair
//! data: tool traces, artifact entries, failure categories, structured
//! output, and cross-run references.
//!
//! S2 consolidation (and S17 follow-up): the cases whose asserted substrings
//! the golden-frame snapshot corpus subsumes were deleted or replaced with
//! snapshot fixtures in `context_fixture/golden_tests.rs` (coverage mapping
//! recorded in `context_fixture/MANIFEST.md`). Every test remaining here owns
//! coverage the corpus deliberately excludes — degenerate inputs the fixture
//! types forbid by construction, multi-pattern failure ordering
//! (HashMap-ordered, so not snapshot-stable), and plan-state machinery
//! (`fail_descendants_of`).

use std::collections::HashMap;

use super::config::build_coordinator_preamble;
use super::context::{PinnedGoal, PriorWorkFrame, TokenBudget};
use super::events::RoutingMode;
use super::orchestrator::Orchestrator;
use super::persistence::build_session_context;
use super::persistence::{
    ArtifactEntry, ErrorContext, RunManifest, RunStatus, TaskSummary, ToolOutcome, ToolTraceEntry,
};
use super::types::{
    FailedTaskRecord, FailureCategory, IterationContext, Plan, PlanningResponse, StepInput, Task,
    TaskState, TaskStatus,
};

// ========================================================================
// Helpers — construct realistic test data
// ========================================================================

fn trace_with_artifact(tool: &str, reasoning: &str, ms: u64, filename: &str) -> ToolTraceEntry {
    ToolTraceEntry {
        tool: tool.into(),
        reasoning: reasoning.into(),
        duration_ms: ms,
        outcome: ToolOutcome::Success {
            output_bytes: 48000,
        },
        artifact_filename: Some(filename.into()),
    }
}

fn complete_task_summary(
    id: usize,
    desc: &str,
    worker: &str,
    preview: &str,
    confidence: &str,
    traces: Vec<ToolTraceEntry>,
    artifacts: Vec<ArtifactEntry>,
) -> TaskSummary {
    TaskSummary {
        task_id: id,
        description: desc.into(),
        status: TaskStatus::Complete,
        worker: Some(worker.into()),
        result_preview: Some(preview.into()),
        confidence: Some(confidence.into()),
        failure_category: None,
        error: None,
        error_context: None,
        tool_trace: traces,
        artifacts,
    }
}

fn sample_manifest(
    run_id: &str,
    goal: &str,
    status: RunStatus,
    tasks: Vec<TaskSummary>,
) -> RunManifest {
    RunManifest {
        run_id: run_id.into(),
        session_id: Some("session_test".into()),
        timestamp: "2026-04-30T14:00:00Z".into(),
        goal: goal.into(),
        status,
        iterations: 1,
        routing_mode: Some(RoutingMode::Orchestrated),
        outcome: Some("2/3 tasks completed".into()),
        response_summary: None,
        task_summaries: tasks,
        artifact_paths: vec![],
    }
}

// ========================================================================
// Soft failure and structured-output edge cases
// ========================================================================

#[test]
fn test_continuation_soft_failure_without_structured_output() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "Inconclusive task", "Investigate ambiguous signal");
    t.fail(
        "Worker did not call submit_result".to_string(),
        FailureCategory::SoftFailure,
    );
    plan.add_task(t);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3, false, 2000);

    // SoftFailure without structured output falls back to bracket format
    assert!(
        prompt.contains("[soft_failure]"),
        "bracket format: {}",
        prompt
    );
    assert!(
        prompt.contains("Worker did not call submit_result"),
        "error text"
    );
}

// ========================================================================
// Frame 2b — Decision turn recording (R3b acceptance)
// ========================================================================

/// R3b acceptance: after a `create_plan` decision, the task description
/// appears at most once across the accumulated conversation and the next
/// continuation prompt (`docs/redesign/ARCHITECTURE.md` section 2.3).
///
/// The accumulated conversation for the planning iteration is exactly what
/// `plan_with_routing` records: the planning prompt as the user turn (built
/// before the plan exists, so it cannot carry the description) and the
/// compact decision text as the assistant turn.
#[test]
fn test_task_description_appears_at_most_once_across_conversation_and_continuation() {
    const TASK_DESCRIPTION: &str = "Inventory the VLAN-4093 switch fabric and capture \
         firmware versions from every distribution switch";
    let query = "Audit the network fabric for firmware drift";

    let decision = PlanningResponse::StepsPlan {
        goal: query.to_string(),
        steps: vec![
            StepInput::LeafTask {
                task: TASK_DESCRIPTION.to_string(),
                worker: Some("operator".to_string()),
                named_check: None,
            },
            StepInput::LeafTask {
                task: "Compare captured firmware versions against the golden baseline".to_string(),
                worker: Some("verifier".to_string()),
                named_check: None,
            },
        ],
        routing_rationale: "Firmware inventory requires switch access through tools.".to_string(),
        planning_summary: "Inventory the fabric, then verify against baseline.".to_string(),
    };

    // The conversation as plan_with_routing accumulates it for iteration 1.
    let planning_prompt = Orchestrator::build_planning_wrapper(query, "", "");
    let decision_turn = Orchestrator::compact_decision_turn(&decision, "");
    assert_eq!(
        decision_turn,
        "create_plan: 2 tasks (operator, verifier). \
         Rationale: Firmware inventory requires switch access through tools.",
        "the recorded assistant turn is the compact decision text"
    );

    // Execute the plan and render the next continuation prompt.
    let mut plan = decision.into_plan().expect("two-leaf plan flattens");
    plan.tasks[0].complete(
        "Collected firmware versions from 14 distribution switches; 3 lag the baseline."
            .to_string(),
    );
    plan.tasks[1].complete("All 3 lagging switches confirmed below golden baseline.".to_string());
    let ctx = IterationContext::new(1, plan, None, Vec::new(), HashMap::new())
        .with_pinned_goal(PinnedGoal::new(query).expect("non-empty query"));
    let continuation = ctx.build_continuation_prompt(3, false, 2000);

    assert!(
        !decision_turn.contains(TASK_DESCRIPTION),
        "recorded decision turn must not replay the task description"
    );
    assert!(
        !continuation.contains(TASK_DESCRIPTION),
        "continuation prompt must not replay the task description"
    );
    let accumulated = format!("{planning_prompt}\n{decision_turn}\n{continuation}");
    assert!(
        accumulated.matches(TASK_DESCRIPTION).count() <= 1,
        "task description appears at most once across conversation + continuation"
    );
}

/// The compact recorder's fallback tiers: a decision `CoordinatorTurn`
/// rejects degrades to the model's streamed text, then to the bare
/// variant name — never a failed run. Owns the MANIFEST §4 exclusion
/// (the corpus's `PlanDecision` forbids degenerate decisions by
/// construction, so no fixture can reach these arms).
#[test]
fn test_compact_decision_turn_fallback_tiers() {
    let degenerate = PlanningResponse::StepsPlan {
        goal: "Audit ingest".to_string(),
        steps: vec![StepInput::LeafTask {
            task: "Enumerate pods".to_string(),
            worker: None,
            named_check: None,
        }],
        routing_rationale: "  ".to_string(),
        planning_summary: String::new(),
    };
    assert_eq!(
        Orchestrator::compact_decision_turn(&degenerate, "I will plan the pod audit now."),
        "I will plan the pod audit now.",
        "model-text tier records the streamed narration"
    );
    assert_eq!(
        Orchestrator::compact_decision_turn(&degenerate, " \n"),
        "StepsPlan",
        "variant-name tier is the structurally task-body-free floor"
    );
}

// ========================================================================
// Frame 1 — Preamble additional coverage
// ========================================================================

#[test]
fn test_preamble_empty_system_prompt() {
    let preamble = build_coordinator_preamble("", false, false);

    assert!(
        preamble.contains("read_artifact"),
        "artifact tool always present"
    );
    assert!(
        preamble.contains("respond_directly"),
        "routing tools present"
    );
}

// ========================================================================
// Frame 4 — Continuation prompt branch gaps
// ========================================================================

#[test]
fn test_continuation_multiple_repeated_failure_patterns() {
    let mut plan = Plan::new("Test goal");
    let mut t0 = Task::new(0, "task A", "First task");
    t0.fail("timeout".to_string(), FailureCategory::AgentTimeout);
    let mut t1 = Task::new(1, "task B", "Second task");
    t1.fail("403".to_string(), FailureCategory::ProviderAuthError);
    plan.add_task(t0);
    plan.add_task(t1);

    let history = vec![
        FailedTaskRecord {
            description: "First task".into(),
            error: "timeout".into(),
            iteration: 1,
            worker: None,
            category: FailureCategory::AgentTimeout,
        },
        FailedTaskRecord {
            description: "First task".into(),
            error: "timeout".into(),
            iteration: 2,
            worker: None,
            category: FailureCategory::AgentTimeout,
        },
        FailedTaskRecord {
            description: "Second task".into(),
            error: "403".into(),
            iteration: 1,
            worker: None,
            category: FailureCategory::ProviderAuthError,
        },
        FailedTaskRecord {
            description: "Second task".into(),
            error: "403".into(),
            iteration: 2,
            worker: None,
            category: FailureCategory::ProviderAuthError,
        },
    ];

    let ctx = IterationContext::new(2, plan, None, history, HashMap::new());
    let prompt = ctx.build_continuation_prompt(3, false, 2000);

    assert!(
        prompt.contains("OBSERVED PATTERNS"),
        "patterns section: {}",
        prompt
    );
    assert!(
        prompt.contains("\"First task\" has failed 2 times"),
        "first pattern"
    );
    assert!(
        prompt.contains("\"Second task\" has failed 2 times"),
        "second pattern"
    );
}

/// A completed task whose result is whitespace-only renders the bare
/// correlation label with the artifact inventory and no evidence line.
/// Owns the MANIFEST §3 exclusion (the corpus composes `EvidenceText`,
/// which forbids whitespace results by construction, so no fixture can
/// reach this arm).
#[test]
fn test_continuation_whitespace_only_result_renders_bare_label() {
    let mut plan = Plan::new("Test goal");
    let mut task = Task::new(0, "gather logs", "Collect the logs").with_worker("analyst");
    task.complete("   \n".to_string());
    plan.add_task(task);

    let traces = HashMap::from([(
        0usize,
        vec![trace_with_artifact(
            "log_search",
            "capture the raw logs",
            900,
            "task-0-analyst-iter-1-log_search-0-output.txt",
        )],
    )]);
    let ctx = IterationContext::new(1, plan, None, Vec::new(), traces);
    let prompt = ctx.build_continuation_prompt(3, false, 2000);

    assert_eq!(
        prompt.matches("- Task 0").count(),
        1,
        "exactly one completed entry: {prompt}"
    );
    assert!(
        prompt.contains(
            "- Task 0 (analyst)\n    [Artifact: task-0-analyst-iter-1-log_search-0-output.txt"
        ),
        "artifact inventory follows the bare label directly — no evidence line: {prompt}"
    );
}

// ========================================================================
// Frame 5 — Session history branch gaps
// ========================================================================

#[test]
fn test_session_history_empty_manifests() {
    let history = build_session_context(&[]);
    assert!(history.is_empty(), "empty manifests returns empty string");
}

#[test]
fn test_session_history_complete_task_no_preview_no_confidence() {
    let manifest = sample_manifest(
        "run_1",
        "Test goal",
        RunStatus::Success,
        vec![TaskSummary {
            task_id: 0,
            description: "Bare task".into(),
            status: TaskStatus::Complete,
            worker: Some("sre".into()),
            result_preview: None,
            confidence: None,
            failure_category: None,
            error: None,
            error_context: None,
            tool_trace: vec![],
            artifacts: vec![],
        }],
    );

    let history = build_session_context(&[manifest]);
    assert!(history.contains("Task 0 [sre] — Complete"), "status line");
    assert!(
        !history.contains("Summary:"),
        "no summary line without preview"
    );
    assert!(!history.contains("Tool chain:"), "no chain without traces");
}

#[test]
fn test_session_history_failed_task_no_error_no_context() {
    let manifest = sample_manifest(
        "run_1",
        "Test goal",
        RunStatus::Failed,
        vec![TaskSummary {
            task_id: 0,
            description: "Failed bare".into(),
            status: TaskStatus::Failed,
            worker: Some("sre".into()),
            result_preview: None,
            confidence: None,
            failure_category: Some(FailureCategory::AgentTimeout),
            error: None,
            error_context: None,
            tool_trace: vec![],
            artifacts: vec![],
        }],
    );

    let history = build_session_context(&[manifest]);
    assert!(history.contains("FAILED"), "failed status");
    assert!(history.contains("agent_timeout"), "category tag");
    assert!(!history.contains("Error:"), "no error line when None");
    assert!(
        !history.contains("Partial progress:"),
        "no partial when no context"
    );
}

#[test]
fn test_session_history_manifest_outcome_none() {
    let mut manifest = sample_manifest(
        "run_1",
        "Test goal",
        RunStatus::Success,
        vec![complete_task_summary(
            0,
            "task",
            "sre",
            "done",
            "high",
            vec![],
            vec![],
        )],
    );
    manifest.outcome = None;

    let history = build_session_context(&[manifest]);
    assert!(history.contains("Test goal"), "goal present");
    assert!(!history.contains("Outcome:"), "no outcome line when None");
}

#[test]
fn test_session_history_error_context_without_partial_result() {
    let manifest = sample_manifest(
        "run_1",
        "Test",
        RunStatus::Failed,
        vec![TaskSummary {
            task_id: 0,
            description: "Failed task".into(),
            status: TaskStatus::Failed,
            worker: Some("sre".into()),
            result_preview: None,
            confidence: None,
            failure_category: Some(FailureCategory::AgentError),
            error: Some("Connection refused".into()),
            error_context: Some(ErrorContext {
                category: FailureCategory::AgentError,
                last_tool_call: Some("get_metrics".into()),
                attempt_count: 2,
                partial_result: None,
            }),
            tool_trace: vec![],
            artifacts: vec![],
        }],
    );

    let history = build_session_context(&[manifest]);
    assert!(history.contains("Connection refused"), "error present");
    assert!(
        !history.contains("Partial progress:"),
        "no partial line when None"
    );
}

#[test]
fn test_session_history_running_task_status() {
    let manifest = sample_manifest(
        "run_1",
        "Test",
        RunStatus::PartialSuccess,
        vec![TaskSummary {
            task_id: 0,
            description: "Stuck task".into(),
            status: TaskStatus::Running,
            worker: Some("sre".into()),
            result_preview: None,
            confidence: None,
            failure_category: None,
            error: None,
            error_context: None,
            tool_trace: vec![],
            artifacts: vec![],
        }],
    );

    let history = build_session_context(&[manifest]);
    assert!(
        history.contains("Task 0 [sre] — running"),
        "Running status renders lowercase: {}",
        prompt_excerpt(&history)
    );
}

// ========================================================================
// Helper
// ========================================================================

fn prompt_excerpt(s: &str) -> &str {
    if s.len() > 200 { &s[..200] } else { s }
}

// ========================================================================
// Frame 3b — Worker prior-work frame (R3c acceptance)
// ========================================================================

#[test]
fn worker_frame_direct_deps_always_admitted_transitive_budget_trimmed_first() {
    use super::context::{AncestorDistance, DependencyRelation, EvidenceEntry, EvidenceText};
    use super::context::{CorrelationLabel, TaskId, WorkerRole};

    fn make_entry(
        id: usize,
        relation: DependencyRelation,
        body: &str,
    ) -> super::context::PriorWorkEntry {
        super::context::PriorWorkEntry {
            label: CorrelationLabel {
                task: TaskId::new(id),
                worker: Some(WorkerRole::new("operator").unwrap()),
            },
            relation,
            evidence: EvidenceEntry::InlineResult {
                result: EvidenceText::new(body).unwrap(),
                claim: None,
            },
            named_check: None,
        }
    }

    let entries = vec![
        make_entry(0, DependencyRelation::Direct, "direct-0"),
        make_entry(1, DependencyRelation::Direct, "direct-1"),
        make_entry(
            2,
            DependencyRelation::Transitive {
                distance: AncestorDistance::new(2).unwrap(),
            },
            "transitive-2",
        ),
    ];

    // Tight budget: keep direct floor, evict all transitive.
    let frame = PriorWorkFrame::assemble(entries, TokenBudget::new(50).unwrap()).unwrap();
    let ids: Vec<usize> = frame
        .entries()
        .iter()
        .map(|e| e.label.task.to_string().parse().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![0, 1],
        "direct entries survive tight budget: {ids:?}"
    );
}

#[test]
fn worker_frame_empty_ancestry_returns_none_no_frame_render() {
    let mut plan = Plan::new("Standalone task");
    let t0 = Task::new(0, "unstarted predecessor", "Not complete");
    let mut t1 = Task::new(1, "current task", "Does something");
    t1.dependencies = vec![0];
    plan.add_task(t0);
    plan.add_task(t1);

    assert!(
        Orchestrator::build_task_context(&plan, 1).is_none(),
        "no completed ancestors means no frame"
    );
}

#[test]
fn test_fail_descendants_of_marks_pending_descendants_dependency_failed_skip_complete_running_failed()
 {
    let mut plan = Plan::new("Chain");

    let mut a = Task::new(0, "A", "Root task");
    a.fail("root failed".to_string(), FailureCategory::AgentError);

    let mut b = Task::new(1, "B", "Depends on A");
    b.dependencies = vec![0];

    let mut c = Task::new(2, "C", "Depends on B");
    c.dependencies = vec![1];

    // Sibling D is complete — should stay complete.
    let mut d = Task::new(3, "D", "Already complete");
    d.complete("done".to_string());

    // Sibling E is already failed with a different category — should stay unchanged.
    let mut e = Task::new(4, "E", "Already failed");
    e.fail("existing error".to_string(), FailureCategory::AgentTimeout);

    plan.add_task(a);
    plan.add_task(b);
    plan.add_task(c);
    plan.add_task(d);
    plan.add_task(e);

    Orchestrator::fail_descendants_of(&mut plan, 0);

    let get = |id: usize| plan.tasks.iter().find(|t| t.id == id).unwrap();

    match &get(1).state {
        TaskState::Failed { error, category } => {
            assert_eq!(*category, FailureCategory::DependencyFailed);
            assert!(error.contains("ancestor task 0 failed"));
        }
        _ => panic!("B should be Failed"),
    }
    match &get(2).state {
        TaskState::Failed { error, category } => {
            assert_eq!(*category, FailureCategory::DependencyFailed);
            assert!(error.contains("ancestor task 0 failed"));
        }
        _ => panic!("C should be Failed"),
    }
    assert!(
        matches!(get(3).state, TaskState::Complete { .. }),
        "complete sibling stays complete"
    );
    match &get(4).state {
        TaskState::Failed { category, .. } => {
            assert_eq!(
                *category,
                FailureCategory::AgentTimeout,
                "already-failed category must not change"
            );
        }
        _ => panic!("E should stay Failed"),
    }
}

#[test]
fn test_fail_descendants_of_is_idempotent() {
    let mut plan = Plan::new("Chain");

    let mut a = Task::new(0, "A", "Root task");
    a.fail("root failed".to_string(), FailureCategory::AgentError);

    let mut b = Task::new(1, "B", "Depends on A");
    b.dependencies = vec![0];

    plan.add_task(a);
    plan.add_task(b);

    Orchestrator::fail_descendants_of(&mut plan, 0);
    let first_error = match &plan.tasks[1].state {
        TaskState::Failed { error, .. } => error.clone(),
        _ => panic!("B should be Failed"),
    };

    Orchestrator::fail_descendants_of(&mut plan, 0);
    let second_error = match &plan.tasks[1].state {
        TaskState::Failed { error, .. } => error.clone(),
        _ => panic!("B should still be Failed"),
    };

    assert_eq!(first_error, second_error);
}
