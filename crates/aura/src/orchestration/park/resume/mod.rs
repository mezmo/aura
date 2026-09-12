//! The resume-side endpoint surfaces: the per-run claim table, the ordered
//! pre-continuation evaluation, and the segment seam the endpoint drives.
//!
//! The claim table is the endpoint's per-run handle, distinct from the
//! park-module's [`super::continuation::ResumingDocumentHandle`], which stays
//! the append-and-publish surface of the resuming document itself.

// Covers three structural survivors of the filled module: the grant's
// Drop-held lease, `FingerprintFault::Fault` (stage-shape symmetry — the
// fingerprint stage cannot fault), and the goldens' TempDir kept alive for
// cleanup. Not removable at the handler fill either.
#![allow(dead_code)]

pub(crate) mod claim;
pub(crate) mod evaluate;
#[cfg(test)]
mod goldens;

pub use claim::{
    MalformedId, ResumeClaimTable, ResumeDocuments, ResumeRunId, ResumeSessionId,
    ValidatedResumePath,
};
pub use evaluate::{
    BlockingEntry, ConflictCode, Diagnostic, EmptyBlocking, EmptySegment, NonEmptyBlocking,
    ParkedToolName, ResumeConflictRow, ResumeEvaluation, ResumeGrant, ResumeRefusal, SegmentError,
    SegmentResult, SegmentTurns, evaluate_resume, run_segment,
};
