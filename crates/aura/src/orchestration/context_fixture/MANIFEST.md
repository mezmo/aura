# S2 golden-frame coverage manifest

Baseline: aura `9df96382`. Anatomy references are to
`docs/redesign/evidence/2026-07-10-coordinator-thread-anatomy.md` (program
repo); defect letters are from
`docs/redesign/evidence/2026-07-10-worker-delegation-contract-audit.md`.

Status: IMPLEMENTED (S2) and extended by S17. The corpus below is landed in
`golden_tests.rs`; every fixture's snapshot is committed under `snapshots/`
and one test per row group renders the complete envelope triple. A row is
either mapped to a fixture or explicitly excluded with a reason, and every
exclusion row that leans on legacy unit coverage names its owning tests
(the retained-test ledger). Rows marked `excluded` are NOT covered by the
S2/S17 envelope-identity claim.

Envelope claim (scope): every fixture snapshots the full aura-level
request triple - system preamble string, ordered message list, serialized
tool-definition JSON. Final provider assembly happens in the pinned rig
fork (rev `8908530`); that mapping is a named residual risk, not a covered
surface. Envelope identity at this seam is a NECESSARY condition for
benchmark-score neutrality, not a sufficient one: a passing corpus proves
the requests are unchanged, never that behavior or scores are - downstream
cards must not read a green snapshot gate as behavioral evidence.

Two further claim qualifiers, marked where they apply:

- **Partial-tools fixtures (§6a).** For the fixtures listed there, the
  tools JSON deliberately omits definitions production would register
  (they need live wiring); those envelopes are reachable modulo the named
  omissions. Every other fixture's triple is complete and reachable.
- **Re-stated rules (R3/R5/R8).** Rows marked "re-stated" snapshot a shape
  the test builder reproduces rather than production emits end-to-end.
  The REQUIRED comparison gates landed in `golden_tests.rs` close the
  cross-iteration trace merge (R5) and the coordinator and worker
  preamble append orders (R3; the coordinator vector-append position
  stays re-stated because that gate runs vector-disabled, and the
  worker scratchpad position is a conditional residue). The R8
  conversation-growth rows are partially production-emitted (the
  per-turn pushes are shared with production; the sequence is
  test-side), and the R8 tool-registration rows stay shape-asserted,
  with each residue named in DESIGN.md.

## Fixture corpus

All coordinator fixtures share one source-built playbook constant
(`SOURCE_PLAYBOOK`, defined once in `golden_tests.rs` and carried by
`PreambleFixture::playbook`) preserving the 14 headed blocks of §1 rows
5-18. Continuation fixtures run on the default 3-cycle budget except
`coordinator_call3_failures` and `coordinator_call4_final_urgency`
(budget 4), so the `(FINAL ATTEMPT)` fill is owned by the urgency
fixture alone. Corpus data pins two determinism constraints: at most one
repeated (handle, category) failure pair per scenario (multi-pattern
`OBSERVED PATTERNS` ordering is HashMap-ordered) and one tool-call
attempt per task per iteration (the production trace loader's
within-iteration file order is filesystem-dependent).

