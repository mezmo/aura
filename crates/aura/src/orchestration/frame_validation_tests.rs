//! Frame-by-frame validation of the orchestration prompt/context flow.
//!
//! Each test constructs realistic orchestration state and verifies that
//! the rendered output for a specific "frame" (preamble, continuation,
//! session history, worker task) contains the expected artifact-repair
//! data: tool traces, artifact entries, failure categories, structured
//! output, and cross-run references.
//!
//! S2 consolidation: the cases whose asserted substrings the golden-frame
//! snapshot corpus subsumes were deleted (mapping recorded in
//! `context_fixture/DESIGN.md`). Every test remaining here owns coverage
//! the corpus deliberately excludes — gated completed-task tool chains,
//! degenerate inputs the fixture types forbid by construction, the
//! session-history catch-all task render, multi-pattern failure ordering
//! (HashMap-ordered, so not snapshot-stable), and plan-state machinery
//! (`fail_descendants_of`).

use std::collections::HashMap;

use super::config::build_coordinator_preamble;
use super::context::{PinnedGoal, PriorWorkFrame, TokenBudget};
use super::events::RoutingMode;
use super::orchestrator::Orchestrator;
use super::persistence::build_session_context;
use super::persistence::{
    ArtifactEntry, ArtifactKind, ErrorContext, RunManifest, RunStatus, TaskSummary, ToolOutcome,
    ToolTraceEntry,
};
use super::types::{
    FailedTaskRecord, FailureCategory, FailureSummary, IterationContext, Plan, PlanningResponse,
    StepInput, StructuredTaskOutput, Task, TaskState, TaskStatus,
};

// ========================================================================
// Helpers — construct realistic test data
// ========================================================================

fn trace(tool: &str, reasoning: &str, ms: u64, err: Option<&str>) -> ToolTraceEntry {
    ToolTraceEntry {
        tool: tool.into(),
        reasoning: reasoning.into(),
        duration_ms: ms,
        outcome: match err {
            Some(msg) => ToolOutcome::Error {
                message: msg.into(),
            },
            None => ToolOutcome::Success { output_bytes: 4096 },
        },
        artifact_filename: None,
    }
}

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

fn result_artifact(filename: &str, size: u64) -> ArtifactEntry {
    ArtifactEntry {
        filename: filename.into(),
        size_bytes: size,
        kind: ArtifactKind::Result,
    }
}

