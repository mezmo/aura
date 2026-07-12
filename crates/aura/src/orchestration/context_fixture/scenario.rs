//! Fixture scenario types: the typed-context backbone of the S2
//! golden-frame corpus.
//!
//! Each type here maps to exactly one business rule of the prompt-assembly
//! path at commit `9df96382`, and names the invalid state it forbids. The
//! full type -> rule -> forbidden-state table is `DESIGN.md` in this
//! directory; the surface/branch coverage ledger is `MANIFEST.md`.
//!
//! Scenario types COMPOSE the existing context-module parse-don't-validate
//! types ([`PinnedGoal`], [`EvidenceText`], [`WorkerClaim`],
//! [`SpilledArtifact`], [`ResultPreview`]) and the production state types
//! ([`Plan`], [`FailureSummary`], [`ToolTraceEntry`], [`RunManifest`],
//! [`OrchestrationConfig`]); they never re-model what those already forbid.

#![expect(
    dead_code,
    reason = "S2 type skeleton: fixture types land before the snapshot tests that consume them (S2 implementation step)"
)]
#![expect(
    unused_variables,
    reason = "S2 type skeleton: constructor bodies are todo!() until the S2 implementation step"
)]

use crate::orchestration::config::OrchestrationConfig;
use crate::orchestration::context::{
    ContextError, EvidenceText, PinnedGoal, ResultPreview, SpilledArtifact, WorkerClaim,
};
use crate::orchestration::persistence::{RunManifest, ToolTraceEntry};
use crate::orchestration::types::{FailureCategory, FailureSummary, Plan, PlanningResponse};
use aura_config::{SkillConfig, VectorStoreConfig};
use std::num::NonZeroUsize;

/// Why a fixture scenario failed to construct.
///
/// Every fallible fixture constructor returns `FixtureError`, so a snapshot
/// test only ever holds a scenario that corresponds to a reachable
/// production state — with one amendment: for the fixtures listed in
/// `MANIFEST.md` §6a, the TOOLS surface is deliberately partial (vector-
/// search and scratchpad tool definitions need live wiring), so those
/// envelopes are reachable modulo the named tool-definition omissions.
/// Everywhere else the (system, messages, tools) triple is complete.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FixtureError {
    /// A context-module value failed its own parsing constructor.
    #[error("context value rejected: {0}")]
    Context(#[from] ContextError),
    /// A mid-thread assistant turn was built from `respond_directly` or
    /// `request_clarification`; both end the run, so no later planning call
    /// can exist after them.
    #[error("mid-thread decision must be create_plan; terminal decisions end the run")]
    TerminalDecisionMidThread,
    /// A decision's steps did not flatten into a plan (`into_plan` returned
    /// `None`), so no iteration could have executed it.
    #[error("plan decision steps do not flatten into an executable plan")]
    UnflattenablePlan,
    /// An iteration fixture supplied a different number of task outcomes
    /// than its decision's flattened plan has tasks.
    #[error("iteration outcomes ({outcomes}) do not match the decision's task count ({tasks})")]
    OutcomeCountMismatch { tasks: usize, outcomes: usize },
    /// A failure summary was attached to an iteration whose tasks all
    /// completed; production populates it only on the failure/blocked path.
    #[error("failure summary requires at least one failed or blocked task outcome")]
    FailureSummaryWithoutFailure,
    /// A continuation thread had no completed iterations; planning call 1
    /// is the `Initial` variant, not an empty continuation.
    #[error("continuation thread has no completed iterations")]
    EmptyContinuationThread,
    /// More completed iterations than the planning budget allows a further
    /// planning call for: the production loop stops at
    /// `max_planning_cycles`, so that envelope is unreachable.
    #[error("{iterations} completed iterations leave no planning call within budget {budget}")]
    IterationsExhaustBudget { iterations: usize, budget: usize },
    /// A planning budget of zero cycles: the orchestration loop never runs
    /// and no coordinator envelope exists.
    #[error("planning budget is zero")]
    ZeroPlanningBudget,
    /// Recon tools requested while the roster inlines worker tool
    /// inventories: production gates recon on `tools_in_planning == None`
    /// (orchestrator.rs create_coordinator), so the combination is
    /// unreachable.
    #[error("recon tools require tools_in_planning = none")]
    ReconRequiresUninlinedTools,
    /// A session-history fixture with no prior-run manifests: the block
    /// only renders when `load_session_manifests` finds at least one.
    #[error("session-history fixture has no prior-run manifests")]
    EmptySessionHistory,
    /// A session-history fixture whose manifests are not sorted
    /// most-recent-first: `load_session_manifests` sorts by timestamp
    /// descending, and `build_session_context` re-reverses that order for
    /// chronological turn numbering — feeding it oldest-first would reverse
    /// the golden chronology.
    #[error("session-history manifests must be sorted most-recent-first")]
    SessionHistoryNotRecentFirst,
    /// An iteration outcome marks complete a task whose named worker is
    /// absent from the roster config: production fails unknown-worker tasks
    /// at worker creation (`create_worker`'s `get_worker` error), so a
    /// COMPLETED unknown-worker task is unreachable. (A FAILED
    /// unknown-worker task is reachable and permitted.)
    #[error("task {task_id} completed under unknown worker '{worker}'")]
    CompletedTaskUnknownWorker { task_id: usize, worker: String },
    /// A populated-frame fixture whose plan graph yields no completed
    /// ancestor for the target task: `build_task_context` would return
    /// `None`, silently degenerating to an empty frame.
    #[error("populated frame fixture has no completed ancestor for task {task_id}")]
    FrameHasNoCompletedAncestor { task_id: usize },
}

