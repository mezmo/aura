//! The park checkpoint (park mode): the document, the commit, the
//! run-scoped guard, and the resume-side continuation surfaces.

mod commit;
mod continuation;
mod document;
mod guard;
mod recorded_decisions;

pub(crate) use commit::{ParkCommitInputs, cancel_run_approvals, commit_from_run_state};
// The rehydrate entry points: consumed by commit 3's tests; the P45 resume
// endpoint consumes them in production.
#[allow(unused_imports)]
pub(crate) use continuation::{
    RehydrateError, ResumeContext, ResumingDocumentHandle, TaskContinuation,
    load_recorded_decisions, replace_tool_result,
};
#[allow(unused_imports)]
pub(crate) use document::{
    PARKED_DOCUMENT_SUFFIX, ParkedRun, RESUMING_DOCUMENT_SUFFIX, RunStateForPark, load_parked_run,
};
pub(crate) use guard::ParkGuard;
pub(crate) use recorded_decisions::{CallKey, RecordedDecisions};

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
