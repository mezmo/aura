# S2 context-fixture type design record

Baseline: aura `9df96382`, branch `card/S2`. Scope: type skeleton only —
every body is `todo!()`; snapshot tests, fixture data, and
`frame_validation_tests.rs` consolidation land in the S2 implementation
step. Coverage ledger: `MANIFEST.md` (same directory).

## Type inventory

Every public type maps to one business rule and names the invalid state it
forbids. Types marked (reused) come from `orchestration::context` or
`orchestration::types`/`persistence` and are composed, not re-modeled.

| Type | Business rule | Forbidden invalid state |
|---|---|---|
| `FixtureError` | Fixture constructors parse, not validate: a scenario either corresponds to a reachable `9df96382` state or does not construct. Amendment: for the partial-tools fixtures in MANIFEST §6a the TOOLS surface is deliberately partial (live-wiring definitions omitted); reachability there is modulo the named omissions | A snapshot test holding an unreachable scenario (beyond the §6a omissions) |
| `PlanningBudget` | Iteration counters and `(FINAL ATTEMPT)` urgency derive from the ONE budget the roster config carries; `CoordinatorScenario::new` derives it from `roster.config().max_planning_cycles` | Zero budget (no planning call exists; urgency arithmetic meaningless); a budget disagreeing with the roster config (no constructor takes both) |
| `ReconTools` | Recon preamble guidance and recon tool definitions appear together (create_coordinator gating) | Preamble/tool-list divergence on recon |
| `HistoryTools` | History-tools sentence and `list_prior_runs` definition appear together | Preamble/tool-list divergence on history tools |
| `CoordinatorToolConfig` | One value drives both the preamble sentence and the tools JSON | Setting the two surfaces independently |
| `SessionHistoryFixture` | The session block renders only when ≥1 prior-run manifest loads, and `build_session_context` receives manifests MOST-RECENT-FIRST (as `load_session_manifests` sorts them) and re-reverses for chronological turn numbering | Header-only session block from an empty manifest list; an oldest-first manifest list (reversed golden chronology) |
| `PreambleFixture` | Preamble = `build_coordinator_preamble` + appends in create_coordinator order (catalog → vector → session) | Reordered appends (fields are typed; only the builder concatenates) |
| `WorkerRosterFixture` | Roster, valid-names line, and tool sections derive from one `OrchestrationConfig` | Roster/names built from different worker sets |
| `PlanDecision` | Only `create_plan` continues the run | Terminal decision (`respond_directly`/`request_clarification`) recorded mid-thread; steps that do not flatten |
| `SpilledStandIn` | Spill promotes the claim summary to the inline prefix (defect C) | Claim-echo prefix without the claim that produced it |
| `CompletedResultFixture` | Inline and spilled renderings are exclusive (`EvidenceEntry::from_completed_result`) | Inline text carrying a spill footer (via reused `EvidenceText`); spilled result without its pointer |
| `FailedResultFixture` | Soft-failure rendering requires a worker claim; claimless `SoftFailure` degrades to hard | Soft failure without a claim |
| `TaskOutcome` | Completed tasks render evidence+artifacts, failed render reports+traces, blocked render label only | Evidence or traces on a task that never ran (`Blocked` has no fields) |
| `IterationFixture` | Continuation evidence describes exactly the tasks the recorded decision created; failure summary exists only on the failure/blocked path; failure history is DERIVED by folding earlier iterations through the `iteration_failures_for_golden` production accessor | Outcome-count/plan-shape mismatch; failure summary on a clean iteration; invented history entries (no field for them) |
| `ContinuationThread` | Continuation call N+1 exists only after ≥1 completed iteration | Empty continuation thread |
| `CoordinatorCall` | Call 1 sends the planning wrapper; later calls send the continuation wrapper over the grown conversation | Continuation without a thread; initial call with one |
| `CoordinatorScenario` | The envelope is a pure function of (preamble, query, roster, thread); the budget derives from the roster config; cross-field states are re-checked at construction | Recon with inlined-tools roster; more iterations than the budget allows a further call for; a COMPLETED outcome on a task naming a worker absent from the roster (production fails unknown-worker tasks at `create_worker`) |
| `ScratchpadWiring` | Scratchpad preamble append happens exactly when scratchpad tools are wired | Append/wiring divergence |
| `WorkerPreambleAppends` | The SHARED appends land after the branch-specific preamble in constructor order (scratchpad → skills); the vector-store append is named-role-only and lives on the `Role` variant | Reordered shared appends; a vector-store append on the generic branch (no field for it — production appends it only inside the named-role branch) |
| `WorkerPreambleFixture` | Named role wraps the role preamble, then the role's post-`retain` assigned vector stores (role-branch order: vector → scratchpad → skills); unassigned tasks get the generic fallback with the shared appends only | Role text on the generic branch and vice versa (each variant carries only its own source); a generic-branch vector append |
| `FrameGraph` | `build_task_context` renders a frame only for a task with ≥1 completed ancestor | A "populated" fixture that silently renders an empty frame |
| `WorkerFrameFixture` | Empty `%%CONTEXT%%` arises on two causally distinct paths (fresh plan vs replan boundary, pre-approved decision 4); production derives `%%YOUR_TASK%%` and the frame from the SAME plan task, so the populated branch derives its task text from the frame graph's target task | An empty-frame fixture with no stated cause; a populated fixture whose task text diverges from its own plan task (no field to diverge with) |
| `WorkerScenario` | Worker envelope = preamble + ONE task-prompt user message + worker tools; the frame branch owns the task text | A worker envelope carrying conversation history (no field for it) |
| `RequestEnvelope` | The S2 identity claim quantifies over exactly (system, messages, tools) | Identity claims over a partial surface |
| `NormalizedSnapshot` | Normalization applies exactly two named rewrite classes, LOCATION-ANCHORED (per message, offset-0 timestamp prefix; first-user-message roster spans) behind an occurrence audit that panics on marker drift | A generic "cleanup" pass, or a flattened-text rewrite that could absorb payload drift |
| `PinnedGoal` (reused) | Goal line is the verbatim original query | Empty/paraphrased goal |
| `EvidenceText`, `ResultPreview`, `SpilledArtifact`, `WorkerClaim` (reused) | Worker-evidence parsing rules (`context/evidence.rs`, `context/label.rs`) | Footered inline text; empty claims/previews/filenames |
| `Plan`, `FailureSummary`, `FailureCategory`, `PlanningResponse`, `ToolTraceEntry`, `RunManifest`, `OrchestrationConfig` (reused) | Production state shapes fed to the real renderers | n/a (production-owned) |

