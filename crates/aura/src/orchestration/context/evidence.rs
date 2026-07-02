//! Evidence-framed per-task entries (`ARCHITECTURE.md` sections 1.3-1.4
//! and 3.3).
//!
//! Completed, failed, and blocked entries carry a correlation label and the
//! worker's own reported evidence. None of these types has a field that can
//! hold the coordinator's task-description text, so the confirmed blur
//! mechanism — replaying imperative instructions next to worker evidence —
//! is unrepresentable.

use super::error::ContextError;
use super::label::{Attestation, CorrelationLabel};
use super::rendered::RenderedContext;
use crate::orchestration::types::FailureCategory;

/// Worker-authored result text that renders inline in an entry.
///
/// This is the full result when it fits under the artifact threshold, or a
/// budget-bounded prefix of it when the worker frame degrades an entry
/// (`evidence/aura-runtime-findings.md` section 8). It never carries a spill
/// footer: a spilled result must parse as [`EvidenceEntry::ArtifactPointer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceText(String);

impl EvidenceText {
    /// Parse inline evidence text.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyEvidenceText`] when the text is empty or
    /// whitespace-only, and [`ContextError::InlineEvidenceCarriesSpillFooter`]
    /// when the text contains the `[Full result (` spill footer.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(text: &str) -> Result<Self, ContextError> {
        todo!()
    }

    /// The inline evidence text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A bounded preview of a raw worker result, standing in when the worker
/// attested no summary.
///
/// The bound itself is applied upstream at spill or budget time; the type
/// records that the text is a preview, not the full result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultPreview(String);

impl ResultPreview {
    /// Parse a result preview.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyResultPreview`] when the text is empty
    /// or whitespace-only.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(text: &str) -> Result<Self, ContextError> {
        todo!()
    }

    /// The preview text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Pointer to a worker result spilled to an artifact file.
///
/// `Display` renders today's footer format verbatim —
/// `[Full result (N chars) saved to artifact: FILE]` — which the
/// architecture keeps unchanged (`ARCHITECTURE.md` sections 1.3 and 6) and
/// which `extract_artifact_footer` keys on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpilledArtifact {
    filename: String,
    full_chars: usize,
}

impl SpilledArtifact {
    /// Parse a spilled-result pointer from its artifact filename and the
    /// full result length in characters.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyArtifactFilename`] when the filename is
    /// empty or whitespace-only.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(filename: &str, full_chars: usize) -> Result<Self, ContextError> {
        todo!()
    }

    /// The artifact filename, readable via `read_artifact`.
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

impl std::fmt::Display for SpilledArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[Full result ({} chars) saved to artifact: {}]",
            self.full_chars, self.filename
        )
    }
}

/// One artifact inventory line for a completed task.
///
/// `Display` renders today's inventory format verbatim —
/// `[Artifact: FILE (N bytes)]` — preserved unchanged as the coordinator's
/// index of `read_artifact` targets (`ARCHITECTURE.md` section 1.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    filename: String,
    bytes: u64,
}

impl ArtifactRef {
    /// Parse an artifact inventory reference.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyArtifactFilename`] when the filename is
    /// empty or whitespace-only.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn new(filename: &str, bytes: u64) -> Result<Self, ContextError> {
        todo!()
    }

    /// The artifact filename, readable via `read_artifact`.
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

impl std::fmt::Display for ArtifactRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[Artifact: {} ({} bytes)]", self.filename, self.bytes)
    }
}

/// What stands in for a spilled result body next to its artifact pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactStandIn {
    /// The worker's own attested summary, used whenever `submit_result` ran.
    Attested(Attestation),
    /// A bounded preview of the raw result, used when the worker attested
    /// nothing (`ARCHITECTURE.md` section 3.3, "otherwise a bounded raw
    /// preview plus footer").
    Preview(ResultPreview),
}

/// The worker-evidence payload of one per-task entry.
///
/// The three variants are the three renderings the architecture allows for
/// completed work: inline text, a stand-in plus artifact pointer, or an
/// attested summary alone (`ARCHITECTURE.md` sections 1.3 and 3.3). Every
/// variant is worker-reported; a spilled result cannot render without its
/// pointer, and a pointer cannot render without a stand-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceEntry {
    /// Worker result text inlined in the entry, with the worker's
    /// attestation when `submit_result` ran.
    InlineResult {
        /// The worker's own result text.
        result: EvidenceText,
        /// The worker's `submit_result` claim, when one exists.
        attestation: Option<Attestation>,
    },
    /// The result spilled to an artifact: a distilled stand-in plus the
    /// pointer to the full result.
    ArtifactPointer {
        /// The distilled body shown next to the pointer.
        stand_in: ArtifactStandIn,
        /// Pointer to the spilled full result.
        artifact: SpilledArtifact,
    },
    /// An attested summary alone, with no spilled body to point at. This is
    /// the budget-degrade path for a result that fit inline but lost its
    /// body to the frame budget (`evidence/aura-runtime-findings.md`
    /// section 8).
    SummaryOnly {
        /// The worker's `submit_result` claim.
        attestation: Attestation,
    },
}

