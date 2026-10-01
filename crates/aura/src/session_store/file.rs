//! File-backed HITL approval store: one JSON file per decision id.
//! Parked approvals survive process restart on a single host. The
//! skill-invocation store shares the same root under `skills/`.
//!
//! Layout:
//!
//! | Path                                 | Content                                                    |
//! | ----------------------------------- | ---------------------------------------------------------- |
//! | `{root}/approvals/{decision_id}.json` | `ParkedApprovalRecord` (the undecided approval)            |
//! | `{root}/decisions/{decision_id}.json` | the resolved envelope: the approval record plus the decision |
//!
//! Store contract (park/reify §2.5):
//!
//! - `resolve` refuses past the approval's `expires_at`, uniformly with an
//!   unknown id.
//! - `resolve` *moves* the approval into the decision file rather than deleting
//!   it, minus its egress headers (a decided id is never notified again):
//!   `get` returns the approval before and after the decision, `decision`
//!   returns the recorded decision, and both are retained until `remove`.
//! - At-most-once `resolve` is the `File::create_new` claim on the decision
//!   file: `AlreadyExists` reads as `NotFound`.
//! - `cancel_request` removes undecided approvals by owner (request) id and
//!   returns them; decided entries are retained until their consumer removes
//!   them.
//! - `list_pending` scans the undecided approvals for the poll reconciler:
//!   corrupt files are warn-and-skipped per id, expired records are
//!   filtered but kept in place for the read-or-expire consult to
//!   terminalize (F02), and an approval file left behind a complete
//!   decision file is unlinked (one behind an undecodable decision file
//!   is kept, as the only intact record).
//!
//! Decision ids are validated as UUIDs before path building, so none address
//! outside the root.
//! Temp-file plus rename prevents partial files after crashes. A `std::sync::Mutex`
//! serializes operations for the single writing process; no operation awaits
//! while holding it, and `list_pending` holds it only to snapshot the
//! directory listing — its per-file reads and decodes run outside it.
//! Publishes and destructive commits also serialize per path on a separate
//! lock map, so a compare+remove commit never overlaps a same-instance
//! registration's rename at one approval path and cannot erase the record
//! the registration published. The per-path locks are per store instance:
//! two handles on one root stay independent, and no cross-process claim
//! is made.
//!
//! Store operations run sync on the blocking pool rather than over
//! `tokio::fs`, which is itself one `spawn_blocking` per call: a whole
//! read-modify-write costs one hop instead of one per file access, and the
//! lock spanning it stays a `std::sync::Mutex` instead of one held across
//! awaits. Join failures map to store errors—a panicked op is a store fault,
//! not a crash. Blocking work is not cancelled when the requester is
//! dropped: a dropped poll still completes resolve's claim-write-move.
//!
//! Crash window: claim-then-write leaves an empty file if process dies
//! mid-resolve. The aftermath fails closed; `decision` reports decode
//! fault, `get` returns the approval. Recovery is deleting the empty file.
//! `decision()` consumers treat `Err(Decode)` on a known id as this
//! recoverable state, not as an unknown id.

mod skill_store;

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::task::{JoinError, spawn_blocking};

use crate::hitl::{
    AcknowledgmentState, AddressedApproval, ApprovalAuthority, ApprovalRead, DecisionId,
    ParkedApproval, ResolveError, ResolvedDecision,
};

use super::record::TerminalRecord;
use super::{AcknowledgeOutcome, ApprovalStore, ParkedApprovalRecord, SessionStoreError};

pub use skill_store::FileSkillInvocationStore;

/// Undecided approvals, one `{decision_id}.json` file per approval.
const APPROVALS_DIR: &str = "approvals";
/// Recorded decisions, one `{decision_id}.json` file per decision.
const DECISIONS_DIR: &str = "decisions";

/// The on-disk shape of a resolved approval: the approval record carried
/// over from `approvals/` plus the terminal record under the `decision`
/// key. Field names are a persisted contract shared by every instance
/// reading the store — rename only with a migration. The `decision` value
/// is the explicitly tagged terminal record (`kind`: `decided` or
/// `timed_out`); a value carrying both decided fields and a `deadline`
/// fails decode instead of falling through to a variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResolvedEntry {
    approval: ParkedApprovalRecord,
    decision: TerminalRecord,
}

/// A file-backed [`ApprovalStore`] over one root directory.
pub struct FileApprovalStore {
    inner: Arc<Inner>,
}

/// The injectable time source the store samples while holding the
/// operation lock. `Arc<dyn Fn>` so tests can drive the deadline and
/// timeout arbitration deterministically.
type StoreClock = Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>;

/// The shared store state: root directory, operation lock, the clock the
/// resolve/read-or-expire deadline sampling reads, and the per-path
/// destructive-commit locks.
struct Inner {
    root: PathBuf,
    lock: Mutex<()>,
    /// One lock per approval path, keyed by this store's own constructed
    /// paths. An entry lives only while a parked approval exists at its
    /// path, so growth is bounded by the approvals in flight.
    /// Mechanism: `with_path_lock`, `evict_path_lock`.
    path_locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    clock: StoreClock,
}

impl FileApprovalStore {
    /// Open (or initialize) the store rooted at `root`, creating both
    /// directories and probing them for writes. Fails fast: a store that
    /// cannot hold files must fail at startup, not on the first approval.
    /// Samples the wall clock.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        Self::open_with_clock(root, Arc::new(chrono::Utc::now))
    }

    /// Open (or initialize) the store sampling an injected clock: the
    /// read-or-expire seam the E2 fill arbitrates deadlines and timeouts
    /// through, under the same lock as resolve and remove.
    pub fn open_with_clock(
        root: impl AsRef<Path>,
        clock: StoreClock,
    ) -> Result<Self, SessionStoreError> {
        let root = root.as_ref();
        private_dir(&root.join(APPROVALS_DIR)).map_err(connect_err)?;
        private_dir(&root.join(DECISIONS_DIR)).map_err(connect_err)?;
        let inner = Arc::new(Inner {
            root: root.to_path_buf(),
            lock: Mutex::new(()),
            path_locks: Mutex::new(HashMap::new()),
            clock,
        });
        inner.probe_writable_sync().map_err(connect_err)?;
        Ok(Self { inner })
    }

    /// Re-run the writability probe off the executor thread.
    pub async fn probe_writable(&self) -> Result<(), SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        spawn_blocking(move || inner.probe_writable_sync().map_err(request_err))
            .await
            .map_err(join_err)?
    }
}

