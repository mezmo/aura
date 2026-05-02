//! Frame-by-frame validation of the orchestration prompt/context flow.
//!
//! Each test constructs realistic orchestration state and verifies that
//! the rendered output for a specific "frame" (preamble, continuation,
//! session history, worker task) contains the expected artifact-repair
//! data: tool traces, artifact entries, failure categories, structured
//! output, and cross-run references.

use std::collections::HashMap;

use super::config::OrchestrationConfig;
use super::events::RoutingMode;
use super::orchestrator::Orchestrator;
use super::persistence::{
    ArtifactEntry, ArtifactKind, ErrorContext, RunManifest, RunStatus, TaskSummary, ToolOutcome,
    ToolTraceEntry,
};
use super::templates::{render_worker_task_prompt, WorkerTaskVars};
use super::types::{
    FailedTaskRecord, FailureCategory, FailureSummary, IterationContext, Plan,
    StructuredTaskOutput, Task, TaskStatus,
};
use super::persistence::build_session_context;

// ========================================================================
// Helpers — construct realistic test data
// ========================================================================

fn trace(tool: &str, reasoning: &str, ms: u64, err: Option<&str>) -> ToolTraceEntry {
    ToolTraceEntry {
        tool: tool.into(),
        reasoning: reasoning.into(),
        duration_ms: ms,
        outcome: match err {
            Some(msg) => ToolOutcome::Error { message: msg.into() },
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
        outcome: ToolOutcome::Success { output_bytes: 48000 },
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
    let mut t1 = Task::new(1, "Query deployments", "Query deployment history for error window");
    t1.fail(
        "403 Forbidden — service account lacks k8s read permissions".to_string(),
        FailureCategory::AgentError,
    );

    // Task 2: blocked
    let t2 = Task::new(2, "Correlate events", "Correlate deployment events with error rates");

    plan.add_task(t0);
    plan.add_task(t1);
    plan.add_task(t2);

    // Tool traces for continuation
    let mut traces = HashMap::new();
    traces.insert(0, vec![
        trace_with_artifact("log_search", "searching for error patterns in payments-api", 8200, "task-0-sre-iter-1-log_search-0-output.txt"),
        trace("get_metrics", "checking connection pool utilization", 3100, None),
    ]);
    traces.insert(1, vec![
        trace("get_deployments", "checking staging baseline", 1200, None),
        trace("get_deployments", "querying prod-us-east-1", 30200, Some("403 Forbidden")),
    ]);

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

    let ctx = IterationContext::new(2, plan, Some(fs), failure_history, traces);
    let prompt = ctx.build_continuation_prompt(3);

    // Header
    assert!(prompt.contains("ITERATION 2 of 3"), "iteration header: {}", prompt);
    assert!(prompt.contains("Goal: Investigate elevated error rates"), "goal");

    // Completed task with structured output + confidence
    assert!(prompt.contains("confidence: high"), "confidence");
    assert!(prompt.contains("Found 47 error groups"), "structured summary");
    assert!(prompt.contains("task-0-sre-iter-1-result.txt"), "artifact footer preserved");

    // Tool chain for completed task
    assert!(prompt.contains("Tool chain: log_search"), "completed tool chain");
    assert!(prompt.contains("searching for error patterns"), "tool reasoning in chain");
    assert!(prompt.contains("get_metrics"), "second tool in chain");

    // Failed task with category
    assert!(prompt.contains("failed [agent_error]"), "failure category");
    assert!(prompt.contains("403 Forbidden"), "error message");

    // Tool chain for failed task
    let failed_chain = prompt.lines().find(|l| l.contains("Tool chain: get_deployments"));
    assert!(failed_chain.is_some(), "failed task has tool chain");
    let chain = failed_chain.unwrap();
    assert!(chain.contains("FAILED: 403 Forbidden"), "chain shows failure");

    // Blocked task
    assert!(prompt.contains("blocked (dependency failed)"), "blocked task");

    // Failure summary + history
    assert!(prompt.contains("FAILURE SUMMARY"), "failure summary section");
    assert!(prompt.contains("FAILURE HISTORY"), "failure history section");
    assert!(prompt.contains("OBSERVED PATTERNS"), "repeated failure patterns");
    assert!(prompt.contains("has failed 2 times"), "failure count");

    // Reuse guidance (has failures + completions)
    assert!(prompt.contains("reuse_result_from"), "reuse guidance");

    // Routing tools
    assert!(prompt.contains("respond_directly"), "routing tool");
    assert!(prompt.contains("read_artifact"), "read_artifact directive");
}

#[test]
fn test_continuation_final_attempt_urgency() {
    let mut plan = Plan::new("Simple goal");
    let mut t = Task::new(0, "task", "do something");
    t.complete("done".to_string());
    plan.add_task(t);

    let ctx = IterationContext::new(3, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    assert!(prompt.contains("(FINAL ATTEMPT)"), "urgency marker: {}", prompt);
}

#[test]
fn test_continuation_mixed_structured_and_raw() {
    let mut plan = Plan::new("Mixed output test");

    // Task 0: structured output via submit_result
    let mut t0 = Task::new(0, "structured", "Task with structured output");
    t0.complete("Full detailed result from structured output".to_string());
    t0.structured_output = Some(StructuredTaskOutput {
        summary: "Concise structured summary".into(),
        confidence: super::tools::submit_result::Confidence::Medium,
    });

    // Task 1: raw output (no submit_result)
    let mut t1 = Task::new(1, "raw", "Task without structured output");
    t1.complete("Raw unstructured worker output text here".to_string());

    plan.add_task(t0);
    plan.add_task(t1);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    // Structured path with no artifact: inlines full result, not summary
    assert!(prompt.contains("Full detailed result from structured output"), "full result inlined");
    assert!(prompt.contains("confidence: medium"), "confidence from structured");
    // Summary is NOT shown when result fits inline (no artifact)
    assert!(!prompt.contains("Concise structured summary"), "summary not shown when no artifact");

    // Raw path: shows full result directly
    assert!(prompt.contains("Raw unstructured worker output"), "raw output");
    // Raw path should NOT contain "confidence:"
    let raw_section = prompt.lines().any(|l| l.contains("Raw unstructured") && !l.contains("confidence:"));
    assert!(raw_section, "no confidence on raw task");
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
                    trace_with_artifact("log_search", "searching error patterns", 8200, "task-0-sre-iter-1-log_search-0-output.txt"),
                    trace("get_metrics", "checking pool utilization", 3100, None),
                    trace("analyze_trends", "correlating with deployment window", 2500, None),
                ],
                vec![
                    result_artifact("task-0-sre-iter-1-result.txt", 3200),
                    tool_artifact("task-0-sre-iter-1-log_search-0-output.txt", 48291, "log_search"),
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
                    trace("get_deployments", "querying prod", 30200, Some("403 Forbidden")),
                ],
                Some("Staging query succeeded (3 deployments found)"),
            ),
            blocked_task_summary(2, "Correlate events", "sre"),
        ],
    );

    let history = build_session_context(&[manifest]);

    // Run header
    assert!(history.contains("run_abc123"), "run_id in header");
    assert!(history.contains("Investigate elevated error rates"), "goal in header");

    // Completed task — hierarchical view
    assert!(history.contains("Task 0 [sre] — Complete"), "task 0 status");
    assert!(history.contains("(high)"), "confidence tag");
    assert!(history.contains("Found 47 error groups"), "result preview");

    // Tool chain for completed task
    assert!(history.contains("log_search"), "tool in chain");
    assert!(history.contains("get_metrics"), "second tool in chain");
    assert!(history.contains("analyze_trends"), "third tool in chain");
    let chain_line = history.lines().find(|l| l.contains("Tool chain:") && l.contains("log_search"));
    assert!(chain_line.is_some(), "tool chain line exists");
    assert!(chain_line.unwrap().contains("→"), "arrow separator in chain");

    // Artifacts for completed task
    assert!(history.contains("task-0-sre-iter-1-result.txt"), "result artifact");
    assert!(history.contains("task-0-sre-iter-1-log_search-0-output.txt"), "tool output artifact");

    // Failed task
    assert!(history.contains("Task 1 [sre] — FAILED"), "task 1 failed status");
    assert!(history.contains("403 Forbidden"), "error in failed task");
    assert!(history.contains("Last tool: get_deployments"), "last tool call surfaced");
    assert!(history.contains("Staging query succeeded"), "partial progress");

    // Failed task tool chain shows failure
    let failed_chain = history.lines().find(|l| l.contains("Tool chain:") && l.contains("FAILED"));
    assert!(failed_chain.is_some(), "failed task chain with FAILED marker");

    // Blocked task
    assert!(history.contains("Task 2 [sre]"), "blocked task present");

    // Cross-run hint
    assert!(history.contains("run_abc123"), "run_id for cross-run access");
}