// ============================================================================
// Coordinator scenario
// ============================================================================

/// The run's planning-cycle budget (`max_planning_cycles`).
///
/// Business rule: the continuation prompt's `ITERATION N of MAX` counters
/// and the `(FINAL ATTEMPT)` urgency derive from the ONE budget the
/// roster's `OrchestrationConfig` carries (`plan_with_routing` passes
/// `self.config.max_planning_cycles` to the continuation wrapper), so
/// [`CoordinatorScenario::new`] DERIVES this value from
/// `roster.config().max_planning_cycles` — a fixture cannot hold a budget
/// that disagrees with its own config. Forbidden states: two sources for
/// one budget surface (no constructor takes a free-standing budget
/// alongside a roster), and a zero budget, under which no planning call
/// exists and the urgency arithmetic (`iteration + 1 >= max`) is
/// meaningless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlanningBudget(NonZeroUsize);

impl PlanningBudget {
    /// Parse a planning budget. Called by [`CoordinatorScenario::new`] on
    /// `roster.config().max_planning_cycles`; scenarios never construct one
    /// directly.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::ZeroPlanningBudget`] for zero.
    pub(crate) fn new(max_planning_cycles: usize) -> Result<Self, FixtureError> {
        todo!()
    }

    /// The budget, as passed to `build_continuation_prompt`.
    pub(crate) fn get(&self) -> usize {
        todo!()
    }
}

/// Whether the coordinator carries the reconnaissance tools.
///
/// Business rule: the `## Reconnaissance Guidance` preamble branch and the
/// `list_tools`/`inspect_tool_params` tool definitions appear together or
/// not at all (config.rs `build_coordinator_preamble` + orchestrator.rs
/// `create_coordinator`). Forbidden state: an envelope whose preamble and
/// tool list disagree about recon — both surfaces derive from this one
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReconTools {
    Included,
    Excluded,
}

/// Whether the coordinator carries the session-history tools.
///
/// Business rule: the preamble's "two **artifact/history tools**" sentence
/// and the `list_prior_runs` tool definition appear together or not at all
/// (config.rs tools-sentence branch + orchestrator.rs `create_coordinator`
/// `include_history_tools`). Forbidden state: preamble/tool-list divergence
/// on history tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryTools {
    Included,
    Excluded,
}

/// The coordinator's optional-tool configuration, read by BOTH the
/// preamble builder and the tool-definition builder.
///
/// Business rule: one value drives the two surfaces (preamble sentence,
/// tools JSON), so they cannot diverge. Forbidden state: a fixture that
/// sets the preamble branch and the tool list independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CoordinatorToolConfig {
    pub(crate) recon: ReconTools,
    pub(crate) history: HistoryTools,
}

