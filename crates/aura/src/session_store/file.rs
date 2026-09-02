//! File-backed HITL approval store: one JSON file per decision id under a
//! root directory, so tickets and decisions survive a process restart — the
//! park/reify V1 durability boundary (single writing process on a single
//! pod; HA, leases, and reapers are out of scope).
//!
//! Layout:
//!
//! | Path                                 | Content                                                    |
//! | ----------------------------------- | ---------------------------------------------------------- |
//! | `{root}/tickets/{decision_id}.json`   | `ParkedApprovalRecord` (the undecided ticket)              |
//! | `{root}/decisions/{decision_id}.json` | the resolved envelope: the ticket record plus the decision |
//!
//! Store contract (park/reify rev 7 §2.5), with the expiry refusal shared by
//! the in-memory backend:
//!
//! - `resolve` refuses past the ticket's `expires_at`, uniformly with an
//!   unknown id; expiry is enforced only by `resolve`.
//! - `resolve` *moves* the ticket into the decision file rather than deleting
//!   it: `get` returns the ticket before and after the decision, `decision`
//!   returns the recorded decision, and both are retained until `remove`.
//! - At-most-once `resolve` is the `File::create_new` claim on the decision
//!   file: `AlreadyExists` reads as `NotFound`.
//! - `cancel_request` removes undecided tickets by owner (request) id;
//!   decided entries are retained until their consumer removes them.
//!
//! Every decision id is validated as a canonical UUID before it touches a
//! path, so no id can address a file outside the store root, and owner ids
//! are matched against ticket contents rather than embedded in paths.
//! Ticket publication is temp-file plus same-directory rename, so a crash
//! never leaves a partial file under a final name. A `std::sync::Mutex`
//! serializes operations for the single writing process; no operation awaits
//! while holding it.
//!
//! Every store operation runs on the blocking thread pool behind its async
//! trait method. A join failure — a panicked store operation — maps to the
//! store request error callers already treat fail-closed, so a panicking
//! store op is a store fault, not a server crash. Blocking work is also not
//! cancelled when the requesting future is dropped: a dropped poll request
//! still completes resolve's claim-write-move.
//!
//! Crash window in `resolve`: the decision file is claimed with
//! `create_new` and then written and synced, so a process death between
//! the two can leave an empty `decisions/{id}.json`. The aftermath fails
//! closed - `resolve` keeps refusing (the claim exists), `decision`
//! reports a decode fault, and `get` still returns the live ticket - and
//! operator recovery is deleting that one empty file, after which the
//! still-present ticket makes the id resolvable again. Consumers of the
//! decision read (`decision()`) must therefore treat `Err(Decode)` on a
//! known id as this recoverable state, not as an unknown id.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::task::{JoinError, spawn_blocking};

use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};

use super::{ApprovalStore, DecisionRecord, ParkedApprovalRecord, SessionStoreError};

/// Undecided tickets, one `{decision_id}.json` file per ticket.
const TICKETS_DIR: &str = "tickets";
/// Recorded decisions, one `{decision_id}.json` file per decision.
const DECISIONS_DIR: &str = "decisions";

/// The on-disk shape of a resolved approval: the ticket record carried over
/// from `tickets/` plus the recorded decision, so a decided ticket stays
/// readable through `get` until `remove`. Field names are a persisted
/// contract shared by every instance reading the store — rename only with a
/// migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResolvedEntry {
    ticket: ParkedApprovalRecord,
    decision: DecisionRecord,
}

/// A file-backed [`ApprovalStore`] over one root directory.
#[derive(Clone)]
pub struct FileApprovalStore {
    inner: Arc<Inner>,
}

/// The shared store state every blocking-pool operation runs against.
struct Inner {
    root: PathBuf,
    /// Serializes compound operations for the single writing process.
    lock: Mutex<()>,
}

impl FileApprovalStore {
    /// Open (or initialize) the store rooted at `root`, creating the ticket
    /// and decision directories. Fails when they cannot be created: a store
    /// that cannot hold files must fail at startup, not on the first
    /// approval.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let root = root.as_ref();
        fs::create_dir_all(root.join(TICKETS_DIR)).map_err(connect_err)?;
        fs::create_dir_all(root.join(DECISIONS_DIR)).map_err(connect_err)?;
        Ok(Self {
            inner: Arc::new(Inner {
                root: root.to_path_buf(),
                lock: Mutex::new(()),
            }),
        })
    }
}

impl Inner {
    fn tickets_dir(&self) -> PathBuf {
        self.root.join(TICKETS_DIR)
    }

    fn decisions_dir(&self) -> PathBuf {
        self.root.join(DECISIONS_DIR)
    }

    fn ticket_path(&self, id: &str) -> PathBuf {
        self.tickets_dir().join(format!("{id}.json"))
    }

    fn decision_path(&self, id: &str) -> PathBuf {
        self.decisions_dir().join(format!("{id}.json"))
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().expect("file approval store lock poisoned")
    }

