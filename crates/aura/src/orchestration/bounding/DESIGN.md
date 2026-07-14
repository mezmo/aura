# S3 unified-bounding type design record

Baseline: aura `3136fe19`, branch `card/S3`. Scope: the `bounding` module
that consolidates every truncate/summarize/spill decision behind one typed
budget config. The skeleton lands first with `todo!()` bodies; the
implementation phase will rewire production call sites without changing any
S2 manifest surface.

This is a pure-consolidation refactor: the repaired skeleton models the
semantics that production already accepts today.  Config values that
production treats as valid (zero thresholds, misordered duplicate-call
thresholds, `max_tools_per_worker = 0`, `result_summary_length = 0`,
summary wider than threshold) are representable; rejecting them would be a
behavior change.

## Type inventory

Every public type maps to one business rule and names the invalid state it
forbids. Types marked (reused) come from existing production modules and are
composed, not re-modeled. Type-implementation-detail types (`ByteWidth`,
`CharWidth`, `NudgeThreshold`, `BlockThreshold`, `TruncateMarker`) are private;
`NonZeroByteThreshold` and `NonZeroDuration` are `pub(crate)` so that the
public enum variants they appear in are well-formed.

| Type | Business rule | Forbidden invalid state |
|---|---|---|
| `BoundingConfig` | Centralizes the byte/char bounding decisions, display limits, and session-history limit used by the coordinator and worker flows. Does NOT own the token budgets (see R7). | A partially-built bounding snapshot, or a fixed-baseline width that does not match the accepted baseline binary |
| `ByteThreshold` | Byte-based bounds that treat zero as a sentinel ("promote all" / "spill all non-empty") keep that semantics explicit | Negative thresholds (unrepresentable via `usize`) |
| `ResultSummaryWidth` | The same config value drives both the artifact stand-in prefix and replan execution-summary error truncation; production accepts zero (empty prefix + marker) and summary wider than threshold | None; any `usize` is representable |
| `ResultSpillBudget` | A worker result either stays inline (byte length at or below threshold) or spills to an artifact with a bounded inline stand-in summary | None; production accepts threshold 0, summary 0, and summary larger than threshold |
| `ResultSpillDecision` | `ResultSpillBudget::decide` returns either the original inline result or the spill stand-in; the caller never measures bytes itself. `truncate_to_summary` serves the replan error site, which truncates to `summary_width` regardless of the spill threshold. | A caller that bypasses `decide`/`truncate_to_summary` and re-measures with a mismatched unit |
| `TruncatedSummary` | Carries the truncated summary text and cut flag produced by `ResultSpillBudget::truncate_to_summary`; implements `Display` for `.to_string()` and exposes `was_truncated()` plus a consuming `into_string()`. | None; any `String` + `bool` pair is representable |
| `SizePromotion` | Tool-output size promotion: threshold 0 means promote all (including empty), a positive threshold means larger than that byte count | None; any `usize` is representable |
| `DurationPromotion` | Tool-output duration promotion: threshold 0 means disabled, a positive threshold means longer than that `std::time::Duration` | None; any `u64` millis is representable |
| `ToolOutputSpillBudget` | Tool outputs are promoted to artifacts when size or duration qualifies; the two zero semantics are asymmetric (size 0 = promote all, duration 0 = disabled) | None beyond non-negativity |
| `ToolListLimit` | The coordinator planning prompt truncates long per-worker tool lists to a bounded count before appending `(+N more)`; `max_tools_per_worker = 0` is `HideAll`: Summary rendering shows `(+N more)` for a nonempty list, while Full rendering omits the tool section | None; any `usize` is representable |
| `DuplicateCallPolicy` | The duplicate-call guard fires based on consecutive identical-call count; zero thresholds fire maximally and `nudge >= block` produces block-only behavior. `NudgeOnly` is not modeled: production always evaluates a finite block threshold first, so every raw `(nudge, block)` pair maps to `BlockOnly` or `NudgeThenBlock`. | None; every raw `(nudge, block)` pair maps to a reachable policy |
| `SessionHistoryLimit` | Prior-run manifest injection into the coordinator preamble is either disabled or bounded to a positive number of most-recent turns | An unbounded injection limit |
| `FailureHandleWidth` | Failure-history task handles are the first line of the description capped at a fixed character width, plus a marker when cut | A zero cap |
| `ErrorPreviewWidth` | Failure entries render a bounded error preview with an explicit `[truncated]` marker, never an unbounded error body | A zero cap |
| `ToolReasoningWidth` | Tool-reasoning lines in the continuation prompt are truncated to a bounded character width with an ellipsis marker | A zero cap |
| `RoutingRationaleWidth` | Routing-rationale log previews are byte-bounded | A zero byte width |
| `TaskDescriptionLogWidth` | Task-description log previews are byte-bounded | A zero byte width |
| `TaskDescriptionSpanWidth` | Task-description tracing-span previews are byte-bounded | A zero byte width |
| `GoalWidth` | Goal/span previews are byte-bounded | A zero byte width |
| `RoutingResponseWidth` | Raw routing-response log previews are byte-bounded | A zero byte width |
| `QueryLogWidth` | Fallback-query log preview is byte-bounded | A zero byte width |
| `LogPreviewWidths` | All observability/tracing string previews draw their limits from one table | Any preview width equal to zero |
| `ResultPreviewWidth` | `TaskSummary.result_preview` is byte-bounded; persisted to the run manifest | A zero byte width |
| `ResponseSummaryWidth` | Manifest `response_summary` is byte-bounded; persisted to the run manifest | A zero byte width |
| `ManifestWidths` | Persisted-manifest string previews (`result_preview`, `response_summary`) draw their limits from one table, separate from observability previews | Any preview width equal to zero |
| `PlanTaskDescriptionWidth` | Fallback `Task.description` content is byte-bounded | A zero byte width |
| `PlanDirectAnswerTaskWidth` | Config-converted direct-answer plan task content is byte-bounded | A zero byte width |
| `PlanDirectAnswerRationaleWidth` | Config-converted direct-answer plan rationale content is byte-bounded | A zero byte width |
| `PlanClarificationTaskWidth` | Config-converted clarification plan task content is byte-bounded | A zero byte width |
| `PlanClarificationQuestionWidth` | Config-converted clarification plan question content is byte-bounded | A zero byte width |
| `PlanContentWidths` | All plan-content string truncations (persisted manifests and worker prompts) draw their limits from one table | Any truncation width equal to zero |
| `ScratchpadBudget` | The token-based scratchpad budget exists only when scratchpad tools are wired for a worker and is scoped to that worker's effective LLM | n/a (infallible wrapper; a zero context window cannot be reliably detected) |
| `TokenBudget` (reused from `context::frame`) | The prior-work frame assembles completed ancestor entries under a non-zero token budget; direct dependencies are the floor and transitive entries fill remaining budget nearest-first | A zero token budget (R2 type already forbids this via `NonZeroUsize`) |
| `ContextBudget` (reused from `scratchpad::context_budget`) | Tracks estimated token consumption from scratchpad tool returns and tokens diverted from the context window | n/a (production-owned) |

