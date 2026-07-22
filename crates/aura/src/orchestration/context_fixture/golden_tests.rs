//! The S2 golden-frame snapshot corpus and the REQUIRED R3/R5 comparison
//! gates.
//!
//! One test per covered `MANIFEST.md` row (or one test covering several
//! rows where the manifest maps them so); every test renders a complete
//! [`RequestEnvelope`] triple through [`assert_envelope_snapshot`], so the
//! committed `.snap` files under `snapshots/` are the byte-identity
//! baseline for refactor cards S3-S6.
//!
//! Fixture data rules:
//! - the shared [`SOURCE_PLAYBOOK`] carries the 14 headed blocks of
//!   MANIFEST §1 rows 5-18;
//! - payloads never embed the normalization markers (the audit panics on
//!   collision);
//! - at most ONE repeated (handle, category) failure pair per scenario:
//!   `OBSERVED PATTERNS` iterates a `HashMap`, so multiple patterns would
//!   snapshot nondeterministically (that branch keeps its
//!   `frame_validation_tests.rs` coverage);
//! - one tool-call attempt per task per iteration: the production trace
//!   loader's within-iteration file order is filesystem-dependent.

use std::collections::HashMap;

use super::envelope::{
    compose_worker_preamble, executed_plan, merged_traces, section_orchestrator,
    worker_tool_definitions,
};
use super::{
    CompletedResultFixture, ContinuationThread, CoordinatorCall, CoordinatorScenario,
    CoordinatorToolConfig, FailedResultFixture, FixtureError, FrameGraph, HistoryTools,
    IterationFixture, NormalizedSnapshot, PlanDecision, PlanningBudget, PreambleFixture,
    ReconTools, ScratchpadWiring, SessionHistoryFixture, SpilledStandIn, TaskOutcome,
    WorkerFrameFixture, WorkerPreambleAppends, WorkerPreambleFixture, WorkerRosterFixture,
    WorkerScenario, assert_envelope_snapshot, coordinator_envelope, normalize, worker_envelope,
};
use crate::config::AgentRuntimeConfig;
use crate::orchestration::config::{ArtifactsConfig, OrchestrationConfig, ToolVisibility};
use crate::orchestration::context::{
    ContextError, EvidenceText, PinnedGoal, ResultPreview, SpilledArtifact, WorkerClaim,
};
use crate::orchestration::events::RoutingMode;
use crate::orchestration::orchestrator::CoordinatorTools;
use crate::orchestration::persistence::{
    ArtifactEntry, ArtifactKind, ErrorContext, ExecutionPersistence, RunManifest, RunStatus,
    TaskSummary, ToolCallRecord, ToolOutcome, ToolTraceEntry,
};
use crate::orchestration::tools::submit_result::Confidence;
use crate::orchestration::tools::{
    InspectToolParamsTool, ListPriorRunsTool, ListToolsTool, ReadArtifactTool, RoutingToolSet,
};
use crate::orchestration::types::{
    FailureCategory, FailureSummary, IterationContext, Plan, PlanningResponse, StepInput,
    StructuredTaskOutput, Task, TaskStatus,
};
use crate::orchestration::{Orchestrator, WorkerConfig};
use aura_config::{LlmConfig, ScratchpadConfig, SkillConfig, SkillName, VectorStoreConfig};

/// The shared coordinator playbook (`[agent].system_prompt`), preserving
/// the 14 headed blocks of MANIFEST §1 rows 5-18 so Gate A can see each
/// block render inside the single `%%ORCHESTRATION_SYSTEM_PROMPT%%` slot.
const SOURCE_PLAYBOOK: &str = "\
You coordinate SRE investigations for the payments platform. Decompose \
queries into worker tasks and route decisively.

ROUTING
Route log questions to the analyst and shell work to the operator.

PHASE BOUNDARY PRINCIPLE
Finish investigation before remediation; never mix the two in one plan.

OPERATING STRATEGY
Start from the narrowest signal that can falsify the leading hypothesis.

INITIAL PLAN CONTRACT
The first plan gathers evidence only; it makes no changes to any system.

EXACT-DATA HANDOFF
Copy exact identifiers, values, and file names into task descriptions.

Decision-packet checklist:
- evidence collected
- hypothesis stated
- next action named

After each iteration, weigh the new evidence before planning further work.

SINGLE-ACTION TASK CONTRACT
Each task performs exactly one action a single worker can complete.

DEPTH-FAILURE RECOVERY
When a worker exhausts its turn budget, split the task rather than retry.

REPLAN BUDGET
Spend replans on new evidence, never on repeating a failed approach.

WORKER SELECTION
Match each task to the worker whose tools cover the task's data source.

PLAN STRUCTURE
Prefer short sequential chains; parallelize only independent lookups.

TASK DESCRIPTIONS
Write self-contained descriptions; workers see no conversation history.";

/// The verbatim user query pinned across every coordinator fixture.
const QUERY: &str = "Investigate the elevated error rates in the payments service \
and report the top failure groups with supporting evidence.";

// ============================================================================
// Shared fixture inputs
// ============================================================================

fn worker(description: &str, preamble: &str, vector_stores: &[&str]) -> WorkerConfig {
    WorkerConfig {
        description: description.to_owned(),
        preamble: preamble.to_owned(),
        mcp_filter: Vec::new(),
        vector_stores: vector_stores.iter().map(|s| (*s).to_owned()).collect(),
        turn_depth: None,
        llm: None,
        scratchpad: None,
        skills: None,
    }
}

fn analyst_operator_workers() -> HashMap<String, WorkerConfig> {
    HashMap::from([
        (
            "analyst".to_owned(),
            worker(
                "Log and metric analysis for the payments platform",
                "You are the payments log analyst. Ground every claim in log evidence.",
                &[],
            ),
        ),
        (
            "operator".to_owned(),
            worker(
                "Shell and deployment operations",
                "You are the operations specialist. Report exact commands and outputs.",
                &[],
            ),
        ),
    ])
}

fn roster_config(
    workers: HashMap<String, WorkerConfig>,
    tools_in_planning: ToolVisibility,
) -> OrchestrationConfig {
    OrchestrationConfig {
        enabled: true,
        workers,
        tools_in_planning,
        ..Default::default()
    }
}

fn vector_store(name: &str, context_prefix: Option<&str>) -> VectorStoreConfig {
    VectorStoreConfig {
        name: name.to_owned(),
        context_prefix: context_prefix.map(str::to_owned),
        ..Default::default()
    }
}

fn skill(name: &str, description: &str) -> SkillConfig {
    SkillConfig {
        name: SkillName::new(name).expect("valid fixture skill name"),
        description: description.to_owned(),
        path: std::path::PathBuf::from(format!("/fixtures/skills/{name}")),
    }
}

fn fixture_skills() -> Vec<SkillConfig> {
    vec![
        skill("log-triage", "Structured log triage playbook"),
        skill("postmortem-draft", "Incident postmortem drafting guide"),
    ]
}

fn preamble(tools: CoordinatorToolConfig) -> PreambleFixture {
    PreambleFixture {
        playbook: SOURCE_PLAYBOOK.to_owned(),
        tools,
        skills: Vec::new(),
        vector_stores: Vec::new(),
        session_history: None,
    }
}

fn no_optional_tools() -> CoordinatorToolConfig {
    CoordinatorToolConfig {
        recon: ReconTools::Excluded,
        history: HistoryTools::Excluded,
    }
}

fn goal() -> PinnedGoal {
    PinnedGoal::new(QUERY).expect("fixture query is non-empty")
}

