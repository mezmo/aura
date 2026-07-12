//! The envelope-composition seam: build the complete aura-level request
//! envelope for a scenario by calling the REAL production assembly
//! functions.
//!
//! # The envelope claim (scope)
//!
//! There is no in-repo seam returning the final provider request; final
//! assembly happens in the pinned rig fork. The envelope snapshotted here
//! is the aura-level triple handed across that boundary: the system
//! preamble string, the ordered message list, and the serialized tool
//! definitions. The rig-fork mapping is a named residual risk in
//! `DESIGN.md` and `MANIFEST.md`.
//!
//! # Production functions called (never re-implemented)
//!
//! - `config::build_coordinator_preamble`, `config::build_worker_preamble`,
//!   `config::build_vector_store_context`, `config::WORKER_PREAMBLE_TEMPLATE`
//! - `crate::skill_tool::render_skill_catalog`,
//!   `crate::skill_tool::SkillToolset::new` (pure over `SkillConfig`; no
//!   filesystem discovery)
//! - `persistence::build_session_context`
//! - `Orchestrator::build_planning_wrapper`,
//!   `Orchestrator::continuation_wrapper_for_golden` (test accessor over the
//!   private `build_continuation_wrapper`),
//!   `Orchestrator::worker_prompt_sections_for_golden` (test accessor over
//!   the private `build_worker_prompt_sections`),
//!   `Orchestrator::iteration_failures_for_golden` (test accessor over the
//!   private `collect_iteration_failures` — the failure-history fold is
//!   production code, not a test-side re-statement),
//!   `Orchestrator::compact_decision_turn`,
//!   `Orchestrator::build_task_context`
//! - `templates::render_worker_task_prompt`
//! - `IterationContext::build_continuation_prompt` (via the continuation
//!   wrapper)
//! - each in-repo `Tool::definition` (routing tools, recon tools,
//!   `read_artifact`, `list_prior_runs`, `submit_result`, and
//!   `load_skill`/`read_skill_file` when skills are configured)
//!
//! What the builder necessarily RE-STATES (each a named false-pass drift
//! risk in `DESIGN.md` R3/R5/R8; the comparison gates landed in
//! `golden_tests.rs` close item 4 and, vector position excepted, item 1 —
//! items 2, 3, 5, and 6 stay named residues):
//!
//! 1. the preamble append order of `create_coordinator`, INCLUDING the
//!    bare `push('\n')` before `build_session_context`;
//! 2. the worker constructor's append order (role branch: vector-store
//!    context, then scratchpad, then skills; generic branch: scratchpad,
//!    then skills);
//! 3. the conversation growth rule of `plan_with_routing` (user wrapper
//!    pushed verbatim, compact assistant turn per prior call);
//! 4. the run-wide tool-trace merge of `load_tool_traces_for_plan` (which
//!    goes through disk persistence in production);
//! 5. the execute-path context glue between the frame render and the task
//!    template: `format!("{}\n\n", context)`, byte-reproduced ahead of
//!    `render_worker_task_prompt`;
//! 6. the tool REGISTRATION ORDER of `build_agent_with_tools` (reached via
//!    `create_coordinator`) and the worker builder's `add_all_tools` (the
//!    definitions themselves are production calls; their sequence is
//!    re-stated).

use std::collections::HashMap;

use rig::completion::{Message, ToolDefinition};
use rig::tool::Tool;

use super::scenario::{
    CoordinatorCall, CoordinatorScenario, FixtureError, HistoryTools, IterationFixture,
    PreambleFixture, ReconTools, ScratchpadWiring, TaskOutcome, WorkerFrameFixture,
    WorkerPreambleAppends, WorkerPreambleFixture, WorkerScenario,
};
use crate::orchestration::config::{
    OrchestrationConfig, build_coordinator_preamble, build_vector_store_context,
    build_worker_preamble,
};
use crate::orchestration::persistence::{
    ExecutionPersistence, ToolTraceEntry, build_session_context,
};
use crate::orchestration::tools::{
    InspectToolParamsTool, ListPriorRunsTool, ListToolsTool, ReadArtifactTool, RoutingToolSet,
    SubmitResultTool,
};
use crate::orchestration::types::{IterationContext, Plan};
use crate::orchestration::{Orchestrator, config::WORKER_PREAMBLE_TEMPLATE};

