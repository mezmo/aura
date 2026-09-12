//! The resume-side endpoint surfaces: the per-run claim table, the ordered
//! pre-continuation evaluation, and the segment seam the endpoint drives.
//!
//! The claim table is the endpoint's per-run handle, distinct from the
//! park-module's [`super::continuation::ResumingDocumentHandle`], which stays
//! the append-and-publish surface of the resuming document itself.

#![allow(dead_code)] // P45 skeleton: the fill unit removes this slice as bodies land

pub(crate) mod claim;
pub(crate) mod evaluate;

pub use claim::{
    MalformedId, ResumeClaimTable, ResumeDocuments, ResumeRunId, ResumeSessionId,
    ValidatedResumePath,
};
pub use evaluate::{
    BlockingEntry, ConflictCode, IdentityBindingState, ResumeConflictRow, ResumeEvaluation,
    ResumeGrant, ResumeRefusal, SegmentResult, evaluate_resume, run_segment,
};
