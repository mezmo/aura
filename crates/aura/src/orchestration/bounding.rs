//! Unified bounding module.
//!
//! One source of truth for every byte/character truncate, summarize, spill,
//! display-limit, and history-limit decision in the orchestrator.  The module
//! exposes strongly-typed limits.  This is a pure-consolidation refactor: it
//! models the semantics that production already accepts today, it does not
//! tighten them.
//!
//! This is the S3 bounding module: function bodies are implemented.  The
//! call-site wiring to production code happens in the implementation phase.


use std::num::NonZeroUsize;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::orchestration::config::OrchestrationConfig;
use crate::string_utils::safe_truncate;

// ============================================================================
// Core config
// ============================================================================

/// Centralizes byte and character bounding decisions and the display/history
/// limits used by the coordinator and worker flows.
///
/// Business rule: every length-bound, spill threshold, and observability
/// preview used by the coordinator and worker flows is derived from one
/// validated snapshot of the orchestration configuration.
///
/// This struct owns the byte/char widths, spill budgets, display limits, and
/// session-history limit.  It does **not** own the token budgets: the
/// prior-work [`TokenBudget`](crate::orchestration::context::frame::TokenBudget)
/// and the scratchpad [`ContextBudget`](crate::scratchpad::ContextBudget) remain
/// owned by their respective modules.
///
/// Forbidden invalid state: a partially-built bounding snapshot, or a
/// fixed-baseline width that does not match the accepted baseline binary.
#[derive(Debug, Clone)]
pub struct BoundingConfig {
    result_spill: ResultSpillBudget,
    tool_output_spill: ToolOutputSpillBudget,
    tool_list: ToolListLimit,
    duplicate_calls: DuplicateCallPolicy,
    session_history: SessionHistoryLimit,
    failure_handle: FailureHandleWidth,
    error_preview: ErrorPreviewWidth,
    tool_reasoning: ToolReasoningWidth,
    log_previews: LogPreviewWidths,
    manifest_widths: ManifestWidths,
    plan_content: PlanContentWidths,
}

impl BoundingConfig {
    /// Build a bounding snapshot from the orchestration config.
    pub fn from_orchestration(config: &OrchestrationConfig) -> Self {
        Self {
            result_spill: ResultSpillBudget::from_config(
                config.result_artifact_threshold(),
                config.result_summary_length(),
            ),
            tool_output_spill: ToolOutputSpillBudget::from_config(
                config.tool_output_artifact_threshold(),
                config.tool_output_duration_threshold_ms(),
            ),
            tool_list: ToolListLimit::new(config.max_tools_per_worker),
            duplicate_calls: DuplicateCallPolicy::new(
                config.duplicate_call_nudge_threshold,
                config.duplicate_call_block_threshold,
            ),
            session_history: SessionHistoryLimit::new(config.session_history_turns()),
            failure_handle: FailureHandleWidth::DEFAULT,
            error_preview: ErrorPreviewWidth::DEFAULT,
            tool_reasoning: ToolReasoningWidth::DEFAULT,
            log_previews: LogPreviewWidths::default_widths(),
            manifest_widths: ManifestWidths::default_widths(),
            plan_content: PlanContentWidths::default_widths(),
        }
    }

    pub fn result_spill(&self) -> &ResultSpillBudget {
        &self.result_spill
    }

    pub fn tool_output_spill(&self) -> &ToolOutputSpillBudget {
        &self.tool_output_spill
    }

    pub fn tool_list_limit(&self) -> ToolListLimit {
        self.tool_list
    }

    pub fn duplicate_call_policy(&self) -> DuplicateCallPolicy {
        self.duplicate_calls
    }

    pub fn session_history_limit(&self) -> SessionHistoryLimit {
        self.session_history
    }

    pub fn failure_handle_width(&self) -> FailureHandleWidth {
        self.failure_handle
    }

    pub fn error_preview_width(&self) -> ErrorPreviewWidth {
        self.error_preview
    }

    pub fn tool_reasoning_width(&self) -> ToolReasoningWidth {
        self.tool_reasoning
    }

    pub fn log_preview_widths(&self) -> LogPreviewWidths {
        self.log_previews
    }

    pub fn manifest_widths(&self) -> ManifestWidths {
        self.manifest_widths
    }

    pub fn plan_content_widths(&self) -> PlanContentWidths {
        self.plan_content
    }
}

