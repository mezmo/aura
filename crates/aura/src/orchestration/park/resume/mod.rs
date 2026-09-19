//! The resume-side endpoint surfaces: the per-run claim table, the ordered
//! pre-continuation evaluation, and the segment seam the endpoint drives.
//!
//! The claim table is the endpoint's per-run handle, distinct from the
//! park-module's [`super::continuation::ResumingDocumentHandle`], which stays
//! the append-and-publish surface of the resuming document itself.

#![allow(dead_code)] // survivors: the grant's Drop-held reservation, the
// fingerprint stage's symmetry variant, the goldens' TempDir cleanup handles,
// and the declared E4 seams (ReservedEvaluation, convert_reserved,
// rename_back_under_reservation) whose fills wire them

pub(crate) mod claim;
pub(crate) mod evaluate;
#[cfg(test)]
mod goldens;

pub use claim::{
    MalformedId, ResumeClaimTable, ResumeDocuments, ResumeRunId, ResumeSessionId,
    ValidatedResumePath,
};
pub use evaluate::{
    BlockingEntry, ConflictCode, Diagnostic, EmptyBlocking, NonEmptyBlocking, ParkedToolName,
    ResumeConflictRow, ResumeEvaluation, ResumeGrant, ResumeRefusal, ResumeStreamEnd, SegmentError,
    evaluate_resume, run_segment_borrowed,
};
// The ordered resume's internal carrier and consuming transition: consumed
// by the E4 fill inside this crate only.
#[allow(unused_imports)]
pub(crate) use evaluate::{ReservedEvaluation, convert_reserved};