#[test]
fn test_session_history_direct_response_run() {
    let manifest = RunManifest {
        run_id: "run_direct".into(),
        session_id: Some("session_test".into()),
        timestamp: "2026-04-30T15:00:00Z".into(),
        goal: "What is 2+2?".into(),
        status: RunStatus::Success,
        iterations: 0,
        routing_mode: Some(RoutingMode::DirectAnswer),
        outcome: Some("Answered directly".into()),
        response_summary: Some("The answer is 4".into()),
        task_summaries: vec![],
        artifact_paths: vec![],
    };

    let history = build_session_context(&[manifest]);

    assert!(history.contains("What is 2+2?"), "goal");
    assert!(history.contains("The answer is 4"), "response summary");
    assert!(history.contains("Answered directly"), "outcome string");
}

#[test]
fn test_session_history_multi_run_chronological() {
    let m1 = RunManifest {
        run_id: "run_001".into(),
        session_id: Some("s".into()),
        timestamp: "2026-04-30T10:00:00Z".into(),
        goal: "First query".into(),
        status: RunStatus::Success,
        iterations: 1,
        routing_mode: Some(RoutingMode::Orchestrated),
        outcome: Some("1/1 tasks completed".into()),
        response_summary: None,
        task_summaries: vec![complete_task_summary(0, "task A", "w", "done A", "high", vec![], vec![])],
        artifact_paths: vec![],
    };
    let m2 = RunManifest {
        run_id: "run_002".into(),
        session_id: Some("s".into()),
        timestamp: "2026-04-30T11:00:00Z".into(),
        goal: "Second query".into(),
        status: RunStatus::Failed,
        iterations: 2,
        routing_mode: Some(RoutingMode::Orchestrated),
        outcome: Some("0/2 tasks completed".into()),
        response_summary: None,
        task_summaries: vec![],
        artifact_paths: vec![],
    };
    let m3 = RunManifest {
        run_id: "run_003".into(),
        session_id: Some("s".into()),
        timestamp: "2026-04-30T12:00:00Z".into(),
        goal: "Third query".into(),
        status: RunStatus::Success,
        iterations: 1,
        routing_mode: Some(RoutingMode::DirectAnswer),
        outcome: Some("Answered directly".into()),
        response_summary: Some("The result is X".into()),
        task_summaries: vec![],
        artifact_paths: vec![],
    };

    // build_session_context expects most-recent-first (as returned by load_session_manifests)
    let history = build_session_context(&[m3, m2, m1]);

    // All three runs present
    assert!(history.contains("First query"), "run 1 goal");
    assert!(history.contains("Second query"), "run 2 goal");
    assert!(history.contains("Third query"), "run 3 goal");
    assert!(history.contains("3 previous"), "turn count");

    // Verify rendered in chronological order (earliest first) despite desc input
    let first_pos = history.find("First query").unwrap();
    let third_pos = history.find("Third query").unwrap();
    assert!(first_pos < third_pos, "chronological order in rendered output");
}

