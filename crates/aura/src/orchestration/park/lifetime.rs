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
    pub fn spawn_tracked<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let reservation = self.reservation.clone();
        self.tracker.spawn(async move {
            let _lease = reservation;
            future.await
        })
    }

    /// Spawn one tracked blocking task under this scope: same registration
    /// and lease-holding contract as [`Self::spawn_tracked`], for the
    /// blocking pool (renames, checkpoint writes, sweeps).
    pub fn spawn_blocking_tracked<F, R>(&self, body: F) -> tokio::task::JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let reservation = self.reservation.clone();
        self.tracker.spawn_blocking(move || {
            let _lease = reservation;
            body()
        })
    }

    /// Wait for every task spawned under this scope to end: the join
    /// barrier the supervisor drains through before the fence may release.
    /// Never a yield-based counter.
    pub async fn drain(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ReservationTable;
    use super::RunExecutionScope;
    use crate::orchestration::types::RunId;

    /// Long enough for a fill to make visible progress, short enough that a
    /// regressed fill (a drain that never joins its tails) fails fast instead
    /// of hanging the suite.
    const GATE_TICK: Duration = Duration::from_millis(500);

    /// Distinct, probe-free run ids parsed through `RunId`'s `FromStr`.
    fn run_id(uuid: &'static str) -> RunId {
        uuid.parse().expect("well-formed run id")
    }

    // Hole: `RunExecutionScope::spawn_tracked` (SKELETON hole inventory
    // row 7) — RED at the tracked-async-spawn `todo!()` site until the fill
    // routes the future through the tracker and returns it output.

    #[tokio::test]
    async fn spawn_tracked_returns_output_through_join_handle() {
        let table = ReservationTable::new();
        let lease = table
            .admit(run_id("28b3f0a6-6c71-4e02-9f0d-25df8e1f7a10"))
            .expect("admission");
        let scope = RunExecutionScope::new(lease);
        let handle = scope.spawn_tracked(async { 21 * 2 });
        let output = tokio::time::timeout(GATE_TICK, handle)
            .await
            .expect("tracked task joins within GATE_TICK")
            .expect("tracked task did not panic");
        assert_eq!(output, 42);
    }

    // Hole: `RunExecutionScope::spawn_tracked` (SKELETON hole inventory
    // row 7) — spawned wrapper keeps its lease reference through the
    // future's ACTUAL completion: the run stays live while the gate is
    // closed (check via a separately-held `ReservationTable`, which shares
    // the live set), and releases only at the task's real tail.

    #[tokio::test]
    async fn spawn_tracked_holds_reservation_through_task_completion() {
        let table = ReservationTable::new();
        let check = table.clone();
        let run = run_id("91f2c4d7-1a55-4b83-8c2e-3b9e6d0f42b1");
        let lease = table.admit(run).expect("admission");
        // `RunExecutionScope::new` consumes the lease into the scope.
        let scope = RunExecutionScope::new(lease);
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let handle = scope.spawn_tracked(async move {
            gate.await.ok();
        });
        // Owner-side handles all dropped: the spawned wrapper's lease
        // reference is now the only fence holder.
        drop(scope);
        assert!(
            check.is_live(run),
            "run stays occupied while every owner handle is dropped but the tracked task still runs"
        );
        release.send(()).expect("gate still open for release");
        tokio::time::timeout(GATE_TICK, handle)
            .await
            .expect("tracked task joins within GATE_TICK after the gate opens")
            .expect("tracked task did not panic");
        assert!(
            !check.is_live(run),
            "run releases only after the tracked task's actual completion"
        );
    }

    // Hole: `RunExecutionScope::spawn_blocking_tracked` (SKELETON hole
    // inventory row 8) — the same fence rule on the blocking pool: live
    // while the body is blocked on the gate, released after it returns.

    #[tokio::test]
    async fn spawn_blocking_tracked_holds_reservation_through_completion() {
        let table = ReservationTable::new();
        let check = table.clone();
        let run = run_id("5c1e8b23-77ad-4e60-9c4d-14a2f8d63e07");
        let lease = table.admit(run).expect("admission");
        let scope = RunExecutionScope::new(lease);
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let handle = scope.spawn_blocking_tracked(move || {
            gate.recv().expect("gate open when receives");
        });
        drop(scope);
        assert!(
            check.is_live(run),
            "run stays occupied while the blocked body still holds its lease reference"
        );
        release.send(()).expect("gate still open for release");
        tokio::time::timeout(GATE_TICK, handle)
            .await
            .expect("tracked blocking task joins within GATE_TICK after the gate opens")
            .expect("tracked blocking task did not panic");
        assert!(
            !check.is_live(run),
            "run releases only after the blocking body returns"
        );
    }

    // Hole: `RunExecutionScope::drain` (SKELETON hole inventory row 9, the
    // RunExecutionScope rule row) — the join barrier: drain does NOT
    // complete while any tracked gate is closed, and completes after all
    // gates release. Never a yield-based counter.

    #[tokio::test]
    async fn drain_waits_for_every_tracked_tail() {
        let table = ReservationTable::new();
        let lease = table
            .admit(run_id("e2a94d14-9a34-42f8-a5b9-7c6d1e0f83b2"))
            .expect("admission");
        let scope = RunExecutionScope::new(lease);
        let (release_one, gate_one) = tokio::sync::oneshot::channel::<()>();
        let (release_two, gate_two) = tokio::sync::oneshot::channel::<()>();
        let handle_one = scope.spawn_tracked(async move {
            gate_one.await.ok();
            "one"
        });
        let handle_two = scope.spawn_tracked(async move {
            gate_two.await.ok();
            "two"
        });
        assert!(
            tokio::time::timeout(GATE_TICK, scope.drain())
                .await
                .is_err(),
            "drain must not complete while any tracked gate is closed"
        );
        release_one
            .send(())
            .expect("gate one still open for release");
        let one = tokio::time::timeout(GATE_TICK, handle_one)
            .await
            .expect("tracked task one joins within GATE_TICK")
            .expect("tracked task one did not panic");
        assert_eq!(one, "one");
        assert!(
            tokio::time::timeout(GATE_TICK, scope.drain())
                .await
                .is_err(),
            "drain must still wait while a tracked gate stays closed"
        );
        release_two
            .send(())
            .expect("gate two still open for release");
        tokio::time::timeout(GATE_TICK, scope.drain())
            .await
            .expect("drain completes after every gate is released");
        let two = tokio::time::timeout(GATE_TICK, handle_two)
            .await
            .expect("tracked task two joins within GATE_TICK")
            .expect("tracked task two did not panic");
        assert_eq!(two, "two");
    }

    // Hole: `RunExecutionScope::drain` — an idle scope's drain returns
    // with nothing tracked at all.

    #[tokio::test]
    async fn drain_completes_with_no_tracked_tasks() {
        let table = ReservationTable::new();
        let lease = table
            .admit(run_id("6f40b8e2-21d3-4a97-88c5-9b1e3f6d0a52"))
            .expect("admission");
        let scope = RunExecutionScope::new(lease);
        tokio::time::timeout(GATE_TICK, scope.drain())
            .await
            .expect("an idle scope drains within GATE_TICK");
    }

    // Hole: `RunExecutionScope::drain` + `spawn_tracked` registration —
    // membership is tracker-tracked, not handle-owned: a dropped
    // `JoinHandle` does not abort the task, so drain waits out its actual
    // tail.

    #[tokio::test]
    async fn drain_waits_for_tasks_whose_join_handles_are_dropped() {
        let table = ReservationTable::new();
        let lease = table
            .admit(run_id("c08d75a1-4e2b-4d36-90f3-58a2e7c41d96"))
            .expect("admission");
        let scope = RunExecutionScope::new(lease);
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let handle = scope.spawn_tracked(async move {
            gate.await.ok();
        });
        drop(handle);
        assert!(
            tokio::time::timeout(GATE_TICK, scope.drain())
                .await
                .is_err(),
            "drain must wait for the tracked tail even after its JoinHandle is dropped"
        );
        release.send(()).expect("gate still open for release");
        tokio::time::timeout(GATE_TICK, scope.drain())
            .await
            .expect("drain completes once the dropped-handle task's actual tail has ended");
    }
}