// ============================================================================
// Private implementation-detail widths
// ============================================================================

/// A non-zero byte width.
///
/// Private implementation detail.  Domain-specific public types below wrap
/// this so that byte-bounded and char-bounded widths are not interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteWidth(NonZeroUsize);

impl ByteWidth {
    // Unused S3 API surface.
    #[allow(dead_code)]
    fn new(bytes: usize) -> Option<Self> {
        NonZeroUsize::new(bytes).map(Self)
    }

    fn get(&self) -> usize {
        self.0.get()
    }
}

/// A non-zero character width.
///
/// Private implementation detail.  Domain-specific public types below wrap
/// this so that char-bounded widths are not interchangeable with byte widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct CharWidth(NonZeroUsize);

impl CharWidth {
    // Unused S3 API surface.
    #[allow(dead_code)]
    fn new(chars: usize) -> Option<Self> {
        NonZeroUsize::new(chars).map(Self)
    }

    fn get(&self) -> usize {
        self.0.get()
    }
}

// ============================================================================
// Unit-aware decision boundaries
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruncateMarker {
    None,
    EllipsisChar,
    Dots,
}

fn truncate_bytes(text: &str, max: usize, marker: TruncateMarker) -> String {
    let (truncated, was_cut) = safe_truncate(text, max);
    match (marker, was_cut) {
        (TruncateMarker::None, _) => truncated.to_string(),
        (TruncateMarker::EllipsisChar, true) => format!("{truncated}…"),
        (TruncateMarker::Dots, true) => format!("{truncated}..."),
        (_, false) => truncated.to_string(),
    }
}

fn truncate_chars(text: &str, max: usize, marker: TruncateMarker) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => {
            let truncated = &text[..cut];
            match marker {
                TruncateMarker::None => truncated.to_string(),
                TruncateMarker::EllipsisChar => format!("{truncated}…"),
                TruncateMarker::Dots => format!("{truncated}..."),
            }
        }
    }
}

/// A byte threshold that may be zero.
///
/// Business rule: some byte-based bounds treat zero as a sentinel
/// (`PromoteAll`, spill every non-empty result, etc.).  This type keeps that
/// semantics explicit.
///
/// Forbidden invalid state: negative thresholds are unrepresentable because
/// the inner type is `usize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ByteThreshold(usize);

impl ByteThreshold {
    pub fn new(bytes: usize) -> Self {
        Self(bytes)
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    pub fn get(&self) -> usize {
        self.0
    }

    pub fn allows_inline(&self, text: &str) -> bool {
        text.len() <= self.0
    }
}

/// A non-zero byte threshold for size-based promotion.
///
/// Implementation detail of [`SizePromotion`]; crate-visible so the public
/// enum variant is well-formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NonZeroByteThreshold(NonZeroUsize);

impl NonZeroByteThreshold {
    // Unused S3 API surface.
    #[allow(dead_code)]
    fn new(bytes: usize) -> Option<Self> {
        NonZeroUsize::new(bytes).map(Self)
    }
}

/// A non-zero duration threshold for duration-based promotion.
///
/// Implementation detail of [`DurationPromotion`]; crate-visible so the public
/// enum variant is well-formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NonZeroDuration(Duration);

impl NonZeroDuration {
    // Unused S3 API surface.
    #[allow(dead_code)]
    fn from_millis(ms: u64) -> Option<Self> {
        if ms == 0 {
            None
        } else {
            Some(Self(Duration::from_millis(ms)))
        }
    }
}

// ============================================================================
// Spill budgets
// ============================================================================

/// Width of a result summary.
///
/// Business rule: production accepts `result_summary_length = 0` (empty
/// prefix plus artifact footer) and `summary > threshold` (the summary can be
/// wider than the spill threshold).  Both states are valid and preserved
/// byte-identically.
///
/// Forbidden invalid state: none; any `usize` is representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultSummaryWidth {
    /// No inline prefix is rendered; only the artifact footer.
    Empty,
    /// Inline prefix is rendered with this positive byte width.
    Limited(NonZeroUsize),
}

impl ResultSummaryWidth {
    pub fn get(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Limited(n) => n.get(),
        }
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.get(), TruncateMarker::None)
    }
}