// ========================================================================
// Frame 1 — Coordinator preamble
// ========================================================================

#[test]
fn test_preamble_dynamic_tool_sections_with_persistence() {
    let config = OrchestrationConfig::default();

    // With history tools (persistence + session_id configured)
    let preamble_with = config.build_coordinator_preamble("You are an SRE assistant.", false, true);
    assert!(preamble_with.contains("read_artifact"), "read_artifact in preamble with history");
    assert!(preamble_with.contains("list_prior_runs"), "list_prior_runs in preamble with history");

    // Without history tools
    let preamble_without = config.build_coordinator_preamble("You are an SRE assistant.", false, false);
    assert!(preamble_without.contains("read_artifact"), "read_artifact always present");
    assert!(!preamble_without.contains("list_prior_runs"), "list_prior_runs absent without history");
}

// ========================================================================
// Frame 3 — Worker task prompt
// ========================================================================

#[test]
fn test_worker_task_context_with_dependency_results() {
    let context = "Completed results from prior tasks:\n\
        Task 0 result: Found 47 error groups. Top: timeouts 38%. \
        [Full result (3200 chars) saved to artifact: task-0-sre-iter-1-result.txt]";

    let rendered = render_worker_task_prompt(&WorkerTaskVars {
        orchestration_goal: "Investigate error rates in payments service",
        context,
        your_task: "Analyze connection pool metrics for the services identified in Task 0",
    });

    assert!(rendered.contains("Investigate error rates"), "goal in worker prompt");
    assert!(rendered.contains("Analyze connection pool"), "task description");
    assert!(rendered.contains("task-0-sre-iter-1-result.txt"), "artifact ref preserved in context");
    assert!(rendered.contains("Found 47 error groups"), "dependency result in context");
    assert!(rendered.contains("submit_result"), "submit_result instruction");
}

