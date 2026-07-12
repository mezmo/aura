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
//! risk in `DESIGN.md` R3/R5/R8, with the comparison gates the
//! implementation step must land):
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
//! 6. the tool REGISTRATION ORDER of `create_coordinator_agent` and the
//!    worker builder (the definitions themselves are production calls;
//!    their sequence is re-stated).

#![expect(
    dead_code,
    reason = "S2 type skeleton: the envelope seam lands before the snapshot tests that consume it (S2 implementation step)"
)]
#![expect(
    unused_variables,
    reason = "S2 type skeleton: builder bodies are todo!() until the S2 implementation step"
)]

use rig::completion::{Message, ToolDefinition};

use super::scenario::{CoordinatorScenario, FixtureError, WorkerScenario};

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
        todo!()
    }
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
    todo!()
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
    todo!()
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
    todo!()
}

/// The worker's in-repo tool definitions: `submit_result`, `read_artifact`,
/// plus `load_skill`/`read_skill_file` when the preamble fixture carries
/// skills (the worker builder registers `SkillToolset` from the same
/// config). MCP, vector-search, and scratchpad tool definitions are
/// excluded with reasons in `MANIFEST.md` §6/§6a.
async fn worker_tool_definitions(
    scenario: &WorkerScenario,
) -> Result<Vec<ToolDefinition>, FixtureError> {
    todo!()
}