/// A truncated summary with a truncation flag.
///
/// Produced by [`ResultSpillBudget::truncate_to_summary`]. Implements
/// [`std::fmt::Display`] so the truncated text is available via `.to_string()`
/// and the truncation flag via [`was_truncated`](Self::was_truncated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedSummary {
    text: String,
    was_truncated: bool,
}

impl TruncatedSummary {
    fn new(text: String, was_truncated: bool) -> Self {
        Self {
            text,
            was_truncated,
        }
    }

    pub fn was_truncated(&self) -> bool {
        self.was_truncated
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    pub fn into_string(self) -> String {
        self.text
    }
}

impl std::fmt::Display for TruncatedSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Budget governing worker-result spill to artifacts.
///
/// Business rule: a result either stays inline (when its byte length is at
/// or below the threshold) or is moved to an artifact file with an inline
/// stand-in summary whose byte width is bounded.
///
/// Forbidden invalid state: none; production accepts threshold 0 (spill every
/// non-empty result), summary 0, and summary larger than threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultSpillBudget {
    threshold: ByteThreshold,
    summary_width: ResultSummaryWidth,
}

impl ResultSpillBudget {
    fn from_config(artifact_threshold: usize, summary_length: usize) -> Self {
        Self {
            threshold: ByteThreshold::new(artifact_threshold),
            summary_width: match NonZeroUsize::new(summary_length) {
                Some(n) => ResultSummaryWidth::Limited(n),
                None => ResultSummaryWidth::Empty,
            },
        }
    }

    #[cfg(test)]
    fn test_budget(threshold: usize, summary: usize) -> Self {
        Self::from_config(threshold, summary)
    }

    pub fn threshold(&self) -> ByteThreshold {
        self.threshold
    }

    pub fn summary_width(&self) -> ResultSummaryWidth {
        self.summary_width
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    pub fn decide(&self, text: &str) -> ResultSpillDecision {
        if self.threshold.allows_inline(text) {
            ResultSpillDecision::Inline
        } else {
            let summary = self.truncate_to_summary(text);
            let was_truncated = summary.was_truncated();
            ResultSpillDecision::Spill {
                summary: summary.into_string(),
                result_len: text.len(),
                was_truncated,
            }
        }
    }

    pub fn truncate_to_summary(&self, text: &str) -> TruncatedSummary {
        let (truncated, was_truncated) = safe_truncate(text, self.summary_width.get());
        TruncatedSummary::new(truncated.to_string(), was_truncated)
    }
}

/// Decision produced by [`ResultSpillBudget::decide`].
// Unused S3 API surface.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultSpillDecision {
    /// Text stays inline unchanged.
    Inline,
    /// Text exceeds the decision threshold; render the bounded summary.
    Spill {
        /// Byte-truncated inline summary.
        summary: String,
        /// Byte length of the original text (for labels and markers).
        result_len: usize,
        /// Whether the summary was actually truncated.
        was_truncated: bool,
    },
}

/// Policy for size-based tool-output promotion.
///
/// Business rule: `tool_output_artifact_threshold = 0` promotes **all**
/// outputs, including empty output.  A positive threshold promotes outputs
/// larger than that byte count.
///
/// Forbidden invalid state: none; any `usize` is representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizePromotion {
    /// Promote every output regardless of size.
    PromoteAll,
    /// Promote outputs larger than this byte threshold.
    LargerThan(NonZeroByteThreshold),
}

impl SizePromotion {
    pub fn from_threshold(bytes: usize) -> Self {
        match NonZeroUsize::new(bytes) {
            Some(n) => Self::LargerThan(NonZeroByteThreshold(n)),
            None => Self::PromoteAll,
        }
    }

    pub fn threshold_bytes(&self) -> usize {
        match self {
            Self::PromoteAll => 0,
            Self::LargerThan(threshold) => threshold.0.get(),
        }
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    pub fn qualifies(&self, output: &str) -> bool {
        match self {
            Self::PromoteAll => true,
            Self::LargerThan(threshold) => output.len() > threshold.0.get(),
        }
    }
}

/// Policy for duration-based tool-output promotion.
///
/// Business rule: `tool_output_duration_threshold_ms = 0` disables
/// duration-based promotion.  A positive threshold promotes calls longer
/// than that duration.
///
/// Forbidden invalid state: none; any `u64` millis is representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationPromotion {
    /// Duration-based promotion is disabled.
    Disabled,
    /// Promote calls longer than this duration.
    LongerThan(NonZeroDuration),
}