fn claim(summary: &str, confidence: Confidence) -> WorkerClaim {
    WorkerClaim::new(summary, confidence).expect("fixture claim summary is non-empty")
}

fn evidence(text: &str) -> EvidenceText {
    EvidenceText::new(text).expect("fixture evidence is inline-parseable")
}

fn spilled(filename: &str, full_chars: usize) -> SpilledArtifact {
    SpilledArtifact::new(filename, full_chars).expect("fixture artifact filename is non-empty")
}

fn leaf(task: &str, worker: Option<&str>) -> StepInput {
    StepInput::LeafTask {
        task: task.to_owned(),
        worker: worker.map(str::to_owned),
        named_check: None,
    }
}

fn decision(rationale: &str, steps: Vec<StepInput>) -> PlanDecision {
    PlanDecision::new(PlanningResponse::StepsPlan {
        goal: QUERY.to_owned(),
        steps,
        routing_rationale: rationale.to_owned(),
        planning_summary: "Gather evidence, then correlate and summarize.".to_owned(),
    })
    .expect("fixture decisions are steps plans that flatten")
}

fn success_trace(tool: &str, reasoning: &str, ms: u64, artifact: Option<&str>) -> ToolTraceEntry {
    ToolTraceEntry {
        tool: tool.to_owned(),
        reasoning: reasoning.to_owned(),
        duration_ms: ms,
        outcome: ToolOutcome::Success { output_bytes: 512 },
        artifact_filename: artifact.map(str::to_owned),
    }
}

fn failed_trace(tool: &str, reasoning: &str, ms: u64, message: &str) -> ToolTraceEntry {
    ToolTraceEntry {
        tool: tool.to_owned(),
        reasoning: reasoning.to_owned(),
        duration_ms: ms,
        outcome: ToolOutcome::Error {
            message: message.to_owned(),
        },
        artifact_filename: None,
    }
}

// ============================================================================
// Session-history manifests (MANIFEST §1 rows 23/23a/23b/23c)
// ============================================================================

/// The prior routed run: Complete summary (named worker, confidence tag,
/// result preview), Failed summary (unassigned worker, category tag,
/// error, last-tool and partial-progress lines), a success+FAILED tool
/// chain, a multi-artifact line, and the cross-run `read_artifact` hint.
fn routed_manifest() -> RunManifest {
    RunManifest {
        run_id: "run-routed-0001".to_owned(),
        session_id: Some("s2-session".to_owned()),
        timestamp: "2026-07-08T09:15:00Z".to_owned(),
        goal: "Triage the payments error spike".to_owned(),
        status: RunStatus::PartialSuccess,
        iterations: 1,
        routing_mode: Some(RoutingMode::Orchestrated),
        outcome: Some("1/2 tasks completed".to_owned()),
        response_summary: None,
        task_summaries: vec![
            TaskSummary {
                task_id: 0,
                description: "Search payments logs for error patterns".to_owned(),
                status: TaskStatus::Complete,
                worker: Some("analyst".to_owned()),
                result_preview: Some("Found 47 error groups; top: connection timeouts".to_owned()),
                confidence: Some("high".to_owned()),
                failure_category: None,
                error: None,
                error_context: None,
                tool_trace: vec![
                    success_trace(
                        "log_search",
                        "searching error patterns",
                        8200,
                        Some("task-0-analyst-iter-1-log_search-0-output.txt"),
                    ),
                    failed_trace(
                        "get_metrics",
                        "pool utilization",
                        3100,
                        "408 upstream timeout",
                    ),
                ],
                artifacts: vec![
                    ArtifactEntry {
                        filename: "task-0-analyst-iter-1-result.txt".to_owned(),
                        size_bytes: 3200,
                        kind: ArtifactKind::Result,
                    },
                    ArtifactEntry {
                        filename: "task-0-analyst-iter-1-log_search-0-output.txt".to_owned(),
                        size_bytes: 48291,
                        kind: ArtifactKind::ToolOutput {
                            tool_name: "log_search".to_owned(),
                        },
                    },
                ],
            },
            TaskSummary {
                task_id: 1,
                description: "Query deployment history for the error window".to_owned(),
                status: TaskStatus::Failed,
                worker: None,
                result_preview: None,
                confidence: None,
                failure_category: Some(FailureCategory::AgentError),
                error: Some("403 Forbidden from the deployment API".to_owned()),
                error_context: Some(ErrorContext {
                    category: FailureCategory::AgentError,
                    last_tool_call: Some("get_deployments".to_owned()),
                    attempt_count: 1,
                    partial_result: Some("Staging query succeeded".to_owned()),
                }),
                tool_trace: vec![],
                artifacts: vec![],
            },
        ],
        artifact_paths: vec![],
    }
}

/// The more recent direct-response run: outcome plus response summary, no
/// task list (and therefore no cross-run hint for its turn).
fn direct_manifest() -> RunManifest {
    RunManifest {
        run_id: "run-direct-0002".to_owned(),
        session_id: Some("s2-session".to_owned()),
        timestamp: "2026-07-09T18:30:00Z".to_owned(),
        goal: "What did the last triage conclude?".to_owned(),
        status: RunStatus::Success,
        iterations: 0,
        routing_mode: Some(RoutingMode::DirectAnswer),
        outcome: Some("Answered directly".to_owned()),
        response_summary: Some("Summarized the prior triage results.".to_owned()),
        task_summaries: vec![],
        artifact_paths: vec![],
    }
}

/// A prior run whose task summaries exercise the catch-all render for
/// Running and Pending statuses (formerly excluded at MANIFEST §1 row
/// 23c, now covered by the `session_history_catch_all` fixture).
fn catch_all_manifest() -> RunManifest {
    RunManifest {
        run_id: "run-catch-all-0001".to_owned(),
        session_id: Some("s2-session".to_owned()),
        timestamp: "2026-07-10T09:15:00Z".to_owned(),
        goal: "Triage the payments error spike".to_owned(),
        status: RunStatus::PartialSuccess,
        iterations: 1,
        routing_mode: Some(RoutingMode::Orchestrated),
        outcome: Some("0/2 tasks completed".to_owned()),
        response_summary: None,
        task_summaries: vec![
            TaskSummary {
                task_id: 0,
                description: "Search payments logs for error patterns".to_owned(),
                status: TaskStatus::Running,
                worker: Some("analyst".to_owned()),
                result_preview: None,
                confidence: None,
                failure_category: None,
                error: None,
                error_context: None,
                tool_trace: vec![],
                artifacts: vec![],
            },
            TaskSummary {
                task_id: 1,
                description: "Query deployment history for the error window".to_owned(),
                status: TaskStatus::Pending,
                worker: Some("analyst".to_owned()),
                result_preview: None,
                confidence: None,
                failure_category: None,
                error: None,
                error_context: None,
                tool_trace: vec![],
                artifacts: vec![],
            },
        ],
        artifact_paths: vec![],
    }
}

fn session_history() -> SessionHistoryFixture {
    // Most-recent-first, exactly as `load_session_manifests` returns them.
    SessionHistoryFixture::new(vec![direct_manifest(), routed_manifest()])
        .expect("fixture manifests are non-empty and recent-first")
}

// ============================================================================
// Coordinator scenarios
// ============================================================================

fn scenario(
    preamble: PreambleFixture,
    roster: WorkerRosterFixture,
    call: CoordinatorCall,
) -> CoordinatorScenario {
    CoordinatorScenario::new(preamble, goal(), roster, call)
        .expect("corpus scenarios are production-reachable")
}

