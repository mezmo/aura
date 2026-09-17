//! The retention-cleanup surface: checkpoint-presence classification, the
//! encapsulated evidence-first deletion order, and the sweep seam.
//!
//! Types and seams only — E6 (retention sweep and orphan classification)
//! and E8 (server bootstrap activation) own every body. Nothing here is
//! wired into the server yet: cleanup activates only after the lifetime,
//! reservation, consult, and retention bodies are filled and green.
#![allow(dead_code)] // the E6/E8 fills construct and drive this surface;
// the marker comes off when the sweep activates

use std::path::Path;

use super::lifetime::ReservationTable;
use super::resume::claim::{ResumeDocuments, ValidatedResumePath};
use super::resume::evaluate::Diagnostic;
use crate::hitl::PendingApprovals;

/// Whether a run's checkpoint is present, confirmed gone, or unreadable. A
/// missing root or an unreadable document is never confirmed absence: the
/// sweep retries, and unowned or corrupt files are reported as operational
/// diagnostics, never deleted by guessing.
#[derive(Debug, Clone)]
pub(crate) enum CheckpointPresence {
    /// A healthy checkpoint document exists under one of the two names:
    /// the run is not absent, and orphan collection does not apply.
    Present,
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

/// One run's reservation-owning cleanup carrier: the lease that binds the
/// run and carries the cleanup eligibility (only an acquired reservation
/// may reread-and-delete), plus the run's two checkpoint paths derived
/// from the SAME validated identity the admission occupied. `Clone` hands
/// the SAME fence to the blocking reread/deletion tails, so the
/// reservation survives even if the awaiting sweep drops.
#[derive(Debug, Clone)]
pub(crate) struct CleanupReservation {
    reservation: super::lifetime::RunReservationLease,
    docs: ResumeDocuments,
}

/// Why a run is not cleanup-eligible: the admission itself is the
/// eligibility proof, so a live occupation (an executing run) is the one
/// refusal — retention revalidation under the acquired fence is E6's
/// body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CleanupAdmissionFault {
    /// A live reservation holds the run: execution is active and cleanup
    /// must not proceed. Nothing changed.
    Executing,
}

impl CleanupReservation {
    /// Acquire the run for cleanup: occupy the run under the shared
    /// reservation table — the admission IS the eligibility proof, never
    /// a clone of an executing run's lease — then derive the checkpoint
    /// paths from the same validated identity, so the fence and the
    /// paths cannot name different runs. A live run answers
    /// [`CleanupAdmissionFault::Executing`].
    pub(crate) fn acquire(
        table: &ReservationTable,
        path: &ValidatedResumePath,
        memory_dir: &str,
    ) -> Result<Self, CleanupAdmissionFault> {
        let reservation = table
            .admit(path.run.run_id())
            .map_err(|_| CleanupAdmissionFault::Executing)?;
        let docs = ResumeDocuments::for_path(path, memory_dir);
        Ok(Self { reservation, docs })
    }

    /// The reservation fencing this cleanup: the run's identity and its
    /// cleanup eligibility.
    pub(crate) fn reservation(&self) -> &super::lifetime::RunReservationLease {
        &self.reservation
    }

    /// The run's two checkpoint paths.
    pub(crate) fn documents(&self) -> &ResumeDocuments {
        &self.docs
    }
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

/// Inspect both checkpoint names for the run and classify presence — the
/// pre-deletion check the sweep runs under an acquired run reservation,
/// after re-reading the clock. The blocking reread tail holds the
/// carrier's lease reference through the work.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub(crate) async fn inspect_checkpoint_presence(
    cleanup: &CleanupReservation,
) -> CheckpointPresence {
    todo!(
        "P45 wave fill unit E6: re-read both checkpoint names under the reservation and classify present vs confirmed absent vs inaccessible vs corrupt"
    )
}

/// Delete one expired run's evidence under the carrier's reservation,
/// strictly past `retention_expires_at` and with no execution active: owned
/// approval and decision rows first, the checkpoint LAST — so a failure
/// midway leaves the expired checkpoint to answer `409 expired` and the
/// next sweep can retry. The blocking deletion tail retains the carrier's
/// lease reference through the work.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub(crate) async fn delete_expired_run(
    cleanup: &CleanupReservation,
    registry: &PendingApprovals,
) -> RunCleanupOutcome {
    todo!(
        "P45 wave fill unit E6: evidence-first, checkpoint-last deletion with retry-on-failure retention, fenced by the cleanup reservation"
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