| Fixture | Envelope |
|---|---|
| `coordinator_call1_recon` | Initial planning call; recon preamble (`tools_in_planning = "none"`); no history tools |
| `coordinator_call1_nonrecon_summary` | Initial planning call; non-recon preamble; Summary roster with POPULATED inventories built config-only (no live MCP): one worker with assigned vector stores exceeding `max_tools_per_worker` (name list + `(+N more)` truncation), one worker with none (`none configured`) |
| `coordinator_call1_full_visibility` | Initial planning call; Full roster branch, populated config-only: a described tool (store present in `agent_config.vector_stores`, `context_prefix` description), an undescribed tool (assigned store name absent from agent config → bare `- name` line), an `(+N more)` remainder, and a no-tools worker |
| `coordinator_call1_no_workers` | Initial planning call; `has_workers() == false` (empty worker sections) |
| `coordinator_preamble_full_appends` | Initial call; skill catalog + vector-store context + source-built session history; history tools included; skill tool definitions (`load_skill`/`read_skill_file`) included - production registers them with the catalog. Partial-tools fixture (§6a): vector-search definitions omitted |
| `session_history_catch_all` | Initial call; session-history block with Running and Pending task summaries, exercising the catch-all render for unfinished tasks |
| `coordinator_call2_clean` | Continuation call 2; 1 iteration, all tasks complete (all failure slots empty) |
| `coordinator_call2_all_failed` | Continuation call 2; all tasks failed (empty COMPLETED section; hard failure w/ >2000-char error; one failed task with empty traces → no trace lines) |
| `coordinator_call3_failures` | Continuation call 3; mixed complete/failed/blocked; failure summary; accumulated + repeated failure history; cross-iteration artifact re-listing; failed task carrying tool traces (unconditional failed-entry trace lines) |
| `coordinator_call4_final_urgency` | Continuation call 4; 3 iterations; budget 4 → `(FINAL ATTEMPT)`; template tail's third occurrence |
| `coordinator_call_completed_task_tool_chain` | Continuation call 2; non-default `show_tool_reasoning_in_continuation = true`; completed tasks with traces render the gated tool-chain lines and `[Artifact: ...]` refs |
| `coordinator_call_all_failure_categories` | Continuation call 2; 10 failed tasks, one per `FailureCategory` variant, exercise every bracket tag in the FAILED TASKS section |
| `tools_coordinator_recon_history` | Initial planning call with recon + history tools BOTH included - a full (system, messages, tools) triple like every fixture: owns preamble block 2's recon+history tools-sentence branch AND the recon/history tool JSON |
| `worker_role_frame_direct` | Named-role worker; populated frame, Direct-only; all three role-branch appends - assigned vector stores (post-`retain`), scratchpad, skill catalog - proving the vector → scratchpad → skills order; skill tool definitions included. Partial-tools fixture (§6a) |
| `worker_role_frame_transitive` | Named-role worker; populated frame, Direct + Transitive (plan-order rendering, defect E visible) |
| `worker_role_frame_spilled_claim_echo` | Populated frame; spilled entry with claim-echo stand-in (defect C byte-identical Summary/Evidence) |
| `worker_frame_spilled_no_preview` | Populated frame; whitespace-prefix spill → `(no inline preview)` pointer-only entry |
| `worker_first_turn_empty` | Empty `%%CONTEXT%%`, fresh-plan first turn (defect B dangling reference) |
| `worker_replan_boundary_empty` | Empty `%%CONTEXT%%`, replan-boundary first turn - DISTINCT branch from fresh-plan empty (mechanically identical render, causally different; pre-approved decision 4) |
| `worker_generic_fallback` | Generic worker preamble (no custom prompt) + scratchpad + skill-catalog appends, proving the generic-branch scratchpad → skills order; skill tool definitions included. Partial-tools fixture (§6a). No vector append: the generic branch never receives one (the append is inside the named-role branch only) |
| `worker_generic_custom` | Generic worker preamble with custom `worker_system_prompt`; no appends (bare generic branch) |

## 1. Coordinator system preamble (anatomy §1a, 23 blocks)