async fn snapshot_coordinator(name: &str, scenario: &CoordinatorScenario) {
    let envelope = coordinator_envelope(scenario)
        .await
        .expect("corpus envelopes assemble");
    assert_envelope_snapshot(name, &envelope);
}

async fn snapshot_worker(name: &str, scenario: &WorkerScenario) {
    let envelope = worker_envelope(scenario)
        .await
        .expect("corpus envelopes assemble");
    assert_envelope_snapshot(name, &envelope);
}

#[tokio::test]
async fn coordinator_call1_recon() {
    let preamble = preamble(CoordinatorToolConfig {
        recon: ReconTools::Included,
        history: HistoryTools::Excluded,
    });
    let roster = WorkerRosterFixture::new(
        roster_config(analyst_operator_workers(), ToolVisibility::None),
        Vec::new(),
    );
    let scenario = scenario(preamble, roster, CoordinatorCall::Initial);
    snapshot_coordinator("coordinator_call1_recon", &scenario).await;
}

#[tokio::test]
async fn coordinator_call1_nonrecon_summary() {
    let mut workers = HashMap::new();
    workers.insert(
        "search".to_owned(),
        worker(
            "Knowledge-base search across incident history",
            "You are the search specialist.",
            &["runbooks", "incidents", "postmortems", "telemetry"],
        ),
    );
    workers.insert(
        "triage".to_owned(),
        worker(
            "First-pass triage without external systems",
            "You are the triage specialist.",
            &[],
        ),
    );
    let config = OrchestrationConfig {
        max_tools_per_worker: 2,
        ..roster_config(workers, ToolVisibility::Summary)
    };
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(config, Vec::new()),
        CoordinatorCall::Initial,
    );
    snapshot_coordinator("coordinator_call1_nonrecon_summary", &scenario).await;
}

#[tokio::test]
async fn coordinator_call1_full_visibility() {
    let mut workers = HashMap::new();
    workers.insert(
        "search".to_owned(),
        worker(
            "Knowledge-base search across incident history",
            "You are the search specialist.",
            &["runbooks", "scratch", "archive"],
        ),
    );
    workers.insert(
        "triage".to_owned(),
        worker(
            "First-pass triage without external systems",
            "You are the triage specialist.",
            &[],
        ),
    );
    let config = OrchestrationConfig {
        max_tools_per_worker: 2,
        ..roster_config(workers, ToolVisibility::Full)
    };
    // Agent-level catalog: `runbooks` is described (context_prefix);
    // `scratch` and `archive` are assigned but absent from the catalog, so
    // their tool lines render bare.
    let catalog = vec![vector_store(
        "runbooks",
        Some("Operational runbooks for the payments platform"),
    )];
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(config, catalog),
        CoordinatorCall::Initial,
    );
    snapshot_coordinator("coordinator_call1_full_visibility", &scenario).await;
}

#[tokio::test]
async fn coordinator_call1_no_workers() {
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(
            roster_config(HashMap::new(), ToolVisibility::Summary),
            Vec::new(),
        ),
        CoordinatorCall::Initial,
    );
    snapshot_coordinator("coordinator_call1_no_workers", &scenario).await;
}

#[tokio::test]
async fn coordinator_preamble_full_appends() {
    let preamble = PreambleFixture {
        playbook: SOURCE_PLAYBOOK.to_owned(),
        tools: CoordinatorToolConfig {
            recon: ReconTools::Excluded,
            history: HistoryTools::Included,
        },
        skills: fixture_skills(),
        vector_stores: vec![vector_store(
            "runbooks",
            Some("Operational runbooks for the payments platform"),
        )],
        session_history: Some(session_history()),
    };
    let scenario = scenario(
        preamble,
        WorkerRosterFixture::new(
            roster_config(analyst_operator_workers(), ToolVisibility::Summary),
            Vec::new(),
        ),
        CoordinatorCall::Initial,
    );
    snapshot_coordinator("coordinator_preamble_full_appends", &scenario).await;
}

/// Session-history block with catch-all Running and Pending task summaries.
#[tokio::test]
async fn session_history_catch_all() {
    let preamble = PreambleFixture {
        playbook: SOURCE_PLAYBOOK.to_owned(),
        tools: no_optional_tools(),
        skills: Vec::new(),
        vector_stores: Vec::new(),
        session_history: Some(
            SessionHistoryFixture::new(vec![catch_all_manifest()]).expect("one prior manifest"),
        ),
    };
    let scenario = scenario(
        preamble,
        WorkerRosterFixture::new(
            roster_config(analyst_operator_workers(), ToolVisibility::Summary),
            Vec::new(),
        ),
        CoordinatorCall::Initial,
    );
    snapshot_coordinator("session_history_catch_all", &scenario).await;
}

#[tokio::test]
async fn tools_coordinator_recon_history() {
    let preamble = preamble(CoordinatorToolConfig {
        recon: ReconTools::Included,
        history: HistoryTools::Included,
    });
    let scenario = scenario(
        preamble,
        WorkerRosterFixture::new(
            roster_config(analyst_operator_workers(), ToolVisibility::None),
            Vec::new(),
        ),
        CoordinatorCall::Initial,
    );
    snapshot_coordinator("tools_coordinator_recon_history", &scenario).await;
}

/// The clean iteration behind `coordinator_call2_clean`: two completed
/// tasks — inline result with a claim (plus an artifact inventory line),
/// and inline result without a claim.
fn clean_iteration() -> IterationFixture {
    let decision = decision(
        "Two evidence-gathering lookups are required before answering.",
        vec![
            leaf(
                "Collect the error groups from the payments logs for the last six hours",
                Some("analyst"),
            ),
            leaf(
                "Assemble the deployment timeline for the same window",
                Some("operator"),
            ),
        ],
    );
    let outcomes = vec![
        TaskOutcome::Complete {
            result: CompletedResultFixture::Inline {
                result: evidence(
                    "Found 47 error groups across 3 services; top: connection timeouts (38%).",
                ),
                claim: Some(claim(
                    "Found 47 error groups; connection timeouts dominate",
                    Confidence::High,
                )),
            },
            traces: vec![
                success_trace(
                    "log_search",
                    "searching payments error patterns",
                    8200,
                    Some("task-0-analyst-iter-1-log_search-0-output.txt"),
                ),
                success_trace("get_metrics", "", 3100, None),
            ],
        },
        TaskOutcome::Complete {
            result: CompletedResultFixture::Inline {
                result: evidence(
                    "Deployment timeline assembled: 3 deploys landed inside the error window.",
                ),
                claim: None,
            },
            traces: vec![],
        },
    ];
    IterationFixture::new(decision, outcomes, None).expect("clean iteration validates")
}

#[tokio::test]
async fn coordinator_call2_clean() {
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(
            roster_config(analyst_operator_workers(), ToolVisibility::Summary),
            Vec::new(),
        ),
        CoordinatorCall::Continuation(
            ContinuationThread::new(vec![clean_iteration()]).expect("one iteration"),
        ),
    );
    snapshot_coordinator("coordinator_call2_clean", &scenario).await;
}

/// The non-default observability knob: completed tasks carry condensed
/// tool-chain lines when `show_tool_reasoning_in_continuation` is enabled.
#[tokio::test]
async fn coordinator_call_completed_task_tool_chain() {
    let mut config = roster_config(analyst_operator_workers(), ToolVisibility::Summary);
    config.artifacts.show_tool_reasoning_in_continuation = true;
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(config, Vec::new()),
        CoordinatorCall::Continuation(
            ContinuationThread::new(vec![clean_iteration()]).expect("one iteration"),
        ),
    );
    snapshot_coordinator("coordinator_call_completed_task_tool_chain", &scenario).await;
}