impl Inner {
    fn approvals_dir(&self) -> PathBuf {
        self.root.join(APPROVALS_DIR)
    }

    fn decisions_dir(&self) -> PathBuf {
        self.root.join(DECISIONS_DIR)
    }

    fn approval_path(&self, id: &str) -> PathBuf {
        self.approvals_dir().join(format!("{id}.json"))
    }

    fn decision_path(&self, id: &str) -> PathBuf {
        self.decisions_dir().join(format!("{id}.json"))
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().expect("file approval store lock poisoned")
    }

    /// Run `f` holding the per-path lock for `path` — the primitive that
    /// makes a destructive commit indivisible against same-instance
    /// publishes. The map lock guards only the lookup; it is released
    /// before `f` runs, so unrelated paths never wait on each other.
    fn with_path_lock<R>(&self, path: &Path, f: impl FnOnce() -> R) -> R {
        let lock = {
            let mut map = self
                .path_locks
                .lock()
                .expect("file approval store path-lock map poisoned");
            Arc::clone(
                map.entry(path.to_path_buf())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = lock.lock().expect("file approval store path lock poisoned");
        f()
    }

    /// Create and unlink an empty probe file in each store directory. This
    /// catches permission and mount faults, not a full disk.
    fn probe_writable_sync(&self) -> io::Result<()> {
        for dir in [self.approvals_dir(), self.decisions_dir()] {
            let probe = dir.join(format!(".{}.probe", uuid::Uuid::new_v4()));
            write_private(&probe, b"")
                .and_then(|()| fs::remove_file(&probe))
                .map_err(|err| {
                    io::Error::new(err.kind(), format!("{} not writable: {err}", dir.display()))
                })?;
        }
        Ok(())
    }

    fn register_sync(&self, parked: ParkedApproval) -> Result<(), SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(&parked.request.decision_id)?;
        let payload = serde_json::to_vec(&ParkedApprovalRecord::from(&parked))
            .expect("approval record serializes to JSON");
        let path = self.approval_path(&id);
        self.with_path_lock(&path, || publish(&path, &payload))
    }

    fn mark_acknowledged_sync(
        &self,
        id: &DecisionId,
    ) -> Result<AcknowledgeOutcome, SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(id)?;
        let path = self.approval_path(&id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            // No approval file: unknown, resolved, removed, or cancelled all
            // read as `Missing`; a missing row is never recreated.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(AcknowledgeOutcome::Missing);
            }
            Err(err) => return Err(request_err(err)),
        };
        // resolve writes the decision before its best-effort approval unlink,
        // so a decision file here marks an already-decided id: the row is no
        // longer pending, and the stale approval file is left untouched.
        if self.decision_path(&id).try_exists().map_err(request_err)? {
            return Ok(AcknowledgeOutcome::Missing);
        }
        // Read-modify-write on the persisted record: only `acknowledgment`
        // changes; every other field round-trips unchanged.
        let mut record: ParkedApprovalRecord =
            serde_json::from_slice(&bytes).map_err(decode_err)?;
        record.acknowledgment = AcknowledgmentState::Acknowledged;
        let payload = serde_json::to_vec(&record).expect("approval record serializes to JSON");
        publish(&path, &payload)?;
        Ok(AcknowledgeOutcome::Acknowledged)
    }

    fn get_sync(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(id)?;
        match fs::read(self.approval_path(&id)) {
            Ok(bytes) => return decode_approval(&bytes).map(Some),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(request_err(err)),
        }
        match fs::read(self.decision_path(&id)) {
            Ok(bytes) => {
                let entry: ResolvedEntry = serde_json::from_slice(&bytes).map_err(decode_err)?;
                restore_approval(entry.approval).map(Some)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(request_err(err)),
        }
    }

    fn resolve_sync(
        &self,
        id: &DecisionId,
        // The authority check runs under this same lock; the signature
        // carries it so no caller can bolt a validate-then-resolve race
        // ahead of it.
        expected_authority: ApprovalAuthority,
        decision: ResolvedDecision,
    ) -> Result<(), ResolveError> {
        let _guard = self.lock();
        let id = canonical_id(id).map_err(ResolveError::Store)?;

        // Reading the approval before claiming avoids claiming unknown ids.
        let mut record = match fs::read(self.approval_path(&id)) {
            Ok(bytes) => serde_json::from_slice::<ParkedApprovalRecord>(&bytes)
                .map_err(|e| ResolveError::Store(decode_err(e)))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // No approval: unknown, resolved, or removed all return `NotFound`.
                return Err(ResolveError::NotFound);
            }
            Err(err) => return Err(ResolveError::Store(request_err(err))),
        };
        // Wrong authority is indistinguishable from unknown: the row stays
        // parked in `list_pending` and nothing is written.
        if record.authority != expected_authority {
            return Err(ResolveError::NotFound);
        }
        if (self.clock)() > record.expires_at {
            return Err(ResolveError::NotFound);
        }
        record.egress_headers = None;
        let payload = serde_json::to_vec(&ResolvedEntry {
            approval: record,
            decision: TerminalRecord::from(&decision),
        })
        .expect("resolved entry serializes to JSON");

        let decision_path = self.decision_path(&id);
        let mut file = match private_file()
            .write(true)
            .create_new(true)
            .open(&decision_path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ResolveError::NotFound);
            }
            Err(err) => return Err(ResolveError::Store(request_err(err))),
        };
        // Sync narrows the empty-file crash window but does not guarantee
        // host-crash durability.
        if let Err(err) = file.write_all(&payload).and_then(|()| file.sync_all()) {
            // Undo claim so retry can take it.
            let _ = fs::remove_file(&decision_path);
            return Err(ResolveError::Store(request_err(err)));
        }
        // After sync commit, removing the approval file is best-effort.
        // Failure leaves a stale approval file; resolve succeeded. The
        // store op lock already serializes this unlink against
        // registration, so no per-path lock is involved — a real destroy
        // here needs only the entry eviction.
        let approval_path = self.approval_path(&id);
        match fs::remove_file(&approval_path) {
            Ok(()) => self.evict_path_lock(&approval_path),
            // A NotFound miss is another op's destroy; that destroyer's
            // own eviction covered the map entry.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                decision_id = %id,
                error = %err,
                "stale approval file remains after resolve; it is benign"
            ),
        }
        Ok(())
    }

    fn decision_sync(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ResolvedDecision>, SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(id)?;
        match fs::read(self.decision_path(&id)) {
            Ok(bytes) => {
                let entry: ResolvedEntry = serde_json::from_slice(&bytes).map_err(decode_err)?;
                match entry.decision.into_decision_record() {
                    // A timed-out row records no decision: the timeout
                    // surfaces through read_or_expire's addressed arm,
                    // never as an outcome here.
                    None => Ok(None),
                    Some(decision) => ResolvedDecision::try_from(decision)
                        .map(Some)
                        .map_err(decode_err),
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(request_err(err)),
        }
    }

    fn remove_sync(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(id)?;
        // Remove both halves; missing halves are fine (idempotent). The
        // op lock already serializes these unlinks against registration,
        // so a remove here needs only the entry eviction — the decision
        // half never materializes an entry, making its evict a no-op.
        for path in [self.approval_path(&id), self.decision_path(&id)] {
            match fs::remove_file(&path) {
                Ok(()) => self.evict_path_lock(&path),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(request_err(err)),
            }
        }
        Ok(())
    }

    fn cancel_request_sync(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let _guard = self.lock();
        let entries = match fs::read_dir(self.approvals_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(request_err(err)),
        };

        // Phase 1 classifies without mutating, so a fault past an earlier
        // file cannot strand an already-removed approval.
        let mut candidates = Vec::new();
        let mut stale_decided = Vec::new();
        for entry in entries {
            let entry = entry.map_err(request_err)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                // A mid-publish temp file, never a stored approval.
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(request_err(err)),
            };
            let parked = match decode_approval(&bytes) {
                Ok(parked) => parked,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(), error = %err,
                        "undecodable approval file skipped by cancel_request"
                    );
                    continue;
                }
            };
            if parked.request.request_id != request_id {
                continue;
            }
            // resolve writes the decision before its best-effort approval
            // unlink, so a decision file here marks the residue of an
            // already-decided id: the recorded decision owns the outcome.
            if self
                .decision_path(&parked.request.decision_id.to_string())
                .try_exists()
                .map_err(request_err)?
            {
                stale_decided.push((path, bytes));
                continue;
            }
            candidates.push((path, parked, bytes));
        }

        // Phase 2 removes; each removal rechecks the file still holds the
        // bytes phase 1 decoded, so a registration that rewrote the path
        // mid-cancel keeps its record for a later cancel. A file that
        // survives drops its record from the returned set so a later
        // cancel can clear it again.
        let mut cleared = Vec::new();
        for (path, parked, bytes) in candidates {
            match self.unlink_if_unchanged(&path, &bytes) {
                Ok(true) => cleared.push(parked),
                Ok(false) => tracing::warn!(
                    path = %path.display(), decision_id = %parked.request.decision_id,
                    "approval file changed under cancel_request; a later cancel can clear it"
                ),
                Err(err) => tracing::warn!(
                    path = %path.display(), decision_id = %parked.request.decision_id, error = %err,
                    "approval file not removed by cancel_request; a later cancel can clear it"
                ),
            }
        }
        for (path, bytes) in stale_decided {
            if let Err(err) = self.unlink_if_unchanged(&path, &bytes) {
                tracing::warn!(
                    path = %path.display(), error = %err,
                    "stale decided approval file not removed by cancel_request"
                );
            }
        }
        Ok(cleared)
    }

    /// Strict variant of [`Inner::cancel_request_sync`]: any fault while
    /// reading, classifying, or unlinking an approval turns into a store
    /// error so the caller knows the cancellation may be incomplete.
    fn cancel_request_strict_sync(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let _guard = self.lock();
        let entries = match fs::read_dir(self.approvals_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(request_err(err)),
        };

        let mut candidates = Vec::new();
        let mut stale_decided = Vec::new();
        for entry in entries {
            let entry = entry.map_err(request_err)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(request_err(err)),
            };
            let parked = match decode_approval(&bytes) {
                Ok(parked) => parked,
                Err(err) => {
                    return Err(decode_err(format!(
                        "approval file {} is undecodable and blocks strict cancellation: {err}",
                        path.display()
                    )));
                }
            };
            if parked.request.request_id != request_id {
                continue;
            }
            if self
                .decision_path(&parked.request.decision_id.to_string())
                .try_exists()
                .map_err(request_err)?
            {
                stale_decided.push((path, bytes));
                continue;
            }
            candidates.push((path, parked, bytes));
        }

        let mut cleared = Vec::new();
        for (path, parked, bytes) in candidates {
            match self.unlink_if_unchanged(&path, &bytes) {
                Ok(true) => cleared.push(parked),
                Ok(false) => {
                    return Err(request_err(format!(
                        "approval file {} changed under strict cancellation",
                        path.display()
                    )));
                }
                Err(err) => return Err(request_err(err)),
            }
        }
        for (path, bytes) in stale_decided {
            match self.unlink_if_unchanged(&path, &bytes) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(request_err(format!(
                        "stale approval file {} changed under strict cancellation",
                        path.display()
                    )));
                }
                Err(err) => return Err(request_err(err)),
            }
        }
        Ok(cleared)
    }

    fn list_pending_sync(&self) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let paths: Vec<PathBuf> = {
            let _guard = self.lock();
            let entries = fs::read_dir(self.approvals_dir()).map_err(request_err)?;
            let mut paths = Vec::new();
            for entry in entries {
                let entry = entry.map_err(request_err)?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    // A mid-publish temp file, never a stored approval.
                    continue;
                }
                paths.push(path);
            }
            paths
        };
        let now = (self.clock)();

        let mut pending = Vec::new();
        for path in paths {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(request_err(err)),
            };
            let parked = match decode_approval(&bytes) {
                Ok(parked) => parked,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(), error = %err,
                        "undecodable approval file skipped by list_pending"
                    );
                    continue;
                }
            };
            // resolve writes the decision before its best-effort approval
            // unlink, so a decision file here marks an already-decided id:
            // the reconciler must not re-poll it. A decision file that
            // decodes as a complete entry makes the approval file residue
            // (which carries the row's credentials), retried for removal on
            // every scan until it is gone; a decision file that does not
            // decode (a write interrupted before its sync) leaves the
            // approval file in place as the only intact record.
            let decision_path = self.decision_path(&parked.request.decision_id.to_string());
            match fs::read(&decision_path) {
                Ok(decision_bytes) => {
                    if serde_json::from_slice::<ResolvedEntry>(&decision_bytes).is_ok() {
                        if let Err(err) = self.unlink_if_unchanged(&path, &bytes) {
                            tracing::warn!(
                                path = %path.display(), error = %err,
                                "decided approval file not removed by list_pending"
                            );
                        }
                    } else {
                        tracing::warn!(
                            path = %decision_path.display(),
                            "incomplete decision file; approval file kept for recovery"
                        );
                    }
                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(request_err(err)),
            }
            // An expired undecided row is filtered from the scan and
            // LEFT IN PLACE: the read-or-expire consult is the one
            // boundary that terminalizes it, writing the durable
            // `TimedOut` record and stripping the row's credentials in
            // the same ceremony resolve uses. Once that decision file
            // exists, the decided-residue arm above cleans the approval
            // file on every later scan — so no credential outlives the
            // consult — while evidence cannot be erased ahead of the
            // resume that must publish the timeout (F02). Eventual
            // terminal-record cleanup belongs to retention, not here.
            if parked.expires_at > now {
                pending.push(parked);
            } else {
                tracing::debug!(
                    decision_id = %parked.request.decision_id,
                    "expired approval kept for the read-or-expire consult"
                );
            }
        }
        Ok(pending)
    }

    /// One read answers Missing / Pending / Addressed under the same lock
    /// resolve and remove hold. Authority is checked on both the pending
    /// and the decided path wherever the row is read, so a wrong channel
    /// reads as missing with no mutation.
    fn read_or_expire_sync(
        &self,
        id: &DecisionId,
        expected_authority: ApprovalAuthority,
    ) -> Result<ApprovalRead, SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(id)?;

        // Terminal-winner precedence: a decidable decision file owns the
        // outcome wherever one exists — a stale approval residue beside it
        // never re-enters the pending path — and an undecodable one is a
        // decode fault, never a fabricated Missing.
        match fs::read(self.decision_path(&id)) {
            Ok(bytes) => {
                let entry: ResolvedEntry = serde_json::from_slice(&bytes).map_err(decode_err)?;
                if entry.approval.authority != expected_authority {
                    return Ok(ApprovalRead::Missing);
                }
                let ResolvedEntry { approval, decision } = entry;
                let restored = restore_approval(approval)?;
                let outcome = AddressedApproval::try_from(decision).map_err(decode_err)?;
                return Ok(ApprovalRead::Addressed {
                    approval: restored,
                    outcome,
                });
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(request_err(err)),
        }

        let mut record = match fs::read(self.approval_path(&id)) {
            Ok(bytes) => {
                serde_json::from_slice::<ParkedApprovalRecord>(&bytes).map_err(decode_err)?
            }
            // No approval file: unknown, resolved, removed, or cancelled all
            // read as `Missing`; a missing row is never recreated.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ApprovalRead::Missing);
            }
            Err(err) => return Err(request_err(err)),
        };
        // Wrong authority is indistinguishable from unknown: the row stays
        // parked and nothing is written.
        if record.authority != expected_authority {
            return Ok(ApprovalRead::Missing);
        }
        // The deadline rule is strictly past: a read sampled exactly at the
        // row's own deadline is still pending. The injected clock is
        // sampled once, under the same lock resolve and remove hold.
        if (self.clock)() <= record.expires_at {
            return restore_approval(record).map(ApprovalRead::Pending);
        }

        // Expire, in resolve's exact ceremony: the egress-stripped record
        // moves into a durable tagged `TimedOut` decision file — the
        // create_new claim commits at-most-once, sync narrows the empty-
        // file crash window, and the approval unlink past the commit is
        // best-effort. The complete record restores BEFORE any credential
        // strip: a corrupt egress header is the stored row's decode fault
        // returned with no mutation, never erased into a durable timeout.
        let mut restored = restore_approval(record.clone())?;
        restored.egress_headers = None;
        record.egress_headers = None;
        let deadline = record.expires_at;
        let payload = serde_json::to_vec(&ResolvedEntry {
            approval: record,
            decision: TerminalRecord::TimedOut { deadline },
        })
        .expect("resolved entry serializes to JSON");
        let decision_path = self.decision_path(&id);
        let mut file = match private_file()
            .write(true)
            .create_new(true)
            .open(&decision_path)
        {
            Ok(file) => file,
            // A terminal winner already exists: the decision file owns the
            // outcome — fall through to reading it instead of re-expiring.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let bytes = fs::read(&decision_path).map_err(request_err)?;
                let winner: ResolvedEntry = serde_json::from_slice(&bytes).map_err(decode_err)?;
                if winner.approval.authority != expected_authority {
                    return Ok(ApprovalRead::Missing);
                }
                let ResolvedEntry { approval, decision } = winner;
                let approval = restore_approval(approval)?;
                let outcome = AddressedApproval::try_from(decision).map_err(decode_err)?;
                return Ok(ApprovalRead::Addressed { approval, outcome });
            }
            Err(err) => return Err(request_err(err)),
        };
        if let Err(err) = file.write_all(&payload).and_then(|()| file.sync_all()) {
            // Undo claim so retry can take it.
            let _ = fs::remove_file(&decision_path);
            return Err(request_err(err));
        }
        // After sync commit, removing the approval file is best-effort.
        match fs::remove_file(self.approval_path(&id)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                decision_id = %id,
                error = %err,
                "stale approval file remains after read-or-expire; it is benign"
            ),
        }
        Ok(ApprovalRead::Addressed {
            approval: restored,
            outcome: AddressedApproval::TimedOut { deadline },
        })
    }

    /// The retained-evidence scan, side-effect-free: nothing is unlinked
    /// and no clock is sampled — retention, not decidability, is the
    /// question, so decided and past-window rows stay in the scan.
    fn retained_rows_sync(&self) -> Result<Vec<super::RetainedApproval>, SessionStoreError> {
        let _guard = self.lock();
        let mut rows = Vec::new();

        // Decisions/ first: a decidable decision file owns its id's
        // classification, so its approval residue never scans as pending.
        // Undecodable files are warn-and-skipped, never fatal to the scan.
        let decisions = match fs::read_dir(self.decisions_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(rows),
            Err(err) => return Err(request_err(err)),
        };
        for entry in decisions {
            let entry = entry.map_err(request_err)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                // A mid-publish temp file, never a stored decision.
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(request_err(err)),
            };
            let addressed: ResolvedEntry = match serde_json::from_slice(&bytes) {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(), error = %err,
                        "undecodable decision file skipped by retained_rows"
                    );
                    continue;
                }
            };
            let ResolvedEntry { approval, decision } = addressed;
            let approval = match restore_approval(approval) {
                Ok(approval) => approval,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(), error = %err,
                        "unrestorable approval side of a decision file skipped by retained_rows"
                    );
                    continue;
                }
            };
            let outcome = match AddressedApproval::try_from(decision) {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(), error = %err,
                        "undecodable decision record skipped by retained_rows"
                    );
                    continue;
                }
            };
            rows.push(super::RetainedApproval::Addressed { approval, outcome });
        }

        // Approvals/: a row without a decidable decision file is pending,
        // inside or past its own window alike. Missing directories are
        // fine (an empty store retains nothing).
        let approvals = match fs::read_dir(self.approvals_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(rows),
            Err(err) => return Err(request_err(err)),
        };
        for entry in approvals {
            let entry = entry.map_err(request_err)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(request_err(err)),
            };
            let parked = match decode_approval(&bytes) {
                Ok(parked) => parked,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(), error = %err,
                        "undecodable approval file skipped by retained_rows"
                    );
                    continue;
                }
            };
            // Residue beside a decidable decision file was already counted
            // from that file; a decision file that is missing or undecodable
            // leaves the approval the pending row — the only intact record.
            let decision_path = self.decision_path(&parked.request.decision_id.to_string());
            match fs::read(&decision_path) {
                Ok(decision_bytes) => {
                    if serde_json::from_slice::<ResolvedEntry>(&decision_bytes).is_ok() {
                        continue;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(request_err(err)),
            }
            rows.push(super::RetainedApproval::Pending(parked));
        }
        Ok(rows)
    }
}