/// The complete aura-level request envelope for one model call.
///
/// Business rule: this triple is everything aura hands the pinned rig fork
/// for a coordinator or worker call — S2's request-envelope-identity
/// claim quantifies over exactly these three surfaces. Forbidden state:
/// an identity claim over a partial surface (rendered text only, tools
/// omitted) — the type carries all three, and the snapshot renderer
/// serializes all three.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RequestEnvelope {
    /// The system preamble string, appends included.
    pub(crate) system: String,
    /// The ordered message list, current prompt last.
    pub(crate) messages: Vec<Message>,
    /// The tool definitions, in the order production registers them.
    pub(crate) tools: Vec<ToolDefinition>,
}

impl RequestEnvelope {
    /// Canonical JSON for the tool definitions. `serde_json`'s default
    /// `BTreeMap`-backed maps make this byte-deterministic for a given
    /// definition set.
    pub(crate) fn tools_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.tools).expect("tool definitions serialize")
    }
}

/// Panic (fail loud, never fall back) when `AURA_ESCAPE_HATCH` is set:
/// the corpus pins the default preamble branch, and an env-shaped preamble
/// must not be snapshotted silently.
fn assert_escape_hatch_unset() {
    assert!(
        std::env::var_os("AURA_ESCAPE_HATCH").is_none(),
        "AURA_ESCAPE_HATCH is set: the S2 corpus pins the default preamble branch \
         (MANIFEST.md section 7); unset it before running the golden-frame tests"
    );
}

/// Compose the coordinator system preamble for a [`PreambleFixture`]:
/// `build_coordinator_preamble` output plus the appends in
/// `create_coordinator` order — skill catalog, vector-store context, then
/// `'\n'` + session history. This append SEQUENCE is the R3 re-statement;
/// the R3 comparison gate byte-checks it against real `create_coordinator`
/// output (`golden_tests.rs`).
pub(super) fn compose_coordinator_preamble(fixture: &PreambleFixture) -> String {
    assert_escape_hatch_unset();
    let mut preamble = build_coordinator_preamble(
        &fixture.playbook,
        fixture.tools.recon == ReconTools::Included,
        fixture.tools.history == HistoryTools::Included,
    );
    if let Some(catalog) = crate::skill_tool::render_skill_catalog(&fixture.skills) {
        preamble.push_str(&catalog);
    }
    if !fixture.vector_stores.is_empty() {
        preamble.push_str(&build_vector_store_context(&fixture.vector_stores));
    }
    if let Some(session) = &fixture.session_history {
        preamble.push('\n');
        preamble.push_str(&build_session_context(session.manifests()));
    }
    preamble
}

/// Apply an iteration's outcomes to its decision's flattened plan, exactly
/// as the execute loop records them: completed results via
/// `Task::complete` over the stored raw result, failures via `Task::fail`,
/// blocked tasks left `Pending`, and `structured_output` carrying the
/// worker claim when one exists.
fn executed_plan(iteration: &IterationFixture) -> Plan {
    let mut plan = iteration.decision().plan();
    for (task, outcome) in plan.tasks.iter_mut().zip(iteration.outcomes()) {
        match outcome {
            TaskOutcome::Complete { result, .. } => {
                task.complete(result.raw_result());
                task.structured_output =
                    result
                        .claim()
                        .map(|claim| crate::orchestration::types::StructuredTaskOutput {
                            summary: claim.summary().to_owned(),
                            confidence: claim.confidence(),
                        });
            }
            TaskOutcome::Failed { report, .. } => match report {
                super::scenario::FailedResultFixture::Hard { error, category } => {
                    task.fail(error.clone(), *category);
                }
                super::scenario::FailedResultFixture::Soft { claim, artifact } => {
                    // The soft path stores the worker's (possibly spilled)
                    // result text as the error; the renderer recovers the
                    // pointer from the trailing footer.
                    let error = match artifact {
                        Some(artifact) => format!("{}\n\n{artifact}", claim.summary()),
                        None => claim.summary().to_owned(),
                    };
                    task.fail(
                        error,
                        crate::orchestration::types::FailureCategory::SoftFailure,
                    );
                    task.structured_output =
                        Some(crate::orchestration::types::StructuredTaskOutput {
                            summary: claim.summary().to_owned(),
                            confidence: claim.confidence(),
                        });
                }
            },
            TaskOutcome::Blocked => {}
        }
    }
    plan
}

