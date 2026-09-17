//! The park checkpoint (park mode): the document, the commit, the
//! run-scoped guard, and the resume-side continuation surfaces.

mod cleanup;
mod commit;
mod continuation;
mod document;
mod guard;
pub(crate) mod lifetime;
mod rebuild;
mod recorded_decisions;
pub(crate) mod resume;
mod retention;

pub(crate) use commit::{
    ParkCommitInputs, cancel_run_approvals, commit_from_run_state, run_owner_id,
};
// `ResumingDocumentHandle` drives the resume segment's tombstones in
// production. `load_recorded_decisions` is consumed through this
// re-export by the park-tree golden suites alone — the module is private
// outside `park`, and the resume consult reaches it by module path — so
// its marker stays.
pub(crate) use continuation::ResumingDocumentHandle;
#[allow(unused_imports)]
pub(crate) use continuation::load_recorded_decisions;
pub(crate) use document::{
    PARKED_DOCUMENT_SUFFIX, ParkedRun, RESUMING_DOCUMENT_SUFFIX, RunStateForPark, load_parked_run,
};
pub(crate) use guard::{ParkGuard, ParkGuardMode};
// The provider-valid context builder for the reconstruction direction
// (P45, R5): the prelude names `CallId`, `NodePreflightInput`,
// `OutcomeWire`, `SegmentPreflight`, and `rebuild_context` through this
// re-export. The rest of the flat list is the design record's
// completeness surface (REBUILD-DESIGN.md's seam table), reached only by
// module path and inference — the marker stays for those names.
pub(crate) use rebuild::{
    CallId, NodePreflightInput, OutcomeWire, SegmentPreflight, rebuild_context,
};
#[allow(unused_imports)]
pub(crate) use rebuild::{
    PreflightError, RebuiltContext, ResolveError, ResolvedCall, ResolvedCallBundle,
    ToolResultPrompt, ValidatedCall, ValidatedCalls, ValidatedNode,
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