/// Source-built prior-run manifests for the `## Session History` preamble
/// block (anatomy doc section 4; no trace-derived golden exists).
///
/// Business rule: `build_session_context` only renders when
/// `load_session_manifests` returns at least one prior-run manifest, and it
/// receives them MOST-RECENT-FIRST (`load_session_manifests` sorts by
/// timestamp descending, then truncates); the renderer iterates in reverse
/// to number turns chronologically. Forbidden states: an empty manifest
/// list producing a header-only session block — absence of the block is
/// modeled as `Option<SessionHistoryFixture>::None` on [`PreambleFixture`],
/// never as an empty fixture — and a manifest list not sorted
/// most-recent-first, which would reverse the golden turn chronology.
#[derive(Debug, Clone)]
pub(crate) struct SessionHistoryFixture(Vec<RunManifest>);

impl SessionHistoryFixture {
    /// Wrap a non-empty list of prior-run manifests, most-recent-first
    /// (non-ascending `timestamp`), exactly as `load_session_manifests`
    /// returns them.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::EmptySessionHistory`] for an empty list and
    /// [`FixtureError::SessionHistoryNotRecentFirst`] when any adjacent
    /// pair is ordered oldest-first.
    pub(crate) fn new(manifests: Vec<RunManifest>) -> Result<Self, FixtureError> {
        todo!()
    }

    /// The manifests, as handed to `build_session_context`.
    pub(crate) fn manifests(&self) -> &[RunManifest] {
        todo!()
    }
}

/// Everything that shapes the coordinator system preamble.
///
/// Business rule: the preamble is `build_coordinator_preamble` output plus
/// up to three appends in the fixed `create_coordinator` order — skill
/// catalog, then vector-store context, then session history
/// (orchestrator.rs create_coordinator). Forbidden state: appends in any
/// other order — the fields are typed and only the envelope builder
/// concatenates them, so a fixture cannot reorder the appends.
///
/// Env pinning: `build_coordinator_preamble` reads `AURA_ESCAPE_HATCH`.
/// The corpus pins the default branch (variable unset); the envelope
/// builder fails loudly if the variable is set rather than snapshotting an
/// unpinned preamble. The stripped branch is excluded with reason in
/// `MANIFEST.md`.
#[derive(Debug, Clone)]
pub(crate) struct PreambleFixture {
    /// The `[agent].system_prompt` playbook substituted into
    /// `{{orchestration_system_prompt}}` (anatomy blocks 5-18 are content
    /// of this single slot).
    pub(crate) playbook: String,
    /// Drives the tools sentence, recon guidance, and the tool-definition
    /// list.
    pub(crate) tools: CoordinatorToolConfig,
    /// Skill catalog append; empty renders no catalog
    /// (`render_skill_catalog` returns `None`).
    pub(crate) skills: Vec<SkillConfig>,
    /// Vector-store context append; empty renders no block.
    pub(crate) vector_stores: Vec<VectorStoreConfig>,
    /// Session-history append; `None` means the block did not fire.
    pub(crate) session_history: Option<SessionHistoryFixture>,
}

/// The worker roster configuration driving the `AVAILABLE WORKERS` section,
/// the valid-worker-names guideline line, and the per-worker tool sections.
///
/// Business rule: all three planning-prompt worker surfaces derive from one
/// `OrchestrationConfig` via `build_worker_prompt_sections`
/// (orchestrator.rs). Forbidden state: a roster and a valid-names list
/// built from different worker sets.
///
/// The wrapped config's `workers` map is HashMap-ordered; the snapshot
/// normalizer (not the fixture) canonicalizes roster ordering. See
/// `normalize.rs`.
#[derive(Debug, Clone)]
pub(crate) struct WorkerRosterFixture(OrchestrationConfig);

impl WorkerRosterFixture {
    /// Wrap the orchestration config whose `workers` and
    /// `tools_in_planning` shape the planning prompt.
    pub(crate) fn new(config: OrchestrationConfig) -> Self {
        todo!()
    }

    /// The wrapped config, as handed to `Orchestrator::new`.
    pub(crate) fn config(&self) -> &OrchestrationConfig {
        todo!()
    }
}

/// The mid-thread routing decision recorded as a compact assistant turn.
///
/// Business rule: only `create_plan` continues the run, so only a
/// steps-plan decision can precede a later planning call
/// (`plan_with_routing` conversation growth). Forbidden state: a
/// `respond_directly` or `request_clarification` turn mid-conversation —
/// both variants are rejected at construction
/// ([`FixtureError::TerminalDecisionMidThread`]).
#[derive(Debug, Clone)]
pub(crate) struct PlanDecision(PlanningResponse);