#[async_trait]
impl ApprovalStore for FileApprovalStore {
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        spawn_blocking(move || inner.register_sync(parked))
            .await
            .map_err(join_err)?
    }

    async fn mark_acknowledged(
        &self,
        id: &DecisionId,
    ) -> Result<AcknowledgeOutcome, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let id = *id;
        spawn_blocking(move || inner.mark_acknowledged_sync(&id))
            .await
            .map_err(join_err)?
    }

    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let id = *id;
        spawn_blocking(move || inner.get_sync(&id))
            .await
            .map_err(join_err)?
    }

    async fn resolve(
        &self,
        id: &DecisionId,
        expected_authority: ApprovalAuthority,
        decision: ResolvedDecision,
    ) -> Result<(), ResolveError> {
        let inner = Arc::clone(&self.inner);
        let id = *id;
        spawn_blocking(move || inner.resolve_sync(&id, expected_authority, decision))
            .await
            .map_err(|err| ResolveError::Store(join_err(err)))?
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ResolvedDecision>, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let id = *id;
        spawn_blocking(move || inner.decision_sync(&id))
            .await
            .map_err(join_err)?
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let id = *id;
        spawn_blocking(move || inner.remove_sync(&id))
            .await
            .map_err(join_err)?
    }

    async fn cancel_request(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let request_id = request_id.to_owned();
        spawn_blocking(move || inner.cancel_request_sync(&request_id))
            .await
            .map_err(join_err)?
    }

    async fn cancel_request_strict(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let request_id = request_id.to_owned();
        spawn_blocking(move || inner.cancel_request_strict_sync(&request_id))
            .await
            .map_err(join_err)?
    }

    async fn list_pending(&self) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        spawn_blocking(move || inner.list_pending_sync())
            .await
            .map_err(join_err)?
    }

    async fn read_or_expire(
        &self,
        id: &DecisionId,
        expected_authority: ApprovalAuthority,
    ) -> Result<ApprovalRead, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let id = *id;
        spawn_blocking(move || inner.read_or_expire_sync(&id, expected_authority))
            .await
            .map_err(join_err)?
    }

    async fn retained_rows(&self) -> Result<Vec<super::RetainedApproval>, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        spawn_blocking(move || inner.retained_rows_sync())
            .await
            .map_err(join_err)?
    }
}