| # | Block | Covered by |
|---|---|---|
| 1 | Title + role paragraph | every coordinator fixture |
| 2 | `## Your Tools` + tools sentence - 4 branches: recon×history | recon+no-history: `coordinator_call1_recon`; non-recon+history: `coordinator_preamble_full_appends`; non-recon+no-history: `coordinator_call1_nonrecon_summary`; recon+history: `tools_coordinator_recon_history` |
| 3 | `## Core Behavior` (6 rules, escape hatch present) | every coordinator fixture (env pinned unset; see exclusions) |
| 4 | `## Custom Instructions` header | every coordinator fixture |
| 5-18 | Playbook rows (intro, ROUTING, PHASE BOUNDARY PRINCIPLE, OPERATING STRATEGY, INITIAL PLAN CONTRACT, EXACT-DATA HANDOFF, decision-packet checklist, after-each-iteration, SINGLE-ACTION TASK CONTRACT, DEPTH-FAILURE RECOVERY, REPLAN BUDGET, WORKER SELECTION, PLAN STRUCTURE, TASK DESCRIPTIONS) | every coordinator fixture: all share the corpus's `SOURCE_PLAYBOOK` constant carrying all 14 headed blocks, substituted into the single `%%ORCHESTRATION_SYSTEM_PROMPT%%` slot (the code treats rows 5-18 as one opaque blob; the shared playbook preserves the per-row heading structure so Gate A can see each block render) |
| 19 | Worker-names-vs-tool-names paragraph (non-recon branch) | `coordinator_call1_nonrecon_summary`; recon-branch `## Reconnaissance Guidance` alternative: `coordinator_call1_recon` |
| 20 | `## Task Description Quality` | every coordinator fixture |
| 21 | `## Planning Guidelines` + JSON plan examples | every coordinator fixture |
| 22 | `## Artifacts` | every coordinator fixture |
| 23 | Session-history block (source-built; no trace-derived golden exists - pre-approved decision 5). Fixture input order: manifests MOST-RECENT-FIRST, exactly as `load_session_manifests` returns them (`build_session_context` re-reverses for chronological turn numbering); enforced by `SessionHistoryFixture::new` | `coordinator_preamble_full_appends` |
| 23a | `session_history.md` slot `%%TURN_ENTRIES%%` (unconditionally filled whenever block 23 renders) | `coordinator_preamble_full_appends` |
| 23b | `session_history.md` slot `%%TURN_COUNT%%` (unconditionally filled whenever block 23 renders) | `coordinator_preamble_full_appends` |
| 23c | Turn-entry shapes inside `%%TURN_ENTRIES%%`. The source-built manifest list carries a routed run and a direct-response run (outcome plus response summary, no task list). The routed run has a Complete task summary (named worker, confidence tag, result preview), a Failed task summary (unassigned worker → `[unassigned]`, category tag, error, error-context last-tool/partial lines), a tool-chain line (success + FAILED outcomes), an artifacts line, and the cross-run `read_artifact` hint | `coordinator_preamble_full_appends`. The catch-all Running/Pending task render is covered by `session_history_catch_all`; the unit-level edge cases (`test_session_history_*_no_preview_no_confidence`, `test_session_history_running_task_status`) remain in `frame_validation_tests.rs` |
| - | Skill catalog append | `coordinator_preamble_full_appends` |
| - | Vector-store context append (`## Available Knowledge Bases`) | `coordinator_preamble_full_appends` |
| - | Append order (catalog → vector stores → `'\n'` + session history) | `coordinator_preamble_full_appends`, COVERED by the landed R3 comparison gate (`gate_r3_coordinator_preamble_matches_create_coordinator`): the composed preamble byte-equals real `create_coordinator` output over a tempdir-backed skills + session-history config. Residue: the gate runs vector-disabled (live-manager construction), so the vector append POSITION between catalog and session stays re-stated (DESIGN.md R3) |

## 2. Initial planning wrapper, msg-1 sub-slots (anatomy §1b slot map)

| Slot | Covered by |
|---|---|
| `Current time:` prefix | every coordinator fixture (normalized; pass 1) |
| "Analyze this user query..." line | every initial-call fixture |
| `USER QUERY:` + verbatim goal | every coordinator fixture |
| `AVAILABLE WORKERS:` roster - `ToolVisibility::None` (descriptions only) | `coordinator_call1_recon` |
| `AVAILABLE WORKERS:` roster - `Summary`: populated tool-name list (config-only `vector_search_*` names from worker `vector_stores` assignments), `format_tool_list` truncation (`(+N more)`), AND the empty branch (`none configured`) | `coordinator_call1_nonrecon_summary` (both branches in one roster) |
| `AVAILABLE WORKERS:` roster - `Full`: described tool line (`- name: description` from `agent_config.vector_stores` `context_prefix`), undescribed tool line (bare `- name`), `(+N more)` remainder, and the no-tools section (no `Tools:` block) | `coordinator_call1_full_visibility` (all four branches in one roster) |
| No-workers branch (empty section, field, guidelines) | `coordinator_call1_no_workers` |
| Routing-tool menu (3 expanded bullets) | every initial-call fixture |
| Worker guidelines + valid-worker-names + time-context bullet | every initial-call fixture with workers (names normalized; pass 2) |
| "Call the appropriate routing tool now." | every initial-call fixture |

## 3. Continuation prompt (`continuation_prompt.md`, all 12 `%%SLOTS%%`)