impl DurationPromotion {
    pub fn from_millis(ms: u64) -> Self {
        if ms == 0 {
            Self::Disabled
        } else {
            Self::LongerThan(NonZeroDuration(Duration::from_millis(ms)))
        }
    }

    pub fn threshold_millis(&self) -> u64 {
        match self {
            Self::Disabled => 0,
            Self::LongerThan(threshold) => threshold.0.as_millis() as u64,
        }
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    pub fn qualifies(&self, duration: Duration) -> bool {
        match self {
            Self::Disabled => false,
            Self::LongerThan(threshold) => duration > threshold.0,
        }
    }
}

/// Budget governing tool-output promotion to artifacts.
///
/// Business rule: a tool output is promoted when it exceeds a byte-size
/// threshold OR a duration threshold.  The two zero semantics are asymmetric:
/// size 0 promotes all; duration 0 disables the duration path.
///
/// Forbidden invalid state: none beyond the inherent non-negativity of the
/// underlying values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolOutputSpillBudget {
    size: SizePromotion,
    duration: DurationPromotion,
}

impl ToolOutputSpillBudget {
    fn from_config(size_threshold: usize, duration_threshold_ms: u64) -> Self {
        Self {
            size: SizePromotion::from_threshold(size_threshold),
            duration: DurationPromotion::from_millis(duration_threshold_ms),
        }
    }

    pub fn size(&self) -> SizePromotion {
        self.size
    }

    pub fn duration(&self) -> DurationPromotion {
        self.duration
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    pub fn size_qualifies(&self, output: &str) -> bool {
        self.size.qualifies(output)
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    pub fn duration_qualifies(&self, duration: Duration) -> bool {
        self.duration.qualifies(duration)
    }
}

// ============================================================================
// Display limits
// ============================================================================

/// Maximum number of tools rendered per worker before `(+N more)`.
///
/// Business rule: the coordinator planning prompt truncates long per-worker
/// tool lists to a bounded count.  `max_tools_per_worker = 0` is the raw
/// display limit; the renderer decides whether that renders the degenerate
/// `(+N more)` list or omits the tool section entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolListLimit {
    /// Render no tools; the display limit is zero.
    ///
    /// Summary rendering shows `(+N more)` for a nonempty list, while Full
    /// rendering omits the tool section when the limit is zero.
    HideAll,
    /// Render at most this many tools before the suffix.
    Limited(NonZeroUsize),
}

impl ToolListLimit {
    pub fn new(count: usize) -> Self {
        match NonZeroUsize::new(count) {
            Some(n) => Self::Limited(n),
            None => Self::HideAll,
        }
    }

    pub fn get(&self) -> usize {
        match self {
            Self::HideAll => 0,
            Self::Limited(n) => n.get(),
        }
    }
}

/// Duplicate-call guard policy.
///
/// Business rule: the guard compares the consecutive identical-call count
/// against the configured thresholds.  A threshold of zero does **not**
/// disable the guard; it makes the guard fire maximally.
///
/// - `block = 0` blocks on the first identical call.
/// - `nudge = 0` nudges from the first identical call.
/// - `nudge >= block` produces block-only behavior (the block check wins).
///
/// `NudgeOnly` is not modeled as a separate variant because production always
/// evaluates a finite block threshold first; every raw `(nudge, block)` pair
/// therefore maps to either [`BlockOnly`](Self::BlockOnly) or
/// [`NudgeThenBlock`](Self::NudgeThenBlock).
///
/// The variant fields use private newtypes ([`NudgeThreshold`], [`BlockThreshold`])
/// so that invalid orderings (`nudge >= block` inside `NudgeThenBlock`) cannot be
/// constructed from outside this module.  Use [`new`](Self::new) or
/// [`BoundingConfig::duplicate_call_policy`] to build a policy.
///
/// Forbidden invalid state: none; every raw `(nudge, block)` pair maps to a
/// reachable policy, and the variants themselves are not directly constructable
/// with invalid orderings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(private_interfaces)]
pub enum DuplicateCallPolicy {
    /// Block after `block` identical calls; no nudge stage.
    BlockOnly {
        /// Number of identical calls before the abort annotation.
        block: BlockThreshold,
    },
    /// Nudge after `nudge` calls, then block after `block` calls.
    NudgeThenBlock {
        /// Number of identical calls before the guidance annotation.
        nudge: NudgeThreshold,
        /// Number of identical calls before the abort annotation.
        block: BlockThreshold,
    },
}

/// Private newtype wrapping the nudge threshold so enum variants cannot be
/// constructed with invalid orderings from outside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NudgeThreshold(usize);

/// Private newtype wrapping the block threshold so enum variants cannot be
/// constructed with invalid orderings from outside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockThreshold(usize);

impl DuplicateCallPolicy {
    fn new(nudge: usize, block: usize) -> Self {
        if nudge >= block {
            Self::BlockOnly {
                block: BlockThreshold(block),
            }
        } else {
            Self::NudgeThenBlock {
                nudge: NudgeThreshold(nudge),
                block: BlockThreshold(block),
            }
        }
    }