// ========================================================================
// Cross-frame scenario
// ========================================================================

#[test]
fn test_session_history_and_continuation_independent_artifact_refs() {
    // Session history from a prior run with artifacts
    let prior_manifest = sample_manifest(
        "run_prior",
        "Earlier investigation",
        RunStatus::Success,
        vec![complete_task_summary(
            0, "Search logs", "sre",
            "Found errors in auth-service",
            "high",
            vec![trace_with_artifact("log_search", "searching", 5000, "task-0-sre-iter-1-log_search-0-output.txt")],
            vec![
                result_artifact("task-0-sre-iter-1-result.txt", 2500),
                tool_artifact("task-0-sre-iter-1-log_search-0-output.txt", 35000, "log_search"),
            ],
        )],
    );

    let history = build_session_context(&[prior_manifest]);
    assert!(history.contains("run_prior"), "prior run_id in session history");
    assert!(history.contains("task-0-sre-iter-1-result.txt"), "prior artifact in history");

    // Current iteration continuation — its own artifacts are independent
    let mut plan = Plan::new("Follow-up investigation");
    let mut t = Task::new(0, "Deeper analysis", "Analyze auth-service in detail");
    let result = "Auth-service has 12 failing endpoints. [Full result (5000 chars) saved to artifact: task-0-sre-iter-1-result.txt]";
    t.complete(result.to_string());
    plan.add_task(t);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let continuation = ctx.build_continuation_prompt(3);

    assert!(continuation.contains("task-0-sre-iter-1-result.txt"), "current artifact in continuation");
    assert!(continuation.contains("Auth-service has 12 failing"), "current result in continuation");
}

// ========================================================================
// Tool output artifact refs in continuation prompt
// ========================================================================

#[test]
fn test_continuation_tool_output_artifacts_visible() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "Search logs", "Search prod logs");
    t.complete("Found errors. [Full result (3200 chars) saved to artifact: task-0-sre-iter-1-result.txt]".to_string());
    plan.add_task(t);

    let mut traces = HashMap::new();
    traces.insert(0, vec![
        trace_with_artifact("log_search", "searching for errors", 8200, "task-0-sre-iter-1-log_search-0-output.txt"),
        trace_with_artifact("get_metrics", "pool utilization", 3100, "task-0-sre-iter-1-get_metrics-1-output.txt"),
        trace("analyze", "correlating", 1500, None),
    ]);

    let ctx = IterationContext::new(1, plan, None, vec![], traces);
    let prompt = ctx.build_continuation_prompt(3);

    // Tool chain line present
    assert!(prompt.contains("Tool chain:"), "chain line: {}", prompt);

    // Tool output artifact refs visible to coordinator for reuse decisions
    assert!(
        prompt.contains("[Tool output: task-0-sre-iter-1-log_search-0-output.txt"),
        "log_search artifact ref: {}", prompt
    );
    assert!(
        prompt.contains("[Tool output: task-0-sre-iter-1-get_metrics-1-output.txt"),
        "get_metrics artifact ref: {}", prompt
    );
    assert!(prompt.contains("48000 bytes"), "artifact size");

    // Tool without artifact should NOT produce a [Tool output:] line
    assert_eq!(
        prompt.matches("[Tool output:").count(), 2,
        "exactly 2 artifact refs (not 3): {}", prompt
    );
}

#[test]
fn test_continuation_failed_task_no_artifact_refs() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "Deploy check", "Check deployments");
    t.fail("403 Forbidden".to_string(), FailureCategory::AgentError);
    plan.add_task(t);

    let mut traces = HashMap::new();
    traces.insert(0, vec![
        trace("get_deployments", "checking staging", 1200, None),
        trace("get_deployments", "querying prod", 30200, Some("403 Forbidden")),
    ]);

    let ctx = IterationContext::new(1, plan, None, vec![], traces);
    let prompt = ctx.build_continuation_prompt(3);

    // Failed tools don't produce artifacts
    assert!(!prompt.contains("[Tool output:"), "no artifact refs for failed tools: {}", prompt);
    assert!(prompt.contains("FAILED: 403 Forbidden"), "failure visible");
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
        let prompt = ctx.build_continuation_prompt(3);

        assert!(
            prompt.contains(&format!("[{}]", display)),
            "category {} should render as [{}] in: {}", display, display, prompt
        );
    }
}