/// Validate decision id as canonical UUID for path safety.
fn canonical_id(id: &DecisionId) -> Result<String, SessionStoreError> {
    let raw = id.to_string();
    if uuid::Uuid::parse_str(&raw).is_ok_and(|parsed| parsed.to_string() == raw) {
        Ok(raw)
    } else {
        Err(SessionStoreError::Request {
            reason: format!("decision id '{raw}' is not in canonical UUID form"),
        })
    }
}

/// Write via temp-file plus rename: atomic within directory.
fn publish(path: &Path, payload: &[u8]) -> Result<(), SessionStoreError> {
    let dir = path.parent().expect("a store file always has a parent");
    let name = path.file_name().expect("a store file is always named");
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let written = write_private(&tmp, payload).and_then(|()| fs::rename(&tmp, path));
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(request_err(err));
    }
    Ok(())
}

impl Inner {
    /// Unlink `path` only if it still holds exactly `expected`; `Ok(true)`
    /// means the file was unlinked here. The compare is redone under the
    /// per-path lock, so a same-instance registration landing between
    /// compare and remove survives. A vanished path was destroyed by
    /// someone whose own eviction may have been refcount-blocked, so every
    /// vanished-path observation retries the eviction.
    fn unlink_if_unchanged(&self, path: &Path, expected: &[u8]) -> io::Result<bool> {
        match compare_bytes(path, expected)? {
            ByteCompare::Unchanged => {}
            ByteCompare::Changed => return Ok(false),
            ByteCompare::Vanished => {
                self.evict_path_lock(path);
                return Ok(false);
            }
        }
        #[cfg(test)]
        unlink_interleave::fire(path);
        let (unlinked, destroyed) =
            self.with_path_lock(path, || match compare_bytes(path, expected)? {
                ByteCompare::Unchanged => match fs::remove_file(path) {
                    Ok(()) => Ok((true, true)),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => Ok((false, true)),
                    Err(err) => Err(err),
                },
                ByteCompare::Changed => Ok((false, false)),
                ByteCompare::Vanished => Ok((false, true)),
            })?;
        if destroyed {
            self.evict_path_lock(path);
        }
        Ok(unlinked)
    }

