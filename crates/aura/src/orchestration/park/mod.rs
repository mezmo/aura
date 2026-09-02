//! The park checkpoint document (park mode): the types, the builder from run
//! state, and the load path.

// No production consumer yet; the park commit drops this allow.
#![allow(dead_code, unused_imports)]

mod document;

#[cfg(test)]
pub(crate) use document::load_parked_run;
pub(crate) use document::{PARKED_DOCUMENT_SUFFIX, RESUMING_DOCUMENT_SUFFIX, RunStateForPark};

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