/// The tool traces an outcome carries (blocked tasks never ran).
fn outcome_traces(outcome: &TaskOutcome) -> &[ToolTraceEntry] {
    match outcome {
        TaskOutcome::Complete { traces, .. } | TaskOutcome::Failed { traces, .. } => traces,
        TaskOutcome::Blocked => &[],
    }
}

/// The in-memory re-statement of `load_tool_traces_for_plan`'s run-wide
/// merge: for each task of the CURRENT plan, concatenate every trace
/// recorded under the same task id across iterations 1..=N, in iteration
/// order, skipping tasks with no records. Production merges through disk
/// persistence (`load_tool_records_for_task` scans
/// `iteration-*/task-{id}.attempt-*.tool-calls.json`); the R5 comparison
/// gate in `golden_tests.rs` byte-checks this fold against that production
/// loader over a tempdir. Fixtures pin one attempt per task per iteration:
/// within one iteration directory the production scan order over multiple
/// attempt files is filesystem-dependent.
pub(super) fn merged_traces(
    iterations: &[IterationFixture],
    current_plan: &Plan,
) -> HashMap<usize, Vec<ToolTraceEntry>> {
    let mut merged = HashMap::new();
    for task in &current_plan.tasks {
        let entries: Vec<ToolTraceEntry> = iterations
            .iter()
            .flat_map(|iteration| {
                iteration
                    .outcomes()
                    .get(task.id)
                    .map(outcome_traces)
                    .unwrap_or(&[])
            })
            .cloned()
            .collect();
        if entries.is_empty() {
            continue;
        }
        merged.insert(task.id, entries);
    }
    merged
}

/// Build the coordinator envelope for the scenario's planning call by
/// calling the production preamble, wrapper, and decision-turn functions
/// and growing the conversation exactly as `plan_with_routing` does.
///
/// Async because worker prompt sections come from a real (MCP-less,
/// persistence-disabled) `Orchestrator` built with `Orchestrator::new`,
/// and tool definitions come from async `Tool::definition` calls.
///
/// # Errors
///
/// Propagates [`FixtureError`] from derived-state assembly. Panics (fail
/// loud, never fall back) when `AURA_ESCAPE_HATCH` is set in the test
/// environment: the corpus pins the default preamble branch.
pub(crate) async fn coordinator_envelope(
    scenario: &CoordinatorScenario,
) -> Result<RequestEnvelope, FixtureError> {
    let system = compose_coordinator_preamble(scenario.preamble());

    let orchestrator = section_orchestrator(scenario).await;
    let (worker_section, _worker_field, worker_guidelines) =
        orchestrator.worker_prompt_sections_for_golden();

    let planning_wrapper = Orchestrator::build_planning_wrapper(
        scenario.query().as_str(),
        &worker_section,
        &worker_guidelines,
    );

    let mut messages = vec![Message::user(planning_wrapper)];
    if let CoordinatorCall::Continuation(thread) = scenario.call() {
        let iterations = thread.iterations();
        let config = scenario.roster().config();
        let mut failure_history = Vec::new();
        for (idx, iteration) in iterations.iter().enumerate() {
            let iteration_number = idx + 1;
            let plan = executed_plan(iteration);
            // Production folds this iteration's failures into the
            // accumulated history BEFORE building the context.
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

            messages.push(Message::assistant(Orchestrator::compact_decision_turn(
                iteration.decision().as_response(),
                "",
            )));
            messages.push(Message::user(
                Orchestrator::continuation_wrapper_for_golden(
                    &context,
                    scenario.budget().get(),
                    config.show_tool_reasoning_in_continuation(),
                    config.result_summary_length(),
                ),
            ));
        }
    }

    let tools = coordinator_tool_definitions(scenario).await?;
    Ok(RequestEnvelope {
        system,
        messages,
        tools,
    })
}

