//! The park checkpoint (park mode): the document, the commit, and the
//! run-scoped guard.

mod commit;
mod document;
mod guard;

pub(crate) use commit::{ParkCommitInputs, cancel_run_approvals, commit_from_run_state};
#[cfg(test)]
pub(crate) use document::load_parked_run;
pub(crate) use document::{PARKED_DOCUMENT_SUFFIX, RESUMING_DOCUMENT_SUFFIX, RunStateForPark};
pub(crate) use guard::ParkGuard;

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