| Slot / branch | Filled by | Empty in |
|---|---|---|
| `%%ITERATION%%` / `%%MAX_ITERATIONS%%` | every continuation fixture | n/a (always filled) |
| `%%URGENCY%%` (`(FINAL ATTEMPT)`) | `coordinator_call4_final_urgency` | `coordinator_call2_clean` |
| `%%SUCCEEDED%%` / `%%TOTAL%%` | every continuation fixture | n/a |
| `%%GOAL%%` (pinned verbatim query) | every continuation fixture | n/a |
| `%%COMPLETED_SECTION%%` | `coordinator_call2_clean` | `coordinator_call2_all_failed` |
| `%%BLOCKED_SECTION%%` | `coordinator_call3_failures` | `coordinator_call2_clean` |
| `%%REDESIGN_SECTION%%` (FAILED TASKS) | `coordinator_call3_failures` | `coordinator_call2_clean` |
| `%%FAILURE_SECTION%%` (summary + gaps) | `coordinator_call3_failures` | `coordinator_call2_clean` |
| `%%FAILURE_HISTORY%%` | `coordinator_call3_failures` | `coordinator_call2_clean` |
| `%%REUSE_GUIDANCE%%` (failed + succeeded > 0) | `coordinator_call3_failures` | `coordinator_call2_clean`, `coordinator_call2_all_failed` |
| Completed entry: inline result + claim | `coordinator_call2_clean` | |
| Completed entry: inline result, no claim | `coordinator_call2_clean` | |
| Completed entry: spilled, claim stand-in | `coordinator_call3_failures` | |
| Completed entry: spilled, raw-preview stand-in | `coordinator_call3_failures` | |
| Completed entry: spilled, pointer-only | covered on the worker side (`worker_frame_spilled_no_preview`); coordinator-side render shares `EvidenceEntry` | |
| Artifact inventory lines (`[Artifact: ...]`) | `coordinator_call2_clean`, `coordinator_call3_failures` | |
| Cross-iteration artifact re-listing (`load_tool_traces_for_plan` run-wide merge by task id) | `coordinator_call3_failures` (same task id across iterations 1-2); the merge rule is COVERED by the landed R5 comparison gate (`gate_r5_trace_merge_matches_persistence_loader`), which writes the same records through a tempdir `ExecutionPersistence` and asserts the harness merge equals the production `load_tool_records_for_task` scan. Residue in DESIGN.md R5: the trivial per-task wrapper loop of `load_tool_traces_for_plan` is reproduced in the gate, and fixtures pin one attempt per task per iteration | |
| Failed entry: hard, `ErrorPreview` truncation marker | `coordinator_call2_all_failed` (>2000-char error) | |
| Failed entry: soft (claim + optional artifact) | `coordinator_call3_failures` | |
| Failed entry: tool-trace lines (`render_tool_chain_lines` - UNCONDITIONAL for failed tasks at `9df96382`, unlike the gated completed-task chain) | `coordinator_call3_failures` (failed task carrying traces) | `coordinator_call2_all_failed` (failed task with empty traces → no lines) |
| Blocked entry (label only) | `coordinator_call3_failures` | |
| Failure history record render | `coordinator_call3_failures` | |
| Repeated-failure detection (`OBSERVED PATTERNS`) | `coordinator_call3_failures` (same handle+category twice; exactly ONE repeated pair, because the patterns list iterates a `HashMap` and a multi-pattern fixture would snapshot nondeterministically). Multi-pattern rendering keeps its unit coverage in the retained `test_continuation_multiple_repeated_failure_patterns` | |
| read_artifact hint line, decision menu, synthesis rules (fixed tail) | every continuation fixture; third tail occurrence: `coordinator_call4_final_urgency` | |
| Whitespace-only completed result fallback (bare label line) | EXCLUDED from the corpus: the fixture composes `EvidenceText`, which forbids whitespace results by construction. Owning tests: `test_continuation_whitespace_only_result_renders_bare_label` (`frame_validation_tests.rs`, the bare-label render arm in `types.rs`; added by the S2 repair round - the arm had no owner before) and `empty_values_are_rejected` (`context/evidence.rs`, the `EvidenceText` rejection that drives that `Err(_)` arm). Candidate for promotion via a fixture variant if S3-S6 touch it. | |
| COMPLETED-entry tool-chain lines (gated by `show_tool_reasoning_in_continuation = true`) | `coordinator_call_completed_task_tool_chain` (non-default knob enabled); the accompanying `[Artifact: ...]` refs are captured there. The unit-level `test_continuation_tool_output_artifacts_visible` was deleted and absorbed by this fixture. |
| All `FailureCategory` bracket tags in the FAILED TASKS section | `coordinator_call_all_failure_categories` (one failed task per variant); the unit-level `test_continuation_all_failure_categories` was deleted and absorbed by this fixture. | |
| Multi-surface continuation scenario (completed, failed, blocked, failure history, artifact re-listing) | `coordinator_call3_failures` + `coordinator_call2_clean` + `coordinator_call_completed_task_tool_chain` + `coordinator_call_all_failure_categories`; the unit-level `test_continuation_full_scenario` was deleted and absorbed across these fixtures. | |
| Session-history full scenario (manifest list, turn entries, tool-chain lines, artifact refs) | `coordinator_preamble_full_appends` + `session_history_catch_all`; the unit-level `test_session_history_full_scenario` was deleted and absorbed across these fixtures. | |