impl PlanDecision {
    /// Accept a steps-plan decision; reject terminal decisions.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::TerminalDecisionMidThread`] for the terminal
    /// variants and [`FixtureError::UnflattenablePlan`] when the steps do
    /// not flatten into an executable plan.
    pub(crate) fn new(decision: PlanningResponse) -> Result<Self, FixtureError> {
        todo!()
    }

    /// The decision, as handed to `compact_decision_turn`.
    pub(crate) fn as_response(&self) -> &PlanningResponse {
        todo!()
    }

    /// The flattened plan this decision creates (validated non-`None` at
    /// construction).
    pub(crate) fn plan(&self) -> Plan {
        todo!()
    }
}

/// What stands in for a spilled result's body ahead of its artifact footer.
///
/// Business rule: the spill path stores a stand-in prefix plus the footer
/// as the task's raw result text; when the worker attested a claim, the
/// claim summary is promoted to the prefix — producing the byte-identical
/// `Summary:`/`Evidence:` duplication (delegation audit defect C).
/// Forbidden state: a claim-echo prefix without the claim that produced it
/// — `ClaimEcho` carries its [`WorkerClaim`] by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpilledStandIn {
    /// Defect C: the worker's claim summary echoed verbatim as the inline
    /// prefix, so `Summary:` and `Evidence:` render the same bytes.
    ClaimEcho { claim: WorkerClaim },
    /// A bounded raw preview ahead of the footer, with the worker's claim
    /// when one exists and differs from the preview.
    RawPreview {
        preview: ResultPreview,
        claim: Option<WorkerClaim>,
    },
    /// Whitespace-only prefix: renders as `(no inline preview)` plus the
    /// pointer (`EvidenceEntry::ArtifactPointerOnly`).
    NoPreview,
}

/// A completed task's stored result, as the worker/spill path wrote it.
///
/// Business rule: a result either stayed inline (under the spill
/// threshold, no footer) or spilled (stand-in prefix plus footer); the two
/// renderings are exclusive (`EvidenceEntry::from_completed_result`).
/// Forbidden state: inline text that secretly carries a spill footer —
/// [`EvidenceText`] rejects exactly what the footer parser accepts — and a
/// spilled result with no artifact pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletedResultFixture {
    /// Result text under the spill threshold, stored raw.
    Inline {
        result: EvidenceText,
        claim: Option<WorkerClaim>,
    },
    /// Result spilled to an artifact: stand-in prefix plus footer.
    Spilled {
        stand_in: SpilledStandIn,
        artifact: SpilledArtifact,
    },
}

impl CompletedResultFixture {
    /// The raw result string exactly as `TaskState::Complete { result }`
    /// stores it (inline text, or stand-in prefix plus rendered footer).
    pub(crate) fn raw_result(&self) -> String {
        todo!()
    }

    /// The worker's claim, when one exists, as `Task::structured_output`
    /// carries it.
    pub(crate) fn claim(&self) -> Option<&WorkerClaim> {
        todo!()
    }
}

/// A failed task's stored failure, as the failure path wrote it.
///
/// Business rule: the continuation renders a soft failure (worker claim
/// plus optional spilled pointer) only when a claim exists; a
/// `SoftFailure` category with no claim degrades to the hard rendering
/// (types.rs `build_continuation_prompt` failure match). Forbidden state:
/// a soft-failure fixture without the worker claim the rendering requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FailedResultFixture {
    /// Hard failure: raw error text plus structured category; the renderer
    /// bounds the error via `ErrorPreview`.
    Hard {
        error: String,
        category: FailureCategory,
    },
    /// Soft failure: the worker submitted a claim reporting it could not
    /// produce a result, with an optional spilled body.
    Soft {
        claim: WorkerClaim,
        artifact: Option<SpilledArtifact>,
    },
}

/// One task's end-of-iteration outcome.
///
/// Business rule: the continuation prompt renders completed tasks with
/// evidence and artifacts, failed tasks with failure reports and traces,
/// and blocked tasks as a bare correlation label (types.rs
/// `build_continuation_prompt`). Forbidden state: evidence or tool traces
/// on a blocked task — it never ran, and the `Blocked` variant has no
/// fields to hold any.
#[derive(Debug, Clone)]
pub(crate) enum TaskOutcome {
    Complete {
        result: CompletedResultFixture,
        traces: Vec<ToolTraceEntry>,
    },
    Failed {
        report: FailedResultFixture,
        traces: Vec<ToolTraceEntry>,
    },
    Blocked,
}