## Envelope seam

Builders call the REAL assembly functions; nothing re-implements prompt
text. Call inventory is in `envelope.rs` module docs. Visibility decisions:

| Production item | Visibility at 9df96382 | Decision |
|---|---|---|
| `build_coordinator_preamble`, `build_worker_preamble`, `build_vector_store_context`, `WORKER_PREAMBLE_TEMPLATE`, `render_skill_catalog`, `build_session_context`, `render_worker_task_prompt`, `IterationContext::build_continuation_prompt` | `pub` | called directly |
| `Orchestrator::build_planning_wrapper`, `Orchestrator::compact_decision_turn`, `Orchestrator::build_task_context` | `pub(crate)` | reachable because the harness lives inside the crate (`#[cfg(test)] mod context_fixture` in `orchestration/mod.rs`) |
| `Orchestrator::build_continuation_wrapper` | private | `#[cfg(test)] pub(crate) fn continuation_wrapper_for_golden` accessor added in `orchestrator.rs`; pure delegation, no test-only behavior |
| `Orchestrator::build_worker_prompt_sections` | private, needs `&self` | `#[cfg(test)] pub(crate) fn worker_prompt_sections_for_golden` accessor; the harness builds a real `Orchestrator` via `Orchestrator::new` with `mcp: None` and no `memory_dir` (disabled persistence), so the sections come from production code over the fixture config |
| `Orchestrator::collect_iteration_failures` | private | `#[cfg(test)] pub(crate) fn iteration_failures_for_golden` accessor; pure delegation — the continuation prompt's failure-history fold is production code, not a test-side re-statement |
| `crate::skill_tool::SkillToolset::new` | `pub` | called directly; pure over `SkillConfig` (no filesystem discovery), so skill tool definitions are real production output |

