//! The endpoint-owned resume claim table and the run's checkpoint document
//! paths.
//!
//! The claim table is the endpoint's per-run handle, distinct from
//! [`super::super::continuation::ResumingDocumentHandle`], which stays the
//! park-module's append-and-publish surface: the table tracks which endpoint
//! evaluation holds a run, the handle mutates the resuming document.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};

use crate::config::SessionId;
use crate::orchestration::persistence::is_safe_path_component;
use crate::orchestration::types::RunId;

use super::super::commit::parked_document_dir;
use super::super::document::{PARKED_DOCUMENT_SUFFIX, RESUMING_DOCUMENT_SUFFIX};
use super::evaluate::Diagnostic;

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

/// Why claiming a run for a resume segment failed.
#[derive(Debug, Clone)]
pub(crate) enum ClaimResumeFault {
    /// A live claim already holds the run.
    Live,
    /// The atomic rename failed; the claim was not taken.
    Io(Diagnostic),
}

/// Process-local registry of live resume claims: at most one resume per run
/// inside this process.
pub struct ResumeClaimTable {
    live: Arc<Mutex<HashSet<RunId>>>,
    #[cfg(test)]
    race_gate: Arc<RaceGate>,
}

impl std::fmt::Debug for ResumeClaimTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeClaimTable")
            .field("live", &self.live)
            .finish()
    }
}

impl Default for ResumeClaimTable {
    fn default() -> Self {
        Self {
            live: Arc::new(Mutex::new(HashSet::new())),
            #[cfg(test)]
            race_gate: Arc::new(RaceGate::new()),
        }
    }
}

impl ResumeClaimTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a live claim holds the run.
    #[must_use]
    pub(crate) fn is_live(&self, run: &ResumeRunId) -> bool {
        self.live
            .lock()
            .expect("resume claim lock")
            .contains(&run.run_id())
    }

    /// Arm this table's rename-back rendezvous for the concurrent golden:
    /// exactly two `rename_back_to_parked` arrivals must occur while the
    /// returned guard is alive, or they hold.
    #[cfg(test)]
    pub(crate) fn arm_rename_back_race(&self) -> RaceGateGuard {
        self.race_gate.armed.store(true, Ordering::SeqCst);
        RaceGateGuard {
            gate: Arc::clone(&self.race_gate),
        }
    }

    /// Rename the run's resuming document back to its parked name while
    /// holding the claim lock, so a concurrent evaluation cannot observe the
    /// half-renamed pair.
    pub(crate) async fn rename_back_to_parked(
        &self,
        docs: &ResumeDocuments,
    ) -> Result<(), Diagnostic> {
        let parked = docs.parked().to_path_buf();
        let resuming = docs.resuming().to_path_buf();
        let live = Arc::clone(&self.live);
        #[cfg(test)]
        let race_gate = Arc::clone(&self.race_gate);
        tokio::task::spawn_blocking(move || -> Result<(), Diagnostic> {
            #[cfg(test)]
            race_gate.meet();
            // The std guard lives only inside this closure: the rename is
            // serialized against `claim_and_resume`'s insert-and-rename, and
            // no guard is ever held across an await.
            let _live = live.lock().expect("resume claim lock");
            std::fs::rename(&resuming, &parked).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound && parked.try_exists().unwrap_or(false)
                {
                    // A concurrent evaluation won the rename-back under the
                    // lock; the document is already at its parked name and
                    // evaluation proceeds against the in-memory document.
                    Ok(())
                } else {
                    Err(Diagnostic::new(format!(
                        "renaming the resuming checkpoint {} back to its parked name failed: {e}",
                        resuming.display()
                    )))
                }
            })
        })
        .await
        .map_err(|e| Diagnostic::new(format!("the rename-back task did not complete: {e}")))?
    }

    /// Insert the claim and rename the parked document to its resuming name
    /// as one step under the claim lock: either both happen or neither does.
    pub(crate) async fn claim_and_resume(
        &self,
        docs: &ResumeDocuments,
    ) -> Result<ResumeLease, ClaimResumeFault> {
        // The documents derive from a validated path (`for_path`), so the
        // parked name's stem is the validated run id: the claim keys on the
        // same run whose document the rename moves.
        let raw_run = docs
            .parked()
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .expect("the claim documents carry the validated run id as their stem");
        let run =
            ResumeRunId::parse(raw_run).expect("the stem of a validated document name re-parses");
        let parked = docs.parked().to_path_buf();
        let resuming = docs.resuming().to_path_buf();
        let live = Arc::clone(&self.live);
        let lease_live = Arc::clone(&self.live);
        tokio::task::spawn_blocking(move || -> Result<ResumeLease, ClaimResumeFault> {
            // One acquisition covers check, insert, rename, and rollback: a
            // second caller's insert observes the live claim before any
            // rename, and a failed rename rolls the insert back, so the
            // claim and the document name move together or not at all.
            let mut live = live.lock().expect("resume claim lock");
            if !live.insert(run.run_id()) {
                return Err(ClaimResumeFault::Live);
            }
            match std::fs::rename(&parked, &resuming) {
                Ok(()) => Ok(ResumeLease {
                    live: lease_live,
                    run,
                }),
                Err(e) => {
                    live.remove(&run.run_id());
                    Err(ClaimResumeFault::Io(Diagnostic::new(format!(
                        "renaming the parked checkpoint {} to its resuming name failed: {e}",
                        parked.display()
                    ))))
                }
            }
        })
        .await
        .map_err(|e| {
            ClaimResumeFault::Io(Diagnostic::new(format!(
                "the claim task did not complete: {e}"
            )))
        })?
    }
}

/// A held resume claim for one run.
#[derive(Debug)]
pub struct ResumeLease {
    live: Arc<Mutex<HashSet<RunId>>>,
    run: ResumeRunId,
}

impl Drop for ResumeLease {
    /// Release the run; a later evaluation may claim it.
    fn drop(&mut self) {
        self.live
            .lock()
            .expect("resume claim lock")
            .remove(&self.run.run_id());
    }
}

/// Deterministic rendezvous for the concurrent rename-back golden. Arming
/// is scoped to ONE claim table (the armed test's world), so the parallel
/// test harness cannot pair an unrelated table's rename with the barrier.
/// With the gate armed, both of that table's rename-back closures hold past
/// `locate_checkpoint` before either contends the claim lock, so the
/// loser's ENOENT path is exercised on every run instead of by scheduling
/// luck. Arming obliges exactly two rename-back arrivals while held.
#[cfg(test)]
pub(crate) struct RaceGate {
    armed: AtomicBool,
    barrier: Barrier,
}

#[cfg(test)]
impl RaceGate {
    pub(crate) fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            barrier: Barrier::new(2),
        }
    }

    fn meet(&self) {
        if self.armed.load(Ordering::SeqCst) {
            self.barrier.wait();
        }
    }
}

/// Disarms the rendezvous on drop, so a panicking test cannot leave the
/// gate armed for whichever test reuses the thread next.
#[cfg(test)]
pub(crate) struct RaceGateGuard {
    gate: Arc<RaceGate>,
}

#[cfg(test)]
impl Drop for RaceGateGuard {
    fn drop(&mut self) {
        self.gate.armed.store(false, Ordering::SeqCst);
    }
}
