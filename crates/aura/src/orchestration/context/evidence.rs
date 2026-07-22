//! Evidence-framed per-task entries (`ARCHITECTURE.md` sections 1.3-1.4
//! and 3.3).
//!
//! Completed, failed, and blocked entries carry a correlation label and the
//! worker's own reported evidence. None of these types has a field that can
//! hold the coordinator's task-description text, so the confirmed blur
//! mechanism — replaying imperative instructions next to worker evidence —
//! is unrepresentable.

use super::error::ContextError;
use super::label::{CorrelationLabel, WorkerClaim, WorkerRole};
use super::named_check::NamedCheck;
use super::rendered::RenderedContext;
pub use crate::orchestration::persistence::{ArtifactRef, SpilledArtifact};
use crate::orchestration::tools::submit_result::Confidence;
use crate::orchestration::types::FailureCategory;

/// Format the parenthesized suffix of a per-task entry line: worker role
/// and, for completed entries, the worker's stated confidence. Renders
/// nothing when neither exists, so an unassigned task shows a bare id.
fn label_suffix(worker: Option<&WorkerRole>, confidence: Option<Confidence>) -> String {
    match (worker, confidence) {
        (Some(worker), Some(confidence)) => format!(" ({worker}, confidence: {confidence})"),
        (Some(worker), None) => format!(" ({worker})"),
        (None, Some(confidence)) => format!(" (confidence: {confidence})"),
        (None, None) => String::new(),
    }
}

/// Indent each line of worker-reported text by 4 spaces for nesting under
/// the entry's label line.
pub(super) fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

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
    /// when the text carries a well-formed trailing spill footer.
    pub fn new(text: &str) -> Result<Self, ContextError> {
        if text.trim().is_empty() {
            return Err(ContextError::EmptyEvidenceText);
        }
        if SpilledArtifact::parse_trailing(text).is_some() {
            return Err(ContextError::InlineEvidenceCarriesSpillFooter);
        }
        Ok(Self(text.to_owned()))
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
    pub fn new(text: &str) -> Result<Self, ContextError> {
        if text.trim().is_empty() {
            return Err(ContextError::EmptyResultPreview);
        }
        Ok(Self(text.to_owned()))
    }

    /// The preview text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What stands in for a spilled result body next to its artifact pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactStandIn {
    /// The worker's own attested summary, used whenever `submit_result` ran.
    Claim(WorkerClaim),
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
    /// claim when `submit_result` ran.
    InlineResult {
        /// The worker's own result text.
        result: EvidenceText,
        /// The worker's `submit_result` claim, when one exists.
        claim: Option<WorkerClaim>,
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
        claim: WorkerClaim,
    },
    /// A spilled result whose prefix before the artifact footer is
    /// whitespace-only. The footer pointer is preserved and the render
    /// notes that no inline preview is available.
    ArtifactPointerOnly {
        /// Pointer to the spilled full result.
        artifact: SpilledArtifact,
    },
}

impl EvidenceEntry {
    /// Parse a completed worker result into its evidence entry.
    ///
    /// Classifies by the presence of the spill footer in `result_text`: a
    /// footered result becomes [`EvidenceEntry::ArtifactPointer`] (attested
    /// summary preferred, bounded preview otherwise); anything else becomes
    /// [`EvidenceEntry::InlineResult`]. The classification is exclusive: the
    /// same text can never parse as both, because [`EvidenceText::new`]
    /// rejects exactly what the footer parser accepts (R2 gate decision 5).
    ///
    /// A footered result whose prefix is whitespace-only becomes
    /// [`EvidenceEntry::ArtifactPointerOnly`] so the pointer is not lost.
    ///
    /// # Errors
    ///
    /// Propagates the constructor errors of the selected variant's fields.
    pub fn from_completed_result(
        result_text: &str,
        claim: Option<WorkerClaim>,
    ) -> Result<Self, ContextError> {
        match SpilledArtifact::parse_trailing_with_offset(result_text) {
            Some((start, artifact)) => {
                let prefix = result_text[..start].trim_end();
                if prefix.is_empty() {
                    return Ok(Self::spilled_no_preview(artifact));
                }
                let stand_in = match claim {
                    Some(claim) => ArtifactStandIn::Claim(claim),
                    // The spill path stores a bounded preview ahead of the
                    // footer; that prefix is the stand-in when no claim
                    // exists.
                    None => ArtifactStandIn::Preview(ResultPreview::new(prefix)?),
                };
                Ok(Self::ArtifactPointer { stand_in, artifact })
            }
            None => Ok(Self::InlineResult {
                result: EvidenceText::new(result_text)?,
                claim,
            }),
        }
    }

