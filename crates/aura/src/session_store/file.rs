//! File-backed HITL approval store: one JSON file per decision id.
//! Parked approvals survive process restart on a single host.
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
//!   unknown id; expiry is enforced only by `resolve`.
//! - `resolve` *moves* the approval into the decision file rather than deleting
//!   it: `get` returns the approval before and after the decision, `decision`
//!   returns the recorded decision, and both are retained until `remove`.
//! - At-most-once `resolve` is the `File::create_new` claim on the decision
//!   file: `AlreadyExists` reads as `NotFound`.
//! - `cancel_request` removes undecided approvals by owner (request) id;
//!   decided entries are retained until their consumer removes them.
//!
//! Decision ids are validated as UUIDs before path building, so none address
//! outside the root.
//! Temp-file plus rename prevents partial files after crashes. A `std::sync::Mutex`
//! serializes operations for the single writing process; no operation awaits
//! while holding it.
//!
//! Store operations run on the blocking pool. Join failures map to store
//! errors—a panicked op is a store fault, not a crash. Blocking work is not
//! cancelled when the requester is dropped: a dropped poll still completes
//! resolve's claim-write-move.
//!
//! Crash window: claim-then-write leaves an empty file if process dies
//! mid-resolve. The aftermath fails closed; `decision` reports decode
//! fault, `get` returns the approval. Recovery is deleting the empty file.
//! `decision()` consumers treat `Err(Decode)` on a known id as this
//! recoverable state, not as an unknown id.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::task::{JoinError, spawn_blocking};

use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};

use super::{ApprovalStore, DecisionRecord, ParkedApprovalRecord, SessionStoreError};

/// Undecided approvals, one `{decision_id}.json` file per approval.
const APPROVALS_DIR: &str = "approvals";
/// Recorded decisions, one `{decision_id}.json` file per decision.
const DECISIONS_DIR: &str = "decisions";

/// The on-disk shape of a resolved approval: the approval record carried
/// over from `approvals/` plus the recorded decision. Field names are a
/// persisted contract shared by every instance reading the store — rename
/// only with a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResolvedEntry {
    approval: ParkedApprovalRecord,
    decision: DecisionRecord,
}

/// A file-backed [`ApprovalStore`] over one root directory.
pub struct FileApprovalStore {
    inner: Arc<Inner>,
}

/// The shared store state: root directory and operation lock.
struct Inner {
    root: PathBuf,
    lock: Mutex<()>,
}

impl FileApprovalStore {
    /// Open (or initialize) the store rooted at `root`, creating both
    /// directories and probing them for writes. Fails fast: a store that
    /// cannot hold files must fail at startup, not on the first approval.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let root = root.as_ref();
        fs::create_dir_all(root.join(APPROVALS_DIR)).map_err(connect_err)?;
        fs::create_dir_all(root.join(DECISIONS_DIR)).map_err(connect_err)?;
        let inner = Arc::new(Inner {
            root: root.to_path_buf(),
            lock: Mutex::new(()),
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

    /// Create and unlink an empty probe file in each store directory. This
    /// catches permission and mount faults, not a full disk.
    fn probe_writable_sync(&self) -> io::Result<()> {
        for dir in [self.approvals_dir(), self.decisions_dir()] {
            let probe = dir.join(format!(".{}.probe", uuid::Uuid::new_v4()));
            fs::write(&probe, b"")
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
        publish(&self.approval_path(&id), &payload)
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
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        let _guard = self.lock();
        let id = canonical_id(id).map_err(ResolveError::Store)?;

        // Reading the approval before claiming avoids claiming unknown ids.
        let record = match fs::read(self.approval_path(&id)) {
            Ok(bytes) => serde_json::from_slice::<ParkedApprovalRecord>(&bytes)
                .map_err(|e| ResolveError::Store(decode_err(e)))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // No approval: unknown, resolved, or removed all return `NotFound`.
                return Err(ResolveError::NotFound);
            }
            Err(err) => return Err(ResolveError::Store(request_err(err))),
        };
        if chrono::Utc::now() > record.expires_at {
            return Err(ResolveError::NotFound);
        }
        let payload = serde_json::to_vec(&ResolvedEntry {
            approval: record,
            decision: DecisionRecord::from(&decision),
        })
        .expect("resolved entry serializes to JSON");

        let decision_path = self.decision_path(&id);
        let mut file = match fs::File::create_new(&decision_path) {
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
        // Failure leaves a stale approval file; resolve succeeded.
        match fs::remove_file(self.approval_path(&id)) {
            Ok(()) => {}
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
    ) -> Result<Option<ApprovalDecision>, SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(id)?;
        match fs::read(self.decision_path(&id)) {
            Ok(bytes) => {
                let entry: ResolvedEntry = serde_json::from_slice(&bytes).map_err(decode_err)?;
                Ok(Some(ApprovalDecision::from(entry.decision)))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(request_err(err)),
        }
    }

    fn remove_sync(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(id)?;
        // Remove both halves; missing halves are fine (idempotent).
        for path in [self.approval_path(&id), self.decision_path(&id)] {
            if let Err(err) = fs::remove_file(&path)
                && err.kind() != io::ErrorKind::NotFound
            {
                return Err(request_err(err));
            }
        }
        Ok(())
    }

    fn cancel_request_sync(&self, request_id: &str) -> Result<(), SessionStoreError> {
        let _guard = self.lock();
        let entries = match fs::read_dir(self.approvals_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(request_err(err)),
        };
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
            match serde_json::from_slice::<ParkedApprovalRecord>(&bytes) {
                Ok(record) if record.request_id == request_id => {
                    fs::remove_file(&path).map_err(request_err)?;
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(), error = %err,
                        "undecodable approval file skipped by cancel_request"
                    );
                }
            }
        }
        Ok(())
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
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        let inner = Arc::clone(&self.inner);
        let id = *id;
        spawn_blocking(move || inner.resolve_sync(&id, decision))
            .await
            .map_err(|err| ResolveError::Store(join_err(err)))?
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ApprovalDecision>, SessionStoreError> {
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

    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let request_id = request_id.to_owned();
        spawn_blocking(move || inner.cancel_request_sync(&request_id))
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
    let written = fs::write(&tmp, payload).and_then(|()| fs::rename(&tmp, path));
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(request_err(err));
    }
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