/// Build the real (MCP-less, persistence-disabled) `Orchestrator` whose
/// production `build_worker_prompt_sections` renders the scenario's worker
/// sections. The roster's `OrchestrationConfig` and agent-level
/// vector-store catalog are the only inputs those sections read.
async fn section_orchestrator(scenario: &CoordinatorScenario) -> Orchestrator {
    let agent_config = crate::config::AgentRuntimeConfig {
        vector_stores: scenario.roster().vector_catalog().to_vec(),
        orchestration: Some(scenario.roster().config().clone()),
        ..Default::default()
    };
    debug_assert!(
        scenario.roster().config().memory_dir().is_none(),
        "fixture rosters must not enable persistence"
    );
    Orchestrator::new(agent_config)
        .await
        .expect("orchestrator construction is infallible with mcp and persistence disabled")
}

/// Build the worker envelope: worker preamble branch plus appends
/// (system), one `render_worker_task_prompt` user message over the frame
/// render, and the worker-side tool definitions.
///
/// # Errors
///
/// Propagates [`FixtureError`] from frame assembly.
pub(crate) async fn worker_envelope(
    scenario: &WorkerScenario,
) -> Result<RequestEnvelope, FixtureError> {
    let system = compose_worker_preamble(&scenario.preamble);

    // Execute-path glue: `build_task_context` output is wrapped as
    // `format!("{}\n\n", context)`; an absent frame leaves the slot empty.
    let context_str = match &scenario.frame {
        WorkerFrameFixture::EmptyFirstTurn { .. }
        | WorkerFrameFixture::EmptyReplanBoundary { .. } => String::new(),
        WorkerFrameFixture::Populated(graph) => {
            let context = Orchestrator::build_task_context(graph.plan(), graph.task_id())
                .expect("populated frame renders: validated at FrameGraph construction");
            format!("{context}\n\n")
        }
    };
    let prompt = crate::orchestration::templates::render_worker_task_prompt(
        &crate::orchestration::templates::WorkerTaskVars {
            context: &context_str,
            your_task: scenario.frame.task_text(),
        },
    );

    let tools = worker_tool_definitions(scenario).await;
    Ok(RequestEnvelope {
        system,
        messages: vec![Message::user(prompt)],
        tools,
    })
}

/// Compose the worker system preamble for a [`WorkerPreambleFixture`],
/// re-stating `create_worker`'s per-branch append order (R3): role branch
/// vector-store context, then scratchpad, then skills; generic branch
/// scratchpad, then skills.
pub(super) fn compose_worker_preamble(fixture: &WorkerPreambleFixture) -> String {
    let (mut preamble, appends) = match fixture {
        WorkerPreambleFixture::Role {
            role_preamble,
            vector_stores,
            appends,
        } => {
            let mut preamble =
                WORKER_PREAMBLE_TEMPLATE.replace("{{worker_system_prompt}}", role_preamble);
            if !vector_stores.is_empty() {
                preamble.push_str(&build_vector_store_context(vector_stores));
            }
            (preamble, appends)
        }
        WorkerPreambleFixture::Generic {
            custom_prompt,
            appends,
        } => {
            let config = OrchestrationConfig {
                worker_system_prompt: custom_prompt.clone(),
                ..Default::default()
            };
            (build_worker_preamble(&config), appends)
        }
    };
    append_shared_worker_sections(&mut preamble, appends);
    preamble
}