/// One completed iteration: the decision that opened it and the per-task
/// outcomes that closed it.
///
/// Business rule: the continuation prompt describes exactly the tasks the
/// recorded decision created, and the failure summary exists only when the
/// iteration had failures or blocked tasks (types.rs `IterationContext`
/// field docs). Forbidden states: an outcome list whose length differs
/// from the decision's flattened task count
/// ([`FixtureError::OutcomeCountMismatch`]), and a failure summary on a
/// clean iteration ([`FixtureError::FailureSummaryWithoutFailure`]).
///
/// Failure HISTORY is deliberately absent here: the envelope builder
/// derives it by folding earlier iterations' failed outcomes, mirroring
/// the orchestrator's accumulation — a fixture cannot invent a history
/// entry no prior iteration produced.
#[derive(Debug, Clone)]
pub(crate) struct IterationFixture {
    decision: PlanDecision,
    outcomes: Vec<TaskOutcome>,
    failure_summary: Option<FailureSummary>,
}

impl IterationFixture {
    /// Bind per-task outcomes and an optional failure summary to the
    /// decision that created the iteration's plan.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::OutcomeCountMismatch`] and
    /// [`FixtureError::FailureSummaryWithoutFailure`] per the rules above.
    pub(crate) fn new(
        decision: PlanDecision,
        outcomes: Vec<TaskOutcome>,
        failure_summary: Option<FailureSummary>,
    ) -> Result<Self, FixtureError> {
        todo!()
    }

    /// The decision that opened this iteration.
    pub(crate) fn decision(&self) -> &PlanDecision {
        todo!()
    }

    /// The per-task outcomes, in plan-task order.
    pub(crate) fn outcomes(&self) -> &[TaskOutcome] {
        todo!()
    }

    /// The iteration's failure summary, when the failure path populated
    /// one.
    pub(crate) fn failure_summary(&self) -> Option<&FailureSummary> {
        todo!()
    }
}

/// The completed iterations behind a continuation planning call, oldest
/// first.
///
/// Business rule: continuation planning call N+1 exists only after N >= 1
/// completed iterations (`plan_with_routing` chooses the continuation
/// wrapper only when a previous `IterationContext` exists). Forbidden
/// state: a continuation thread with zero iterations — planning call 1 is
/// [`CoordinatorCall::Initial`].
#[derive(Debug, Clone)]
pub(crate) struct ContinuationThread(Vec<IterationFixture>);

impl ContinuationThread {
    /// Wrap a non-empty iteration list, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::EmptyContinuationThread`] for an empty list.
    pub(crate) fn new(iterations: Vec<IterationFixture>) -> Result<Self, FixtureError> {
        todo!()
    }

    /// The completed iterations, oldest first.
    pub(crate) fn iterations(&self) -> &[IterationFixture] {
        todo!()
    }
}

/// Which planning call the envelope captures.
///
/// Business rule: planning call 1 sends the initial planning wrapper;
/// calls 2..=N send the continuation wrapper over the grown conversation
/// (`plan_with_routing`). Forbidden state: a continuation envelope with no
/// conversation behind it, or an initial envelope carrying one.
#[derive(Debug, Clone)]
pub(crate) enum CoordinatorCall {
    /// Planning call 1: fresh query, empty coordinator conversation.
    Initial,
    /// Planning call `iterations + 1`: the conversation carries one
    /// (user wrapper, assistant compact-decision) pair per prior call.
    Continuation(ContinuationThread),
}

/// A complete coordinator planning-call scenario: everything needed to
/// reproduce the request envelope of planning call N at commit `9df96382`.
///
/// Business rule: the envelope is a pure function of preamble
/// configuration, verbatim query, worker roster, and the
/// completed-iteration thread; the planning budget is DERIVED from
/// `roster.config().max_planning_cycles` (production's only budget
/// source), and the cross-field constructor re-checks the states
/// individual fields cannot forbid alone. Forbidden states (checked in
/// [`CoordinatorScenario::new`]):
/// - a zero-cycle roster config ([`FixtureError::ZeroPlanningBudget`], via
///   the budget derivation);
/// - recon tools with an inlined-tools roster
///   ([`FixtureError::ReconRequiresUninlinedTools`]);
/// - more completed iterations than the budget allows a further planning
///   call for ([`FixtureError::IterationsExhaustBudget`]);
/// - a COMPLETED task outcome whose plan task names a worker absent from
///   the roster config ([`FixtureError::CompletedTaskUnknownWorker`]) —
///   production fails unknown-worker tasks at worker creation, so only
///   FAILED outcomes may carry an unknown worker name.
#[derive(Debug, Clone)]
pub(crate) struct CoordinatorScenario {
    preamble: PreambleFixture,
    query: PinnedGoal,
    roster: WorkerRosterFixture,
    budget: PlanningBudget,
    call: CoordinatorCall,
}