    #[cfg(test)]
    fn test_policy(nudge: usize, block: usize) -> Self {
        Self::new(nudge, block)
    }

    pub fn nudge_threshold(&self) -> Option<usize> {
        match self {
            Self::BlockOnly { .. } => None,
            Self::NudgeThenBlock { nudge, .. } => Some(nudge.0),
        }
    }

    pub fn block_threshold(&self) -> usize {
        match self {
            Self::BlockOnly { block } => block.0,
            Self::NudgeThenBlock { block, .. } => block.0,
        }
    }
}

// ============================================================================
// Session history limit
// ============================================================================

/// Bound on prior-run manifests injected into the coordinator preamble.
///
/// Business rule: session history is either disabled or bounded to a
/// positive number of most-recent manifests.
///
/// Forbidden invalid state: an unbounded injection limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHistoryLimit {
    /// Session history injection is disabled.
    Disabled,
    /// At most this many prior manifests are injected.
    Limited(NonZeroUsize),
}

impl SessionHistoryLimit {
    pub fn new(count: usize) -> Self {
        match NonZeroUsize::new(count) {
            Some(n) => Self::Limited(n),
            None => Self::Disabled,
        }
    }

    pub fn get(&self) -> usize {
        match self {
            Self::Disabled => 0,
            Self::Limited(n) => n.get(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Limited(_))
    }
}

// ============================================================================
// Character caps
// ============================================================================

/// Character cap for failure-history task handles.
///
/// Business rule: a failed task's identity handle is its first line capped
/// at a fixed width, plus a truncation marker when cut.
///
/// Forbidden invalid state: a zero cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureHandleWidth(CharWidth);

impl FailureHandleWidth {
    /// Default cap matching the accepted baseline binary.
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(120) {
        Some(n) => CharWidth(n),
        None => panic!("fixed failure-handle cap must be non-zero"),
    });

    /// Truncate to the cap, returning the text without marker and whether
    /// anything was cut.  The caller owns the marker decision.
    pub fn truncate_with_flag(&self, text: &str) -> (String, bool) {
        match text.char_indices().nth(self.0.get()) {
            None => (text.to_string(), false),
            Some((cut, _)) => (text[..cut].to_string(), true),
        }
    }

    // Unused S3 API surface.
    #[allow(dead_code)]
    fn truncate(&self, text: &str) -> String {
        truncate_chars(text, self.0.get(), TruncateMarker::None)
    }
}

/// Character cap for failed-task error previews.
///
/// Business rule: failure entries show a bounded error preview with an
/// explicit `[truncated]` marker, never an unbounded error body.
///
/// Forbidden invalid state: a zero cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPreviewWidth(CharWidth);

impl ErrorPreviewWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(2000) {
        Some(n) => CharWidth(n),
        None => panic!("fixed error-preview cap must be non-zero"),
    });

    /// Truncate to the cap, returning the text without marker and whether
    /// anything was cut.  The caller owns the marker decision.
    pub fn truncate_with_flag(&self, text: &str) -> (String, bool) {
        match text.char_indices().nth(self.0.get()) {
            None => (text.to_string(), false),
            Some((cut, _)) => (text[..cut].to_string(), true),
        }
    }

    pub fn truncate(&self, text: &str) -> String {
        truncate_chars(text, self.0.get(), TruncateMarker::None)
    }
}