#[test]
fn test_continuation_soft_failure_with_structured_output() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "Inconclusive task", "Investigate ambiguous signal");
    t.fail("Worker reported inconclusive findings".to_string(), FailureCategory::SoftFailure);
    t.structured_output = Some(StructuredTaskOutput {
        summary: "Found some evidence but insufficient for conclusions".into(),
        confidence: super::tools::submit_result::Confidence::Low,
    });
    plan.add_task(t);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    // SoftFailure with structured output uses the summary path
    assert!(prompt.contains("soft_failure"), "soft_failure tag: {}", prompt);
    assert!(prompt.contains("low confidence"), "confidence shown");
    assert!(prompt.contains("Found some evidence"), "summary from structured output");
}

#[test]
fn test_continuation_soft_failure_without_structured_output() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "Inconclusive task", "Investigate ambiguous signal");
    t.fail("Worker did not call submit_result".to_string(), FailureCategory::SoftFailure);
    plan.add_task(t);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    // SoftFailure without structured output falls back to bracket format
    assert!(prompt.contains("[soft_failure]"), "bracket format: {}", prompt);
    assert!(prompt.contains("Worker did not call submit_result"), "error text");
}

// ========================================================================
// Session history — RoutingMode::Routed variant
// ========================================================================

#[test]
fn test_session_history_routed_single_worker() {
    let manifest = RunManifest {
        run_id: "run_routed".into(),
        session_id: Some("s".into()),
        timestamp: "2026-04-30T16:00:00Z".into(),
        goal: "Check k8s pod status".into(),
        status: RunStatus::Success,
        iterations: 1,
        routing_mode: Some(RoutingMode::Routed),
        outcome: Some("1/1 tasks completed".into()),
        response_summary: None,
        task_summaries: vec![complete_task_summary(
            0, "Get pod status", "sre", "3 pods running, 0 pending", "high",
            vec![trace("kubectl_get_pods", "listing pods in prod namespace", 2000, None)],
            vec![],
        )],
        artifact_paths: vec![],
    };

    let history = build_session_context(&[manifest]);

    assert!(history.contains("Check k8s pod status"), "goal");
    assert!(history.contains("Task 0 [sre] — Complete"), "task status");
    assert!(history.contains("3 pods running"), "result preview");
    assert!(history.contains("kubectl_get_pods"), "tool in chain");
}

// ========================================================================
// Frame 2 — Planning / routing prompt (CRITICAL — was entirely missing)
// ========================================================================

#[test]
fn test_planning_wrapper_basic_structure() {
    let prompt = Orchestrator::build_planning_wrapper(
        "What are the error rates in the payments service?",
        "\n\nAVAILABLE WORKERS:\n## sre\nSRE tools for log and metric analysis\nTools: log_search, get_metrics",
        "\n- Assign each task to a worker\n- Valid worker names: \"sre\"",
        "",
    );

    assert!(prompt.contains("USER QUERY: What are the error rates"), "query present");
    assert!(prompt.contains("AVAILABLE WORKERS"), "worker section present");
    assert!(prompt.contains("log_search, get_metrics"), "tool names present");
    assert!(prompt.contains("respond_directly"), "routing tool 1");
    assert!(prompt.contains("create_plan"), "routing tool 2");
    assert!(prompt.contains("request_clarification"), "routing tool 3");
    assert!(prompt.contains("Call EXACTLY ONE"), "exclusivity instruction");
    assert!(prompt.contains("Valid worker names: \"sre\""), "worker guidelines");
}

#[test]
fn test_planning_wrapper_no_workers() {
    let prompt = Orchestrator::build_planning_wrapper(
        "What is 2+2?",
        "",
        "",
        "",
    );

    assert!(prompt.contains("USER QUERY: What is 2+2?"), "query present");
    assert!(!prompt.contains("AVAILABLE WORKERS"), "no worker section");
    assert!(prompt.contains("respond_directly"), "routing tools still present");
}

#[test]
fn test_planning_wrapper_with_error_section() {
    let error = "\n\nPREVIOUS PLANNING ERROR:\nFailed to parse plan JSON: expected object, got array\nPlease try again with valid JSON.";
    let prompt = Orchestrator::build_planning_wrapper(
        "Investigate error rates",
        "\n\nAVAILABLE WORKERS:\n## sre\nSRE analysis",
        "",
        error,
    );

    assert!(prompt.contains("PREVIOUS PLANNING ERROR"), "error section present");
    assert!(prompt.contains("expected object, got array"), "error detail");
    assert!(prompt.contains("USER QUERY: Investigate error rates"), "query still present");
}