impl CoordinatorScenario {
    /// Assemble and cross-validate a coordinator scenario, deriving the
    /// planning budget from the roster config.
    ///
    /// # Errors
    ///
    /// Returns the cross-field [`FixtureError`] variants named on the type.
    pub(crate) fn new(
        preamble: PreambleFixture,
        query: PinnedGoal,
        roster: WorkerRosterFixture,
        call: CoordinatorCall,
    ) -> Result<Self, FixtureError> {
        todo!()
    }

    /// The preamble configuration.
    pub(crate) fn preamble(&self) -> &PreambleFixture {
        todo!()
    }

    /// The verbatim user query (also the pinned continuation goal).
    pub(crate) fn query(&self) -> &PinnedGoal {
        todo!()
    }

    /// The worker roster configuration.
    pub(crate) fn roster(&self) -> &WorkerRosterFixture {
        todo!()
    }

    /// The planning budget, derived at construction from
    /// `roster.config().max_planning_cycles`.
    pub(crate) fn budget(&self) -> PlanningBudget {
        todo!()
    }

    /// Which planning call this scenario captures.
    pub(crate) fn call(&self) -> &CoordinatorCall {
        todo!()
    }
}

// ============================================================================
// Worker scenario
// ============================================================================

/// Whether the worker's scratchpad tooling was wired up.
///
/// Business rule: the scratchpad preamble append happens exactly when the
/// scratchpad tools were configured for the worker (orchestrator.rs worker
/// construction). Forbidden state: the append without the wiring, or the
/// wiring without the append — both derive from this one value.
///
/// Claim note: production wiring additionally requires an accessible MCP
/// tool matching a scratchpad threshold, and the scratchpad tool
/// DEFINITIONS need live storage/token-counter wiring — `Wired` fixtures
/// therefore have a deliberately-partial tools surface (`MANIFEST.md` §6a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScratchpadWiring {
    Wired,
    NotWired,
}

/// The config-conditional appends shared by BOTH worker preamble branches.
///
/// Business rule: the shared appends land after the branch-specific
/// preamble in the fixed worker-construction order — scratchpad preamble,
/// then skill catalog (orchestrator.rs worker construction, after the
/// role/generic branch). Forbidden state: any other append order, or a
/// vector-store append on the generic branch — the vector-store block is
/// appended only inside the named-role branch, so it lives on
/// [`WorkerPreambleFixture::Role`], not here.
#[derive(Debug, Clone)]
pub(crate) struct WorkerPreambleAppends {
    /// Scratchpad preamble append.
    pub(crate) scratchpad: ScratchpadWiring,
    /// Skill catalog append; empty renders no catalog.
    pub(crate) skills: Vec<SkillConfig>,
}

/// Which worker preamble branch the envelope uses.
///
/// Business rule: a named worker renders the worker template around the
/// role's own preamble, then the vector-store context for the role's
/// ASSIGNED stores (the post-`retain` list — production intersects the
/// config-level stores with the role's `vector_stores` names); an
/// unassigned task renders the generic fallback around the config-level
/// worker prompt or its placeholder default, and NEVER receives the
/// vector-store block — the append is inside the named-role branch only
/// (orchestrator.rs worker construction; config.rs
/// `build_worker_preamble`). Forbidden states: a role preamble on the
/// generic branch or vice versa, and a generic preamble carrying a
/// vector-store append — the `Generic` variant has no field for one.
///
/// Role-branch append order: vector-store context, then the shared
/// [`WorkerPreambleAppends`] (scratchpad, then skills). Generic-branch
/// order: shared appends only.
#[derive(Debug, Clone)]
pub(crate) enum WorkerPreambleFixture {
    /// Task assigned to a configured worker role.
    Role {
        /// The role's `[orchestration.worker.*].preamble` text.
        role_preamble: String,
        /// The role's assigned vector stores, post-`retain`; empty renders
        /// no block.
        vector_stores: Vec<VectorStoreConfig>,
        appends: WorkerPreambleAppends,
    },
    /// Task left unassigned: generic fallback template.
    Generic {
        /// `[orchestration].worker_system_prompt`; `None` renders the
        /// template's placeholder default.
        custom_prompt: Option<String>,
        appends: WorkerPreambleAppends,
    },
}