No production visibility was widened; the only product-file edits are the
three `#[cfg(test)]`-gated delegating accessors. Test-only dependency
added: `insta = "1"` in `[dev-dependencies]`.

## Normalization design (pre-approved decision 2)

Test-side only; no `HashMap` → `BTreeMap` product change (deferred to
S3/S4) and no clock injection. Exactly two passes, both LOCATION-AWARE —
they run per message over the structured envelope BEFORE the snapshot
document is flattened, so a payload byte (query, task result, playbook,
tool description) containing a marker can never be rewritten:

1. **Timestamp scrub.** Rewrite `Current time: \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z`
   to `Current time: <TIMESTAMP>`, ANCHORED at byte offset 0 of a
   user-message body — the only place production emits it
   (`chrono::Utc::now()` PREFIXES `build_planning_wrapper` and
   `build_continuation_wrapper` output; RFC3339, seconds precision, Z
   suffix). Occurrences elsewhere are payload and are left untouched.
2. **Worker-order canonicalization.** Sort lexicographically, in place,
   INSIDE THE FIRST USER MESSAGE ONLY (the initial planning wrapper — the
   only message that renders rosters): the per-worker entries under its
   `AVAILABLE WORKERS:` heading (line entries for the None roster;
   `## name` blocks for Summary/Full), and the quoted names after its
   `Valid worker names:` line. Sources: `OrchestrationConfig::workers`
   HashMap iteration in `format_workers_for_prompt`,
   `available_worker_names`, and the Summary/Full section builders.

**Occurrence audit.** Before either pass rewrites anything,
`audit_normalization_markers` proves only the expected generated
occurrences will be touched, and panics (fail loud) otherwise: user
messages are all-or-none on the timestamp prefix (coordinator envelopes
all carry it, worker envelopes none — mixed means builder drift); a
malformed `Current time: ` prefix at offset 0 is a defect, not a skip;
the roster markers appear at most once each, only in the first user
message, and nowhere else in the envelope. Fixture payloads must not
embed the markers; the audit makes a collision a loud failure instead of
a silent mis-normalization.

Tool JSON needs no pass: `serde_json`'s default `BTreeMap`-backed maps
(no `preserve_order` feature in this workspace) serialize
byte-deterministically.

Anything not matched by these two anchored passes is compared
byte-for-byte. The
byte-identity assertion mode for S3-S6 is `assert_envelope_snapshot` run
with snapshot updating disabled; it will be proven by a no-op refactor in
the S2 implementation step.

## Residual risks (named, per the epic's Verification section)

- **R1 — rig-fork mapping.** The final provider request is assembled in
  the pinned rig fork (rev `8908530`). Envelope identity at the aura seam
  is a necessary condition for request identity, not proof of it — and
  request identity is itself only a NECESSARY condition for
  benchmark-score neutrality, never sufficient: downstream cards must not
  read a passing snapshot gate as behavioral evidence.
- **R2 — timing.** Timestamps are normalized away; nothing here verifies
  time-dependent behavior (timeouts, durations in tool traces are fixture
  constants).
