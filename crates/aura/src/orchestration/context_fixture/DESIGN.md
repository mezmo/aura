# S2 context-fixture type design record

Baseline: aura `9df96382`, branch `card/S2`. Scope: the type design and
its implementation. The skeleton landed at `710112b0`; the implementation
step filled every body, landed the snapshot corpus and the REQUIRED R3/R5
comparison gates, and consolidated `frame_validation_tests.rs` (ledgers
in the measurement section below). Coverage ledger: `MANIFEST.md` (same
directory).

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
| `WorkerRosterFixture` | Roster, valid-names line, and tool sections derive from one `OrchestrationConfig`. Implementation-step amendment: the fixture also carries the agent-level `[[vector_stores]]` catalog, because Full-visibility tool descriptions read a second production input (`agent_config.vector_stores` in `get_all_tool_descriptions`) that the skeleton conflated with the coordinator preamble append | Roster/names built from different worker sets |
| `PlanDecision` | Only `create_plan` continues the run | Terminal decision (`respond_directly`/`request_clarification`) recorded mid-thread; steps that do not flatten |
| `SpilledStandIn` | Spill promotes the claim summary to the inline prefix (defect C) | Claim-echo prefix without the claim that produced it |
| `CompletedResultFixture` | Inline and spilled renderings are exclusive (`EvidenceEntry::from_completed_result`) | Inline text carrying a spill footer (via reused `EvidenceText`); spilled result without its pointer |
| `FailedResultFixture` | Soft-failure rendering requires a worker claim; claimless `SoftFailure` degrades to hard | Soft failure without a claim |
| `TaskOutcome` | Completed tasks render evidence and artifacts; failed tasks render failure reports plus traces. Blocked tasks render a bare correlation label | Evidence or traces on a task that never ran (`Blocked` has no fields) |
| `IterationFixture` | Continuation evidence describes exactly the tasks the recorded decision created; failure summary exists only on the failure/blocked path; failure history is DERIVED by folding earlier iterations through the `iteration_failures_for_golden` production accessor | Outcome-count/plan-shape mismatch; failure summary on a clean iteration; invented history entries (no field for them) |
| `ContinuationThread` | Continuation call N+1 exists only after ≥1 completed iteration | Empty continuation thread |
| `CoordinatorCall` | Call 1 sends the planning wrapper; later calls send the continuation wrapper over the grown conversation | Continuation without a thread; initial call with one |
| `CoordinatorScenario` | The envelope is a pure function of (preamble, query, roster, thread); the budget derives from the roster config; cross-field states are re-checked at construction | Recon with inlined-tools roster; more iterations than the budget allows a further call for; a COMPLETED outcome on a task naming a worker absent from the roster (production fails unknown-worker tasks at `create_worker`) |
| `ScratchpadWiring` | Scratchpad preamble append happens exactly when scratchpad tools are wired | Append/wiring divergence |
| `WorkerPreambleAppends` | The SHARED appends land after the branch-specific preamble in constructor order (scratchpad → skills); the vector-store append is named-role-only and lives on the `Role` variant | Reordered shared appends; a vector-store append on the generic branch (no field for it - production appends it only inside the named-role branch) |
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
| `Orchestrator::collect_iteration_failures` | private | `#[cfg(test)] pub(crate) fn iteration_failures_for_golden` accessor; pure delegation - the continuation prompt's failure-history fold is production code, not a test-side re-statement |
| `Orchestrator::create_coordinator` | private | `#[cfg(test)] pub(crate) async fn coordinator_preamble_for_golden` accessor, added in the implementation step so the REQUIRED R3 gate compares against real `create_coordinator` output; pure delegation returning only the assembled preamble |
| `crate::skill_tool::SkillToolset::new` | `pub` | called directly; pure over `SkillConfig` (no filesystem discovery), so skill tool definitions are real production output |

No production visibility was widened; the only product-file edits are the
four `#[cfg(test)]`-gated delegating accessors (three from the skeleton,
one added by the implementation step for the R3 gate). Test-only
dependency added: `insta = "1"` in `[dev-dependencies]`.

## Normalization design (pre-approved decision 2)

Test-side only; no `HashMap` → `BTreeMap` product change (deferred to
S3/S4) and no clock injection. Exactly two passes, both LOCATION-AWARE:
they run per message over the structured envelope BEFORE the snapshot
document is flattened, so a payload byte (query, task result, playbook,
tool description) containing a marker can never be rewritten:

