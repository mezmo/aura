//! The shared run-reservation and park-execution lifetime surface.
//!
//! The resume claim table generalizes into a shared run reservation: one run
//! is occupied under the table's short standard lock, and the fence survives
//! in blocking work through an `Arc` reference to the lease — an awaiting
//! request dropping never releases a run whose rename is still in flight. The
//! standard lock is never held across an await.
//!
//! [`RunExecutionScope`] bundles what park-owned execution carries everywhere
//! — the reservation lease, the cancellation token, and the task tracker —
//! so a detached spawn can be registered before it starts and joined before
//! its fence is released.
#![allow(dead_code)] // the E4 fill (shared reservation table: admit,
// authorize, empty-resume recovery) constructs and releases the lease; the
// L fills inject scopes. Marker removed when those land.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::orchestration::types::RunId;

/// A shared fence over one occupied run: `Clone` hands out references to the
/// same occupation, and the run is released only when the last reference
/// drops, after the supervisor and every tracked child and file operation has
/// ended.
#[derive(Debug, Clone)]
pub struct RunReservationLease {
    pub(crate) run: RunId,
    pub(crate) live: Arc<Mutex<HashSet<RunId>>>,
}

impl RunReservationLease {
    /// The occupied run.
    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.run
    }
}

impl Drop for RunReservationLease {
    fn drop(&mut self) {
        todo!(
            "P45 wave fill unit E4: release the run reservation under the table's short standard lock when the last lease reference ends"
        )
    }
}

/// Why a run could not be reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationFault {
    /// A live reservation already occupies the run.
    Live,
}

/// Park-owned execution state: the reservation lease fencing the run, the
/// cancellation token every spawn selects on, and the tracker every detached
/// task is registered with before it starts.
///
/// Non-park calls carry no scope; [`None`] on `ToolCallContext` keeps the
/// current unscoped behavior.
#[derive(Debug)]
pub struct RunExecutionScope {
    reservation: RunReservationLease,
    cancellation: CancellationToken,
    tracker: TaskTracker,
}

impl RunExecutionScope {
    /// Arm a scope over one held reservation.
    #[must_use]
    pub fn new(reservation: RunReservationLease) -> Arc<Self> {
        Arc::new(Self {
            reservation,
            cancellation: CancellationToken::new(),
            tracker: TaskTracker::new(),
        })
    }

    /// The reservation fencing this run.
    #[must_use]
    pub fn reservation(&self) -> &RunReservationLease {
        &self.reservation
    }

    /// The scope's cancellation token.
    #[must_use]
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// The tracker every detached spawn registers with.
    #[must_use]
    pub fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}
