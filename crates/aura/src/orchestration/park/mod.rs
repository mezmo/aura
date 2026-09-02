//! The park checkpoint: document, commit, and run-scoped guard (park mode).
//!
//! A parked run stops dispatching at the quiescence verdict and records
//! itself as a [`ParkedRun`] document under
//! `{memory_dir}/{session_id}/parked/{run_id}.json`. The document — not any
//! in-memory state — is the record a later resume reads; its awaiting set
//! re-derives from disk alone, and its `executed` list is the tombstone that
//! marks a resumed document terminal.
//!
//! - [`document`] — the checkpoint types, the builder from run state, and the
//!   load path.
//! - [`commit`] — publication (temp write + same-directory rename), the
//!   awaiting-set refresh against the approval store, the config
//!   fingerprint, and the no-checkpoint cancellation sweep.
//! - [`guard`] — the run-scoped guard that cancels the run's parked
//!   approvals when a run ends without a published checkpoint.

mod commit;
mod document;
mod guard;

pub(crate) use commit::{ParkCommitInputs, cancel_run_approvals, commit_from_run_state};
pub(crate) use document::RunStateForPark;
#[cfg(test)]
pub(crate) use document::load_parked_run;
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