#[tokio::test]
async fn coordinator_call2_all_failed() {
    let decision = decision(
        "Both lookups need tool execution.",
        vec![
            leaf(
                "Collect the error groups from the payments logs for the last six hours",
                Some("analyst"),
            ),
            leaf(
                "Assemble the deployment timeline for the same window",
                Some("operator"),
            ),
        ],
    );
    // Hard failure past the 2000-character preview bound, so the
    // truncation marker renders; both tasks carry EMPTY traces, so no
    // failed-entry chain lines render.
    let long_error = format!("upstream error while streaming logs: {}", "x".repeat(2100));
    let outcomes = vec![
        TaskOutcome::Failed {
            report: FailedResultFixture::Hard {
                error: long_error,
                category: FailureCategory::AgentError,
            },
            traces: vec![],
        },
        TaskOutcome::Failed {
            report: FailedResultFixture::Hard {
                error: "worker timed out before producing a result".to_owned(),
                category: FailureCategory::AgentTimeout,
            },
            traces: vec![],
        },
    ];
    let iteration = IterationFixture::new(
        decision,
        outcomes,
        Some(FailureSummary {
            reasoning: "Execution failed: 2 task(s) failed, 0 task(s) blocked by dependencies."
                .to_owned(),
            gaps: vec!["Some tasks could not complete due to errors".to_owned()],
        }),
    )
    .expect("all-failed iteration validates");
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(
            roster_config(analyst_operator_workers(), ToolVisibility::Summary),
            Vec::new(),
        ),
        CoordinatorCall::Continuation(ContinuationThread::new(vec![iteration]).expect("one")),
    );
    snapshot_coordinator("coordinator_call2_all_failed", &scenario).await;
}

/// Every `FailureCategory` variant renders in the FAILED TASKS section.
#[tokio::test]
async fn coordinator_call_all_failure_categories() {
    let categories = vec![
        (FailureCategory::AgentTimeout, "agent timeout"),
        (FailureCategory::ContextOverflow, "context overflow"),
        (FailureCategory::DepthExhausted, "depth exhausted"),
        (FailureCategory::LoopDetected, "loop detected"),
        (FailureCategory::ProviderOverloaded, "provider overloaded"),
        (FailureCategory::ProviderAuthError, "provider auth error"),
        (FailureCategory::ProviderNotFound, "provider not found"),
        (FailureCategory::DependencyFailed, "dependency failed"),
        (FailureCategory::SoftFailure, "soft failure"),
        (FailureCategory::AgentError, "agent error"),
    ];
    let steps: Vec<StepInput> = categories
        .iter()
        .enumerate()
        .map(|(i, (_, msg))| leaf(&format!("Task {i}: {msg}"), Some("analyst")))
        .collect();
    let decision = decision("Exercise every failure category in one iteration.", steps);
    let outcomes: Vec<TaskOutcome> = categories
        .iter()
        .map(|(category, msg)| TaskOutcome::Failed {
            report: FailedResultFixture::Hard {
                error: format!("error: {msg}"),
                category: *category,
            },
            traces: vec![],
        })
        .collect();
    let iteration = IterationFixture::new(
        decision,
        outcomes,
        Some(FailureSummary {
            reasoning: "Execution failed: all tasks failed.".to_owned(),
            gaps: vec!["All failure categories exercised".to_owned()],
        }),
    )
    .expect("all-failure iteration validates");
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(
            roster_config(analyst_operator_workers(), ToolVisibility::Summary),
            Vec::new(),
        ),
        CoordinatorCall::Continuation(ContinuationThread::new(vec![iteration]).expect("one")),
    );
    snapshot_coordinator("coordinator_call_all_failure_categories", &scenario).await;
}

/// Iterations 1-2 behind `coordinator_call3_failures` (also the R5 gate's
/// trace data). Iteration 2 re-uses task id 0 and repeats iteration 1's
/// failed handle+category, so the continuation shows cross-iteration
/// artifact re-listing and a repeated-failure pattern.
fn failure_thread_iterations() -> Vec<IterationFixture> {
    let iteration_one = IterationFixture::new(
        decision(
            "Evidence gathering requires log access.",
            vec![
                leaf(
                    "Collect the error groups from the payments logs for the last six hours",
                    Some("analyst"),
                ),
                leaf("Query deployment history for the error window", None),
            ],
        ),
        vec![
            TaskOutcome::Complete {
                // Defect C: the spilled stand-in echoes the claim summary,
                // so Summary/Evidence render byte-identically.
                result: CompletedResultFixture::Spilled {
                    stand_in: SpilledStandIn::ClaimEcho {
                        claim: claim(
                            "Found 47 error groups; connection timeouts dominate",
                            Confidence::High,
                        ),
                    },
                    artifact: spilled("task-0-analyst-iter-1-result.txt", 5200),
                },
                traces: vec![success_trace(
                    "log_search",
                    "searching payments error patterns",
                    8200,
                    Some("task-0-analyst-iter-1-log_search-0-output.txt"),
                )],
            },
            TaskOutcome::Failed {
                report: FailedResultFixture::Hard {
                    error: "403 Forbidden from the deployment API".to_owned(),
                    category: FailureCategory::AgentError,
                },
                traces: vec![],
            },
        ],
        Some(FailureSummary {
            reasoning: "Execution failed: 1 task(s) failed, 0 task(s) blocked by dependencies."
                .to_owned(),
            gaps: vec!["Deployment history unavailable due to permissions".to_owned()],
        }),
    )
    .expect("iteration 1 validates");

    let iteration_two = IterationFixture::new(
        decision(
            "Retry the deployment lookup and correlate the evidence.",
            vec![
                leaf(
                    "Re-run the error-group collection over the widened twelve-hour window",
                    Some("analyst"),
                ),
                leaf(
                    "Query deployment history for the error window",
                    Some("analyst"),
                ),
                leaf(
                    "Correlate the deployment timeline with the error groups",
                    Some("operator"),
                ),
                leaf(
                    "Draft the incident summary from the correlated evidence",
                    Some("operator"),
                ),
            ],
        ),
        vec![
            TaskOutcome::Complete {
                // Spilled with a bounded raw preview standing in (no claim).
                result: CompletedResultFixture::Spilled {
                    stand_in: SpilledStandIn::RawPreview {
                        preview: ResultPreview::new(
                            "Widened window confirms 52 error groups; timeouts still dominate.",
                        )
                        .expect("non-empty preview"),
                        claim: None,
                    },
                    artifact: spilled("task-0-analyst-iter-2-result.txt", 6100),
                },
                traces: vec![success_trace(
                    "log_search",
                    "re-running with the widened window",
                    9400,
                    Some("task-0-analyst-iter-2-log_search-0-output.txt"),
                )],
            },
            TaskOutcome::Failed {
                report: FailedResultFixture::Hard {
                    error: "403 Forbidden from the deployment API".to_owned(),
                    category: FailureCategory::AgentError,
                },
                // Unconditional failed-entry chain: success with reasoning,
                // success without, then the failing call.
                traces: vec![
                    success_trace("get_deployments", "checking staging first", 1200, None),
                    success_trace("get_deployments", "", 900, None),
                    failed_trace(
                        "get_deployments",
                        "querying prod-us-east-1",
                        30200,
                        "403 Forbidden",
                    ),
                ],
            },
            TaskOutcome::Failed {
                report: FailedResultFixture::Soft {
                    claim: claim(
                        "Only partial correlation evidence was recoverable",
                        Confidence::Low,
                    ),
                    artifact: Some(spilled("task-2-operator-iter-2-result.txt", 5200)),
                },
                traces: vec![],
            },
            TaskOutcome::Blocked,
        ],
        Some(FailureSummary {
            reasoning: "Execution failed: 2 task(s) failed, 1 task(s) blocked by dependencies."
                .to_owned(),
            gaps: vec![
                "Deployment history remains unavailable".to_owned(),
                "Incident summary blocked on the correlation task".to_owned(),
            ],
        }),
    )
    .expect("iteration 2 validates");

    vec![iteration_one, iteration_two]
}

