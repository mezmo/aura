//! Failure-history identity and records (`ARCHITECTURE.md` section 1.5).

use super::error::ContextError;
use super::evidence::ErrorPreview;
use super::label::{IterationNumber, WorkerRole};
use super::rendered::RenderedContext;
use crate::orchestration::types::FailureCategory;

/// Stable identity for a failed task: the first line of its description,
/// cut at 120 characters, with a trailing `...` marker appended after the
/// cut (R2 gate decision Q4: the marker sits outside the cap, so a cut
/// handle is `MAX_CHARS` plus the marker, identically in display and in
/// the grouping key).
///
/// The handle is derived once, at record time, so a re-issued identical
/// task produces an identical handle and repeat detection still fires
/// (`ARCHITECTURE.md` section 1.5). The full imperative task text stays out
/// of the continuation context entirely.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FailureHandle(String);

impl FailureHandle {
    /// Hard cap on the retained first-line text, in characters; the
    /// truncation marker is appended after this cap.
    pub const MAX_CHARS: usize = 120;

    /// Marker appended after the cap when truncation cut the first line.
    pub const TRUNCATION_MARKER: &'static str = "...";

    /// Derive the handle from a coordinator task description: first line,
    /// capped at [`Self::MAX_CHARS`], marked when cut. Leading whitespace
    /// carries no identity, so the first line is taken after trimming the
    /// start of the description.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyFailureDescription`] when the
    /// description is empty or whitespace-only.
    pub fn from_description(description: &str) -> Result<Self, ContextError> {
        let first_line = description
            .trim_start()
            .lines()
            .next()
            .unwrap_or("")
            .trim_end();
        if first_line.is_empty() {
            return Err(ContextError::EmptyFailureDescription);
        }
        // R2 gate decision 4: the marker is appended after the cap, so a
        // cut handle is MAX_CHARS plus the marker, identically in display
        // and in the grouping key.
        match first_line.char_indices().nth(Self::MAX_CHARS) {
            Some((cut, _)) => Ok(Self(format!(
                "{}{}",
                &first_line[..cut],
                Self::TRUNCATION_MARKER
            ))),
            None => Ok(Self(first_line.to_owned())),
        }
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
        let worker = self
            .worker
            .as_ref()
            .map(|worker| format!(" (worker: {worker})"))
            .unwrap_or_default();
        RenderedContext::new(format!(
            "- Iteration {}: \"{}\"{} - [{}] {}",
            self.iteration,
            self.handle.as_str(),
            worker,
            self.category,
            self.error
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // R2 gate decision 4: the `...` marker is appended after the
    // 120-character cut, so a cut handle is MAX_CHARS plus the marker,
    // identically in display and in the grouping key.
    #[test]
    fn cut_handle_appends_marker_after_cap() {
        let description = "d".repeat(200);
        let handle = FailureHandle::from_description(&description).expect("non-empty");
        assert_eq!(
            handle.as_str().chars().count(),
            FailureHandle::MAX_CHARS + FailureHandle::TRUNCATION_MARKER.chars().count(),
            "cut handle is the cap plus the marker"
        );
        assert!(handle.as_str().ends_with(FailureHandle::TRUNCATION_MARKER));
        assert!(
            handle
                .as_str()
                .starts_with(&description[..FailureHandle::MAX_CHARS]),
            "the retained text is the first MAX_CHARS characters"
        );

        // Display identity and grouping identity come from one truncation:
        // a re-issued identical description produces an identical handle.
        let reissued = FailureHandle::from_description(&description).expect("non-empty");
        assert_eq!(handle, reissued);

        // At or under the cap, nothing is cut and nothing is marked.
        let exact = "e".repeat(FailureHandle::MAX_CHARS);
        let uncut = FailureHandle::from_description(&exact).expect("non-empty");
        assert_eq!(uncut.as_str(), exact);
    }

    #[test]
    fn handle_is_first_line_of_description() {
        let handle = FailureHandle::from_description(
            "Install QEMU and launch Windows 3.11 with full configuration.\nRun these steps in order: (1) install qemu-system-i386",
        )
        .expect("non-empty");
        assert_eq!(
            handle.as_str(),
            "Install QEMU and launch Windows 3.11 with full configuration."
        );

        assert_eq!(
            FailureHandle::from_description("  \n\t"),
            Err(ContextError::EmptyFailureDescription)
        );
    }

    #[test]
    fn record_renders_handle_worker_category_and_preview() {
        let record = FailureRecord {
            iteration: IterationNumber::new(2).expect("valid iteration"),
            handle: FailureHandle::from_description("Gather logs").expect("non-empty"),
            worker: Some(WorkerRole::new("operations").expect("non-empty")),
            category: FailureCategory::AgentTimeout,
            error: ErrorPreview::new("Timeout contacting service"),
        };
        assert_eq!(
            record.render().as_str(),
            "- Iteration 2: \"Gather logs\" (worker: operations) - [agent_timeout] Timeout contacting service"
        );

        let (handle, category) = record.repeat_key();
        assert_eq!(handle.as_str(), "Gather logs");
        assert_eq!(category, FailureCategory::AgentTimeout);
    }
}