#[test]
fn test_planning_wrapper_multi_worker_guidelines() {
    let prompt = Orchestrator::build_planning_wrapper(
        "Investigate infrastructure issues",
        "\n\nAVAILABLE WORKERS:\n## sre\nSRE tools\n\n## dev\nDev tools",
        "\n- Assign each task to a worker\n- Valid worker names: \"sre\", \"dev\"\n- Choose the worker whose tools best match",
        "",
    );

    assert!(prompt.contains("\"sre\", \"dev\""), "multiple worker names");
    assert!(prompt.contains("## sre"), "sre worker section");
    assert!(prompt.contains("## dev"), "dev worker section");
}

// ========================================================================
// Frame 1 — Preamble additional coverage
// ========================================================================

#[test]
fn test_preamble_recon_tools_enabled() {
    let config = OrchestrationConfig::default();
    let preamble = config.build_coordinator_preamble("Custom SRE instructions.", true, false);

    assert!(preamble.contains("list_tools"), "list_tools present with recon");
    assert!(preamble.contains("inspect_tool_params"), "inspect_tool_params present");
    assert!(preamble.contains("reconnaissance"), "recon guidance section");
    assert!(preamble.contains("Custom SRE instructions"), "agent system prompt injected");
}

#[test]
fn test_preamble_recon_and_history_tools_combined() {
    let config = OrchestrationConfig::default();
    let preamble = config.build_coordinator_preamble("Domain prompt.", true, true);

    assert!(preamble.contains("list_tools"), "recon tool");
    assert!(preamble.contains("inspect_tool_params"), "recon tool");
    assert!(preamble.contains("read_artifact"), "artifact tool");
    assert!(preamble.contains("list_prior_runs"), "history tool");
    assert!(preamble.contains("two **artifact/history tools**"), "combined tool count");
}

#[test]
fn test_preamble_empty_system_prompt() {
    let config = OrchestrationConfig::default();
    let preamble = config.build_coordinator_preamble("", false, false);

    assert!(preamble.contains("read_artifact"), "artifact tool always present");
    assert!(preamble.contains("respond_directly"), "routing tools present");
}

// ========================================================================
// Frame 3 — Worker task prompt additional coverage
// ========================================================================

#[test]
fn test_worker_task_empty_context() {
    let rendered = render_worker_task_prompt(&WorkerTaskVars {
        orchestration_goal: "Investigate error rates",
        context: "",
        your_task: "Search logs for error patterns",
    });

    assert!(rendered.contains("YOUR TASK: Search logs"), "task present");
    assert!(rendered.contains("Investigate error rates"), "goal present");
    assert!(rendered.contains("submit_result"), "submit_result instruction");
}

// ========================================================================
// Frame 4 — Continuation prompt branch gaps
// ========================================================================

#[test]
fn test_continuation_running_task_renders_as_blocked() {
    let mut plan = Plan::new("Test goal");
    let t0 = Task::new(0, "running task", "This task is still running");
    plan.add_task(t0);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    assert!(prompt.contains("blocked (dependency failed)"), "Running renders as blocked: {}", prompt);
}

#[test]
fn test_continuation_clean_success_no_failure_sections() {
    let mut plan = Plan::new("Simple goal");
    let mut t0 = Task::new(0, "task A", "First task");
    t0.complete("Done A".to_string());
    let mut t1 = Task::new(1, "task B", "Second task");
    t1.complete("Done B".to_string());
    plan.add_task(t0);
    plan.add_task(t1);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    assert!(prompt.contains("COMPLETED TASKS"), "completed section present");
    assert!(prompt.contains("2 of 2 tasks succeeded"), "all succeeded");
    assert!(!prompt.contains("FAILED TASKS"), "no failed section");
    assert!(!prompt.contains("FAILURE SUMMARY"), "no failure summary");
    assert!(!prompt.contains("FAILURE HISTORY"), "no failure history");
    assert!(!prompt.contains("OBSERVED PATTERNS"), "no patterns");
    assert!(!prompt.contains("reuse_result_from"), "no reuse guidance on clean success");
}

#[test]
fn test_continuation_preserve_artifact_footer_no_footer() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "short result task", "Task with short result");
    t.complete("Short result, no artifact needed".to_string());
    plan.add_task(t);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    assert!(prompt.contains("Short result, no artifact needed"), "full result present");
    assert!(!prompt.contains("..."), "no truncation marker");
    assert!(!prompt.contains("[Full result"), "no artifact footer");
}

#[test]
fn test_continuation_reuse_guidance_absent_when_all_failed() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "failing", "This fails");
    t.fail("error".to_string(), FailureCategory::AgentError);
    plan.add_task(t);

    let ctx = IterationContext::new(1, plan, None, vec![], HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    assert!(!prompt.contains("reuse_result_from"), "no reuse when zero succeeded: {}", prompt);
}