## Visibility/seam table

The skeleton introduces the `bounding` module and adds a single module
declaration in `orchestration/mod.rs`. No production call sites are rewired
in this phase.

| Production item today | Visibility at 3136fe19 | S3 seam plan |
|---|---|---|
| `OrchestrationConfig` pub fields (`max_tools_per_worker`, `duplicate_call_nudge_threshold`, `duplicate_call_block_threshold`) and accessors (`result_artifact_threshold`, `result_summary_length`, `tool_output_artifact_threshold`, `tool_output_duration_threshold_ms`, `session_history_turns`) | `pub` | Replaced by `BoundingConfig::from_orchestration` and typed accessors |
| `maybe_create_artifact` (orchestrator.rs) | private | Will consume `BoundingConfig::result_spill()` |
| `PersistenceWrapper` size/duration thresholds | `pub` constructor via `PersistenceWrapperParams` | Will consume `BoundingConfig::tool_output_spill()` |
| `format_tool_list` / worker-section builders (orchestrator.rs) | private | Will consume `BoundingConfig::tool_list_limit()` |
| `DuplicateCallGuard::new` | `pub` | Will consume `BoundingConfig::duplicate_call_policy()` |
| `load_session_manifests` limit | `pub` | Will consume `BoundingConfig::session_history_limit()` |
| `FailureHandle::from_description` (context/failure_history.rs) | `pub` | Will consume `BoundingConfig::failure_handle_width()` |
| `ErrorPreview::new` (context/evidence.rs) | `pub` | Will consume `BoundingConfig::error_preview_width()` |
| `truncate_reasoning` (types.rs) | private free function | Will consume `BoundingConfig::tool_reasoning_width()` |
| `safe_truncate` / log-preview sites (orchestrator.rs) | `pub(crate)` / private | Will consume `BoundingConfig::log_preview_widths()` |
| `truncate_query` plan-content sites (orchestrator.rs) | private | Will consume `BoundingConfig::plan_content_widths()` |
| `Agent::scratchpad_budget` (builder.rs) | `pub(crate)` | Will receive a `ScratchpadBudget` wrapper from the S3+ seam |
| `PriorWorkFrame::assemble` (context/frame.rs) | `pub` | Continues to use the reused `TokenBudget` |

No production visibility is widened by the skeleton. The only file touched
outside the new module is `orchestration/mod.rs`, which adds the module
declaration.

## Consolidation inventory (what this module unifies)

The following sites are the current source of truth for each bounding
decision. The implementation phase will route each group through the typed
type above, preserving byte-identical output on every S2 manifest surface.

- **Byte-based artifact spill**: `maybe_create_artifact` result threshold
  (`result_artifact_threshold`, default 4000) and summary width
  (`result_summary_length`, default 2000).  Production uses `result.len()`
  (bytes) for both threshold and summary; the artifact stand-in prints
  "Full result ({} chars)" while reporting `result.len()` bytes — a label
  lie the implementation must preserve byte-identically.