#[tokio::test]
async fn coordinator_call3_failures() {
    // Budget 4 keeps `%%URGENCY%%` empty here; `(FINAL ATTEMPT)` is owned
    // by `coordinator_call4_final_urgency` (MANIFEST §3).
    let config = OrchestrationConfig {
        max_planning_cycles: 4,
        ..roster_config(analyst_operator_workers(), ToolVisibility::Summary)
    };
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(config, Vec::new()),
        CoordinatorCall::Continuation(
            ContinuationThread::new(failure_thread_iterations()).expect("two iterations"),
        ),
    );
    snapshot_coordinator("coordinator_call3_failures", &scenario).await;
}

#[tokio::test]
async fn coordinator_call4_final_urgency() {
    let iteration = |window: &str| {
        IterationFixture::new(
            decision(
                "One focused lookup continues the investigation.",
                vec![leaf(
                    &format!("Collect the error groups for the {window} window"),
                    Some("analyst"),
                )],
            ),
            vec![TaskOutcome::Complete {
                result: CompletedResultFixture::Inline {
                    result: evidence(&format!(
                        "The {window} window shows the same three dominant failure groups."
                    )),
                    claim: None,
                },
                traces: vec![],
            }],
            None,
        )
        .expect("urgency iterations validate")
    };
    let config = OrchestrationConfig {
        max_planning_cycles: 4,
        ..roster_config(analyst_operator_workers(), ToolVisibility::Summary)
    };
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(config, Vec::new()),
        CoordinatorCall::Continuation(
            ContinuationThread::new(vec![
                iteration("six-hour"),
                iteration("twelve-hour"),
                iteration("twenty-four-hour"),
            ])
            .expect("three iterations"),
        ),
    );
    snapshot_coordinator("coordinator_call4_final_urgency", &scenario).await;
}

// ============================================================================
// Worker scenarios
// ============================================================================

const ROLE_PREAMBLE: &str = "You are the payments log analyst. Ground every claim in log evidence.";

fn no_appends() -> WorkerPreambleAppends {
    WorkerPreambleAppends {
        scratchpad: ScratchpadWiring::NotWired,
        skills: Vec::new(),
    }
}

fn bare_role_preamble() -> WorkerPreambleFixture {
    WorkerPreambleFixture::Role {
        role_preamble: ROLE_PREAMBLE.to_owned(),
        vector_stores: Vec::new(),
        appends: no_appends(),
    }
}

fn completed_task(id: usize, description: &str, result: &CompletedResultFixture) -> Task {
    let mut task = Task::new(id, description, "fixture ancestor");
    task.complete(result.raw_result());
    task.structured_output = result.claim().map(|claim| StructuredTaskOutput {
        summary: claim.summary().to_owned(),
        confidence: claim.confidence(),
    });
    task
}

/// A two-task plan: completed ancestor 0, target task 1 (direct only).
fn direct_frame(ancestor: &CompletedResultFixture, target_description: &str) -> FrameGraph {
    let mut plan = Plan::new(QUERY);
    let mut ancestor_task = completed_task(
        0,
        "Collect the error groups from the payments logs",
        ancestor,
    );
    ancestor_task.worker = Some("analyst".to_owned());
    plan.add_task(ancestor_task);
    plan.add_task(Task::new(1, target_description, "fixture target").with_dependency(0));
    FrameGraph::new(plan, 1).expect("direct frame renders")
}

#[tokio::test]
async fn worker_role_frame_direct() {
    let scenario = WorkerScenario {
        preamble: WorkerPreambleFixture::Role {
            role_preamble: ROLE_PREAMBLE.to_owned(),
            vector_stores: vec![
                vector_store(
                    "runbooks",
                    Some("Operational runbooks for the payments platform"),
                ),
                vector_store("telemetry", None),
            ],
            appends: WorkerPreambleAppends {
                scratchpad: ScratchpadWiring::Wired,
                skills: fixture_skills(),
            },
        },
        frame: WorkerFrameFixture::Populated(direct_frame(
            &CompletedResultFixture::Inline {
                result: evidence(
                    "Found 47 error groups across 3 services; top: connection timeouts (38%).",
                ),
                claim: Some(claim(
                    "Found 47 error groups; connection timeouts dominate",
                    Confidence::High,
                )),
            },
            "Correlate the error groups with the deployment timeline",
        )),
    };
    snapshot_worker("worker_role_frame_direct", &scenario).await;
}

#[tokio::test]
async fn worker_role_frame_transitive() {
    // Oldest ancestor (task 0) is transitive at distance 2; task 1 is the
    // direct dependency. Plan-order rendering keeps task 0 first (defect E).
    let mut plan = Plan::new(QUERY);
    let mut task0 = completed_task(
        0,
        "Collect the error groups from the payments logs",
        &CompletedResultFixture::Inline {
            result: evidence("Found 47 error groups across 3 services."),
            claim: None,
        },
    );
    task0.worker = Some("analyst".to_owned());
    plan.add_task(task0);
    let mut task1 = completed_task(
        1,
        "Assemble the deployment timeline",
        &CompletedResultFixture::Inline {
            result: evidence("Deployment timeline assembled: 3 deploys in the error window."),
            claim: Some(claim(
                "Three deploys landed in the window",
                Confidence::Medium,
            )),
        },
    );
    task1.worker = Some("operator".to_owned());
    task1.dependencies = vec![0];
    plan.add_task(task1);
    plan.add_task(
        Task::new(
            2,
            "Correlate the deployment timeline with the error groups",
            "fixture target",
        )
        .with_dependency(1),
    );
    let scenario = WorkerScenario {
        preamble: bare_role_preamble(),
        frame: WorkerFrameFixture::Populated(
            FrameGraph::new(plan, 2).expect("transitive frame renders"),
        ),
    };
    snapshot_worker("worker_role_frame_transitive", &scenario).await;
}

#[tokio::test]
async fn worker_role_frame_spilled_claim_echo() {
    let scenario = WorkerScenario {
        preamble: bare_role_preamble(),
        frame: WorkerFrameFixture::Populated(direct_frame(
            &CompletedResultFixture::Spilled {
                stand_in: SpilledStandIn::ClaimEcho {
                    claim: claim(
                        "Found 47 error groups; connection timeouts dominate",
                        Confidence::High,
                    ),
                },
                artifact: spilled("task-0-analyst-iter-1-result.txt", 5200),
            },
            "Correlate the error groups with the deployment timeline",
        )),
    };
    snapshot_worker("worker_role_frame_spilled_claim_echo", &scenario).await;
}