    fn register_sync(&self, parked: ParkedApproval) -> Result<(), SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(&parked.request.decision_id)?;
        let payload = serde_json::to_vec(&ParkedApprovalRecord::from(&parked))
            .expect("approval record serializes to JSON");
        publish(&self.ticket_path(&id), &payload)
    }

    fn get_sync(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
        let _guard = self.lock();
        let id = canonical_id(id)?;
        match fs::read(self.ticket_path(&id)) {
            Ok(bytes) => return decode_ticket(&bytes).map(Some),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(request_err(err)),
        }
        // §2.5 retention: a decided ticket moved into the decision file is
        // still readable through `get` until `remove`.
        match fs::read(self.decision_path(&id)) {
            Ok(bytes) => {
                let entry: ResolvedEntry = serde_json::from_slice(&bytes).map_err(decode_err)?;
                restore_ticket(entry.ticket).map(Some)
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

        // Read the undecided ticket before claiming: the claim must not fire
        // for an unknown or already-decided id.
        let record = match fs::read(self.ticket_path(&id)) {
            Ok(bytes) => serde_json::from_slice::<ParkedApprovalRecord>(&bytes)
                .map_err(|e| ResolveError::Store(decode_err(e)))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // No undecided ticket: unknown, already resolved, or removed
                // — all collapse to `NotFound`, and the claim is untouched.
                return Err(ResolveError::NotFound);
            }
            Err(err) => return Err(ResolveError::Store(request_err(err))),
        };
        // Expiry is enforced only by `resolve` (§2.5): past `expires_at` the
        // id is as good as unknown, and the ticket stays for `get` until
        // `remove`.
        if chrono::Utc::now() > record.expires_at {
            return Err(ResolveError::NotFound);
        }
        let payload = serde_json::to_vec(&ResolvedEntry {
            ticket: record,
            decision: DecisionRecord::from(&decision),
        })
        .expect("resolved entry serializes to JSON");

        // `File::create_new` is the at-most-once claim: exactly one `resolve`
        // ever creates the decision file; every other attempt (a repeat, or a
        // racer the lock would have serialized) sees `AlreadyExists` and
        // reads as `NotFound`.
        let decision_path = self.decision_path(&id);
        let mut file = match fs::File::create_new(&decision_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ResolveError::NotFound);
            }
            Err(err) => return Err(ResolveError::Store(request_err(err))),
        };
        // Best-effort beyond the stated durability boundary (a process
        // restart): the page cache already carries the write across
        // restart, and no other write path fsyncs, so this sync only
        // narrows the empty-file crash window documented atop the module
        // rather than promising host-crash durability.
        if let Err(err) = file.write_all(&payload).and_then(|()| file.sync_all()) {
            // Undo the claim: the decision was never fully written, and a
            // retry must be able to take the claim again.
            let _ = fs::remove_file(&decision_path);
            return Err(ResolveError::Store(request_err(err)));
        }
        // The sync above is the commit: the decision is recorded. Removing
        // the ticket file is best-effort past it — `Ok(())` and `NotFound`
        // pass, and any other failure leaves a stale ticket that is benign
        // (`get` returns the identical record from either file, a repeat
        // resolve still fails closed on the claim, and `remove` /
        // `cancel_request` still clean it up), so resolve has already
        // succeeded.
        match fs::remove_file(self.ticket_path(&id)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                decision_id = %id,
                error = %err,
                "stale ticket file remains after resolve; it is benign"
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
        // Both halves of a possibly-decided entry go; a missing half is not
        // an error (remove is idempotent).
        for path in [self.ticket_path(&id), self.decision_path(&id)] {
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
        let entries = match fs::read_dir(self.tickets_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(request_err(err)),
        };
        for entry in entries {
            let entry = entry.map_err(request_err)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                // A mid-publish temp file, never a ticket.
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(request_err(err)),
            };
            match serde_json::from_slice::<ParkedApprovalRecord>(&bytes) {
                // Only undecided tickets are cancelled; a decided entry is
                // retained until its consumer removes it (§2.5).
                Ok(record) if record.request_id == request_id => {
                    fs::remove_file(&path).map_err(request_err)?;
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(), error = %err,
                        "undecodable ticket file skipped by cancel_request"
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
        let request_id = request_id.to_string();
        spawn_blocking(move || inner.cancel_request_sync(&request_id))
            .await
            .map_err(join_err)?
    }
}

/// The canonical UUID form of a decision id, refusing anything that could
/// address a file outside the store root once embedded in a file name.
/// `DecisionId`'s `Display` is canonical today; this is the defense-in-depth
/// check that keeps path building safe if construction paths ever widen.
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

/// Write `payload` to `path` via a uniquely-named temp file in the same
/// directory followed by a rename: publication is atomic within the
/// directory, so a crash never leaves a partial file under a final name, and
/// a crashed publisher leaves at most an orphaned dot-prefixed temp.
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

/// Decode a stored ticket file back to the domain.
fn decode_ticket(bytes: &[u8]) -> Result<ParkedApproval, SessionStoreError> {
    let record: ParkedApprovalRecord = serde_json::from_slice(bytes).map_err(decode_err)?;
    restore_ticket(record)
}

/// Restore a decoded ticket record to the domain.
fn restore_ticket(record: ParkedApprovalRecord) -> Result<ParkedApproval, SessionStoreError> {
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

/// Map a `spawn_blocking` join failure (a panicked store operation) to the
/// store request error.
fn join_err(err: JoinError) -> SessionStoreError {
    SessionStoreError::Request {
        reason: format!("file store task failed: {err}"),
    }
}
