//! The endpoint-owned resume claim table and the run's checkpoint document
//! paths.
//!
//! The claim table is the endpoint's per-run handle, distinct from
//! [`super::super::continuation::ResumingDocumentHandle`], which stays the
//! park-module's append-and-publish surface: the table tracks which endpoint
//! evaluation holds a run, the handle mutates the resuming document.

use std::path::{Path, PathBuf};
use std::str::FromStr;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc;

use crate::config::SessionId;
use crate::orchestration::persistence::is_safe_path_component;
use crate::orchestration::types::RunId;

use super::super::commit::parked_document_dir;
use super::super::document::{PARKED_DOCUMENT_SUFFIX, RESUMING_DOCUMENT_SUFFIX};
use super::evaluate::Diagnostic;
use crate::orchestration::park::lifetime::{
    ReservationFault, ReservationTable, RunReservationLease,
};

/// Why a raw path segment failed validation. Every variant is
/// diagnostic-only: no caller branches on the reason.
#[derive(Debug, Clone)]
pub enum MalformedId {
    /// The run id did not parse as a UUID.
    NotAUuid(Diagnostic),
    /// The segment was empty, carried a path separator, or carried a parent
    /// reference.
    UnsafePathComponent(Diagnostic),
}

impl std::fmt::Display for MalformedId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAUuid(diagnostic) => write!(f, "{diagnostic}"),
            Self::UnsafePathComponent(diagnostic) => write!(f, "{diagnostic}"),
        }
    }
}

/// A path-validated session id: safe as a single filesystem path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResumeSessionId(SessionId);

impl ResumeSessionId {
    /// Parse a raw path segment, rejecting anything unsafe as a path
    /// component before any filesystem access.
    pub fn parse(raw: &str) -> Result<Self, MalformedId> {
        if is_safe_path_component(raw) {
            Ok(Self(SessionId::new(raw)))
        } else {
            Err(MalformedId::UnsafePathComponent(Diagnostic::new(format!(
                "session id {raw:?} is not a safe path component"
            ))))
        }
    }
}

impl AsRef<str> for ResumeSessionId {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for ResumeSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// A path-validated run id: parses as a UUID and is safe as a single
/// filesystem path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResumeRunId(RunId);

impl ResumeRunId {
    /// Parse a raw path segment as a UUID and reject anything unsafe as a
    /// path component, before any filesystem access.
    pub fn parse(raw: &str) -> Result<Self, MalformedId> {
        let run = RunId::from_str(raw)
            .map_err(|e| MalformedId::NotAUuid(Diagnostic::new(e.to_string())))?;
        if !is_safe_path_component(raw) {
            return Err(MalformedId::UnsafePathComponent(Diagnostic::new(format!(
                "run id {raw:?} is not a safe path component"
            ))));
        }
        Ok(Self(run))
    }

    /// The inner run id.
    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.0
    }
}

impl std::fmt::Display for ResumeRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Both validated path segments of a resume request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedResumePath {
    pub session: ResumeSessionId,
    pub run: ResumeRunId,
}

impl ValidatedResumePath {
    /// Parse both segments; the first rejection wins.
    pub fn parse(session: &str, run: &str) -> Result<Self, MalformedId> {
        let session = ResumeSessionId::parse(session)?;
        let run = ResumeRunId::parse(run)?;
        Ok(Self { session, run })
    }
}

/// A run's two checkpoint filenames under its session's parked directory.
#[derive(Debug, Clone)]
pub struct ResumeDocuments {
    parked: PathBuf,
    resuming: PathBuf,
}

impl ResumeDocuments {
    /// Derive the two filenames from a validated path and the memory root.
    #[must_use]
    pub fn for_path(path: &ValidatedResumePath, memory_dir: &str) -> Self {
        let dir = parked_document_dir(memory_dir, Some(path.session.as_ref()));
        Self {
            parked: dir.join(format!("{}{PARKED_DOCUMENT_SUFFIX}", path.run)),
            resuming: dir.join(format!("{}{RESUMING_DOCUMENT_SUFFIX}", path.run)),
        }
    }

    /// The published checkpoint filename.
    pub(crate) fn parked(&self) -> &Path {
        &self.parked
    }

    /// The in-progress checkpoint filename.
    pub(crate) fn resuming(&self) -> &Path {
        &self.resuming
    }
}

