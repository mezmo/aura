//! The shared run-reservation and park-execution lifetime surface.
//!
//! The resume claim table generalizes into a shared run reservation: one run
//! is occupied under the table's short standard lock, and the fence survives
//! in blocking work through clones of the lease — an awaiting request
//! dropping never releases a run whose rename is still in flight. The
//! standard lock is never held across an await.
//!
//! [`RunReservationLease`] wraps one private [`ReservationInner`]: the run
//! identity plus the table reference exist only behind
//! [`ReservationTable`]'s admission paths, and the run is released
//! synchronously when the last reference to that one reservation drops —
//! after the supervisor and every tracked child and file operation has
//! ended. Admission and lease construction are ONE seam: the lease's
//! constructor is module-private and reachable only from the table's
//! check-and-insert, so no crate-visible path can assemble a lease for a
//! run it never admitted.
//!
//! [`RunExecutionScope`] bundles what park-owned execution carries everywhere
//! — the reservation lease, the cancellation token, and the task tracker —
//! and is the only surface detached park work spawns through: the tracked
//! spawn helpers register before spawning and hold a lease reference through
//! actual completion, so a detached task cannot spawn unregistered or
//! outlive its fence.
#![allow(dead_code)] // the L fill injects scopes into `ToolCallContext` call
// sites and the E4 fill (shared reservation table: admit, authorize,
// empty-resume recovery) threads the lease through the ordered resume.
// Marker removed when those land.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::orchestration::types::RunId;

/// The private per-reservation state one [`RunReservationLease`] wraps: the
/// occupied run and the shared table it was admitted into. Private so no
/// crate-visible field can assemble a lease that never reserved its run.
#[derive(Debug)]
struct ReservationInner {
    run: RunId,
    live: Arc<Mutex<HashSet<RunId>>>,
}

impl Drop for ReservationInner {
    /// The last-reference fence: the final drop of this one reservation's
    /// handles releases the run under the table's short standard lock.
    fn drop(&mut self) {
        self.live
            .lock()
            .expect("resume claim lock")
            .remove(&self.run);
    }
}

/// A shared fence over one occupied run: `Clone` hands out references to the
/// same occupation, and the run is released only when the last reference
/// drops, after the supervisor and every tracked child and file operation has
/// ended.
#[derive(Debug, Clone)]
pub struct RunReservationLease {
    inner: Arc<ReservationInner>,
}

impl RunReservationLease {
    /// Arm the lease for a run whose admission into `live` already succeeded
    /// under the table's short standard lock. PRIVATE: only
    /// [`ReservationTable`]'s admission paths construct a lease — admission
    /// and construction are one seam, so no crate-visible surface can
    /// assemble a lease for a run it never admitted.
    fn armed(run: RunId, live: Arc<Mutex<HashSet<RunId>>>) -> Self {
        Self {
            inner: Arc::new(ReservationInner { run, live }),
        }
    }

    /// The occupied run.
    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.inner.run
    }
}

/// The shared occupied-run registry: the table every park-owned execution
/// reserves through. The set and its lock are private to this module, so the
/// only paths that can occupy a run or test occupation are the admission
/// seams below — [`ResumeClaimTable`] and the factory inject this type, not
/// the raw lock.
///
/// [`ResumeClaimTable`]: super::resume::claim::ResumeClaimTable
#[derive(Debug, Clone, Default)]
pub(crate) struct ReservationTable {
    live: Arc<Mutex<HashSet<RunId>>>,
}

/// Why an admission-with-step failed: the run was already occupied, or the
/// under-lock step failed and the occupation rolled back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdmissionFault<E> {
    /// A live reservation already holds the run; nothing changed.
    Live,
    /// The under-lock step failed; the occupation was rolled back and no
    /// lease exists.
    Step(E),
}

impl ReservationTable {
    /// An empty table.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether a live reservation holds the run.
    pub(crate) fn is_live(&self, run: RunId) -> bool {
        self.live.lock().expect("resume claim lock").contains(&run)
    }