/// The config-conditional appends shared by both worker branches, in
/// constructor order: scratchpad preamble, then skill catalog.
fn append_shared_worker_sections(preamble: &mut String, appends: &WorkerPreambleAppends) {
    if appends.scratchpad == ScratchpadWiring::Wired {
        preamble.push_str(crate::scratchpad::SCRATCHPAD_PREAMBLE);
    }
    if let Some(catalog) = crate::skill_tool::render_skill_catalog(&appends.skills) {
        preamble.push_str(&catalog);
    }
}

/// A persistence handle for definition-only tool construction; the
/// definitions never touch it.
fn disabled_persistence() -> std::sync::Arc<tokio::sync::Mutex<ExecutionPersistence>> {
    std::sync::Arc::new(tokio::sync::Mutex::new(ExecutionPersistence::disabled()))
}

/// The coordinator's in-repo tool definitions for the scenario's
/// [`super::scenario::CoordinatorToolConfig`]: the three routing tools,
/// `read_artifact`, recon and history tools when included, plus
/// `load_skill`/`read_skill_file` when the preamble fixture carries skills
/// (production registers `SkillToolset` together with the catalog append)
/// — in production registration order (recon, vector, routing,
/// read_artifact, history, skills). Vector-search definitions are excluded
/// (`MANIFEST.md` §6a: live-manager construction), so vector-configured
/// fixtures have a deliberately-partial tools surface.
async fn coordinator_tool_definitions(
    scenario: &CoordinatorScenario,
) -> Result<Vec<ToolDefinition>, FixtureError> {
    let mut tools = Vec::new();
    if scenario.preamble().tools.recon == ReconTools::Included {
        // With `mcp: None`, production hands the recon tools empty
        // name/schema maps; the definitions are static either way.
        tools.push(
            ListToolsTool::new(Vec::new())
                .definition(String::new())
                .await,
        );
        tools.push(
            InspectToolParamsTool::new(HashMap::new())
                .definition(String::new())
                .await,
        );
    }

    let routing = RoutingToolSet::new();
    tools.push(routing.respond_directly.definition(String::new()).await);
    tools.push(routing.create_plan.definition(String::new()).await);
    tools.push(
        routing
            .request_clarification
            .definition(String::new())
            .await,
    );

    tools.push(
        ReadArtifactTool::new(disabled_persistence())
            .definition(String::new())
            .await,
    );

    if scenario.preamble().tools.history == HistoryTools::Included {
        tools.push(
            ListPriorRunsTool::new(disabled_persistence(), std::path::PathBuf::new())
                .definition(String::new())
                .await,
        );
    }

    if let Some(toolset) = crate::skill_tool::SkillToolset::new(&scenario.preamble().skills) {
        tools.push(toolset.load.definition(String::new()).await);
        tools.push(toolset.read_file.definition(String::new()).await);
    }

    Ok(tools)
}

/// The worker's in-repo tool definitions in production registration order
/// (`Agent::add_all_tools`): `read_artifact`, `submit_result`, then
/// `load_skill`/`read_skill_file` when the preamble fixture carries skills
/// (the worker builder registers `SkillToolset` from the same config).
/// MCP, vector-search, and scratchpad tool definitions are excluded with
/// reasons in `MANIFEST.md` §6/§6a.
async fn worker_tool_definitions(scenario: &WorkerScenario) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();
    tools.push(
        ReadArtifactTool::new(disabled_persistence())
            .definition(String::new())
            .await,
    );
    tools.push(
        SubmitResultTool::new(std::sync::Arc::new(tokio::sync::Mutex::new(None)))
            .definition(String::new())
            .await,
    );

    let skills = match &scenario.preamble {
        WorkerPreambleFixture::Role { appends, .. }
        | WorkerPreambleFixture::Generic { appends, .. } => &appends.skills,
    };
    if let Some(toolset) = crate::skill_tool::SkillToolset::new(skills) {
        tools.push(toolset.load.definition(String::new()).await);
        tools.push(toolset.read_file.definition(String::new()).await);
    }

    tools
}