    /// Drop a destroyed path's lock entry when only the map itself still
    /// holds it. The count is read and the entry removed in one map-lock
    /// critical section — the same lock every waiter clones under — so a
    /// count of one is stable: no waiter is in flight and the entry can
    /// go, while more means a waiter holds a clone across its critical
    /// section and the entry stays for a later destroy to evict. Because
    /// clones and removal are serialized on the same lock, an entry
    /// removed at count one leaves its mutex unreachable — every later
    /// lookup inserts a fresh lock instead of joining a dead one, so two
    /// locks for one path never coexist.
    fn evict_path_lock(&self, path: &Path) {
        let mut map = self
            .path_locks
            .lock()
            .expect("file approval store path-lock map poisoned");
        if map
            .get(path)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            map.remove(path);
        }
    }
}

/// What an identity compare found at `path` against the bytes a sweep
/// decoded.
enum ByteCompare {
    Unchanged,
    Changed,
    Vanished,
}

/// Compare `path`'s current bytes against `expected`, keeping a
/// changed-but-alive file distinct from a vanished one — the two demand
/// opposite lock-entry outcomes (keep vs evict), so NotFound must never
/// collapse into "changed".
fn compare_bytes(path: &Path, expected: &[u8]) -> io::Result<ByteCompare> {
    match fs::read(path) {
        Ok(current) if current == expected => Ok(ByteCompare::Unchanged),
        Ok(_) => Ok(ByteCompare::Changed),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(ByteCompare::Vanished),
        Err(err) => Err(err),
    }
}