1. **Timestamp scrub.** Rewrite `Current time: \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z`
   to `Current time: <TIMESTAMP>`, ANCHORED at byte offset 0 of a
   user-message body - the only place production emits it
   (`chrono::Utc::now()` PREFIXES `build_planning_wrapper` and
   `build_continuation_wrapper` output; RFC3339, seconds precision, Z
   suffix). Occurrences elsewhere are payload and are left untouched.
2. **Worker-order canonicalization.** Sort lexicographically, in place,
   INSIDE THE FIRST USER MESSAGE ONLY (the initial planning wrapper - the
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
all carry it, worker envelopes none - mixed means builder drift); a
malformed `Current time: ` prefix at offset 0 is a defect, not a skip;
the roster markers appear at most once each, only in the first user
message, and nowhere else in the envelope. Fixture payloads must not
embed the markers; the audit makes a collision a loud failure instead of
a silent mis-normalization.

Tool JSON needs no pass: `serde_json`'s default `BTreeMap`-backed maps
(no `preserve_order` feature in this workspace) serialize
byte-deterministically.

Anything not matched by these two anchored passes is compared
byte-for-byte. The byte-identity assertion mode for S3-S6 is
`assert_envelope_snapshot` run with snapshot updating disabled
(`INSTA_UPDATE=no`); the no-op-refactor proof is recorded in the
verification section below.

## Residual risks (named, per the epic's Verification section)

- **R1 - rig-fork mapping.** The final provider request is assembled in
  the pinned rig fork (rev `8908530`). Envelope identity at the aura seam
  is a necessary condition for request identity, not proof of it - and
  request identity is itself only a NECESSARY condition for
  benchmark-score neutrality, never sufficient: downstream cards must not
  read a passing snapshot gate as behavioral evidence.
- **R2 - timing.** Timestamps are normalized away; nothing here verifies
  time-dependent behavior (timeouts, durations in tool traces are fixture
  constants).
