//! Failure-history identity and records (`ARCHITECTURE.md` section 1.5).

use super::error::ContextError;
use super::evidence::ErrorPreview;
use super::label::{IterationNumber, WorkerRole};
use super::rendered::RenderedContext;
use crate::orchestration::types::FailureCategory;

/// Stable identity for a failed task: the first line of its description,
/// hard-capped at 120 characters with a trailing `...` marker when cut.
///
/// The handle is derived once, at record time, so a re-issued identical
/// task produces an identical handle and repeat detection still fires
/// (`ARCHITECTURE.md` section 1.5). The full imperative task text stays out
/// of the continuation context entirely.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FailureHandle(String);

impl FailureHandle {
    /// Hard cap on handle length, in characters.
    pub const MAX_CHARS: usize = 120;

    /// Marker appended when truncation cut the first line.
    pub const TRUNCATION_MARKER: &'static str = "...";

    /// Derive the handle from a coordinator task description: first line,
    /// capped at [`Self::MAX_CHARS`], marked when cut.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyFailureDescription`] when the
    /// description is empty or whitespace-only.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: truncation body lands with the implementation cards"
    )]
    pub fn from_description(description: &str) -> Result<Self, ContextError> {
        todo!()
    }

    /// The handle text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One accumulated failure-history record, rendered as
/// `- Iteration {n}: "{handle}" (worker: w) - [{category}] {error preview}`.
///
/// The record stores a [`FailureHandle`], not the raw description, so the
/// display identity and the repeat-detection grouping key share one
/// truncation applied at record time (`ARCHITECTURE.md` section 1.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureRecord {
    /// Which iteration the failure occurred in.
    pub iteration: IterationNumber,
    /// Truncated identity of the failed task.
    pub handle: FailureHandle,
    /// Worker the task was dispatched to, when one was assigned.
    pub worker: Option<WorkerRole>,
    /// Structured failure classification.
    pub category: FailureCategory,
    /// Bounded error preview.
    pub error: ErrorPreview,
}

impl FailureRecord {
    /// The repeated-failure grouping key: (handle, category). Two records
    /// group together only when both match, so the same task failing under
    /// different categories is not flagged as a repeat.
    pub fn repeat_key(&self) -> (&FailureHandle, FailureCategory) {
        (&self.handle, self.category)
    }

    /// Render the record for the `FAILURE HISTORY:` section.
    pub fn render(&self) -> RenderedContext {
        todo!()
    }
}