    /// Build an artifact-pointer entry with no inline preview.
    pub fn spilled_no_preview(artifact: SpilledArtifact) -> Self {
        Self::ArtifactPointerOnly { artifact }
    }

    /// The worker's claim carried by this evidence, if any.
    pub(super) fn claim(&self) -> Option<&WorkerClaim> {
        match self {
            Self::InlineResult { claim, .. } => claim.as_ref(),
            Self::ArtifactPointer { stand_in, .. } => match stand_in {
                ArtifactStandIn::Claim(claim) => Some(claim),
                ArtifactStandIn::Preview(_) => None,
            },
            Self::SummaryOnly { claim } => Some(claim),
            Self::ArtifactPointerOnly { .. } => None,
        }
    }

    /// Split the worker-reported body into its inline evidence text and the
    /// optional spill footer.
    ///
    /// The two parts render with the decisive `[Check: ...]` line between
    /// them, so a declared check sits above the `[Full result ...]` footer on
    /// a spilled entry (S46 packet section 8 View 2) while still following the
    /// evidence text on an inline entry. `None` on the second element marks an
    /// inline entry with no spill footer.
    pub(crate) fn body_parts(&self) -> (String, Option<String>) {
        match self {
            Self::InlineResult { result, .. } => (result.as_str().to_owned(), None),
            Self::ArtifactPointer { stand_in, artifact } => {
                let stand_in_text = match stand_in {
                    ArtifactStandIn::Claim(claim) => claim.summary(),
                    ArtifactStandIn::Preview(preview) => preview.as_str(),
                };
                (stand_in_text.to_owned(), Some(artifact.to_string()))
            }
            Self::SummaryOnly { claim } => (claim.summary().to_owned(), None),
            Self::ArtifactPointerOnly { artifact } => {
                ("(no inline preview)".to_owned(), Some(artifact.to_string()))
            }
        }
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

    /// Marker appended by `Display` when truncation cut the error text, so
    /// every cut is visible at the render site (R2 gate decision 5).
    pub const TRUNCATION_MARKER: &'static str = " [truncated]";

    /// Truncate raw error text to at most [`Self::MAX_CHARS`] characters,
    /// recording whether anything was cut.
    pub fn new(raw_error: &str, width: crate::orchestration::bounding::ErrorPreviewWidth) -> Self {
        let (text, truncated) = width.truncate_with_flag(raw_error);
        Self { text, truncated }
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

/// Renders the bounded text, with [`ErrorPreview::TRUNCATION_MARKER`]
/// appended when truncation cut the original: no silent cuts.
impl std::fmt::Display for ErrorPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)?;
        if self.truncated {
            f.write_str(Self::TRUNCATION_MARKER)?;
        }
        Ok(())
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
    /// The reconciled decisive named check, when the task declared one
    /// (design-panel P4): the worker's carried result when the identities
    /// match, `NOT RUN` otherwise. Rendered as a `[Check: ...]` line after the
    /// evidence on every entry shape, so a declared check the worker did not
    /// answer stays visible even when the bulk result spilled. `None` on the
    /// common checkless path.
    pub named_check: Option<NamedCheck>,
}

impl CompletedEntry {
    /// Render the entry for the `COMPLETED TASKS:` section: the correlation
    /// label line, the indented worker evidence, the decisive check line when
    /// the task declared one, and the artifact inventory lines
    /// (`ARCHITECTURE.md` sections 1.3 and 1.4; S46 packet section 8).
    pub fn render(&self) -> RenderedContext {
        let confidence = self.evidence.claim().map(WorkerClaim::confidence);
        let mut text = format!(
            "- Task {}{}",
            self.label.task,
            label_suffix(self.label.worker.as_ref(), confidence)
        );
        let (body_text, spill_footer) = self.evidence.body_parts();
        text.push('\n');
        text.push_str(&indent(&body_text));
        // The decisive check line sits directly below the evidence text and
        // above the spill footer, so a declared check stays visible ahead of
        // the `[Full result ...]` pointer on a spilled entry (S46 packet
        // section 8 View 2); on an inline entry there is no footer, so it
        // simply follows the evidence text.
        if let Some(named_check) = &self.named_check {
            text.push_str(&format!("\n    {}", named_check.render_line()));
        }
        if let Some(footer) = spill_footer {
            text.push_str(&format!("\n    {footer}"));
        }
        for artifact in &self.artifacts {
            text.push_str(&format!("\n    {artifact}"));
        }
        RenderedContext::new(text)
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
        claim: WorkerClaim,
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
        let task = self.label.task;
        let suffix = label_suffix(self.label.worker.as_ref(), None);
        let text = match &self.report {
            FailureReport::Hard { category, error } => {
                format!("- Task {task}{suffix} -> failed [{category}]: {error}")
            }
            FailureReport::Soft { claim, artifact } => {
                let mut text = format!(
                    "- Task {task}{suffix} -> soft_failure ({} confidence)\n{}",
                    claim.confidence(),
                    indent(claim.summary())
                );
                if let Some(artifact) = artifact {
                    text.push_str(&format!("\n    {artifact}"));
                }
                text
            }
        };
        RenderedContext::new(text)
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
        RenderedContext::new(format!(
            "- Task {}{} -> blocked (dependency failed)",
            self.label.task,
            label_suffix(self.label.worker.as_ref(), None)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::label::TaskId;
    use super::*;
    use crate::orchestration::bounding::ErrorPreviewWidth;

    const FOOTER: &str =
        "[Full result (5000 chars) saved to artifact: task-0-operator-iter-1-result.txt]";

    fn claim() -> WorkerClaim {
        WorkerClaim::new("Found 47 error groups across 3 services", Confidence::High)
            .expect("non-empty summary")
    }

    fn label(worker: Option<&str>) -> CorrelationLabel {
        CorrelationLabel {
            task: TaskId::new(0),
            worker: worker.map(|w| WorkerRole::new(w).expect("non-empty role")),
        }
    }

    // R2 gate decision 5: a value renders by exactly one degrade path,
    // truncation happens at most once, and every cut is marked.
    #[test]
    fn degrade_paths_are_mutually_exclusive() {
        let body = "b".repeat(100);
        let spilled = format!("{body}\n\n{FOOTER}");

        // A spilled result parses only as an artifact pointer...
        let entry = EvidenceEntry::from_completed_result(&spilled, Some(claim()))
            .expect("spilled result parses");
        assert!(
            matches!(entry, EvidenceEntry::ArtifactPointer { .. }),
            "footered text must take the artifact-pointer path"
        );
        // ...and the same text is unrepresentable as inline evidence.
        assert_eq!(
            EvidenceText::new(&spilled),
            Err(ContextError::InlineEvidenceCarriesSpillFooter)
        );

        // Rendered, the spilled entry shows the stand-in and the pointer
        // exactly once, never the raw body next to them.
        let rendered = CompletedEntry {
            label: label(Some("operator")),
            evidence: entry,
            artifacts: vec![],
            named_check: None,
        }
        .render();
        let rendered = rendered.as_str();
        assert_eq!(rendered.matches("[Full result (").count(), 1);
        assert!(rendered.contains("Found 47 error groups"));
        assert!(
            !rendered.contains(&body),
            "raw body must not render next to the attested summary"
        );

        // An inline result renders by the inline path alone: full text, no
        // footer, no truncation.
        let inline = EvidenceEntry::from_completed_result("all checks passed", Some(claim()))
            .expect("fitting result parses");
        let rendered = CompletedEntry {
            label: label(Some("operator")),
            evidence: inline,
            artifacts: vec![],
            named_check: None,
        }
        .render();
        assert!(rendered.as_str().contains("    all checks passed"));
        assert!(!rendered.as_str().contains("[Full result ("));

        // Error previews truncate at most once and mark every cut.
        let long = "e".repeat(ErrorPreview::MAX_CHARS + 1000);
        let preview = ErrorPreview::new(&long, ErrorPreviewWidth::DEFAULT);
        assert!(preview.was_truncated());
        assert_eq!(preview.as_str().chars().count(), ErrorPreview::MAX_CHARS);
        let shown = preview.to_string();
        assert!(shown.ends_with(ErrorPreview::TRUNCATION_MARKER));
        assert_eq!(shown.matches(ErrorPreview::TRUNCATION_MARKER).count(), 1);

        let short = ErrorPreview::new("Connection refused", ErrorPreviewWidth::DEFAULT);
        assert!(!short.was_truncated());
        assert_eq!(short.to_string(), "Connection refused");
    }

    // R2 gate decision 2: at direct distance the full result renders when
    // it fits; a claim tags it, it does not replace it.
    #[test]
    fn full_result_stays_inline_when_it_fits_despite_claim() {
        let entry =
            EvidenceEntry::from_completed_result("Full detailed worker result", Some(claim()))
                .expect("fitting result parses");
        let EvidenceEntry::InlineResult { result, claim } = entry else {
            panic!("fitting result must parse inline, got {entry:?}");
        };
        assert_eq!(result.as_str(), "Full detailed worker result");
        assert!(claim.is_some(), "the claim rides along without replacing");
    }

    #[test]
    fn spilled_result_without_claim_uses_bounded_preview_stand_in() {
        let spilled = format!("Bounded preview text.\n\n{FOOTER}");
        let entry =
            EvidenceEntry::from_completed_result(&spilled, None).expect("spilled result parses");
        let EvidenceEntry::ArtifactPointer { stand_in, artifact } = entry else {
            panic!("footered text must take the artifact-pointer path, got {entry:?}");
        };
        let ArtifactStandIn::Preview(preview) = stand_in else {
            panic!("no claim means the bounded preview stands in");
        };
        assert_eq!(preview.as_str(), "Bounded preview text.");
        assert_eq!(artifact.filename(), "task-0-operator-iter-1-result.txt");
        assert_eq!(artifact.to_string(), FOOTER);
    }

    #[test]
    fn malformed_footer_stays_inline() {
        let text = "mentions [Full result ( but is not a footer";
        assert!(SpilledArtifact::parse_trailing(text).is_none());
        let entry = EvidenceEntry::from_completed_result(text, None).expect("parses inline");
        assert!(matches!(entry, EvidenceEntry::InlineResult { .. }));
    }

    #[test]
    fn empty_values_are_rejected() {
        assert_eq!(
            EvidenceText::new("  \n"),
            Err(ContextError::EmptyEvidenceText)
        );
        assert_eq!(
            ResultPreview::new(""),
            Err(ContextError::EmptyResultPreview)
        );
        assert_eq!(
            SpilledArtifact::new(" ", 100),
            Err(ContextError::EmptyArtifactFilename)
        );
        assert_eq!(
            ArtifactRef::new("", 100),
            Err(ContextError::EmptyArtifactFilename)
        );
    }

    #[test]
    fn completed_entry_renders_label_evidence_and_inventory() {
        let entry = CompletedEntry {
            label: label(Some("operator")),
            evidence: EvidenceEntry::from_completed_result(
                "VM launched; VNC on 5901",
                Some(claim()),
            )
            .expect("parses inline"),
            artifacts: vec![
                ArtifactRef::new("task-0-operator-iter-1-result.txt", 2143).expect("valid ref"),
            ],
            named_check: None,
        };
        assert_eq!(
            entry.render().as_str(),
            "- Task 0 (operator, confidence: high)\n    VM launched; VNC on 5901\n    [Artifact: task-0-operator-iter-1-result.txt (2143 bytes)]"
        );
    }

    #[test]
    fn failed_and_blocked_entries_render_label_only_headers() {
        let failed = FailedEntry {
            label: label(Some("verifier")),
            report: FailureReport::Hard {
                category: FailureCategory::DepthExhausted,
                error: ErrorPreview::new(
                    "MaxDepthError (reached limit: 16)",
                    ErrorPreviewWidth::DEFAULT,
                ),
            },
        };
        assert_eq!(
            failed.render().as_str(),
            "- Task 0 (verifier) -> failed [depth_exhausted]: MaxDepthError (reached limit: 16)"
        );

        let soft = FailedEntry {
            label: label(Some("analyst")),
            report: FailureReport::Soft {
                claim: WorkerClaim::new("Found partial matches only", Confidence::Low)
                    .expect("non-empty summary"),
                artifact: SpilledArtifact::parse_trailing(&format!("body\n\n{FOOTER}")),
            },
        };
        assert_eq!(
            soft.render().as_str(),
            format!(
                "- Task 0 (analyst) -> soft_failure (low confidence)\n    Found partial matches only\n    {FOOTER}"
            )
        );

        let blocked = BlockedEntry {
            label: label(Some("operator")),
        };
        assert_eq!(
            blocked.render().as_str(),
            "- Task 0 (operator) -> blocked (dependency failed)"
        );

        let unassigned = BlockedEntry { label: label(None) };
        assert_eq!(
            unassigned.render().as_str(),
            "- Task 0 -> blocked (dependency failed)"
        );
    }

    // S46 packet section 8: a declared check renders a `[Check: ...]` line
    // after the evidence, and it survives result spill — the deciding datum
    // stays in front of the coordinator even when the bulk spilled to an
    // artifact (design-panel P4, packet section 7).
    #[test]
    fn completed_entry_renders_declared_check_inline_and_through_spill() {
        let performed = NamedCheck::parse(
            "per-directory entry count (max 30)",
            Some("VIOLATION: g00000 has 53"),
            None,
        )
        .expect("valid check");
        let inline = CompletedEntry {
            label: label(Some("analyst")),
            evidence: EvidenceEntry::from_completed_result("Resharding complete.", Some(claim()))
                .expect("parses inline"),
            artifacts: vec![],
            named_check: Some(performed),
        };
        assert!(
            inline.render().as_str().contains(
                "\n    [Check: per-directory entry count (max 30) -> VIOLATION: g00000 has 53]"
            ),
            "inline check line: {}",
            inline.render().as_str()
        );

        // A declared check the worker did not carry renders NOT RUN, and the
        // line rides alongside the spilled stand-in.
        let not_run = NamedCheck::not_run("per-directory entry count (max 30)").expect("valid");
        let spilled = format!("attested summary prefix\n\n{FOOTER}");
        let entry = CompletedEntry {
            label: label(Some("analyst")),
            evidence: EvidenceEntry::from_completed_result(&spilled, Some(claim()))
                .expect("spilled result parses"),
            artifacts: vec![],
            named_check: Some(not_run),
        };
        let rendered = entry.render().as_str().to_string();
        assert!(
            rendered.contains("[Full result ("),
            "spill footer present: {rendered}"
        );
        assert!(
            rendered.contains("[Check: per-directory entry count (max 30) -> NOT RUN]"),
            "check survives the spill: {rendered}"
        );
        // The check line renders above the spill footer, not below it (S46
        // Gate A finding A3, packet section 8 View 2).
        let check_at = rendered
            .find("[Check:")
            .expect("check line present in spilled render");
        let footer_at = rendered
            .find("[Full result (")
            .expect("spill footer present in spilled render");
        assert!(
            check_at < footer_at,
            "check line must precede the [Full result ...] footer: {rendered}"
        );
    }

    // S46 Gate A finding A3: a carried decisive check on a spilled entry
    // renders its result on the `[Check: ...]` line, above the `[Full result
    // ...]` footer, with the stand-in summary above the check (packet section
    // 8 View 2). The stand-in, check, and footer appear in that exact order.
    #[test]
    fn completed_entry_renders_carried_check_above_spill_footer() {
        let performed = NamedCheck::parse(
            "desktop framebuffer read",
            Some("18/288000 non-black; desktop NOT reached"),
            None,
        )
        .expect("valid check");
        let spilled = format!("QEMU alive; screen 18/288000 non-black.\n\n{FOOTER}");
        let entry = CompletedEntry {
            label: label(Some("verifier")),
            evidence: EvidenceEntry::from_completed_result(&spilled, Some(claim()))
                .expect("spilled result parses"),
            artifacts: vec![],
            named_check: Some(performed),
        };
        let rendered = entry.render().as_str().to_string();
        let stand_in_at = rendered
            .find("Found 47 error groups")
            .expect("stand-in summary present");
        let check_at = rendered.find("[Check:").expect("check line present");
        let footer_at = rendered
            .find("[Full result (")
            .expect("spill footer present");
        assert!(
            stand_in_at < check_at && check_at < footer_at,
            "order must be stand-in, then check, then footer: {rendered}"
        );
    }

    #[test]
    fn from_completed_result_whitespace_only_prefix_returns_artifact_pointer_only() {
        let text = format!("   \n\n{FOOTER}");
        let entry = EvidenceEntry::from_completed_result(&text, None)
            .expect("whitespace-only prefix parses");
        assert!(
            matches!(entry, EvidenceEntry::ArtifactPointerOnly { .. }),
            "expected ArtifactPointerOnly, got {entry:?}"
        );
        let (body_text, footer) = entry.body_parts();
        assert_eq!(body_text, "(no inline preview)");
        assert!(
            footer
                .expect("spilled entry carries a footer")
                .contains(FOOTER)
        );
    }

    #[test]
    fn from_completed_result_whitespace_only_prefix_renders_no_inline_preview_tag_and_footer() {
        let text = format!("   \n\n{FOOTER}");
        let entry = EvidenceEntry::from_completed_result(&text, None).expect("parses");
        let rendered = CompletedEntry {
            label: label(Some("operator")),
            evidence: entry,
            artifacts: vec![],
            named_check: None,
        }
        .render();
        let rendered = rendered.as_str();
        assert!(
            rendered.contains("(no inline preview)"),
            "missing tag: {rendered}"
        );
        assert!(rendered.contains(FOOTER), "missing footer: {rendered}");
    }

    #[test]
    fn bare_label_fallback_unreachable_for_whitespace_only_prefix() {
        let text = format!("   \n\n{FOOTER}");
        assert!(
            EvidenceEntry::from_completed_result(&text, None).is_ok(),
            "whitespace-only prefix + footer must parse successfully"
        );
    }
}