/// A plan graph position that yields a populated prior-work frame.
///
/// Business rule: `Orchestrator::build_task_context` renders a frame only
/// for a task with at least one COMPLETED ancestor; the graph shape
/// (direct-only vs direct-plus-transitive) and spilled entries are all
/// expressed through the wrapped [`Plan`]'s dependency edges and stored
/// task results. Forbidden state: a populated-frame fixture whose graph
/// yields no frame ([`FixtureError::FrameHasNoCompletedAncestor`]) — the
/// silent empty-frame degeneration cannot masquerade as coverage.
#[derive(Debug, Clone)]
pub(crate) struct FrameGraph {
    plan: Plan,
    task_id: usize,
}

impl FrameGraph {
    /// Bind a plan graph to the task whose frame is captured.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::FrameHasNoCompletedAncestor`] when the plan
    /// yields no completed ancestor for `task_id`.
    pub(crate) fn new(plan: Plan, task_id: usize) -> Result<Self, FixtureError> {
        todo!()
    }

    /// The plan, as handed to `Orchestrator::build_task_context`.
    pub(crate) fn plan(&self) -> &Plan {
        todo!()
    }

    /// The task whose frame is rendered.
    pub(crate) fn task_id(&self) -> usize {
        todo!()
    }
}

/// The worker's prior-work frame branch, carrying the `%%YOUR_TASK%%`
/// source for its branch.
///
/// Business rule: an empty `%%CONTEXT%%` slot arises on two CAUSALLY
/// DISTINCT paths — the first worker turn of a fresh plan, and the first
/// worker turn after a replan boundary (cross-iteration evidence dropped
/// with the removed `PriorIteration` channel at `9df96382`). The renders
/// are byte-identical; the manifest tracks them as distinct branches, so
/// the enum encodes the cause. Production derives the task text and the
/// frame from the SAME plan task (the ready-task tuple in the execute
/// loop), so on the populated branch the task text derives from the frame
/// graph's target task — the empty variants carry a free task string
/// (any coordinator-authored description is reachable there). Forbidden
/// states: an "empty frame" fixture with no stated cause; a populated
/// fixture that renders empty (see [`FrameGraph`]); and a populated
/// fixture whose task text diverges from its own plan task — there is no
/// field to diverge with.
#[derive(Debug, Clone)]
pub(crate) enum WorkerFrameFixture {
    /// First worker turn of a fresh plan: no ancestors exist yet.
    EmptyFirstTurn {
        /// The coordinator-authored task description (`%%YOUR_TASK%%`).
        task: String,
    },
    /// First worker turn of a replan-boundary plan: prior-iteration
    /// evidence exists but no in-plan ancestor does (mechanically the same
    /// render as [`Self::EmptyFirstTurn`]; causally distinct).
    EmptyReplanBoundary {
        /// The coordinator-authored task description (`%%YOUR_TASK%%`).
        task: String,
    },
    /// A populated frame from completed same-plan ancestors; `%%YOUR_TASK%%`
    /// is the graph's target-task description.
    Populated(FrameGraph),
}

impl WorkerFrameFixture {
    /// The `%%YOUR_TASK%%` text: the carried string on the empty variants,
    /// the target task's description on the populated branch.
    pub(crate) fn task_text(&self) -> &str {
        todo!()
    }
}

/// A complete worker call scenario: preamble branch and frame branch (the
/// frame branch owns the task text; see [`WorkerFrameFixture`]).
///
/// Business rule: the worker envelope is the worker preamble (system),
/// exactly one user message from `render_worker_task_prompt` over the
/// task text and the frame render, and the worker-side tool definitions.
/// Forbidden state: a worker envelope with coordinator conversation
/// history — the type has no field for one, matching the "workers do NOT
/// see conversation history" contract.
#[derive(Debug, Clone)]
pub(crate) struct WorkerScenario {
    /// Preamble branch and appends.
    pub(crate) preamble: WorkerPreambleFixture,
    /// The `%%CONTEXT%%` frame branch, carrying the `%%YOUR_TASK%%` source.
    pub(crate) frame: WorkerFrameFixture,
}