/// Why claiming a run for a resume segment failed. The fault arms carry the
/// typed classification the endpoint rows need — a known filesystem
/// availability failure vs an internal task failure — with the diagnostic
/// for server-side logging only.
#[derive(Debug, Clone)]
pub(crate) enum ClaimResumeFault {
    /// A live claim already holds the run.
    Live,
    /// A known filesystem availability failure: the transition's rename
    /// could not be performed. The endpoint's 503 `reify_unavailable` row.
    Unavailable(Diagnostic),
    /// An internal fault outside the availability class — e.g. the blocking
    /// task itself failed to complete. The endpoint's 500 `reify_failed`
    /// row.
    Internal(Diagnostic),
}

/// Process-local registry of live resume claims: at most one resume per run
/// inside this process. The occupied-run set lives inside the injected
/// [`ReservationTable`] (admission and lease construction are that table's
/// one seam — this type holds no path to assemble a lease by hand).
pub struct ResumeClaimTable {
    reservations: ReservationTable,
    #[cfg(test)]
    fence_gate: Arc<FenceGate>,
}

impl std::fmt::Debug for ResumeClaimTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeClaimTable")
            .field("reservations", &self.reservations)
            .finish()
    }
}

impl Default for ResumeClaimTable {
    fn default() -> Self {
        Self {
            reservations: ReservationTable::new(),
            #[cfg(test)]
            fence_gate: Arc::new(FenceGate::new()),
        }
    }
}

impl ResumeClaimTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve one run under the table's short standard lock: the shared
    /// occupation every park-owned execution fences on. The lock is held for
    /// the check-and-insert only — never across an await — and the returned
    /// lease is shared, so blocking work keeps the fence after an awaiting
    /// request drops. A second reservation of a live run fails
    /// [`ReservationFault::Live`].
    ///
    /// This is the generalized reservation entry the resume ordering reserves
    /// through (step 2 of the ownership contract); the claim-and-rename step
    /// converts the same reservation into a grant with no ownerless gap.
    pub fn reserve(&self, run: &ResumeRunId) -> Result<RunReservationLease, ReservationFault> {
        self.reservations.admit(run.run_id())
    }

    /// Ordered-resume step 3's rename-back, fenced by the held reservation:
    /// the blocking rename tail holds a lease reference through completion,
    /// so an awaiting request dropping never releases a run whose rename-back
    /// is in flight. Only the availability and internal arms can arise here
    /// — the run is already reserved, so `Live` is impossible.
    pub(crate) async fn rename_back_under_reservation(
        &self,
        reservation: &RunReservationLease,
        docs: &ResumeDocuments,
    ) -> Result<(), ClaimResumeFault> {
        let parked = docs.parked().to_path_buf();
        let resuming = docs.resuming().to_path_buf();
        #[cfg(test)]
        let fence_gate = Arc::clone(&self.fence_gate);
        // The blocking tail MOVES a lease reference in: the fence survives an
        // awaiting request dropping, so the run stays reserved until the
        // rename has actually completed.
        let lease = reservation.clone();
        tokio::task::spawn_blocking(move || -> Result<(), ClaimResumeFault> {
            // Deterministic rendezvous for the fence-lifetime golden, the
            // retired pre-reservation seam's convention carried onto this
            // fenced tail: while armed, the tail signals arrival and holds on
            // the release channel BEFORE renaming.
            #[cfg(test)]
            fence_gate.meet();
            // The held reservation is the whole fence: the run is already
            // reserved, so no second caller can be renaming, and the table's
            // standard lock is never held across this tail. The lease
            // binding keeps the fence alive through the rename's completion.
            let _lease = lease;
            // The safe rename-back semantics of the retired pre-reservation
            // seam hold exactly: `NotFound` while the parked name already
            // exists reads as success (the name is already restored); every
            // other io error fails.
            std::fs::rename(&resuming, &parked).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound && parked.try_exists().unwrap_or(false)
                {
                    Ok(())
                } else {
                    Err(ClaimResumeFault::Unavailable(Diagnostic::new(format!(
                        "renaming the resuming checkpoint {} back to its parked name failed: {e}",
                        resuming.display()
                    ))))
                }
            })
        })
        .await
        .map_err(|e| {
            ClaimResumeFault::Internal(Diagnostic::new(format!(
                "the rename-back task did not complete: {e}"
            )))
        })?
    }

    /// Whether a live claim holds the run.
    #[must_use]
    pub(crate) fn is_live(&self, run: &ResumeRunId) -> bool {
        self.reservations.is_live(run.run_id())
    }

    /// Arm this table's fenced-tail rendezvous for the lease-lifetime golden:
    /// while the returned guard is alive, this table's next fenced rename-back
    /// tail signals arrival on the guard's receiver and holds on its release
    /// sender before renaming. The gate is scoped to ONE table, so the
    /// parallel test harness cannot pair an unrelated table's tail.
    #[cfg(test)]
    pub(crate) fn arm_fenced_rename(&self) -> FenceGateGuard {
        // `mpsc::channel()` hands back `(Sender, Receiver)`: the tail side
        // takes the arrival sender and the release receiver, and the guard
        // gets the opposite ends.
        let (arrival_to_tail, arrival_to_test) = mpsc::channel();
        let (release_to_test, release_to_tail) = mpsc::channel();
        self.fence_gate.arm_with(arrival_to_tail, release_to_tail);
        FenceGateGuard {
            gate: Arc::clone(&self.fence_gate),
            arrival: arrival_to_test,
            release: release_to_test,
        }
    }
}

