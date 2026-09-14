//! The retention-cleanup surface: confirmed-absence classification, the
//! encapsulated evidence-first deletion order, and the sweep seam.
//!
//! Types and seams only — E6 (retention sweep and orphan classification)
//! and E8 (server bootstrap activation) own every body. Nothing here is
//! wired into the server yet: cleanup activates only after the lifetime,
//! reservation, consult, and retention bodies are filled and green.
#![allow(dead_code)] // the E6/E8 fills construct and drive this surface;
// the marker comes off when the sweep activates

use std::path::Path;

use super::resume::claim::ResumeDocuments;
use super::resume::evaluate::Diagnostic;
use crate::hitl::PendingApprovals;

/// Whether a run's checkpoint is (not) confirmed absent. A missing root or
/// an unreadable document is never confirmed absence: the sweep retries,
/// and unowned or corrupt files are reported as operational diagnostics,
/// never deleted by guessing.
#[derive(Debug, Clone)]
pub(crate) enum CheckpointAbsence {
    /// No checkpoint under either name: confirmed gone.
    ConfirmedAbsent,
    /// The checkpoint root or document could not be read — absence is NOT
    /// confirmed and nothing may be deleted.
    Inaccessible(Diagnostic),
    /// A document exists under one of the names but does not decode: the
    /// run stays, reported as a diagnostic; corrupt evidence is never
    /// treated as absent evidence.
    Corrupt(Diagnostic),
}

/// The outcome of one expired run's cleanup.
#[derive(Debug, Clone)]
pub(crate) enum RunCleanupOutcome {
    /// Owned approval and decision rows deleted first, then the checkpoint
    /// last: the run's evidence is gone.
    Removed,
    /// A deletion failed; the expired checkpoint is retained so the next
    /// startup or sweep can retry. Evidence that remains keeps answering
    /// `409 expired`; after full cleanup the run reads absent.
    RetainedForRetry(Diagnostic),
}

/// Re-read both checkpoint names for the run and classify absence — the
/// pre-deletion check the sweep runs under an acquired run reservation,
/// after re-reading the clock.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub(crate) async fn confirm_checkpoint_absence(docs: &ResumeDocuments) -> CheckpointAbsence {
    todo!(
        "P45 wave fill unit E6: re-read both checkpoint names and classify confirmed absence vs inaccessible vs corrupt"
    )
}

/// Delete one expired run's evidence under an acquired run reservation,
/// strictly past `retention_expires_at` and with no execution active: owned
/// approval and decision rows first, the checkpoint LAST — so a failure
/// midway leaves the expired checkpoint to answer `409 expired` and the
/// next sweep can retry.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub(crate) async fn delete_expired_run(
    registry: &PendingApprovals,
    docs: &ResumeDocuments,
) -> RunCleanupOutcome {
    todo!(
        "P45 wave fill unit E6: evidence-first, checkpoint-last deletion with retry-on-failure retention"
    )
}

/// Scan one owned checkpoint root for retained run documents: the sweep's
/// per-root enumeration, off the async executor and without network work
/// under a lock. Each entry names a candidate run whose approval rows the
/// store's retained scan supplies; grouping and classification belong to
/// the sweep.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub(crate) async fn scan_checkpoint_root(root: &Path) -> Result<Vec<String>, Diagnostic> {
    todo!(
        "P45 wave fill unit E6: enumerate one owned checkpoint root's run documents; a missing or unreadable root is a diagnostic, never confirmed absence"
    )
}