- **Token-based scratchpad budget**: per-worker `ContextBudget` built in
  `create_worker` and attached to `Agent::scratchpad_budget`.
- **Token-based prior-work frame budget**: `context::frame::TokenBudget`
  default 8000 tokens.
- **Byte-based observability caps**: `safe_truncate` literals for routing
  rationale (80), task-description log preview (100), task-description
  tracing span (200), goal (200), raw routing response (300), fallback-query
  log preview (100).
- **Byte-based manifest caps**: `safe_truncate` literals for
  `TaskSummary.result_preview` (200) and manifest `response_summary` (200);
  persisted to the run manifest, separate from observability previews.
- **Byte-based plan-content caps**: `truncate_query` literals for fallback
  `Task.description` (100), converted direct-answer task (80) and rationale
  (100), and converted clarification task (80) and question (100).
- **Ad-hoc character caps**: `FailureHandle::MAX_CHARS` (120),
  `ErrorPreview::MAX_CHARS` (2000), `truncate_reasoning` (100).
- **Display limits**: `max_tools_per_worker` (default 10) and
  `DuplicateCallGuard` nudge/block thresholds (default 3/5).
- **History limit**: `session_history_turns` (default 3).
- **Tool-output promotion**: `tool_output_artifact_threshold` (default 500
  bytes) and `tool_output_duration_threshold_ms` (default 5000 ms).

## Residual risks

- **R1 - byte vs. character mismatch.** The current `safe_truncate` operates
  on UTF-8 bytes, while several config fields and comments describe
  "characters." Consolidation keeps the byte mechanism for byte-bounded
  surfaces and the character mechanism for char-bounded surfaces; a future
  card must decide whether to align the config labels or the implementation,
  because changing the unit would be a behavior change.
- **R2 - fail-open spill defect (S14).** `maybe_create_artifact` returns the
  full unbounded result when persistence fails to write the artifact. This
  is a behavior change and is intentionally NOT fixed here; it is recorded
  for S14.
- **R3 - SSE-handler truncation.** The comments in `events.rs` note that
  `planning_response`, `result`, and tool `result` fields are "truncated to
  Option in SSE." The actual byte caps live in the web-server SSE handlers,
  outside the `orchestration/` scope, so this module cannot unify them.
- **R4 - token-counter approximations.** The prior-work frame uses a
  4-char-per-token heuristic, while the scratchpad budget uses a real
  tokenizer. Consolidation does not merge or colocate these two token budgets;
  they remain owned by their respective modules (see R7).
- **R5 - HashMap ordering.** Consolidation does not change any iteration
  order, so S2 normalization pass 2 (worker-order sort) remains required.
- **R6 - config-load boundary behavior change.** The original skeleton's
  validation could have rejected configs production accepts today (zero
  thresholds, misordered duplicate-call thresholds, `max_tools_per_worker =
  0`). Repairs 1-2 model these as valid states; R6 records that any FUTURE
  tightening is a behavior change requiring its own card (candidate: S14).
- **R7 - token budgets not colocated.** `BoundingConfig` centralizes the
  byte/char bounding decisions, display limits, and session-history limit, but
  does NOT own the two token budgets. The prior-work `TokenBudget`
  (4-character heuristic, `context::frame`) and the scratchpad `ContextBudget`
  (real tokenizer, `scratchpad::context_budget`) remain owned by their
  respective modules. Consolidation does not unify the two approximation
  methods (R4) nor relocate their ownership. This is a narrowed claim from the
  original skeleton, which overstated centralization.
- **R8 - SessionHistoryLimit compaction future.** `SessionHistoryLimit` today
  models only Disabled or a positive turn count (drop-old behavior). A future
  behavior change could let it return a compaction-budget option instead of
  dropping old history (relevant to emergency compaction even though the
  program philosophically avoids compaction). That is a behavior change
  requiring its own card; S3 models only the drop semantics production accepts
  today.
- **R9 - Truncation marker inconsistency.** The width types use three marker
  styles: no marker (most byte log/manifest previews and the char
  `FailureHandle`/`ErrorPreview` caps), ASCII dots `"..."` (routing rationale
  and plan-content byte widths), and the single ellipsis character `"…"` (the
  char `ToolReasoningWidth`). This is preserved byte-identically from
  production; a future card that unifies markers would be a behavior change.
- **R10 - `was_truncated` signal asymmetry.** `ResultSpillBudget::truncate_to_summary`
  returns `TruncatedSummary` carrying `was_truncated()`, but the char-cap types
  (`FailureHandleWidth`, `ErrorPreviewWidth`) return a bare `String` from their
  `truncate` methods with no cut flag. The prior domain constructors
  (`FailureHandle::from_description`, `ErrorPreview::new`) need that signal to
  gate their display markers. Phase B wiring will either need a
  `was_truncated`-returning variant on the char-cap types or must re-detect the
  cut by string comparison. This is a Phase B design decision, not a Phase A
  behavior break (call sites are unwired, manifest unchanged).
