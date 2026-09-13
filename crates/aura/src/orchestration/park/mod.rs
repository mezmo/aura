//! The park checkpoint (park mode): the document, the commit, the
//! run-scoped guard, and the resume-side continuation surfaces.

mod commit;
mod continuation;
mod document;
mod guard;
mod rebuild;
mod recorded_decisions;
pub(crate) mod resume;

pub(crate) use commit::{
    ParkCommitInputs, cancel_run_approvals, commit_from_run_state, run_owner_id,
};
// The rehydrate entry points: `ResumingDocumentHandle` drives the resume
// segment's tombstones in production; `load_recorded_decisions` and
// `RehydrateError` are consumed through this re-export by commit 3's tests
// (the resume consult reaches them by module path), so the unused-import
// marker stays until those consumers name the re-export path.
#[allow(unused_imports)]
pub(crate) use continuation::{RehydrateError, ResumingDocumentHandle, load_recorded_decisions};
#[allow(unused_imports)]
pub(crate) use document::{
    PARKED_DOCUMENT_SUFFIX, ParkedRun, RESUMING_DOCUMENT_SUFFIX, RunStateForPark, load_parked_run,
};
pub(crate) use guard::ParkGuard;
// The provider-valid context builder for the reconstruction direction
// (P45, R5): the stage-3 wiring points the substitution prelude at it,
// consuming the construction path by name (`NodePreflightInput`,
// `SegmentPreflight`, `CallId`, `OutcomeWire`, `rebuild_context`). The
// remaining re-exported names are reached only through method returns
// and inference, never by name in production — the stage-2b frames
// exercise them inside the module — so the unused-import marker stays.
#[allow(unused_imports)]
pub(crate) use rebuild::{
    CallId, NodePreflightInput, OutcomeWire, PreflightError, RebuiltContext, ResolveError,
    ResolvedCall, ResolvedCallBundle, SegmentPreflight, ToolResultPrompt, ValidatedCall,
    ValidatedCalls, ValidatedNode, rebuild_context,
};
pub(crate) use recorded_decisions::{CallKey, PeekOutcome, RecordedDecisions};

use std::collections::HashMap;

use crate::orchestration::ParkSnapshot;

/// The park record for one awaiting task: the blocking attempt number and
/// the conversation captured when the worker's stream was cancelled.
#[derive(Debug, Clone)]
pub(crate) struct ParkedTaskRecord {
    pub attempt: usize,
    pub snapshot: ParkSnapshot,
}

/// Park records for a run's awaiting tasks, keyed by task id.
pub(crate) type ParkedTaskRecords = HashMap<usize, ParkedTaskRecord>;