/// Deterministic rendezvous for the fenced rename tail's lease-lifetime
/// golden ([`ResumeClaimTable::arm_fenced_rename`]). `meet` runs at the top
/// of the blocking tail: while armed, it signals arrival and blocks on the
/// release channel BEFORE the rename, so a test can drop every outer lease
/// reference and prove the tail's own lease clone keeps the run reserved
/// until the rename actually completes. Fail-open on a closed channel: a
/// panicking or departed test can never deadlock the tail.
#[cfg(test)]
pub(crate) struct FenceGate {
    armed: AtomicBool,
    rendezvous: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

#[cfg(test)]
impl FenceGate {
    pub(crate) fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            rendezvous: Mutex::new(None),
        }
    }

    /// Store the tail-side channel ends and arm the gate.
    fn arm_with(&self, arrival_from_tail: mpsc::Sender<()>, release_to_tail: mpsc::Receiver<()>) {
        *self
            .rendezvous
            .lock()
            .expect("the fence gate's lock is healthy") =
            Some((arrival_from_tail, release_to_tail));
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Disarm the gate; a later `meet` proceeds without rendezvous.
    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }

    /// The tail's rendezvous: while armed, take the stored channel ends (a
    /// spent or absent rendezvous proceeds), signal arrival, and hold until
    /// released.
    fn meet(&self) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        let Some((arrival, release)) = self
            .rendezvous
            .lock()
            .expect("the fence gate's lock is healthy")
            .take()
        else {
            // A second arrival while armed: the rendezvous is spent, so this
            // tail proceeds.
            return;
        };
        // Either end may already be gone if the test departed: both channel
        // operations fail open so the tail never deadlocks behind it.
        let _ = arrival.send(());
        let _ = release.recv();
    }
}

/// Disarms the rendezvous on drop and carries the test's ends of the channel
/// pairs: `arrival` receives the tail's arrival signal, `release` lets the
/// held tail proceed.
#[cfg(test)]
pub(crate) struct FenceGateGuard {
    gate: Arc<FenceGate>,
    pub(crate) arrival: mpsc::Receiver<()>,
    pub(crate) release: mpsc::Sender<()>,
}

#[cfg(test)]
impl Drop for FenceGateGuard {
    fn drop(&mut self) {
        self.gate.disarm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_admits_rejects_live_and_releases_on_drop() {
        let table = ResumeClaimTable::new();
        let run = ResumeRunId::parse("0191e8c0-aaaa-7000-8000-0000000000e4").unwrap();

        let lease = table
            .reserve(&run)
            .expect("a fresh run reserves under the shared table");
        assert!(
            table.is_live(&run),
            "the reserved run is live while the lease is held"
        );

        match table.reserve(&run) {
            Err(ReservationFault::Live) => {}
            other => panic!("a second reservation of a live run must be Live, got {other:?}"),
        }

        drop(lease);
        assert!(
            !table.is_live(&run),
            "the final lease reference dropping releases the run"
        );
    }
}