#[tokio::test]
async fn worker_frame_spilled_no_preview() {
    let scenario = WorkerScenario {
        preamble: bare_role_preamble(),
        frame: WorkerFrameFixture::Populated(direct_frame(
            &CompletedResultFixture::Spilled {
                stand_in: SpilledStandIn::NoPreview,
                artifact: spilled("task-0-analyst-iter-1-result.txt", 5200),
            },
            "Correlate the error groups with the deployment timeline",
        )),
    };
    snapshot_worker("worker_frame_spilled_no_preview", &scenario).await;
}

/// The two empty-`%%CONTEXT%%` fixtures share one task text and preamble:
/// the renders are byte-identical (pre-approved decision 4), and the two
/// snapshots demonstrate it; only the CAUSE differs.
const EMPTY_FRAME_TASK: &str =
    "Collect the error groups from the payments logs for the last six hours";

#[tokio::test]
async fn worker_first_turn_empty() {
    let scenario = WorkerScenario {
        preamble: bare_role_preamble(),
        frame: WorkerFrameFixture::EmptyFirstTurn {
            task: EMPTY_FRAME_TASK.to_owned(),
        },
    };
    snapshot_worker("worker_first_turn_empty", &scenario).await;
}

#[tokio::test]
async fn worker_replan_boundary_empty() {
    let scenario = WorkerScenario {
        preamble: bare_role_preamble(),
        frame: WorkerFrameFixture::EmptyReplanBoundary {
            task: EMPTY_FRAME_TASK.to_owned(),
        },
    };
    snapshot_worker("worker_replan_boundary_empty", &scenario).await;
}

#[tokio::test]
async fn worker_generic_fallback() {
    let scenario = WorkerScenario {
        preamble: WorkerPreambleFixture::Generic {
            custom_prompt: None,
            appends: WorkerPreambleAppends {
                scratchpad: ScratchpadWiring::Wired,
                skills: fixture_skills(),
            },
        },
        frame: WorkerFrameFixture::EmptyFirstTurn {
            task: EMPTY_FRAME_TASK.to_owned(),
        },
    };
    snapshot_worker("worker_generic_fallback", &scenario).await;
}

#[tokio::test]
async fn worker_generic_custom() {
    let scenario = WorkerScenario {
        preamble: WorkerPreambleFixture::Generic {
            custom_prompt: Some(
                "Prefer structured summaries over prose; cite exact values.".to_owned(),
            ),
            appends: no_appends(),
        },
        frame: WorkerFrameFixture::EmptyFirstTurn {
            task: EMPTY_FRAME_TASK.to_owned(),
        },
    };
    snapshot_worker("worker_generic_custom", &scenario).await;
}

// ============================================================================
// REQUIRED comparison gates (DESIGN.md R3/R5)
// ============================================================================