/// Character cap for tool-reasoning previews in continuation prompts.
///
/// Business rule: the `_aura_reasoning` text string forwarded into
/// continuation prompts is truncated to a bounded character width with an
/// ellipsis marker.  This cap applies to the reasoning text, not to a
/// reasoning-token budget.
///
/// Forbidden invalid state: a zero cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReasoningWidth(CharWidth);

impl ToolReasoningWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(100) {
        Some(n) => CharWidth(n),
        None => panic!("fixed tool-reasoning cap must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_chars(text, self.0.get(), TruncateMarker::EllipsisChar)
    }
}

// ============================================================================
// Byte-bounded observability previews
// ============================================================================

/// Byte width for routing-rationale log previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutingRationaleWidth(ByteWidth);

impl RoutingRationaleWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(80) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed routing-rationale width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::Dots)
    }
}

/// Byte width for task-description log previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskDescriptionLogWidth(ByteWidth);

impl TaskDescriptionLogWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(100) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed task-description log width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::None)
    }
}

/// Byte width for task-description tracing-span previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskDescriptionSpanWidth(ByteWidth);

impl TaskDescriptionSpanWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(200) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed task-description span width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::None)
    }
}

/// Byte width for goal/span previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalWidth(ByteWidth);

impl GoalWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(200) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed goal width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::None)
    }
}

/// Byte width for raw routing-response previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutingResponseWidth(ByteWidth);

impl RoutingResponseWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(300) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed routing-response width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::None)
    }
}

/// Byte width for query-log previews.
///
/// Business rule: the fallback-plan query log preview truncates the original
/// query for a tracing line, not for plan content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryLogWidth(ByteWidth);

impl QueryLogWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(100) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed query-log width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::None)
    }
}

/// Collection of byte widths used for observability/tracing string previews.
///
/// Business rule: every truncation of a query, task description, model
/// response, or routing rationale in logs/tracing uses a width from this
/// single table.
///
/// Forbidden invalid state: any preview width equal to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogPreviewWidths {
    routing_rationale: RoutingRationaleWidth,
    task_description_log: TaskDescriptionLogWidth,
    task_description_span: TaskDescriptionSpanWidth,
    goal: GoalWidth,
    routing_response: RoutingResponseWidth,
    query_log: QueryLogWidth,
}

impl LogPreviewWidths {
    fn default_widths() -> Self {
        Self {
            routing_rationale: RoutingRationaleWidth::DEFAULT,
            task_description_log: TaskDescriptionLogWidth::DEFAULT,
            task_description_span: TaskDescriptionSpanWidth::DEFAULT,
            goal: GoalWidth::DEFAULT,
            routing_response: RoutingResponseWidth::DEFAULT,
            query_log: QueryLogWidth::DEFAULT,
        }
    }

    pub fn routing_rationale(&self) -> RoutingRationaleWidth {
        self.routing_rationale
    }

    pub fn task_description_log(&self) -> TaskDescriptionLogWidth {
        self.task_description_log
    }

    pub fn task_description_span(&self) -> TaskDescriptionSpanWidth {
        self.task_description_span
    }

    pub fn goal(&self) -> GoalWidth {
        self.goal
    }

    pub fn routing_response(&self) -> RoutingResponseWidth {
        self.routing_response
    }

    pub fn query_log(&self) -> QueryLogWidth {
        self.query_log
    }
}

// ============================================================================
// Byte-bounded manifest previews
// ============================================================================

/// Byte width for task-summary result previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultPreviewWidth(ByteWidth);

impl ResultPreviewWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(200) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed result-preview width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::None)
    }
}

/// Byte width for manifest response summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseSummaryWidth(ByteWidth);

impl ResponseSummaryWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(200) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed response-summary width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::None)
    }
}

/// Collection of byte widths used for persisted manifest string previews.
///
/// Business rule: `TaskSummary.result_preview` and manifest `response_summary`
/// are written to the persisted run manifest, so they live outside the
/// observability log-preview table.
///
/// Forbidden invalid state: any preview width equal to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestWidths {
    result_preview: ResultPreviewWidth,
    response_summary: ResponseSummaryWidth,
}

impl ManifestWidths {
    fn default_widths() -> Self {
        Self {
            result_preview: ResultPreviewWidth::DEFAULT,
            response_summary: ResponseSummaryWidth::DEFAULT,
        }
    }

    pub fn result_preview(&self) -> ResultPreviewWidth {
        self.result_preview
    }