fn tool_artifact(filename: &str, size: u64, tool: &str) -> ArtifactEntry {
    ArtifactEntry {
        filename: filename.into(),
        size_bytes: size,
        kind: ArtifactKind::ToolOutput {
            tool_name: tool.into(),
        },
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

fn failed_task_summary(
    id: usize,
    desc: &str,
    worker: &str,
    error: &str,
    category: FailureCategory,
    traces: Vec<ToolTraceEntry>,
    partial: Option<&str>,
) -> TaskSummary {
    TaskSummary {
        task_id: id,
        description: desc.into(),
        status: TaskStatus::Failed,
        worker: Some(worker.into()),
        result_preview: None,
        confidence: None,
        failure_category: Some(category),
        error: Some(error.into()),
        error_context: Some(ErrorContext {
            category,
            last_tool_call: traces.last().map(|t| t.tool.clone()),
            attempt_count: 1,
            partial_result: partial.map(|s| s.into()),
        }),
        tool_trace: traces,
        artifacts: vec![],
    }
}

fn blocked_task_summary(id: usize, desc: &str, worker: &str) -> TaskSummary {
    TaskSummary {
        task_id: id,
        description: desc.into(),
        status: TaskStatus::Pending,
        worker: Some(worker.into()),
        result_preview: None,
        confidence: None,
        failure_category: None,
        error: None,
        error_context: None,
        tool_trace: vec![],
        artifacts: vec![],
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
// Frame 4 — Continuation prompt
// ========================================================================

#[test]
fn test_continuation_full_scenario() {
    let mut plan = Plan::new("Investigate elevated error rates in payments service");

    // Task 0: complete with structured output + artifact footer + tool traces
    let mut t0 = Task::new(0, "Search prod logs", "Search prod logs for error patterns");
    let result_with_footer = "Found 47 error groups across 3 services. Top failures: connection timeouts (38%), OOM (12%), TLS (3%). \
        [Full result (3200 chars) saved to artifact: task-0-sre-iter-1-result.txt]";
    t0.complete(result_with_footer.to_string());
    t0.structured_output = Some(StructuredTaskOutput {
        summary: "Found 47 error groups. Top: connection timeouts 38%, OOM 12%, TLS 3%".into(),
        confidence: super::tools::submit_result::Confidence::High,
    });

    // Task 1: failed with tool chain showing success→failure
    let mut t1 = Task::new(
        1,
        "Query deployments",
        "Query deployment history for error window",
    );
    t1.fail(
        "403 Forbidden — service account lacks k8s read permissions".to_string(),
        FailureCategory::AgentError,
    );

    // Task 2: blocked
    let t2 = Task::new(
        2,
        "Correlate events",
        "Correlate deployment events with error rates",
    );

    plan.add_task(t0);
    plan.add_task(t1);
    plan.add_task(t2);

    // Tool traces for continuation
    let mut traces = HashMap::new();
    traces.insert(
        0,
        vec![
            trace_with_artifact(
                "log_search",
                "searching for error patterns in payments-api",
                8200,
                "task-0-sre-iter-1-log_search-0-output.txt",
            ),
            trace(
                "get_metrics",
                "checking connection pool utilization",
                3100,
                None,
            ),
        ],
    );
    traces.insert(
        1,
        vec![
            trace("get_deployments", "checking staging baseline", 1200, None),
            trace(
                "get_deployments",
                "querying prod-us-east-1",
                30200,
                Some("403 Forbidden"),
            ),
        ],
    );

    let failure_history = vec![
        FailedTaskRecord {
            description: "Query deployment history for error window".into(),
            error: "403 Forbidden".into(),
            iteration: 1,
            worker: Some("sre".into()),
            category: FailureCategory::AgentError,
        },
        FailedTaskRecord {
            description: "Query deployment history for error window".into(),
            error: "403 Forbidden".into(),
            iteration: 2,
            worker: Some("sre".into()),
            category: FailureCategory::AgentError,
        },
    ];

    let fs = FailureSummary {
        reasoning: "1 task failed, 1 blocked by dependency".into(),
        gaps: vec!["Deployment history unavailable due to permissions".into()],
    };

    let ctx = IterationContext::new(2, plan, Some(fs), failure_history, traces).with_pinned_goal(
        PinnedGoal::new("Investigate elevated error rates in payments service")
            .expect("non-empty query"),
    );
    let prompt = ctx.build_continuation_prompt(3, true, 2000);

    // Header
    assert!(
        prompt.contains("ITERATION 2 of 3"),
        "iteration header: {}",
        prompt
    );
    assert!(
        prompt.contains(
            "Goal (verbatim from the original request): Investigate elevated error rates"
        ),
        "goal line pinned to the original query"
    );

    // Evidence framing: no coordinator task description replays next to
    // worker evidence in any per-task section.
    assert!(
        !prompt.contains("Search prod logs"),
        "completed task description must not render: {}",
        prompt
    );
    assert!(
        !prompt.contains("Query deployments"),
        "failed task description must not render"
    );
    assert!(
        !prompt.contains("Correlate events"),
        "blocked task description must not render"
    );

    // Completed task with structured output + confidence
    assert!(prompt.contains("confidence: high"), "confidence");
    assert!(
        prompt.contains("Found 47 error groups"),
        "structured summary"
    );
    assert!(
        prompt.contains("task-0-sre-iter-1-result.txt"),
        "artifact footer preserved"
    );

    // Tool chain for completed task
    assert!(
        prompt.contains("Tool chain: log_search"),
        "completed tool chain"
    );
    assert!(
        prompt.contains("searching for error patterns"),
        "tool reasoning in chain"
    );
    assert!(prompt.contains("get_metrics"), "second tool in chain");

    // Failed task with category
    assert!(prompt.contains("failed [agent_error]"), "failure category");
    assert!(prompt.contains("403 Forbidden"), "error message");

    // Tool chain for failed task
    let failed_chain = prompt
        .lines()
        .find(|l| l.contains("Tool chain: get_deployments"));
    assert!(failed_chain.is_some(), "failed task has tool chain");
    let chain = failed_chain.unwrap();
    assert!(
        chain.contains("FAILED: 403 Forbidden"),
        "chain shows failure"
    );

    // Blocked task
    assert!(
        prompt.contains("blocked (dependency failed)"),
        "blocked task"
    );

    // Failure summary + history
    assert!(
        prompt.contains("FAILURE SUMMARY"),
        "failure summary section"
    );
    assert!(
        prompt.contains("FAILURE HISTORY"),
        "failure history section"
    );
    assert!(
        prompt.contains("OBSERVED PATTERNS"),
        "repeated failure patterns"
    );
    assert!(prompt.contains("has failed 2 times"), "failure count");

    // Result forwarding guidance (has failures + completions)
    assert!(
        prompt.contains("Workers cannot see prior iteration results"),
        "result forwarding guidance"
    );

    // Routing tools
    assert!(prompt.contains("respond_directly"), "routing tool");
    assert!(prompt.contains("read_artifact"), "read_artifact directive");
}

// ========================================================================
// Frame 5 — Session history
// ========================================================================

#[test]
fn test_session_history_full_scenario() {
    let manifest = sample_manifest(
        "run_abc123",
        "Investigate elevated error rates in payments service",
        RunStatus::PartialSuccess,
        vec![
            complete_task_summary(
                0,
                "Search prod logs for error patterns",
                "sre",
                "Found 47 error groups. Top: timeouts 38%, OOM 12%",
                "high",
                vec![
                    trace_with_artifact(
                        "log_search",
                        "searching error patterns",
                        8200,
                        "task-0-sre-iter-1-log_search-0-output.txt",
                    ),
                    trace("get_metrics", "checking pool utilization", 3100, None),
                    trace(
                        "analyze_trends",
                        "correlating with deployment window",
                        2500,
                        None,
                    ),
                ],
                vec![
                    result_artifact("task-0-sre-iter-1-result.txt", 3200),
                    tool_artifact(
                        "task-0-sre-iter-1-log_search-0-output.txt",
                        48291,
                        "log_search",
                    ),
                ],
            ),
            failed_task_summary(
                1,
                "Query deployment history",
                "sre",
                "403 Forbidden",
                FailureCategory::AgentError,
                vec![
                    trace("get_deployments", "checking staging", 1200, None),
                    trace(
                        "get_deployments",
                        "querying prod",
                        30200,
                        Some("403 Forbidden"),
                    ),
                ],
                Some("Staging query succeeded (3 deployments found)"),
            ),
            blocked_task_summary(2, "Correlate events", "sre"),
        ],
    );

    let history = build_session_context(&[manifest]);

    // Run header
    assert!(history.contains("run_abc123"), "run_id in header");
    assert!(
        history.contains("Investigate elevated error rates"),
        "goal in header"
    );

    // Completed task — hierarchical view
    assert!(history.contains("Task 0 [sre] — Complete"), "task 0 status");
    assert!(history.contains("(high)"), "confidence tag");
    assert!(history.contains("Found 47 error groups"), "result preview");

    // Tool chain for completed task
    assert!(history.contains("log_search"), "tool in chain");
    assert!(history.contains("get_metrics"), "second tool in chain");
    assert!(history.contains("analyze_trends"), "third tool in chain");
    let chain_line = history
        .lines()
        .find(|l| l.contains("Tool chain:") && l.contains("log_search"));
    assert!(chain_line.is_some(), "tool chain line exists");
    assert!(
        chain_line.unwrap().contains("→"),
        "arrow separator in chain"
    );

    // Artifacts for completed task
    assert!(
        history.contains("task-0-sre-iter-1-result.txt"),
        "result artifact"
    );
    assert!(
        history.contains("task-0-sre-iter-1-log_search-0-output.txt"),
        "tool output artifact"
    );

    // Failed task
    assert!(
        history.contains("Task 1 [sre] — FAILED"),
        "task 1 failed status"
    );
    assert!(history.contains("403 Forbidden"), "error in failed task");
    assert!(
        history.contains("Last tool: get_deployments"),
        "last tool call surfaced"
    );
    assert!(
        history.contains("Staging query succeeded"),
        "partial progress"
    );

    // Failed task tool chain shows failure
    let failed_chain = history
        .lines()
        .find(|l| l.contains("Tool chain:") && l.contains("FAILED"));
    assert!(
        failed_chain.is_some(),
        "failed task chain with FAILED marker"
    );

    // Blocked task
    assert!(history.contains("Task 2 [sre]"), "blocked task present");

    // Cross-run hint
    assert!(
        history.contains("run_abc123"),
        "run_id for cross-run access"
    );
}

// ========================================================================
// Tool output artifact refs in continuation prompt
// ========================================================================

#[test]
fn test_continuation_tool_output_artifacts_visible() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "Search logs", "Search prod logs");
    t.complete(
        "Found errors. [Full result (3200 chars) saved to artifact: task-0-sre-iter-1-result.txt]"
            .to_string(),
    );
    plan.add_task(t);

    let mut traces = HashMap::new();
    traces.insert(
        0,
        vec![
            trace_with_artifact(
                "log_search",
                "searching for errors",
                8200,
                "task-0-sre-iter-1-log_search-0-output.txt",
            ),
            trace_with_artifact(
                "get_metrics",
                "pool utilization",
                3100,
                "task-0-sre-iter-1-get_metrics-1-output.txt",
            ),
            trace("analyze", "correlating", 1500, None),
        ],
    );

    let ctx = IterationContext::new(1, plan, None, vec![], traces);
    let prompt = ctx.build_continuation_prompt(3, true, 2000);

    // Tool chain line present
    assert!(prompt.contains("Tool chain:"), "chain line: {}", prompt);

    // Artifact inventory visible to coordinator for read_artifact calls
    assert!(
        prompt.contains("[Artifact: task-0-sre-iter-1-log_search-0-output.txt"),
        "log_search artifact ref: {}",
        prompt
    );
    assert!(
        prompt.contains("[Artifact: task-0-sre-iter-1-get_metrics-1-output.txt"),
        "get_metrics artifact ref: {}",
        prompt
    );
    assert!(prompt.contains("48000 bytes"), "artifact size");

    // Tool without artifact should NOT produce an [Artifact:] line
    assert_eq!(
        prompt.matches("[Artifact:").count(),
        2,
        "exactly 2 artifact refs (not 3): {}",
        prompt
    );
}

// ========================================================================
// All FailureCategory variants in continuation prompt
// ========================================================================

#[test]
fn test_continuation_all_failure_categories() {
    let categories = vec![
        (FailureCategory::AgentTimeout, "agent_timeout"),
        (FailureCategory::ContextOverflow, "context_overflow"),
        (FailureCategory::DepthExhausted, "depth_exhausted"),
        (FailureCategory::LoopDetected, "loop_detected"),
        (FailureCategory::ProviderOverloaded, "provider_overloaded"),
        (FailureCategory::ProviderAuthError, "provider_auth_error"),
        (FailureCategory::DependencyFailed, "dependency_failed"),
        (FailureCategory::AgentError, "agent_error"),
    ];

    for (category, display) in categories {
        let mut plan = Plan::new("Test goal");
        let mut t = Task::new(0, "failing task", "This task fails");
        t.fail(format!("error for {}", display), category);
        plan.add_task(t);

        let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
        let prompt = ctx.build_continuation_prompt(3, true, 2000);

        assert!(
            prompt.contains(&format!("[{}]", display)),
            "category {} should render as [{}] in: {}",
            display,
            display,
            prompt
        );
    }
}

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
            },
            StepInput::LeafTask {
                task: "Compare captured firmware versions against the golden baseline".to_string(),
                worker: Some("verifier".to_string()),
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