- **R3 - re-stated append orders.** `create_coordinator`'s preamble append
  order (including the bare `push('\n')` before `build_session_context`)
  and the worker constructor's per-branch append orders are re-stated by
  the test builder (the production orchestration of those appends is
  inseparable from MCP/vector/persistence wiring). GATE STATUS: the
  coordinator-side comparison LANDED and passes
  (`gate_r3_coordinator_preamble_matches_create_coordinator` in
  `corpus.rs`): the composed preamble byte-equals real
  `create_coordinator` output over a tempdir-backed config with skills
  and session history enabled and vector stores disabled, through the
  `coordinator_preamble_for_golden` accessor. Residues: (a) the vector
  append position stays re-stated (live-manager construction); (b) the
  worker-side `create_worker` comparison proved INFEASIBLE without new
  production code - `create_worker` only captures its assembled preamble
  when the prompt journal is enabled (`AURA_PROMPT_JOURNAL`, an env
  mutation the corpus's env-pinning stance forbids in tests), so a pure
  delegating accessor returns an empty string and a capturing accessor
  would change production code. The worker per-branch append orders stay
  RE-STATED in MANIFEST §5; S3+ bounding-module work should add the seam.
- **R4 - event side effects.** Persistence writes, journal records,
  stream events, and artifact I/O ordering are outside the envelope and
  unverified here.
- **R5 - trace-merge re-statement.** `load_tool_traces_for_plan` merges
  tool records per task id across the run via disk persistence; the
  builder reproduces the merge in memory - a false-pass path. GATE
  STATUS: LANDED and passing
  (`gate_r5_trace_merge_matches_persistence_loader` in `corpus.rs`): the
  `coordinator_call3_failures` trace data is written through a
  tempdir-backed `ExecutionPersistence` across two iterations and the
  harness merge is asserted equal to the production
  `load_tool_records_for_task` scan mapped through `ToolTraceEntry::from`.
  Residues: `load_tool_traces_for_plan` itself is private, so the gate
  reproduces its trivial per-task loop (skip-empty plus the `From`
  mapping) around the production disk-scan merge; and the corpus pins one
  attempt per task per iteration because the production scan's
  within-iteration attempt-file order is filesystem-dependent.
- **R6 - MCP-sourced inventory content.** Summary/Full roster fixtures run
  with `mcp: None`; MCP-SOURCED tool names/descriptions differ per live
  deployment. Config-derived inventory content (vector-store tool names,
  descriptions, truncation) IS covered - see MANIFEST §2.
- **R7 - escape hatch.** The corpus pins `AURA_ESCAPE_HATCH` unset (fail
  loud if set); the stripped-preamble branch is uncovered by snapshots.
- **R8 - conversation-growth and tool-registration-order re-statement.**
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
  each deleted test is enumerated below against the fixture that
  subsumes it.
- **Retained-test ledger.** Every MANIFEST exclusion row that leans on
  legacy unit coverage names its owning tests; those tests are RETAINED -
  deleting one invalidates the exclusion's justification, and the
  manifest row must be re-dispositioned in the same change.

### Measured outcome (S2 implementation)

Delta over the fixed boundary, `git diff --stat 9df96382..HEAD` on
`frame_validation_tests.rs` + `context_fixture{,.rs}/*.rs`:
3,230 insertions, 902 deletions, net +2,328 test `.rs` lines
(`frame_validation_tests.rs` alone: 1,992 → 1,099 lines). Reported
separately per the boundary rule: 4,490 committed `.snap` lines and the
two Markdown records. The card's "net test-LOC reduction" did not
materialize as a negative number over this boundary: the corpus
infrastructure (typed fixtures, envelope seam, normalizer, gates) costs
more lines than the 29 subsumed assertion suites saved, and the boundary
deliberately includes that infrastructure so the number cannot be gamed.
Tooling caveat for the epic's C8 arithmetic: `scripts/loc_measure.py`
classifies the harness `.rs` files as PRODUCT lines because their
`#[cfg(test)]` gate sits at the declaration site in
`orchestration/mod.rs`, not inside each file; the program-side
measurement needs a classification fix before C8 sums card deltas.

Deleted cases (29), each mapped to the subsuming fixture; negative
assertions ("X must not render") are subsumed by byte-identity, since
any new occurrence changes the snapshot:

| Deleted test | Subsuming fixture |
|---|---|
| `test_continuation_final_attempt_urgency` | `coordinator_call4_final_urgency` |
| `test_continuation_mixed_structured_and_raw` | `coordinator_call2_clean` (inline+claim and inline no-claim entries) |
| `test_continuation_clean_success_no_failure_sections` | `coordinator_call2_clean` |
| `test_continuation_short_result_no_artifact` | `coordinator_call2_clean` |
| `test_continuation_result_forwarding_absent_when_all_failed` | `coordinator_call2_all_failed` |
| `test_continuation_soft_failure_with_structured_output` | `coordinator_call3_failures` (soft failure with claim + artifact) |
| `test_continuation_failed_task_no_artifact_refs` | `coordinator_call3_failures` (failed chain with FAILED marker; no artifact refs on failed entries) |
| `test_continuation_failure_history_worker_none` | `coordinator_call3_failures` (iteration-1 record is worker-less) |
| `test_continuation_empty_reasoning_in_tool_chain` | `coordinator_call3_failures` (failed-entry chain carries quoted and unquoted reasoning) |
| `test_continuation_running_task_renders_as_blocked` | `coordinator_call3_failures` (same `Pending \| Running` render arm) |
| `test_continuation_section_ordering` | `coordinator_call3_failures` (all five sections, in order) |
| `test_planning_wrapper_basic_structure` | every initial-call fixture (`coordinator_call1_recon` and peers) |
| `test_planning_wrapper_no_workers` | `coordinator_call1_no_workers` |
| `test_planning_wrapper_multi_worker_guidelines` | `coordinator_call1_nonrecon_summary` |
| `test_preamble_dynamic_tool_sections_with_persistence` | `coordinator_preamble_full_appends` + `coordinator_call1_nonrecon_summary` (block-2 history/no-history branches) |
| `test_preamble_recon_tools_enabled` | `coordinator_call1_recon` |
| `test_preamble_recon_and_history_tools_combined` | `tools_coordinator_recon_history` |
| `test_session_history_direct_response_run` | `coordinator_preamble_full_appends` |
| `test_session_history_multi_run_chronological` | `coordinator_preamble_full_appends` (two-manifest chronology + turn count) |
| `test_session_history_routed_single_worker` | `coordinator_preamble_full_appends` |
| `test_session_history_task_with_no_worker` | `coordinator_preamble_full_appends` (shared `unassigned` resolution line) |
| `test_session_history_no_artifacts_no_crossrun_hint` | `coordinator_preamble_full_appends` (direct-response turn renders hint-free) |
| `test_session_history_current_time_placeholder_replaced` | `coordinator_preamble_full_appends` (byte-locked SYSTEM section) |
| `test_session_history_multi_artifact_listing` | `coordinator_preamble_full_appends` (comma-joined artifacts line) |
| `test_session_history_and_continuation_independent_artifact_refs` | `coordinator_preamble_full_appends` + `coordinator_call3_failures` |
| `test_worker_task_context_with_dependency_results` | `worker_role_frame_spilled_claim_echo` |
| `test_worker_task_empty_context` | `worker_first_turn_empty` |
| `worker_frame_omits_imperative_task_text` | `worker_role_frame_direct` + `worker_role_frame_transitive` |
| `worker_frame_renders_read_only_prior_work_header_with_evidence_sentence` | every populated-frame worker fixture |

Retained cases (18), each owning coverage the corpus deliberately
excludes:

- `test_continuation_full_scenario`,
  `test_continuation_tool_output_artifacts_visible`: gated completed-task
  tool chains (`show_tool_reasoning_in_continuation = true`, MANIFEST §3
  exclusion).
- `test_continuation_all_failure_categories`: all eight hard categories;
  the corpus renders two.
- `test_continuation_soft_failure_without_structured_output`: claimless
  `SoftFailure` degrade, which `FailedResultFixture` forbids by
  construction.
- `test_continuation_multiple_repeated_failure_patterns`: multi-pattern
  `OBSERVED PATTERNS` ordering (HashMap-ordered, not snapshot-stable).
- `test_task_description_appears_at_most_once_across_conversation_and_continuation`:
  R3b acceptance property.
- `test_preamble_empty_system_prompt`: empty-playbook input.
- `test_session_history_empty_manifests`: empty manifest list, which
  `SessionHistoryFixture` forbids.
- `test_session_history_full_scenario`,
  `test_session_history_running_task_status`: the Pending/Running
  catch-all task render (MANIFEST 23c exclusion).
- `test_session_history_complete_task_no_preview_no_confidence`,
  `test_session_history_failed_task_no_error_no_context`,
  `test_session_history_manifest_outcome_none`,
  `test_session_history_error_context_without_partial_result`:
  absent-optional-field manifest shapes the fixture manifests do not
  carry.
- `worker_frame_direct_deps_always_admitted_transitive_budget_trimmed_first`:
  frame budget eviction (MANIFEST §5 exclusion, with `context/frame.rs`).
- `worker_frame_empty_ancestry_returns_none_no_frame_render`: the
  `build_task_context` `None` branch.
- `test_fail_descendants_of_marks_pending_descendants_dependency_failed_skip_complete_running_failed`,
  `test_fail_descendants_of_is_idempotent`: plan-state machinery, not an
  envelope surface.

## Verification record (S2 implementation step)

- **Corpus and gates.** `cargo test --package aura --lib context_fixture`:
  27 tests (18 snapshot fixtures, the R3 and R5 comparison gates, the
  constructor-validation and normalizer-audit tests), all passing; full
  `cargo test --package aura --lib`: 840 passing. Snapshots are stable
  across repeated runs under `INSTA_UPDATE=no`.
- **Slot coverage.** `slot_coverage.sh` (this directory, run from the
  worktree root) proves every envelope-surface `%%SLOT%%` and `{{slot}}`
  renders filled in at least one snapshot, the empty-able slots also
  render empty somewhere, and no snapshot carries a raw placeholder
  token. The empty-`%%CONTEXT%%` witness is the frame subtitle, because
  the task template's own line 3 mentions `READ-ONLY PRIOR WORK`
  unconditionally (defect B).
- **Byte-identity mode, positive proof.** No-op refactor applied to
  `build_planning_wrapper` (extracted the `chrono::Utc::now()` call into
  a local binding), then `INSTA_UPDATE=no cargo test --package aura --lib
  context_fixture`: `27 passed; 0 failed`, zero pending `.snap.new`
  files. The refactor was reverted; the committed tree carries no trace
  of it.
- **Byte-identity mode, negative control.** A one-byte change to the
  planning wrapper text (trailing `.` → `!`) failed every coordinator
  snapshot under `INSTA_UPDATE=no` ("snapshot assertion for
  'coordinator_call1_recon' failed", and peers). Reverted.
- **Spill byte layout.** Confirmed against `maybe_create_artifact`
  (orchestrator.rs): `{preview}\n\n[Full result (N chars) saved to
  artifact: FILE]`; `CompletedResultFixture::raw_result` reproduces it,
  and the defect-C snapshots baseline on it.
- **Frame seam decision.** `worker_envelope` drives
  `Orchestrator::build_task_context` through `FrameGraph` only; the
  eviction branch stays excluded per MANIFEST §5 with its
  `context/frame.rs` unit coverage.