/// Deterministic interleaving point for the sweep's destructive boundary:
/// the moment between `unlink_if_unchanged`'s unlocked identity compare
/// and its per-path-locked commit, where a registration can publish fresh
/// bytes at the swept path. A test-installed hook fires there, on the
/// calling thread. The hook may take the per-path lock — it is not yet
/// held — but must not re-enter the store's operation lock:
/// `list_pending` reaches the boundary lock-free, `cancel_request` holds
/// that lock across its unlinks.
#[cfg(test)]
mod unlink_interleave {
    use std::path::Path;
    use std::sync::Mutex;

    type Hook = Box<dyn Fn(&Path) + Send>;

    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);

    /// Uninstalls the hook on drop, so a failed test cannot leak it into
    /// the rest of the battery.
    pub(super) struct HookGuard;

    impl Drop for HookGuard {
        fn drop(&mut self) {
            *HOOK.lock().expect("unlink interleave slot poisoned") = None;
        }
    }

    /// Fire `hook` at every compare/remove boundary until the returned
    /// guard drops.
    pub(super) fn install(hook: Hook) -> HookGuard {
        *HOOK.lock().expect("unlink interleave slot poisoned") = Some(hook);
        HookGuard
    }

    pub(super) fn fire(path: &Path) {
        // The hook runs outside the slot lock so a boundary reached from
        // inside a hook nests instead of deadlocking.
        let hook = HOOK.lock().expect("unlink interleave slot poisoned").take();
        if let Some(hook) = hook {
            hook(path);
            *HOOK.lock().expect("unlink interleave slot poisoned") = Some(hook);
        }
    }
}

/// Open options that create files readable by the owner only.
fn private_file() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Write `payload` to a new or truncated owner-only file. A file that
/// already exists at `path` is tightened to owner-only after truncation
/// and before the payload is written, so a permissive leftover never
/// holds new content.
pub(crate) fn write_private(path: &Path, payload: &[u8]) -> io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut file = private_file()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    #[cfg(unix)]
    file.set_permissions(<fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600))?;
    file.write_all(payload)
}

/// Create `path` and any missing parents as owner-only directories. A
/// `path` that already exists is tightened to owner-only, so a directory
/// created under a permissive umask by an earlier version stops exposing
/// the records inside it the next time the store or park path opens.
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)?;
    #[cfg(unix)]
    fs::set_permissions(
        path,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )?;
    Ok(())
}

/// Decode a stored approval file.
fn decode_approval(bytes: &[u8]) -> Result<ParkedApproval, SessionStoreError> {
    let record: ParkedApprovalRecord = serde_json::from_slice(bytes).map_err(decode_err)?;
    restore_approval(record)
}

/// Restore an approval record.
fn restore_approval(record: ParkedApprovalRecord) -> Result<ParkedApproval, SessionStoreError> {
    ParkedApproval::try_from(record).map_err(decode_err)
}

fn connect_err(reason: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Connect {
        reason: reason.to_string(),
    }
}

fn request_err(reason: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Request {
        reason: reason.to_string(),
    }
}

fn decode_err(reason: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Decode {
        reason: reason.to_string(),
    }
}

/// Join failure maps to store error.
fn join_err(err: JoinError) -> SessionStoreError {
    SessionStoreError::Request {
        reason: format!("file store task failed: {err}"),
    }
}

#[cfg(all(test, unix))]
mod private_mode_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{private_dir, write_private};

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_dir_tightens_an_existing_permissive_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("approvals");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mode_of(&target), 0o755, "fixture is permissive");

        private_dir(&target).unwrap();

        assert_eq!(mode_of(&target), 0o700);
    }

    #[test]
    fn write_private_tightens_a_permissive_leftover_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".leftover.tmp");
        std::fs::write(&target, b"stale").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode_of(&target), 0o644, "fixture is permissive");

        write_private(&target, b"fresh").unwrap();

        assert_eq!(mode_of(&target), 0o600);
        assert_eq!(std::fs::read(&target).unwrap(), b"fresh");
    }
}

/// The concurrent half of the sweep's destructive contract; the
/// end-to-end recovery half is the aura-web-server battery's
/// `stale_sweep_cannot_unlink_a_replaced_record`. Every sweep-driven
/// unlink checks the file against the bytes the sweep decoded
/// (`unlink_if_unchanged`, the exact code path the scan's residue
/// unlink takes; the expiry arm keeps evidence for the consult), so
/// the interleaving — stale record read, replace with a fresh record,
/// unlink — is driven directly through that seam.
#[cfg(test)]
mod unlink_recheck_tests {
    use std::sync::Arc;

    use crate::hitl::{
        AgentScope, ApprovalAuthority, ApprovalDecision, ApprovalItem, ApprovalOrigin,
        ApprovalRequest, DecisionId, PROTOCOL_VERSION, ParkedApproval, ResolvedDecision,
    };
    use crate::session_store::{ApprovalStore, ParkedApprovalRecord, SessionStoreError};

    use super::{FileApprovalStore, TerminalRecord, unlink_interleave};