impl EvidenceEntry {
    /// Parse a completed worker result into its evidence entry.
    ///
    /// Classifies by the presence of the spill footer in `result_text`: a
    /// footered result becomes [`EvidenceEntry::ArtifactPointer`] (attested
    /// summary preferred, bounded preview otherwise); anything else becomes
    /// [`EvidenceEntry::InlineResult`].
    ///
    /// # Errors
    ///
    /// Propagates the constructor errors of the selected variant's fields.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: parsing body lands with the implementation cards"
    )]
    pub fn from_completed_result(
        result_text: &str,
        attestation: Option<Attestation>,
    ) -> Result<Self, ContextError> {
        todo!()
    }
}

/// Truncated error text with an explicit truncation marker.
///
/// Failure entries and failure-history records show a bounded error
/// preview, never an unbounded error body (`ARCHITECTURE.md` sections 1.3
/// and 1.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorPreview {
    text: String,
    truncated: bool,
}

impl ErrorPreview {
    /// Hard cap on preview length, in characters. Matches the default
    /// `result_summary_length` width that today's renderer reuses for error
    /// truncation; owning the bound here decouples error width from that
    /// config knob (R2 gate decision Q6).
    pub const MAX_CHARS: usize = 2000;

    /// Truncate raw error text to at most [`Self::MAX_CHARS`] characters,
    /// recording whether anything was cut.
    #[expect(
        unused_variables,
        reason = "R2 type skeleton: truncation body lands with the implementation cards"
    )]
    pub fn new(raw_error: &str) -> Self {
        todo!()
    }

    /// The bounded error text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether truncation cut the original error text.
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }
}

/// A completed task's continuation entry: correlation label, worker
/// evidence, and the artifact inventory.
///
/// Renders as `- Task {id} ({role}, confidence: {c})` followed by the
/// indented evidence and `[Artifact: ...]` inventory lines
/// (`ARCHITECTURE.md` sections 1.3 and 1.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedEntry {
    /// Task id and worker role; no instruction text.
    pub label: CorrelationLabel,
    /// The worker's own reported evidence.
    pub evidence: EvidenceEntry,
    /// Artifact inventory lines for this task; empty when the task produced
    /// no artifacts.
    pub artifacts: Vec<ArtifactRef>,
}

impl CompletedEntry {
    /// Render the entry for the `COMPLETED TASKS:` section.
    pub fn render(&self) -> RenderedContext {
        todo!()
    }
}

/// Why a failed task failed, as reported in its continuation entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureReport {
    /// Hard failure: structured category plus a bounded error preview
    /// (`ARCHITECTURE.md` section 1.3, failed-task rendering).
    Hard {
        /// Structured failure classification.
        category: FailureCategory,
        /// Bounded error preview.
        error: ErrorPreview,
    },
    /// Soft failure: the worker submitted a result but reported it could
    /// not produce one. Keeps today's rendering — summary, confidence, and
    /// any artifact footer (`ARCHITECTURE.md` section 1.3, soft failures).
    Soft {
        /// The worker's `submit_result` claim.
        attestation: Attestation,
        /// Pointer to a spilled body, when one exists.
        artifact: Option<SpilledArtifact>,
    },
}

/// A failed task's continuation entry.
///
/// Renders as `- Task {id} ({role}) -> failed [{category}]: {error}` with
/// no task description (`ARCHITECTURE.md` section 1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedEntry {
    /// Task id and worker role; no instruction text.
    pub label: CorrelationLabel,
    /// The structured failure report.
    pub report: FailureReport,
}

impl FailedEntry {
    /// Render the entry for the `FAILED TASKS:` section.
    pub fn render(&self) -> RenderedContext {
        todo!()
    }
}

/// A blocked task's continuation entry: a dependency failed, so the task
/// never ran and has no evidence.
///
/// Renders as `- Task {id} ({role}) -> blocked (dependency failed)`
/// (`ARCHITECTURE.md` section 1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedEntry {
    /// Task id and worker role; no instruction text.
    pub label: CorrelationLabel,
}

impl BlockedEntry {
    /// Render the entry for the `BLOCKED TASKS:` section.
    pub fn render(&self) -> RenderedContext {
        todo!()
    }
}