- **R3 — re-stated append orders.** `create_coordinator`'s preamble append
  order (including the bare `push('\n')` before `build_session_context`)
  and the worker constructor's per-branch append orders are re-stated by
  the test builder (the production orchestration of those appends is
  inseparable from MCP/vector/persistence wiring). If production reorders
  appends, the snapshots move WITH it only if the builder is updated —
  a false-pass path, so MANIFEST marks those rows RE-STATED, not covered.
  REQUIRED implementation-step gate (not optional): a comparison test
  asserting the builder's coordinator preamble byte-equals the preamble
  `create_coordinator` produces over a tempdir-backed config with skills
  and session history enabled and vector stores disabled (the vector
  append position stays re-stated — live-manager construction — and is
  named as the residue). A worker-side comparison against `create_worker`
  output is required on the same terms if a seam proves reachable, else
  the residue is named in the implementation report.
- **R4 — event side effects.** Persistence writes, journal records,
  stream events, and artifact I/O ordering are outside the envelope and
  unverified here.
- **R5 — trace-merge re-statement.** `load_tool_traces_for_plan` merges
  tool records per task id across the run via disk persistence; the
  builder reproduces the merge in memory — a false-pass path, so MANIFEST
  marks the re-listing row's merge rule RE-STATED. REQUIRED
  implementation-step gate: a comparison test writing trace records
  through a tempdir-backed persistence and asserting the builder's merge
  equals `load_tool_traces_for_plan` output for the same records.
- **R6 — MCP-sourced inventory content.** Summary/Full roster fixtures run
  with `mcp: None`; MCP-SOURCED tool names/descriptions differ per live
  deployment. Config-derived inventory content (vector-store tool names,
  descriptions, truncation) IS covered — see MANIFEST §2.
- **R7 — escape hatch.** The corpus pins `AURA_ESCAPE_HATCH` unset (fail
  loud if set); the stripped-preamble branch is uncovered by snapshots.
- **R8 — conversation-growth and tool-registration-order re-statement.**
  The `plan_with_routing` growth rule (user wrapper, compact assistant
  turn per prior call) executes inside the live model loop, and tool
  REGISTRATION order lives in `create_coordinator_agent`/the worker
  builder; both are re-stated by the envelope builder with no seam to
  compare against. Coverage of those rows is SHAPE-ONLY: the snapshots
  lock today's shape and detect builder drift, but a production
  reordering moves the corpus only if the builder moves with it. Named
  here because no cheaper closure exists without product refactors that
  are out of S2 scope; S3+ bounding-module work should add the seam.

## Net-reduction measurement contract (card acceptance, falsifiable)

The card's net test-LOC reduction is reported against a fixed boundary so
it cannot be selectively scoped:

- **Boundary.** `.rs` lines under
  `crates/aura/src/orchestration/frame_validation_tests.rs` plus
  `crates/aura/src/orchestration/context_fixture/` (committed `.snap`
  files and these two Markdown records are excluded from the LOC ledger
  and reported separately). Delta = `git diff --stat` over exactly that
  boundary between `9df96382` and the implementation commit.
- **Deletion candidates.** Only `frame_validation_tests.rs` cases whose
  asserted substrings are subsumed by a corpus fixture may be deleted;
  the implementation report enumerates each deleted test by name against
  the fixture that subsumes it.
- **Retained-test ledger.** Every MANIFEST exclusion row that leans on
  legacy unit coverage names its owning tests; those tests are RETAINED —
  deleting one invalidates the exclusion's justification, and the
  manifest row must be re-dispositioned in the same change. The
  implementation report lists the retained set explicitly.

## Open items for the S2 implementation step

- Confirm the exact raw-result byte layout the spill path writes
  (`CompletedResultFixture::raw_result`) against
  `persistence_wrapper.rs`/spill code before baselining defect-C
  snapshots.
- Decide whether `worker_envelope` drives `Orchestrator::build_task_context`
  through `FrameGraph` only (current design) or also exercises
  `PriorWorkFrame::assemble` directly for the eviction branch (currently
  excluded, see MANIFEST §5).
- Land the REQUIRED R3 and R5 comparison gates (see residual risks) —
  the RE-STATED manifest rows stay uncovered until they pass.
- Prove byte-identity mode with a no-op refactor; report the net test-LOC
  delta per the measurement contract above.