#[test]
fn test_continuation_failure_history_worker_none() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "task", "A task");
    t.fail("oops".to_string(), FailureCategory::AgentError);
    plan.add_task(t);

    let history = vec![FailedTaskRecord {
        description: "A task".into(),
        error: "oops".into(),
        iteration: 1,
        worker: None,
        category: FailureCategory::AgentError,
    }];

    let ctx = IterationContext::new(1, plan, None, history, HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    assert!(prompt.contains("FAILURE HISTORY"), "history present");
    // Should NOT contain "(worker: )" with empty worker
    assert!(!prompt.contains("(worker: )"), "no empty worker parens: {}", prompt);
}

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
            description: "First task".into(), error: "timeout".into(),
            iteration: 1, worker: None, category: FailureCategory::AgentTimeout,
        },
        FailedTaskRecord {
            description: "First task".into(), error: "timeout".into(),
            iteration: 2, worker: None, category: FailureCategory::AgentTimeout,
        },
        FailedTaskRecord {
            description: "Second task".into(), error: "403".into(),
            iteration: 1, worker: None, category: FailureCategory::ProviderAuthError,
        },
        FailedTaskRecord {
            description: "Second task".into(), error: "403".into(),
            iteration: 2, worker: None, category: FailureCategory::ProviderAuthError,
        },
    ];

    let ctx = IterationContext::new(2, plan, None, history, HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    assert!(prompt.contains("OBSERVED PATTERNS"), "patterns section: {}", prompt);
    assert!(prompt.contains("\"First task\" has failed 2 times"), "first pattern");
    assert!(prompt.contains("\"Second task\" has failed 2 times"), "second pattern");
}