## 4. Conversation growth (anatomy §1b)

| Rule | Covered by |
|---|---|
| User turn pushed verbatim (planning wrapper, then each continuation wrapper) | every continuation fixture - COVERED by the S3 R8 comparison gate (`gate_r8_conversation_growth` in `golden_tests.rs`). Conversation growth goes through the production `push_user_turn` / `push_assistant_turn` helpers (formerly inline in `plan_with_routing`). The SEQUENCE (how many iterations and in what order) stays test-side, so the growth rule is partially production-emitted rather than re-stated |
| Compact assistant turn (`compact_decision_turn` → `CoordinatorTurn::render`, `create_plan` variant, ~136-char shape) | every continuation fixture (turn TEXT is production code via `compact_decision_turn`; its position in the conversation is now partially production-emitted via the shared `push_assistant_turn` helper - the sequence is still test-side, so R8 is partially closed) |
| Terminal decision turns (`respond_directly` / `request_clarification` renders) | EXCLUDED: terminal decisions end the run, so they never precede a later planning call inside one envelope; owning test: `terminal_turns_record_the_model_text_verbatim` (`context/turn.rs`). `PlanDecision` makes them unrepresentable mid-thread. |
| Compact-turn fallback tiers (model text / bare variant name) | EXCLUDED: degenerate-decision path (empty rationale/plan), unreachable from validated fixtures; owning test: `test_compact_decision_turn_fallback_tiers` (`frame_validation_tests.rs`, added by the S2 repair round - no prior test invoked the fallback arms). |
| Correction retry message (`ROUTING_TOOL_REQUIRED` as attempt > 1 prompt) | EXCLUDED: parse-failure path only; constant pinned in `prompt_constants.rs`. |
| External `chat_history` prefix | PINNED EMPTY in all fixtures: the benchmark adapter issues one POST per task with no prior chat. Multi-turn chat prefixes are out of the S2 claim. |

## 5. Worker call surfaces