    /// Occupy `run` under the table's short standard lock — the
    /// check-and-insert is the whole critical section, never held across an
    /// await — returning the lease whose final reference releases the run.
    /// The ONLY plain-admission seam; every [`RunReservationLease`] is born
    /// here or in [`Self::admit_with`].
    pub(crate) fn admit(&self, run: RunId) -> Result<RunReservationLease, ReservationFault> {
        let mut live = self.live.lock().expect("resume claim lock");
        if live.insert(run) {
            Ok(RunReservationLease::armed(run, Arc::clone(&self.live)))
        } else {
            Err(ReservationFault::Live)
        }
    }

    /// Occupy `run`, then run `step` while still holding the table's lock:
    /// `step` succeeding makes the occupation and its transition (the
    /// claim-and-rename) move together; `step` failing rolls the occupation
    /// back, so neither happens. The lease is constructed only on the
    /// all-succeeded path.
    pub(crate) fn admit_with<E>(
        &self,
        run: RunId,
        step: impl FnOnce() -> Result<(), E>,
    ) -> Result<RunReservationLease, AdmissionFault<E>> {
        let mut live = self.live.lock().expect("resume claim lock");
        if !live.insert(run) {
            return Err(AdmissionFault::Live);
        }
        match step() {
            Ok(()) => Ok(RunReservationLease::armed(run, Arc::clone(&self.live))),
            Err(step_fault) => {
                live.remove(&run);
                Err(AdmissionFault::Step(step_fault))
            }
        }
    }

    /// Run `step` while holding the table's short standard lock: the
    /// mutual-exclusion seam for transitions that must appear atomic
    /// against [`Self::admit_with`]'s insert-and-step (the interim
    /// rename-back). Never held across an await — callers run their step
    /// on the blocking pool.
    pub(crate) fn under_standard_lock<R>(&self, step: impl FnOnce() -> R) -> R {
        let _guard = self.live.lock().expect("resume claim lock");
        step()
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
/// current unscoped behavior. The tracker is not exposed directly: tracked
/// spawning goes through [`Self::spawn_tracked`] and
/// [`Self::spawn_blocking_tracked`], which register before spawning and keep
/// a lease reference alive through actual completion; cancellation and drain
/// are exposed on their own.
#[derive(Debug)]
pub struct RunExecutionScope {
    reservation: RunReservationLease,
    cancellation: CancellationToken,
    tracker: TaskTracker,
}

impl RunExecutionScope {
    /// Arm a scope over one held reservation. The establishment primitive:
    /// called ONCE per resumed run when its grant is assembled from the
    /// reservation (and once per initial park-enabled run); everything
    /// else receives a clone of that one `Arc` —
    /// [`ResumeGrant::execution_scope`] never mints a second token or
    /// tracker for the run.
    ///
    /// [`ResumeGrant::execution_scope`]:
    /// super::resume::evaluate::ResumeGrant::execution_scope
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

    /// Signal cancellation to everything running under this scope.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Spawn one tracked async task under this scope: the wrapper registers
    /// the task with the tracker BEFORE it starts, and the spawned wrapper
    /// owns a lease reference through the future's actual completion — a
    /// dropped scope or awaiting request never releases the run while the
    /// task still runs.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by P45 wave fill units"
    )]
    pub fn spawn_tracked<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        todo!(
            "P45 wave fill unit L1: tracked async spawn — register before spawn, hold a lease reference through actual completion"
        )
    }

    /// Spawn one tracked blocking task under this scope: same registration
    /// and lease-holding contract as [`Self::spawn_tracked`], for the
    /// blocking pool (renames, checkpoint writes, sweeps).
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by P45 wave fill units"
    )]
    pub fn spawn_blocking_tracked<F, R>(&self, body: F) -> tokio::task::JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        todo!(
            "P45 wave fill unit L1: tracked blocking spawn — register before spawn, hold a lease reference through actual completion"
        )
    }

    /// Wait for every task spawned under this scope to end: the join
    /// barrier the supervisor drains through before the fence may release.
    /// Never a yield-based counter.
    pub async fn drain(&self) {
        todo!(
            "P45 wave fill unit L1: close the tracker to new spawns and wait for every tracked tail to end"
        )
    }
}