    pub fn response_summary(&self) -> ResponseSummaryWidth {
        self.response_summary
    }
}

// ============================================================================
// Byte-bounded plan-content truncation
// ============================================================================

/// Byte width for fallback plan task descriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanTaskDescriptionWidth(ByteWidth);

impl PlanTaskDescriptionWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(100) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed plan task-description width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::Dots)
    }
}

/// Byte width for converted direct-answer plan tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanDirectAnswerTaskWidth(ByteWidth);

impl PlanDirectAnswerTaskWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(80) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed plan direct-answer task width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::Dots)
    }
}

/// Byte width for converted direct-answer plan rationales.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanDirectAnswerRationaleWidth(ByteWidth);

impl PlanDirectAnswerRationaleWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(100) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed plan direct-answer rationale width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::Dots)
    }
}

/// Byte width for converted clarification plan tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanClarificationTaskWidth(ByteWidth);

impl PlanClarificationTaskWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(80) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed plan clarification task width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::Dots)
    }
}

/// Byte width for converted clarification plan questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanClarificationQuestionWidth(ByteWidth);

impl PlanClarificationQuestionWidth {
    pub const DEFAULT: Self = Self(match NonZeroUsize::new(100) {
        Some(n) => ByteWidth(n),
        None => panic!("fixed plan clarification question width must be non-zero"),
    });

    pub fn truncate(&self, text: &str) -> String {
        truncate_bytes(text, self.0.get(), TruncateMarker::Dots)
    }
}

/// Collection of byte widths used for plan-content string truncation.
///
/// Business rule: plan content (`Task.description`, routing rationales, and
/// config-converted plan fields) reaches persisted manifests and worker
/// prompts, so it is separated from observability log previews.
///
/// Forbidden invalid state: any truncation width equal to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanContentWidths {
    task_description: PlanTaskDescriptionWidth,
    direct_answer_task: PlanDirectAnswerTaskWidth,
    direct_answer_rationale: PlanDirectAnswerRationaleWidth,
    clarification_task: PlanClarificationTaskWidth,
    clarification_question: PlanClarificationQuestionWidth,
}

impl PlanContentWidths {
    fn default_widths() -> Self {
        Self {
            task_description: PlanTaskDescriptionWidth::DEFAULT,
            direct_answer_task: PlanDirectAnswerTaskWidth::DEFAULT,
            direct_answer_rationale: PlanDirectAnswerRationaleWidth::DEFAULT,
            clarification_task: PlanClarificationTaskWidth::DEFAULT,
            clarification_question: PlanClarificationQuestionWidth::DEFAULT,
        }
    }

    pub fn task_description(&self) -> PlanTaskDescriptionWidth {
        self.task_description
    }

    pub fn direct_answer_task(&self) -> PlanDirectAnswerTaskWidth {
        self.direct_answer_task
    }

    pub fn direct_answer_rationale(&self) -> PlanDirectAnswerRationaleWidth {
        self.direct_answer_rationale
    }

    pub fn clarification_task(&self) -> PlanClarificationTaskWidth {
        self.clarification_task
    }

    pub fn clarification_question(&self) -> PlanClarificationQuestionWidth {
        self.clarification_question
    }
}

// ============================================================================
// Token-based scratchpad budget
// ============================================================================

/// Token-based scratchpad budget attached to a worker agent.
///
/// Business rule: a scratchpad budget exists only when scratchpad tools are
/// wired for a worker; the budget is scoped to that worker's effective LLM.
///
/// `ContextBudget.context_window` is private and `usable_budget() == 0` cannot
/// distinguish a zero context window from a small window rounded to zero after
/// the safety margin, so the wrapper is infallible.
// Unused S3 API surface.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ScratchpadBudget(crate::scratchpad::ContextBudget);

// Unused S3 API surface.
#[allow(dead_code)]
impl ScratchpadBudget {
    pub fn new(budget: crate::scratchpad::ContextBudget) -> Self {
        Self(budget)
    }