| Surface / branch | Covered by |
|---|---|
| Role preamble branch (`WORKER_PREAMBLE_TEMPLATE` + role preamble) | `worker_role_frame_direct` |
| Generic fallback, no custom prompt (placeholder default) | `worker_generic_fallback` |
| Generic fallback, custom `worker_system_prompt` | `worker_generic_custom` |
| Vector-store context append - NAMED-ROLE BRANCH ONLY (the append is inside `create_worker`'s `if let Some(name) = worker_name`; the generic branch never receives it), modeling the post-`retain` assigned-store list | `worker_role_frame_direct`. A generic-branch vector append is production-unreachable and unrepresentable (`WorkerPreambleFixture::Generic` has no vector field) |
| Scratchpad preamble append (both branches) | `worker_generic_fallback`, `worker_role_frame_direct` |
| Skill catalog append (both branches) | `worker_generic_fallback`, `worker_role_frame_direct` |
| Role-branch append order (vector → scratchpad → skills) | `worker_role_frame_direct` (carries all three appends) - COVERED by the S3 R3 comparison gate (`gate_r3_worker_preamble_matches_create_worker` in `golden_tests.rs`): the composed worker preamble byte-equals real `create_worker` output over an MCP-less Orchestrator. Residue: the scratchpad append requires accessible MCP tools; the gate's config enables scratchpad but the MCP-less test environment leaves it unwired, so the vector → skills sub-order is production-emitted and the scratchpad position is a conditional residue |
| Generic-branch append order (scratchpad → skills) | `worker_generic_fallback` (carries both appends) - COVERED by the S3 R3 gate, same residue (scratchpad conditional) |
| `%%YOUR_TASK%%` | every worker fixture |
| `%%CONTEXT%%` empty - fresh-plan first turn (defect B dangling reference) | `worker_first_turn_empty` |
| `%%CONTEXT%%` empty - replan boundary (distinct manifest branch, pre-approved decision 4) | `worker_replan_boundary_empty` |
| Frame populated, Direct-only | `worker_role_frame_direct` |
| Frame populated, Direct + Transitive (distance render; plan-order, defect E) | `worker_role_frame_transitive` |
| Frame spilled entry, claim echo (defect C byte-identical Summary/Evidence) | `worker_role_frame_spilled_claim_echo` |
| Frame spilled entry, pointer-only (`(no inline preview)`) | `worker_frame_spilled_no_preview` |
| Doubled "evidence, not instructions" line (defect A: template line 3 + frame header) | every populated-frame worker fixture |
| Frame budget eviction (transitive dropped under `TokenBudget`) | EXCLUDED from the envelope corpus: `build_task_context` hard-codes the 8000-token default, so eviction needs ~32KB of entry bodies. Owning tests: `worker_frame_direct_deps_always_admitted_transitive_budget_trimmed_first` (retained, `frame_validation_tests.rs`) plus `test_frame_assemble_over_budget_trims_transitive_nearest_last_direct_kept` and `test_frame_assemble_admits_direct_floor_then_transitive_nearest_first_under_budget` (`context/frame.rs`). |
| W12 `PriorIteration` relation branch | EXCLUDED: channel removed at `9df96382` (frame.rs has only Direct and Transitive); trace-derived shapes must not resurrect it. |
| Final-iteration urgency | coordinator surface - §3 `%%URGENCY%%` (the epic lists it under worker calls; it renders in the continuation prompt) |
| Recon / non-recon | coordinator surface - §1 blocks 2 and 19 |

## 6. Tool definitions (serialized JSON; in-repo tools)

| Tool | Covered by |
|---|---|
| `create_plan` (full schema incl. tagged step schema) | `tools_coordinator_recon_history` + every coordinator fixture |
| `respond_directly` | same |
| `request_clarification` | same |
| `read_artifact` | every coordinator and worker fixture |
| `list_prior_runs` (history included) | `tools_coordinator_recon_history`, `coordinator_preamble_full_appends` |
| `list_tools` (recon included) | `tools_coordinator_recon_history`, `coordinator_call1_recon` |
| `inspect_tool_params` (recon included) | same |
| `submit_result` | every worker fixture |
| Skill tool definitions (`load_skill`, `read_skill_file`) | `coordinator_preamble_full_appends` (coordinator: `SkillToolset` registers with the catalog append), `worker_role_frame_direct` and `worker_generic_fallback` (workers: the builder registers `SkillToolset` from the same config). `SkillToolset::new` is pure over `SkillConfig` - no filesystem discovery needed for definitions |
| MCP tool definitions | EXCLUDED: external servers; not in-repo surfaces. Fixtures pin `mcp: None`, itself a reachable no-MCP deployment |
| `DynamicVectorSearchTool` definition | EXCLUDED: requires a live vector-store manager (`VectorStoreManager::from_config`); the preamble context block IS covered (§1, §5). Fixtures carrying a vector append are partial-tools fixtures (§6a) |
| Scratchpad exploration tool definitions | EXCLUDED: construction requires live token-counter/storage wiring; the preamble append IS covered (§5). Fixtures with `ScratchpadWiring::Wired` are partial-tools fixtures (§6a). Candidate for promotion in S3 (bounding module) if cheap |

### 6a. Partial-tools fixtures (amended reachability)

For exactly these fixtures the tools JSON deliberately omits definitions
production would register alongside the snapshotted preamble append; the
(system, messages) surfaces are complete and reachable, and the tools
surface is reachable modulo the named omissions. Every fixture not listed
here carries a complete, reachable triple.

| Fixture | Omitted definitions | Why |
|---|---|---|
| `coordinator_preamble_full_appends` | `vector_search_*` | live vector-store manager |
| `worker_role_frame_direct` | `vector_search_*`, scratchpad exploration tools | live manager; live token-counter/storage |
| `worker_generic_fallback` | scratchpad exploration tools | live token-counter/storage |

### 6b. Tool registration order (R8 comparison gates)

| Gate | Claim | Status |
|---|---|---|
| `gate_r8_coordinator_tool_order` | Coordinator tool registration order matches `build_agent_with_tools` | SHAPE-ASSERTED: the gate mirrors the production order via `coordinator_tool_order_for_golden`, but it does not call `build_agent_with_tools` directly; a production reordering could false-pass. The gate detects drift in the re-stated order, not in production. |
| `gate_r8_worker_tool_order` | Worker tool definition order matches `Agent::add_all_tools` | SHAPE-ASSERTED: the gate asserts the order returned by `worker_tool_definitions` against a hard-coded vector; a production reordering could false-pass. |

## 7. Exclusions and pinned environment (claim boundary)

| Item | Reason |
|---|---|
| Rig-fork final request assembly (rev `8908530`) | No in-repo seam returns the final provider request; the S2 claim is the aura-level triple. Residual risk R1 in DESIGN.md. |
| `AURA_ESCAPE_HATCH=false` preamble strip | Env-mutating branch; `std::env::set_var` is unsafe under edition 2024 and races parallel tests. The builder asserts the variable is UNSET and fails loudly otherwise. Covered at unit level in `config.rs` tests only if added there; named residual risk R7. |
| Live MCP tool inventories for the Summary and Full rosters | Tests run with `mcp: None`, so `resolve_worker_tools` yields empty maps; production inventories depend on live servers. The branch STRUCTURE is covered; inventory content is not. Residual risk R6. |
| Duplicate-call guard templates (`duplicate_call_guidance.md`, `duplicate_call_abort.md`) | Mid-run tool-output injections on the duplicate-loop path, not planning/worker envelope surfaces. Card-acceptance scope (resolved for Gate A): the card's every-`%%SLOT%%` acceptance line is satisfied over ENVELOPE-SURFACE templates; these two templates' `%%TOOL_NAME%%`/`%%COUNT%%` slots keep their `duplicate_call_guard` unit coverage. `session_history.md`'s two slots are envelope-surface and have explicit rows (§1, 23a/23b). |
| Context-overflow suggestions (`prompt_constants::context_overflow`) | Error-path strings returned to the caller, never sent to the model. |
| `WORKER_SUBMIT_RESULT` correction | Worker retry path; injected only after a missing `submit_result` call. |
| Timing, event side effects, artifact I/O ordering | Outside the envelope by definition; named residual risks R2/R4. |

## 8. Normalization (what identity tolerates)

Exactly two rewrite classes (test-side only; pre-approved decision 2),
both LOCATION-AWARE - applied per message on the structured envelope
before flattening, never over flattened snapshot text:

1. `Current time: <rfc3339>` → `Current time: <TIMESTAMP>`, anchored at
   byte offset 0 of a user-message body (the wrappers PREFIX their
   output; live clock in `build_planning_wrapper` /
   `build_continuation_wrapper`). The same text elsewhere is payload and
   is never rewritten.
2. HashMap-ordered worker spans sorted lexicographically, inside the
   initial planning wrapper (the first user message) only: roster entries
   under `AVAILABLE WORKERS:` and the quoted list after
   `Valid worker names:`.

An occurrence audit runs before any rewrite and panics on drift instead
of rewriting or skipping silently: user messages must be all-or-none on
the timestamp prefix; a malformed `Current time: ` prefix at offset 0 is
a defect; the roster markers must appear at most once each, only in the
first user message, and nowhere else in the envelope. Fixture payloads
must not embed the markers - the audit turns a collision into a loud
failure, so a payload byte can never be silently normalized away.

Every other byte is compared exactly: a difference outside these two
anchored classes fails the snapshot.