/// R3 (coordinator side): the harness-composed preamble byte-equals the
/// preamble the REAL `create_coordinator` assembles over a tempdir-backed
/// config with skills and session history enabled and vector stores
/// disabled. The vector append position stays re-stated (live-manager
/// construction) and is named as the residue in `DESIGN.md`.
#[tokio::test]
async fn gate_r3_coordinator_preamble_matches_create_coordinator() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let memory_dir = tempdir.path().join("memory");
    let session_id = "s2-gate-session";

    // A prior-run manifest on disk makes `load_session_manifests` return
    // exactly the fixture's manifest list.
    let prior = routed_manifest();
    let prior_dir = memory_dir.join(session_id).join("run-prior-0001");
    std::fs::create_dir_all(&prior_dir).expect("prior run dir");
    std::fs::write(
        prior_dir.join("manifest.json"),
        serde_json::to_string_pretty(&prior).expect("manifest serializes"),
    )
    .expect("manifest written");

    let skills = fixture_skills();
    let orchestration = OrchestrationConfig {
        enabled: true,
        tools_in_planning: ToolVisibility::None,
        artifacts: ArtifactsConfig {
            memory_dir: Some(memory_dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
        ..Default::default()
    };
    let agent_config = AgentRuntimeConfig {
        llm: LlmConfig::Ollama {
            model: "llama3".to_owned(),
            base_url: None,
            max_tokens: None,
            context_window: None,
            temperature: None,
            fallback_tool_parsing: false,
            additional_params: None,
        },
        agent: aura_config::AgentSettings {
            system_prompt: SOURCE_PLAYBOOK.to_owned(),
            skills: skills.clone(),
            ..Default::default()
        },
        session_id: Some(session_id.to_owned()),
        orchestration: Some(orchestration),
        ..Default::default()
    };
    let orchestrator = Orchestrator::new(agent_config)
        .await
        .expect("gate orchestrator constructs");
    let real = orchestrator
        .coordinator_preamble_for_golden(true)
        .await
        .expect("create_coordinator assembles the real preamble");

    let fixture = PreambleFixture {
        playbook: SOURCE_PLAYBOOK.to_owned(),
        tools: CoordinatorToolConfig {
            recon: ReconTools::Included,
            history: HistoryTools::Included,
        },
        skills,
        vector_stores: Vec::new(),
        session_history: Some(SessionHistoryFixture::new(vec![prior]).expect("one prior manifest")),
    };
    let composed = super::envelope::compose_coordinator_preamble(&fixture);

    assert_eq!(
        composed, real,
        "R3: the harness-composed coordinator preamble must byte-equal \
         create_coordinator output (append order drifted?)"
    );
}

/// R3 (worker side): the harness-composed worker preamble byte-equals the
/// preamble the REAL `create_worker` assembles for both the named-role branch
/// (with assigned vector stores and skills) and the generic branch (no custom
/// prompt, no vector stores, skills only).  Scratchpad is enabled in config but
/// the test environment has no MCP, so production cannot wire scratchpad tools
/// and does not append the scratchpad preamble; the comparison fixtures use
/// `NotWired` to match that production output.  The vector → skills order is
/// the closed portion of the residue; the scratchpad append position stays a
/// conditional residue (DESIGN.md).
#[tokio::test]
async fn gate_r3_worker_preamble_matches_create_worker() {
    let skills = fixture_skills();
    let vector_stores = vec![
        vector_store(
            "runbooks",
            Some("Operational runbooks for the payments platform"),
        ),
        vector_store("telemetry", None),
    ];

    let worker_config = WorkerConfig {
        description: "Payments log analyst".to_owned(),
        preamble: ROLE_PREAMBLE.to_owned(),
        mcp_filter: Vec::new(),
        vector_stores: vec!["runbooks".to_owned(), "telemetry".to_owned()],
        turn_depth: None,
        llm: None,
        scratchpad: None,
        skills: None,
    };
    let mut workers = HashMap::new();
    workers.insert("role-worker".to_owned(), worker_config);

    let orchestration = OrchestrationConfig {
        enabled: true,
        workers,
        ..Default::default()
    };
    let agent_config = AgentRuntimeConfig {
        llm: LlmConfig::Ollama {
            model: "llama3".to_owned(),
            base_url: None,
            max_tokens: None,
            context_window: None,
            temperature: None,
            fallback_tool_parsing: false,
            additional_params: None,
        },
        agent: aura_config::AgentSettings {
            system_prompt: "unused".to_owned(),
            skills: skills.clone(),
            scratchpad: Some(ScratchpadConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        },
        vector_stores: vector_stores.clone(),
        orchestration: Some(orchestration),
        ..Default::default()
    };
    let orchestrator = Orchestrator::new(agent_config)
        .await
        .expect("gate orchestrator constructs with mcp and persistence disabled");

    let real = orchestrator
        .worker_preamble_for_golden(0, 1, Some("role-worker"))
        .await
        .expect("create_worker assembles the real worker preamble");

    let fixture = WorkerPreambleFixture::Role {
        role_preamble: ROLE_PREAMBLE.to_owned(),
        vector_stores,
        appends: WorkerPreambleAppends {
            // No MCP in the test environment means production cannot wire
            // scratchpad tools, so the scratchpad preamble is not appended.
            // Match that production output rather than the full fixture.
            scratchpad: ScratchpadWiring::NotWired,
            skills: skills.clone(),
        },
    };
    let composed = compose_worker_preamble(&fixture);

    assert_eq!(
        composed, real,
        "R3: the harness-composed worker preamble must byte-equal \
         create_worker output (append order drifted?)"
    );

    // Generic-worker branch: no custom prompt, no vector stores, skills only.
    // Scratchpad tools cannot wire without accessible MCP tools, so the gate
    // matches production output with `NotWired`.
    let real_generic = orchestrator
        .worker_preamble_for_golden(0, 1, None)
        .await
        .expect("create_worker assembles the real generic worker preamble");

    let fixture_generic = WorkerPreambleFixture::Generic {
        custom_prompt: None,
        appends: WorkerPreambleAppends {
            scratchpad: ScratchpadWiring::NotWired,
            skills,
        },
    };
    let composed_generic = compose_worker_preamble(&fixture_generic);

    assert_eq!(
        composed_generic, real_generic,
        "R3: the harness-composed generic worker preamble must byte-equal \
         create_worker output (append order drifted?)"
    );
}

/// R5: the harness's in-memory trace merge equals the production
/// disk-persistence merge (`load_tool_records_for_task` scanned per task
/// across iterations, mapped through `ToolTraceEntry::from`) for the same
/// records, written through a tempdir-backed `ExecutionPersistence`.
/// Trace data is the `coordinator_call3_failures` corpus data, so the
/// gate covers exactly the merge the snapshots rely on.
#[tokio::test]
async fn gate_r5_trace_merge_matches_persistence_loader() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let mut persistence = ExecutionPersistence::new(tempdir.path().join("memory"), None)
        .await
        .expect("gate persistence constructs");

    let iterations = failure_thread_iterations();
    let record_for = |entry: &ToolTraceEntry| ToolCallRecord {
        tool: entry.tool.clone(),
        arguments: serde_json::json!({}),
        reasoning: entry.reasoning.clone(),
        output: match &entry.outcome {
            ToolOutcome::Success { output_bytes } => {
                Some("o".repeat(usize::try_from(*output_bytes).expect("small fixture output")))
            }
            ToolOutcome::Error { .. } => None,
        },
        error: match &entry.outcome {
            ToolOutcome::Success { .. } => None,
            ToolOutcome::Error { message } => Some(message.clone()),
        },
        duration_ms: entry.duration_ms,
        artifact_filename: entry.artifact_filename.clone(),
    };

    for (idx, iteration) in iterations.iter().enumerate() {
        if idx > 0 {
            persistence.start_new_iteration();
        }
        for (task_id, outcome) in iteration.outcomes().iter().enumerate() {
            let traces = match outcome {
                TaskOutcome::Complete { traces, .. } | TaskOutcome::Failed { traces, .. } => traces,
                TaskOutcome::Blocked => continue,
            };
            for entry in traces {
                persistence
                    .append_tool_call(task_id, 1, &record_for(entry))
                    .await
                    .expect("tool record written");
            }
        }
    }

    // Production merge: `load_tool_traces_for_plan`'s per-task loop over
    // the pub `load_tool_records_for_task` disk scan (the loop itself is
    // reproduced here — a named residue in DESIGN.md R5).
    let current_plan = {
        let last = iterations.last().expect("two iterations");
        let mut plan = last.decision().plan();
        for (task, outcome) in plan.tasks.iter_mut().zip(last.outcomes()) {
            if let TaskOutcome::Complete { result, .. } = outcome {
                task.complete(result.raw_result());
            }
        }
        plan
    };
    let mut production: HashMap<usize, Vec<ToolTraceEntry>> = HashMap::new();
    for task in &current_plan.tasks {
        let records = persistence.load_tool_records_for_task(task.id).await;
        if records.is_empty() {
            continue;
        }
        production.insert(task.id, records.iter().map(ToolTraceEntry::from).collect());
    }

    let harness = super::envelope::merged_traces(&iterations, &current_plan);

    assert_eq!(
        serde_json::to_value(&harness).expect("harness merge serializes"),
        serde_json::to_value(&production).expect("production merge serializes"),
        "R5: the harness's in-memory trace merge must equal the production \
         disk-persistence merge for the same records"
    );
}

// ============================================================================
// Constructor validation (parse-don't-validate spot checks)
// ============================================================================

#[test]
fn fixture_constructors_reject_unreachable_states() {
    assert!(matches!(
        PlanningBudget::new(0),
        Err(FixtureError::ZeroPlanningBudget)
    ));
    assert!(matches!(
        SessionHistoryFixture::new(vec![]),
        Err(FixtureError::EmptySessionHistory)
    ));
    assert!(matches!(
        SessionHistoryFixture::new(vec![routed_manifest(), direct_manifest()]),
        Err(FixtureError::SessionHistoryNotRecentFirst)
    ));
    assert!(matches!(
        ContinuationThread::new(vec![]),
        Err(FixtureError::EmptyContinuationThread)
    ));
    assert!(matches!(
        PlanDecision::new(PlanningResponse::Direct {
            response: "done".to_owned(),
            routing_rationale: "r".to_owned(),
            response_summary: None,
        }),
        Err(FixtureError::TerminalDecisionMidThread)
    ));
    assert!(matches!(
        IterationFixture::new(
            decision("one task", vec![leaf("a task", Some("analyst"))]),
            vec![],
            None
        ),
        Err(FixtureError::OutcomeCountMismatch {
            tasks: 1,
            outcomes: 0
        })
    ));
    assert!(matches!(
        IterationFixture::new(
            decision("one task", vec![leaf("a task", Some("analyst"))]),
            vec![TaskOutcome::Complete {
                result: CompletedResultFixture::Inline {
                    result: evidence("done"),
                    claim: None,
                },
                traces: vec![],
            }],
            Some(FailureSummary::default())
        ),
        Err(FixtureError::FailureSummaryWithoutFailure)
    ));
    // Recon with inlined worker tools is unreachable.
    assert!(matches!(
        CoordinatorScenario::new(
            preamble(CoordinatorToolConfig {
                recon: ReconTools::Included,
                history: HistoryTools::Excluded,
            }),
            goal(),
            WorkerRosterFixture::new(
                roster_config(analyst_operator_workers(), ToolVisibility::Summary),
                Vec::new(),
            ),
            CoordinatorCall::Initial,
        ),
        Err(FixtureError::ReconRequiresUninlinedTools)
    ));
    // A COMPLETED outcome under a worker absent from the roster is
    // unreachable (production fails unknown-worker tasks at create_worker).
    assert!(matches!(
        CoordinatorScenario::new(
            preamble(no_optional_tools()),
            goal(),
            WorkerRosterFixture::new(
                roster_config(HashMap::new(), ToolVisibility::Summary),
                Vec::new(),
            ),
            CoordinatorCall::Continuation(
                ContinuationThread::new(vec![clean_iteration()]).expect("one iteration"),
            ),
        ),
        Err(FixtureError::CompletedTaskUnknownWorker { task_id: 0, .. })
    ));
    // More iterations than the budget allows a further planning call for.
    let one_cycle = OrchestrationConfig {
        max_planning_cycles: 1,
        ..roster_config(analyst_operator_workers(), ToolVisibility::Summary)
    };
    assert!(matches!(
        CoordinatorScenario::new(
            preamble(no_optional_tools()),
            goal(),
            WorkerRosterFixture::new(one_cycle, Vec::new()),
            CoordinatorCall::Continuation(
                ContinuationThread::new(failure_thread_iterations()).expect("two iterations"),
            ),
        ),
        Err(FixtureError::IterationsExhaustBudget {
            iterations: 2,
            budget: 1
        })
    ));
    // A populated-frame fixture whose graph yields no frame.
    let mut plan = Plan::new(QUERY);
    plan.add_task(Task::new(0, "unstarted predecessor", "fixture"));
    plan.add_task(Task::new(1, "target", "fixture").with_dependency(0));
    assert!(matches!(
        FrameGraph::new(plan, 1),
        Err(FixtureError::FrameHasNoCompletedAncestor { task_id: 1 })
    ));
    // Reused context types keep their own parsing rules.
    assert!(matches!(
        EvidenceText::new(
            "body\n\n[Full result (5200 chars) saved to artifact: task-0-analyst-iter-1-result.txt]"
        ),
        Err(ContextError::InlineEvidenceCarriesSpillFooter)
    ));
}

/// The two empty-`%%CONTEXT%%` snapshots are byte-identical (pre-approved
/// decision 4): the branches differ causally, not mechanically. The
/// normalized document embeds all three envelope surfaces (system,
/// messages, tools JSON), so one equality covers the full triple.
#[tokio::test]
async fn empty_frame_branches_render_byte_identically() {
    let fresh = WorkerScenario {
        preamble: bare_role_preamble(),
        frame: WorkerFrameFixture::EmptyFirstTurn {
            task: EMPTY_FRAME_TASK.to_owned(),
        },
    };
    let replan = WorkerScenario {
        preamble: bare_role_preamble(),
        frame: WorkerFrameFixture::EmptyReplanBoundary {
            task: EMPTY_FRAME_TASK.to_owned(),
        },
    };
    let fresh_snapshot: NormalizedSnapshot =
        normalize(&worker_envelope(&fresh).await.expect("fresh envelope"));
    let replan_snapshot: NormalizedSnapshot =
        normalize(&worker_envelope(&replan).await.expect("replan envelope"));
    assert_eq!(fresh_snapshot, replan_snapshot);
}

// ============================================================================
// REQUIRED comparison gates (DESIGN.md R8)
// ============================================================================

fn disabled_persistence() -> std::sync::Arc<tokio::sync::Mutex<ExecutionPersistence>> {
    std::sync::Arc::new(tokio::sync::Mutex::new(ExecutionPersistence::disabled()))
}

/// R8 (coordinator tool registration order): the tool names returned by the
/// production seam mirror the order `build_agent_with_tools` uses when all
/// optional tool groups are present.
#[tokio::test]
async fn gate_r8_coordinator_tool_order() {
    let routing = RoutingToolSet::new();
    let tools = CoordinatorTools::new_for_golden_test(
        Some(ListToolsTool::new(Vec::new())),
        Some(InspectToolParamsTool::new(HashMap::new())),
        Vec::new(), // vector tools are live-manager-constructed (MANIFEST §6a)
        routing,
        Some(ReadArtifactTool::new(disabled_persistence())),
        Some(ListPriorRunsTool::new(
            disabled_persistence(),
            std::path::PathBuf::new(),
        )),
        crate::skill_tool::SkillToolset::new(&fixture_skills()),
    );
    let order = Orchestrator::coordinator_tool_order_for_golden(&tools);
    assert_eq!(
        order,
        vec![
            "list_tools",
            "inspect_tool_params",
            "respond_directly",
            "create_plan",
            "request_clarification",
            "read_artifact",
            "list_prior_runs",
            "load_skill",
            "read_skill_file",
        ],
        "R8: coordinator tool registration order must match build_agent_with_tools"
    );
}

/// R8 (worker tool registration order): `worker_tool_definitions` returns the
/// in-repo worker tools in the order `Agent::add_all_tools` registers them.
#[tokio::test]
async fn gate_r8_worker_tool_order() {
    let scenario = WorkerScenario {
        preamble: WorkerPreambleFixture::Generic {
            custom_prompt: None,
            appends: WorkerPreambleAppends {
                scratchpad: ScratchpadWiring::NotWired,
                skills: fixture_skills(),
            },
        },
        frame: WorkerFrameFixture::EmptyFirstTurn {
            task: EMPTY_FRAME_TASK.to_owned(),
        },
    };
    let tools = worker_tool_definitions(&scenario).await;
    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(
        names,
        vec![
            "read_artifact".to_owned(),
            "submit_result".to_owned(),
            "load_skill".to_owned(),
            "read_skill_file".to_owned(),
        ],
        "R8: worker tool definition order must match Agent::add_all_tools"
    );
}

/// R8 (conversation growth): the envelope builder grows the coordinator
/// conversation using the same production helpers that `plan_with_routing`
/// uses.
#[tokio::test]
async fn gate_r8_conversation_growth() {
    let config = OrchestrationConfig {
        max_planning_cycles: 4,
        ..roster_config(analyst_operator_workers(), ToolVisibility::Summary)
    };
    let scenario = scenario(
        preamble(no_optional_tools()),
        WorkerRosterFixture::new(config, Vec::new()),
        CoordinatorCall::Continuation(
            ContinuationThread::new(failure_thread_iterations()).expect("two iterations"),
        ),
    );
    let envelope = coordinator_envelope(&scenario)
        .await
        .expect("corpus envelope assembles");

    let orchestrator = section_orchestrator(&scenario).await;
    let (worker_section, _worker_field, worker_guidelines) =
        orchestrator.worker_prompt_sections_for_golden();
    let planning_wrapper = Orchestrator::build_planning_wrapper(
        scenario.query().as_str(),
        &worker_section,
        &worker_guidelines,
    );

    let mut expected = vec![rig::completion::Message::user(planning_wrapper)];
    if let CoordinatorCall::Continuation(thread) = scenario.call() {
        let iterations = thread.iterations();
        let config = scenario.roster().config();
        let mut failure_history = Vec::new();
        for (idx, iteration) in iterations.iter().enumerate() {
            let iteration_number = idx + 1;
            let plan = executed_plan(iteration);
            failure_history.extend(Orchestrator::iteration_failures_for_golden(
                &plan,
                iteration_number,
            ));
            let traces = merged_traces(&iterations[..=idx], &plan);
            let context = IterationContext::new(
                iteration_number,
                plan,
                iteration.failure_summary().cloned(),
                failure_history.clone(),
                traces,
            )
            .with_pinned_goal(scenario.query().clone());

            Orchestrator::push_assistant_turn_for_golden(
                &mut expected,
                iteration.decision().as_response(),
                "",
            );
            Orchestrator::push_user_turn_for_golden(
                &mut expected,
                &Orchestrator::continuation_wrapper_for_golden(
                    &context,
                    scenario.budget().get(),
                    config.show_tool_reasoning_in_continuation(),
                    config.result_summary_length(),
                ),
            );
        }
    }

    assert_eq!(
        envelope.messages, expected,
        "R8: the envelope builder must grow the conversation exactly like plan_with_routing"
    );
}