    pub fn inner(&self) -> &crate::scratchpad::ContextBudget {
        &self.0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_width_emoji_boundary() {
        let width = RoutingRationaleWidth(ByteWidth::new(8).unwrap());
        let got = width.truncate("Hello 🎉 World");
        assert_eq!(got, "Hello ...");
    }

    #[test]
    fn char_width_cjk_boundary() {
        let width = FailureHandleWidth(CharWidth::new(2).unwrap());
        assert_eq!(width.truncate("日本語テスト"), "日本");
    }

    #[test]
    fn result_spill_decide_branches() {
        let budget = ResultSpillBudget::test_budget(10, 5);
        assert_eq!(budget.decide("short"), ResultSpillDecision::Inline);

        let text = "this text is longer than ten bytes";
        match budget.decide(text) {
            ResultSpillDecision::Spill {
                summary,
                result_len,
                was_truncated,
            } => {
                assert_eq!(result_len, text.len());
                assert_eq!(summary, "this ");
                assert!(was_truncated);
            }
            _ => panic!("expected Spill"),
        }

        let zero_threshold = ResultSpillBudget::test_budget(0, 0);
        match zero_threshold.decide("x") {
            ResultSpillDecision::Spill {
                summary,
                was_truncated,
                ..
            } => {
                assert!(summary.is_empty());
                assert!(was_truncated);
            }
            _ => panic!("expected Spill"),
        }

        let wide_summary = ResultSpillBudget::test_budget(5, 100);
        assert_eq!(wide_summary.summary_width().get(), 100);
        let long = "a".repeat(10);
        match wide_summary.decide(long.as_str()) {
            ResultSpillDecision::Spill {
                summary,
                was_truncated,
                ..
            } => {
                assert_eq!(summary, long);
                assert!(!was_truncated);
            }
            _ => panic!("expected Spill"),
        }
    }

    #[test]
    fn duplicate_call_policy_collapse() {
        let eq = DuplicateCallPolicy::test_policy(5, 5);
        assert!(matches!(eq, DuplicateCallPolicy::BlockOnly { .. }));
        assert_eq!(eq.nudge_threshold(), None);
        assert_eq!(eq.block_threshold(), 5);

        let gt = DuplicateCallPolicy::test_policy(7, 3);
        assert!(matches!(gt, DuplicateCallPolicy::BlockOnly { .. }));
        assert_eq!(gt.nudge_threshold(), None);
        assert_eq!(gt.block_threshold(), 3);

        let nudge = DuplicateCallPolicy::test_policy(3, 5);
        assert!(matches!(nudge, DuplicateCallPolicy::NudgeThenBlock { .. }));
        assert_eq!(nudge.nudge_threshold(), Some(3));
        assert_eq!(nudge.block_threshold(), 5);
    }

    #[test]
    fn session_history_limit_states() {
        let disabled = SessionHistoryLimit::new(0);
        assert!(matches!(disabled, SessionHistoryLimit::Disabled));
        assert!(!disabled.is_enabled());
        assert_eq!(disabled.get(), 0);

        let limited = SessionHistoryLimit::new(3);
        assert!(matches!(limited, SessionHistoryLimit::Limited(n) if n.get() == 3));
        assert!(limited.is_enabled());
        assert_eq!(limited.get(), 3);
    }

    #[test]
    fn truncate_marker_styles() {
        let reasoning = ToolReasoningWidth::DEFAULT.truncate("a".repeat(101).as_str());
        assert!(reasoning.ends_with('…'));
        assert_eq!(reasoning.chars().count(), 101);

        let rationale = RoutingRationaleWidth::DEFAULT.truncate("a".repeat(81).as_str());
        assert!(rationale.ends_with("..."));
        assert_eq!(rationale.len(), 83);

        let goal = GoalWidth::DEFAULT.truncate("a".repeat(201).as_str());
        assert!(!goal.ends_with("...") && !goal.ends_with('…'));
        assert_eq!(goal.len(), 200);
    }

    #[test]
    fn truncate_marker_suppressed_when_not_cut() {
        assert_eq!(RoutingRationaleWidth::DEFAULT.truncate("short"), "short");
        assert_eq!(ToolReasoningWidth::DEFAULT.truncate("short"), "short");
    }

    #[test]
    fn truncated_summary_display() {
        let budget = ResultSpillBudget::test_budget(100, 5);

        let long = budget.truncate_to_summary("hello world");
        assert_eq!(long.to_string(), "hello");
        assert!(long.was_truncated());

        let short = budget.truncate_to_summary("hi");
        assert_eq!(short.to_string(), "hi");
        assert!(!short.was_truncated());
    }
}