#[test]
fn test_continuation_empty_reasoning_in_tool_chain() {
    let mut plan = Plan::new("Test goal");
    let mut t = Task::new(0, "task", "A task");
    t.complete("Done".to_string());
    plan.add_task(t);

    let mut traces = HashMap::new();
    traces.insert(0, vec![
        trace("tool_a", "", 1000, None),
        trace("tool_b", "has reasoning", 2000, None),
    ]);

    let ctx = IterationContext::new(1, plan, None, vec![], traces);
    let prompt = ctx.build_continuation_prompt(3);

    let chain_line = prompt.lines().find(|l| l.contains("Tool chain:")).unwrap();
    assert!(chain_line.contains("tool_a (1.0s)"), "empty reasoning omits quotes");
    assert!(chain_line.contains("tool_b (2.0s, \"has reasoning\")"), "non-empty reasoning has quotes");
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
fn test_session_history_task_with_no_worker() {
    let manifest = sample_manifest(
        "run_1", "Test goal", RunStatus::Success,
        vec![TaskSummary {
            task_id: 0,
            description: "Unassigned task".into(),
            status: TaskStatus::Complete,
            worker: None,
            result_preview: Some("Task completed".into()),
            confidence: None,
            failure_category: None,
            error: None,
            error_context: None,
            tool_trace: vec![],
            artifacts: vec![],
        }],
    );

    let history = build_session_context(&[manifest]);
    assert!(history.contains("unassigned"), "None worker renders as 'unassigned'");
    assert!(history.contains("Task completed"), "preview present");
}

#[test]
fn test_session_history_complete_task_no_preview_no_confidence() {
    let manifest = sample_manifest(
        "run_1", "Test goal", RunStatus::Success,
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
    assert!(!history.contains("Summary:"), "no summary line without preview");
    assert!(!history.contains("Tool chain:"), "no chain without traces");
}

#[test]
fn test_session_history_failed_task_no_error_no_context() {
    let manifest = sample_manifest(
        "run_1", "Test goal", RunStatus::Failed,
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
    assert!(!history.contains("Partial progress:"), "no partial when no context");
}

#[test]
fn test_session_history_no_artifacts_no_crossrun_hint() {
    let manifest = sample_manifest(
        "run_no_artifacts", "Simple query", RunStatus::Success,
        vec![complete_task_summary(0, "Simple task", "sre", "Done", "high", vec![], vec![])],
    );

    let history = build_session_context(&[manifest]);
    assert!(!history.contains("use run_id="), "no cross-run hint when no artifacts: {}", prompt_excerpt(&history));
}

#[test]
fn test_session_history_manifest_outcome_none() {
    let mut manifest = sample_manifest(
        "run_1", "Test goal", RunStatus::Success,
        vec![complete_task_summary(0, "task", "sre", "done", "high", vec![], vec![])],
    );
    manifest.outcome = None;

    let history = build_session_context(&[manifest]);
    assert!(history.contains("Test goal"), "goal present");
    assert!(!history.contains("Outcome:"), "no outcome line when None");
}

#[test]
fn test_session_history_current_time_placeholder_replaced() {
    let manifest = sample_manifest(
        "run_1", "Test", RunStatus::Success,
        vec![complete_task_summary(0, "t", "w", "d", "high", vec![], vec![])],
    );

    let history = build_session_context(&[manifest]);
    assert!(!history.contains("%%CURRENT_TIME%%"), "template placeholder must be replaced");
    assert!(history.contains("Current time:"), "time label present");
}

#[test]
fn test_session_history_error_context_without_partial_result() {
    let manifest = sample_manifest(
        "run_1", "Test", RunStatus::Failed,
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
    assert!(!history.contains("Partial progress:"), "no partial line when None");
}

// ========================================================================
// Section ordering and structural integrity
// ========================================================================

#[test]
fn test_continuation_section_ordering() {
    let mut plan = Plan::new("Order test");
    let mut t0 = Task::new(0, "complete", "Completed task");
    t0.complete("done".to_string());
    let mut t1 = Task::new(1, "failed", "Failed task");
    t1.fail("error".to_string(), FailureCategory::AgentError);
    let t2 = Task::new(2, "blocked", "Blocked task");
    plan.add_task(t0);
    plan.add_task(t1);
    plan.add_task(t2);

    let fs = FailureSummary {
        reasoning: "One failed".into(),
        gaps: vec!["Gap".into()],
    };
    let history = vec![FailedTaskRecord {
        description: "Failed task".into(), error: "error".into(),
        iteration: 1, worker: None, category: FailureCategory::AgentError,
    }];

    let ctx = IterationContext::new(1, plan, Some(fs), history, HashMap::new());
    let prompt = ctx.build_continuation_prompt(3);

    let completed_pos = prompt.find("COMPLETED TASKS").unwrap();
    let blocked_pos = prompt.find("BLOCKED TASKS").unwrap();
    let failed_pos = prompt.find("FAILED TASKS").unwrap();
    let summary_pos = prompt.find("FAILURE SUMMARY").unwrap();
    let history_pos = prompt.find("FAILURE HISTORY").unwrap();

    assert!(completed_pos < blocked_pos, "COMPLETED before BLOCKED");
    assert!(blocked_pos < failed_pos, "BLOCKED before FAILED");
    assert!(failed_pos < summary_pos, "FAILED before SUMMARY");
    assert!(summary_pos < history_pos, "SUMMARY before HISTORY");
}

// ========================================================================
// Helper
// ========================================================================

// ========================================================================
// Remaining edge cases from final review
// ========================================================================

#[test]
fn test_session_history_running_task_status() {
    let manifest = sample_manifest(
        "run_1", "Test", RunStatus::PartialSuccess,
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
    assert!(history.contains("Task 0 [sre] — running"), "Running status renders lowercase: {}", prompt_excerpt(&history));
}

#[test]
fn test_session_history_multi_artifact_listing() {
    let manifest = sample_manifest(
        "run_1", "Test", RunStatus::Success,
        vec![complete_task_summary(
            0, "Multi-artifact task", "sre", "Produced lots of data", "high",
            vec![
                trace_with_artifact("log_search", "searching", 5000, "task-0-sre-iter-1-log_search-0-output.txt"),
                trace_with_artifact("get_metrics", "metrics", 3000, "task-0-sre-iter-1-get_metrics-1-output.txt"),
            ],
            vec![
                result_artifact("task-0-sre-iter-1-result.txt", 4200),
                tool_artifact("task-0-sre-iter-1-log_search-0-output.txt", 48000, "log_search"),
                tool_artifact("task-0-sre-iter-1-get_metrics-1-output.txt", 12000, "get_metrics"),
            ],
        )],
    );

    let history = build_session_context(&[manifest]);
    let artifacts_line = history.lines().find(|l| l.contains("Artifacts:")).unwrap();

    assert!(artifacts_line.contains("task-0-sre-iter-1-result.txt"), "result artifact");
    assert!(artifacts_line.contains("task-0-sre-iter-1-log_search-0-output.txt"), "log_search artifact");
    assert!(artifacts_line.contains("task-0-sre-iter-1-get_metrics-1-output.txt"), "get_metrics artifact");
    assert_eq!(artifacts_line.matches(", ").count(), 2, "3 artifacts separated by 2 commas");
}

// ========================================================================
// Helper
// ========================================================================

fn prompt_excerpt(s: &str) -> &str {
    if s.len() > 200 { &s[..200] } else { s }
}