    /// A representative parked approval for `decision_id`, expiring far
    /// out — the battery fixture's shape.
    fn make_parked(decision_id: DecisionId) -> ParkedApproval {
        let now = chrono::Utc::now();
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "test-instance".to_string(),
                decision_id,
                request_id: "req-replaced".to_string(),
                scope: AgentScope::Single { session_id: None },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "kubectl_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "kubectl_delete".to_string(),
                    tool_namespace: None,
                    arguments: serde_json::json!({"pod": "web-1"}),
                    tool_call_intent: Some("restarting to pick up the config change".to_string()),
                }],
            },
            registered_at: now,
            expires_at: now + chrono::Duration::hours(1),
            authority: ApprovalAuthority::WebhookPoll,
            egress_headers: None,
            acknowledgment: crate::hitl::AcknowledgmentState::RequiresNotification,
        }
    }

    /// Publish the durable terminal record for `parked` under `dir` —
    /// the state the scan's decided-residue arm keys on. Once the
    /// decision exists, the approval file is residue the sweep unlinks;
    /// the expiry arm alone keeps evidence for the consult.
    fn write_terminal_record(dir: &tempfile::TempDir, parked: &ParkedApproval) {
        let entry = super::ResolvedEntry {
            approval: ParkedApprovalRecord::from(parked),
            decision: TerminalRecord::TimedOut {
                deadline: parked.expires_at,
            },
        };
        let decisions = dir.path().join("decisions");
        std::fs::create_dir_all(&decisions).unwrap();
        std::fs::write(
            decisions.join(format!("{}.json", parked.request.decision_id)),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_stale_sweep_snapshot_cannot_unlink_a_replaced_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(dir.path()).unwrap();
        let id = DecisionId::generate();

        // The snapshot a sweep decoded, and the racing registration that
        // rewrites the same path with a fresh record.
        store.register(make_parked(id)).await.unwrap();
        let path = dir.path().join("approvals").join(format!("{id}.json"));
        let stale = std::fs::read(&path).unwrap();
        store.register(make_parked(id)).await.unwrap();
        let current = std::fs::read(&path).unwrap();
        assert_ne!(
            stale, current,
            "fixture: the racing registration rewrote it"
        );

        assert!(
            !store.inner.unlink_if_unchanged(&path, &stale).unwrap(),
            "a stale snapshot must not unlink the replaced record"
        );
        assert!(path.exists(), "the replaced record is retained");
        assert!(
            store.inner.unlink_if_unchanged(&path, &current).unwrap(),
            "the identity the file holds still unlinks"
        );
        assert!(!path.exists(), "the matched record is unlinked");
    }

    /// The boundary the recheck alone cannot close: a registration that
    /// publishes fresh bytes at the swept path after the identity compare
    /// but before the remove owns the path, so its record must survive —
    /// the sweep's unlink decision was made from the stale bytes. The
    /// interleaving is forced through the test seam at that exact
    /// boundary, single-threaded, same store instance: no timing luck.
    #[test]
    fn a_registration_landing_between_compare_and_remove_survives_the_sweep() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(dir.path()).unwrap();
        let id = DecisionId::generate();

        // The record the sweep will decide to unlink: expired and
        // already terminalized, so the scan's decided-residue arm owns
        // its unlink (the expiry arm keeps evidence for the consult).
        let mut expired = make_parked(id);
        expired.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        store.inner.register_sync(expired.clone()).unwrap();
        write_terminal_record(&dir, &expired);
        let path = dir.path().join("approvals").join(format!("{id}.json"));
        let stale = std::fs::read(&path).unwrap();

        // The racing registration: a live record at the same path, published
        // by the same store instance exactly while the sweep sits between
        // its compare and its remove.
        let fresh = make_parked(id);
        let fresh_payload = serde_json::to_vec(&ParkedApprovalRecord::from(&fresh)).unwrap();
        assert_ne!(
            stale, fresh_payload,
            "fixture: the racing registration publishes different bytes"
        );
        let fired = Arc::new(AtomicBool::new(false));
        let registrar = Arc::clone(&store.inner);
        let swept_path = path.clone();
        let to_publish = fresh.clone();
        let fired_flag = Arc::clone(&fired);
        let _boundary = unlink_interleave::install(Box::new(move |boundary: &std::path::Path| {
            // Other tests reach this boundary concurrently; only the
            // swept path belongs to this interleaving.
            if boundary != swept_path.as_path() {
                return;
            }
            // The hook contract is repeatable; this pin fires it once.
            registrar.register_sync(to_publish.clone()).unwrap();
            fired_flag.store(true, Ordering::SeqCst);
        }));

        store.inner.list_pending_sync().unwrap();

        assert!(
            fired.load(Ordering::SeqCst),
            "fixture: the boundary hook ran"
        );
        let on_disk = std::fs::read(&path)
            .expect("the registration published mid-compare survives the sweep's unlink");
        assert_eq!(
            on_disk, fresh_payload,
            "the surviving record is the fresh registration, not the swept stale one"
        );
    }

    /// The destroy that evicts: a registration materializes its path's
    /// lock entry, and the residue sweep's commit destroys the decided
    /// record and evicts the entry with it, so the map holds nothing
    /// beyond the approvals actually parked. Single-threaded end to
    /// end — the refcount is one at eviction because nothing else holds
    /// a clone.
    #[test]
    fn a_sweep_destroy_evicts_the_destroyed_path_lock_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(dir.path()).unwrap();
        let id = DecisionId::generate();

        let mut expired = make_parked(id);
        expired.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        store.inner.register_sync(expired.clone()).unwrap();
        write_terminal_record(&dir, &expired);
        let path = dir.path().join("approvals").join(format!("{id}.json"));
        assert!(
            store
                .inner
                .path_locks
                .lock()
                .expect("file approval store path-lock map poisoned")
                .contains_key(&path),
            "fixture: the registration materialized the path's lock entry"
        );

        let pending = store.inner.list_pending_sync().unwrap();

        assert!(
            pending.is_empty(),
            "fixture: the expired record is not pending"
        );
        assert!(
            !path.exists(),
            "fixture: the sweep destroyed the expired record"
        );
        assert!(
            store
                .inner
                .path_locks
                .lock()
                .expect("file approval store path-lock map poisoned")
                .is_empty(),
            "the destroyed path's lock entry is evicted"
        );
    }

    /// The leak the vanished-path compares close: a destroyer whose
    /// eviction is refcount-blocked by an in-flight waiter clone leaves
    /// the path's lock entry behind, and the trailing waiter that next
    /// observes the path — now vanished — retries the eviction. Forced
    /// deterministically, single-threaded: the waiter is a clone held
    /// straight off the map, the destroyer is the sweep through
    /// `list_pending_sync`, and the trailing waiter is the same commit
    /// seam re-run against the gone path, where the lock-free first
    /// compare is what observes the vanish.
    #[test]
    fn a_trailing_waiter_on_a_vanished_path_retries_the_blocked_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(dir.path()).unwrap();
        let id = DecisionId::generate();

        let mut expired = make_parked(id);
        expired.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        store.inner.register_sync(expired.clone()).unwrap();
        write_terminal_record(&dir, &expired);
        let path = dir.path().join("approvals").join(format!("{id}.json"));
        let snapshot = std::fs::read(&path).unwrap();

        // The in-flight waiter: a clone of the path's lock entry held
        // across the destroyer's commit, keeping the refcount above one.
        let held = store
            .inner
            .path_locks
            .lock()
            .expect("file approval store path-lock map poisoned")
            .get(&path)
            .cloned()
            .expect("fixture: the registration materialized the path's lock entry");

        store.inner.list_pending_sync().unwrap();

        assert!(!path.exists(), "fixture: the sweep destroyed the record");
        assert!(
            store
                .inner
                .path_locks
                .lock()
                .expect("file approval store path-lock map poisoned")
                .contains_key(&path),
            "fixture: the held clone refcount-blocked the destroy's eviction"
        );

        // The trailing waiter: it decoded the record before the destroy
        // and now commits against a path that no longer exists. The
        // vanished-path observation carries destroy semantics and evicts
        // what the blocked destroy left behind.
        drop(held);
        let unlinked = store.inner.unlink_if_unchanged(&path, &snapshot).unwrap();
        assert!(
            !unlinked,
            "the trailing waiter unlinked nothing — the file was already gone"
        );
        assert!(
            store
                .inner
                .path_locks
                .lock()
                .expect("file approval store path-lock map poisoned")
                .is_empty(),
            "the trailing waiter's vanished-path observation evicted the blocked entry"
        );
    }

    /// The resolve-side half of the same eviction contract: a registered
    /// approval materializes its path's lock entry, and resolve's
    /// best-effort approval unlink — reached under the store op lock, with
    /// no per-path lock involved — destroys the file and evicts the entry
    /// with it, so the map holds nothing beyond the approvals actually
    /// parked. Single-threaded end to end — the refcount is one at
    /// eviction because nothing else holds a clone.
    #[test]
    fn a_resolve_destroy_evicts_the_destroyed_path_lock_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(dir.path()).unwrap();
        let id = DecisionId::generate();

        store.inner.register_sync(make_parked(id)).unwrap();
        let path = dir.path().join("approvals").join(format!("{id}.json"));
        assert!(
            store
                .inner
                .path_locks
                .lock()
                .expect("file approval store path-lock map poisoned")
                .contains_key(&path),
            "fixture: the registration materialized the path's lock entry"
        );

        store
            .inner
            .resolve_sync(
                &id,
                ApprovalAuthority::WebhookPoll,
                ResolvedDecision::from(ApprovalDecision::Approved),
            )
            .unwrap();

        assert!(
            !path.exists(),
            "fixture: resolve destroyed the approval file"
        );
        assert!(
            store
                .inner
                .path_locks
                .lock()
                .expect("file approval store path-lock map poisoned")
                .is_empty(),
            "the resolved path's lock entry is evicted"
        );
    }

    /// The stale-decided arm of strict cancellation must treat a record that
    /// changes between classification and unlink as an incomplete
    /// cancellation. The race is forced through the same interleaving seam the
    /// other unlink-recheck tests use: a fresh payload is published at the
    /// boundary between `unlink_if_unchanged`'s lock-free compare (which saw
    /// the stale bytes) and its locked remove (which must then see changed
    /// bytes and refuse).
    #[tokio::test]
    async fn strict_cancel_rejects_a_changed_stale_approval_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(dir.path()).unwrap();
        let id = DecisionId::generate();
        let parked = make_parked(id);
        store.register(parked.clone()).await.unwrap();
        write_terminal_record(&dir, &parked);

        let path = dir.path().join("approvals").join(format!("{id}.json"));
        let stale = std::fs::read(&path).unwrap();

        // A fresh registration at the same path, ready to be published by the
        // boundary hook after the stale-decided arm has classified the record.
        let fresh = make_parked(id);
        let fresh_payload = serde_json::to_vec(&ParkedApprovalRecord::from(&fresh)).unwrap();
        assert_ne!(
            stale, fresh_payload,
            "fixture: the fresh payload differs from the stale one"
        );

        let swept_path = path.clone();
        let _boundary = unlink_interleave::install(Box::new(move |boundary: &std::path::Path| {
            if boundary != swept_path.as_path() {
                return;
            }
            std::fs::write(boundary, &fresh_payload)
                .expect("boundary hook publishes the fresh payload");
        }));

        let result = store.cancel_request_strict("req-replaced").await;
        let err = match result {
            Ok(cleared) => panic!(
                "a changed stale approval must fail strict cancellation, but cleared {}",
                cleared.len()
            ),
            Err(err) => err,
        };
        assert!(
            matches!(err, SessionStoreError::Request { .. }),
            "expected a request error naming the changed record, got {err:?}"
        );
        assert!(
            path.exists(),
            "the changed stale approval file is retained for retry"
        );
    }
}

/// The missing-directory half of the enumeration-errors-surface contract:
/// an `approvals/` directory that vanishes after open — external cleanup,
/// a misconfigured mount — must surface the enumeration fault to the poll
/// reconciler, never an empty pending set it would read as "nothing to
/// do". The not-a-directory half is pinned in the aura-web-server battery
/// (`list_pending_reports_enumeration_error`).
#[cfg(test)]
mod list_pending_missing_dir_tests {
    use crate::session_store::SessionStoreError;

    use super::FileApprovalStore;

    #[test]
    fn list_pending_reports_a_missing_approvals_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(dir.path()).unwrap();
        std::fs::remove_dir_all(dir.path().join("approvals")).unwrap();

        let err = match store.inner.list_pending_sync() {
            Ok(pending) => panic!(
                "an absent approvals directory must not read as a pending set of {}",
                pending.len()
            ),
            Err(err) => err,
        };
        assert!(
            matches!(err, SessionStoreError::Request { .. }),
            "expected the enumeration fault to surface, got {err:?}"
        );
    }
}
