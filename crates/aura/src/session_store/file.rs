//! File-backed session-store capabilities: durable parked approvals and run
//! records with no infrastructure beyond a writable directory.
//!
//! On-disk layout under the store root, a persisted contract shared by every
//! process reading the store:
//!
//! | Path                                  | Contents                            |
//! | ------------------------------------- | ----------------------------------- |
//! | `{root}/approvals/{decision_id}.json`    | one [`ParkedApprovalRecord`]        |
//! | `{root}/approvals/{decision_id}.decided` | durable [`WakeReason`] sidecar      |
//! | `{root}/approvals/*.tmp`                 | an interrupted write, ignored       |
//! | `{root}/approvals/*.taken`               | an interrupted take, ignored        |
//! | `{root}/approvals/*.probe`               | an interrupted open check, ignored  |
//! | `{root}/runs/{session_id}.json`       | one [`SessionRecord`]               |
//! | `{root}/runs/*.tmp`                   | an interrupted write, ignored       |
//!
//! There is no file-backed [`EventBus`](super::EventBus): a decision published
//! to a process that is no longer awaiting it has no reader, so the file
//! backend pairs durable approvals with the process-local bus
//! (`docs/adr/2026-07-21-hitl-park-reify.md` decision 14).
//!
//! Every guarantee rests on a filesystem primitive rather than a process-local
//! lock, so a root on shared storage behaves the same as one on a local disk.
//!
//! Durability invariant: every directory entry this backend creates is synced
//! into the directory holding it, before the store is handed out or a write is
//! acknowledged. That covers the record files, the `approvals/` and `runs/`
//! directories, and any ancestor of the configured root that `open` had to
//! create. Committing the outermost of those means syncing the pre-existing
//! directory it landed in, so that directory is reached as well; what stays
//! unsynced is its own entry one level higher up. The deployment is assumed to
//! have supplied that path already durable (a mount point, a package-created
//! data directory).
//!
//! # Run-store atomicity
//!
//! The run store implements compare-and-swap on a single session record. The
//! chosen atomicity strategy is a **stable POSIX advisory lockfile** alongside
//! the record file (`{session_id}.lock`). `fs4` acquires an exclusive `flock`
//! on that lockfile around the read-modify-write critical section, so only one
//! process can mutate a session at a time. The lockfile is never renamed, so
//! waiters always synchronize on the same inode even as the record file is
//! atomically replaced beneath it. Inside the critical section a new record is
//! written to a uniquely named temporary file and renamed over the record path,
//! giving the usual crash-atomic "old or new, never torn" guarantee. The
//! directory is synced after the rename so the new entry survives a host crash.
//!
//! `flock` is per-process on Unix, so it does not by itself serialize tasks
//! within the same process. An in-process `std::sync::Mutex` guards the store,
//! ensuring that concurrent operations in one runtime cannot accidentally
//! bypass the cross-process lock. The combination is: one process serializes
//! all its run-store operations, and the filesystem lock serializes across
//! processes.
//!
//! The lease itself is the fencing authority: a stale generation is rejected
//! inside the locked critical section before any write happens. Two processes
//! over one root therefore cannot corrupt the record, because the lock
//! serializes access and the generation check fences stale writers.
//!
//! POSIX only. Committing a directory entry needs `fsync` on a directory
//! handle, which Windows does not offer, so [`FileApprovalStore::open`] and
//! [`FileRunStore::open`] refuse to run there rather than silently dropping the
//! durability claim.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use fs4::fs_std::FileExt;
use uuid::Uuid;

use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};
use crate::orchestration::park::{
    AgentInstanceId, CasError, FencingGeneration, Lease, LeaseTtl, ParkCommit, RunEvent, RunState,
    SessionId, SessionRecord, WakeReason,
};

use super::{
    ApprovalStore, ParkedApprovalRecord, RunStore, RunStoreError, SessionStoreError,
    decode_run_record, encode_run_record,
};

#[cfg(not(windows))]
const APPROVALS_DIR: &str = "approvals";
const RECORD_EXTENSION: &str = "json";
const TAKEN_EXTENSION: &str = "taken";
/// Extension for durable decision sidecars; distinct from `.json` so
/// `record_paths` does not pick them up as approval records.
const DECIDED_EXTENSION: &str = "decided";
#[cfg(not(windows))]
const PROBE_EXTENSION: &str = "probe";
/// Non-empty, so the probe's file sync has bytes to flush.
#[cfg(not(windows))]
const PROBE_PAYLOAD: &[u8] = b"aura session store probe";

/// Parked approvals as one JSON file per decision, under a directory root.
pub struct FileApprovalStore {
    dir: PathBuf,
}

impl FileApprovalStore {
    /// Open the approval directory under `root`, creating and durably
    /// committing any directory it has to make.
    ///
    /// Returning `Ok` means the mount has passed a dry run of a park, covering:
    ///
    /// - creating and writing a file
    /// - `fsync` on that file
    /// - `fsync` on the approval directory
    /// - removing the file
    ///
    /// A mount that fails any step is rejected here rather than at the first
    /// park.
    ///
    /// Supported on POSIX platforms only; see the durability invariant in the
    /// module documentation.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let root = root.into();
        tokio::task::spawn_blocking(move || Self::open_blocking(&root))
            .await
            .map_err(|e| SessionStoreError::Connect {
                reason: format!("file store open did not complete: {e}"),
            })?
    }

    #[cfg(windows)]
    fn open_blocking(_root: &Path) -> Result<Self, SessionStoreError> {
        Err(SessionStoreError::Connect {
            reason: "the file session store is not supported on windows: committing a \
                     directory entry requires fsync on a directory handle, which windows \
                     does not provide, so parked approvals could not be made durable. Use \
                     AURA_SESSION_STORE=memory or AURA_SESSION_STORE=redis"
                .to_string(),
        })
    }

    #[cfg(not(windows))]
    fn open_blocking(root: &Path) -> Result<Self, SessionStoreError> {
        // Absolute from here down. A relative root's ancestor chain ends in the
        // empty path, which has no parent to sync, and a store that outlives a
        // change of working directory would otherwise stop naming its own
        // files.
        let root = std::path::absolute(root).map_err(|e| SessionStoreError::Connect {
            reason: format!("cannot resolve {}: {e}", root.display()),
        })?;
        let dir = root.join(APPROVALS_DIR);
        let created = uncreated_ancestors(&dir);
        fs::create_dir_all(&dir).map_err(|e| SessionStoreError::Connect {
            reason: format!("cannot create {}: {e}", dir.display()),
        })?;
        commit_created_dirs(&created)?;
        // `create_dir_all` says nothing about what this mount will accept.
        // Deferring that discovery to the first park is not equivalent: a park
        // failing to persist is not fatal to the park, so the fault would
        // surface as a caller waiting out its whole timeout instead of as a
        // startup error.
        probe_writable(&dir)?;
        Ok(Self { dir })
    }

    /// Confirm the approval directory is still reachable, catching a root that
    /// was unmounted or removed after startup.
    pub async fn ping(&self) -> Result<(), SessionStoreError> {
        self.blocking(|dir| {
            let meta = fs::metadata(&dir).map_err(|e| io_err("stat", &dir, &e))?;
            if meta.is_dir() {
                Ok(())
            } else {
                Err(SessionStoreError::Request {
                    reason: format!("{} is not a directory", dir.display()),
                })
            }
        })
        .await
    }

    /// Run a filesystem operation off the async executor: a store root may sit
    /// on network storage, where one call can block for a long time.
    async fn blocking<T, F>(&self, op: F) -> Result<T, SessionStoreError>
    where
        F: FnOnce(PathBuf) -> Result<T, SessionStoreError> + Send + 'static,
        T: Send + 'static,
    {
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || op(dir))
            .await
            .map_err(|e| SessionStoreError::Request {
                reason: format!("file store operation did not complete: {e}"),
            })?
    }
}

#[async_trait]
impl ApprovalStore for FileApprovalStore {
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError> {
        let record = ParkedApprovalRecord::from(&parked);
        self.blocking(move |dir| write_record(&dir, &record)).await
    }

    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
        let id = *id;
        self.blocking(move |dir| read_record(&record_path(&dir, &id)))
            .await
    }

    async fn resolve(
        &self,
        id: &DecisionId,
        _decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        let id = *id;
        match self.blocking(move |dir| take_record(&dir, &id)).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(ResolveError::NotFound),
            Err(err) => Err(ResolveError::Store(err)),
        }
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ApprovalDecision>, SessionStoreError> {
        todo!("staged for #271: read back the decision recorded for {id} (P7-completion)")
    }

    async fn resolve_durable(
        &self,
        id: &DecisionId,
        _decision: ApprovalDecision,
    ) -> Result<WakeReason, ResolveError> {
        let id = *id;
        let resolved_at = chrono::Utc::now();
        match self
            .blocking(move |dir| {
                // Check the record exists without consuming it.
                let path = record_path(&dir, &id);
                match fs::metadata(&path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(io_err("metadata", &path, &e)),
                    Ok(_) => {}
                }
                // Write the wake reason durably so a restarted process can replay it.
                let wake = WakeReason::DecisionResolved {
                    decision_id: id,
                    resolved_at,
                };
                let sidecar = dir.join(format!("{id}.{DECIDED_EXTENSION}"));
                let payload = serde_json::to_vec(&wake).expect("wake reason serializes to JSON");
                write_synced(&sidecar, &payload)?;
                sync_dir(&dir)?;
                Ok(Some(wake))
            })
            .await
        {
            Ok(Some(wake)) => Ok(wake),
            Ok(None) => Err(ResolveError::NotFound),
            Err(err) => Err(ResolveError::Store(err)),
        }
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        let id = *id;
        self.blocking(move |dir| take_record(&dir, &id).map(|_| ()))
            .await
    }

    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError> {
        let request_id = request_id.to_owned();
        self.blocking(move |dir| remove_by_request(&dir, &request_id))
            .await
    }
}

/// Directory for run records under the store root.
#[cfg(not(windows))]
const RUNS_DIR: &str = "runs";

/// Durable run records as one JSON file per session, under a directory root.
///
/// See the module-level documentation for the atomicity strategy.
pub struct FileRunStore {
    dir: PathBuf,
    /// In-process guard so concurrent operations in one runtime cannot bypass
    /// the cross-process `flock` on the record file.
    global: Arc<std::sync::Mutex<()>>,
}

impl FileRunStore {
    /// Open the run-record directory under `root`, creating and durably
    /// committing it. Refuses to run on Windows for the same durability
    /// reasons as [`FileApprovalStore::open`].
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let root = root.into();
        tokio::task::spawn_blocking(move || Self::open_blocking(&root))
            .await
            .map_err(|e| SessionStoreError::Connect {
                reason: format!("file run store open did not complete: {e}"),
            })?
    }

    #[cfg(windows)]
    fn open_blocking(_root: &Path) -> Result<Self, SessionStoreError> {
        Err(SessionStoreError::Connect {
            reason: "the file run store is not supported on windows: committing a directory \
                     entry requires fsync on a directory handle, which windows does not provide. \
                     Use AURA_SESSION_STORE=memory or AURA_SESSION_STORE=redis"
                .to_string(),
        })
    }

    #[cfg(not(windows))]
    fn open_blocking(root: &Path) -> Result<Self, SessionStoreError> {
        let root = std::path::absolute(root).map_err(|e| SessionStoreError::Connect {
            reason: format!("cannot resolve {}: {e}", root.display()),
        })?;
        let dir = root.join(RUNS_DIR);
        let created = uncreated_ancestors(&dir);
        fs::create_dir_all(&dir).map_err(|e| SessionStoreError::Connect {
            reason: format!("cannot create {}: {e}", dir.display()),
        })?;
        commit_created_dirs(&created)?;
        probe_writable(&dir)?;
        Ok(Self {
            dir,
            global: Arc::new(std::sync::Mutex::new(())),
        })
    }

    async fn blocking<T, F>(&self, op: F) -> Result<T, RunStoreError>
    where
        F: FnOnce(PathBuf, Arc<std::sync::Mutex<()>>) -> Result<T, RunStoreError> + Send + 'static,
        T: Send + 'static,
    {
        let dir = self.dir.clone();
        let global = Arc::clone(&self.global);
        tokio::task::spawn_blocking(move || op(dir, global))
            .await
            .map_err(|e| SessionStoreError::Request {
                reason: format!("file run store operation did not complete: {e}"),
            })?
    }
}

#[async_trait]
impl RunStore for FileRunStore {
    async fn create(&self, record: SessionRecord) -> Result<(), RunStoreError> {
        self.blocking(move |dir, global| {
            let _guard = global.lock().expect("run store lock poisoned");
            create_record(&dir, &record)
        })
        .await
    }

    async fn load(&self, session: SessionId) -> Result<Option<SessionRecord>, RunStoreError> {
        self.blocking(move |dir, _global| {
            let path = run_record_path(&dir, &session);
            read_run_record(&path)
        })
        .await
    }

    async fn acquire_lease(
        &self,
        session: SessionId,
        holder: AgentInstanceId,
        ttl: LeaseTtl,
    ) -> Result<Lease, RunStoreError> {
        self.blocking(move |dir, global| {
            let _guard = global.lock().expect("run store lock poisoned");
            mutate(&dir, session, |record| {
                let now = chrono::Utc::now();
                if let Some(ref lease) = record.lease
                    && lease.expires_at > now
                {
                    return Err(RunStoreError::LeaseHeld {
                        holder: lease.holder,
                        expires_at: lease.expires_at,
                    });
                }

                let next_generation = record.generation.next();
                let expires_at = now
                    + chrono::Duration::from_std(ttl.get()).map_err(|e| {
                        RunStoreError::Store(SessionStoreError::Request {
                            reason: format!("lease ttl does not fit in chrono duration: {e}"),
                        })
                    })?;
                let lease = Lease {
                    holder,
                    acquired_at: now,
                    heartbeat_at: now,
                    expires_at,
                    generation: next_generation,
                };
                record.lease = Some(lease.clone());
                record.generation = next_generation;
                Ok(lease)
            })
        })
        .await
    }

    async fn heartbeat_lease(
        &self,
        session: SessionId,
        generation: FencingGeneration,
        ttl: LeaseTtl,
    ) -> Result<Lease, RunStoreError> {
        self.blocking(move |dir, global| {
            let _guard = global.lock().expect("run store lock poisoned");
            mutate(&dir, session, |record| {
                let (holder, acquired_at) = {
                    let lease = require_live_lease(record, generation)?;
                    (lease.holder, lease.acquired_at)
                };

                let now = chrono::Utc::now();
                let expires_at = now
                    + chrono::Duration::from_std(ttl.get()).map_err(|e| {
                        RunStoreError::Store(SessionStoreError::Request {
                            reason: format!("lease ttl does not fit in chrono duration: {e}"),
                        })
                    })?;
                let renewed = Lease {
                    holder,
                    acquired_at,
                    heartbeat_at: now,
                    expires_at,
                    generation,
                };
                record.lease = Some(renewed.clone());
                Ok(renewed)
            })
        })
        .await
    }

    async fn release_lease(
        &self,
        session: SessionId,
        generation: FencingGeneration,
    ) -> Result<(), RunStoreError> {
        self.blocking(move |dir, global| {
            let _guard = global.lock().expect("run store lock poisoned");
            mutate(&dir, session, |record| {
                if generation != record.generation {
                    return Err(RunStoreError::Cas(CasError::GenerationMismatch {
                        presented: generation,
                        current: record.generation,
                    }));
                }
                record.lease = None;
                Ok(())
            })
        })
        .await
    }

    async fn apply(
        &self,
        session: SessionId,
        presented: FencingGeneration,
        event: RunEvent,
    ) -> Result<SessionRecord, RunStoreError> {
        self.blocking(move |dir, global| {
            let _guard = global.lock().expect("run store lock poisoned");
            mutate(&dir, session, |record| {
                require_live_lease(record, presented)?;
                let next = record
                    .clone()
                    .apply(presented, event)
                    .map_err(RunStoreError::Cas)?;
                *record = next.clone();
                Ok(next)
            })
        })
        .await
    }

    async fn park(
        &self,
        session: SessionId,
        presented: FencingGeneration,
        commit: ParkCommit,
    ) -> Result<SessionRecord, RunStoreError> {
        self.blocking(move |dir, global| {
            let _guard = global.lock().expect("run store lock poisoned");
            mutate(&dir, session, |record| {
                require_live_lease(record, presented)?;
                let next = record
                    .clone()
                    .park(presented, commit)
                    .map_err(RunStoreError::Cas)?;
                *record = next.clone();
                Ok(next)
            })
        })
        .await
    }
}

fn run_record_path(dir: &Path, session: &SessionId) -> PathBuf {
    dir.join(format!("{session}.json"))
}

fn run_record_lock_path(dir: &Path, session: &SessionId) -> PathBuf {
    dir.join(format!("{session}.lock"))
}

fn create_record(dir: &Path, record: &SessionRecord) -> Result<(), RunStoreError> {
    if !matches!(record.state, RunState::Created) {
        return Err(RunStoreError::Cas(CasError::StateMismatch {
            actual: "non-Created",
        }));
    }

    let path = run_record_path(dir, &record.session.id);
    let lock_path = run_record_lock_path(dir, &record.session.id);

    // Serialize every operation on this session through the stable sidecar
    // lockfile. A concurrent mutator that also takes this lock cannot enter
    // its critical section while the record is being created, and no reader
    // can see the target path until the write is complete.
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .map_err(|e| io_err("open", &lock_path, &e))?;

    lock.lock_exclusive().map_err(|e| {
        RunStoreError::Store(SessionStoreError::Request {
            reason: format!("cannot lock {}: {e}", lock_path.display()),
        })
    })?;

    // Write the complete payload to a uniquely named temp file and flush it
    // before any reader can see the path. Cleanup the temp file on error so
    // interrupted creates do not leave debris.
    let payload = encode_run_record(record)?;
    let staged = dir.join(format!("{}.{}.tmp", record.session.id, Uuid::now_v7()));
    write_synced(&staged, payload.as_bytes()).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })?;

    // Verify the target does not already exist while holding the lock, then
    // atomically publish the fully-written record and commit the directory
    // entry. A concurrent reader sees either nothing or the whole record.
    if path.exists() {
        let _ = fs::remove_file(&staged);
        return Err(RunStoreError::SessionExists {
            session: record.session.id,
        });
    }

    fs::rename(&staged, &path).map_err(|e| {
        let _ = fs::remove_file(&staged);
        io_err("commit", &path, &e)
    })?;
    sync_dir(dir).map_err(Into::into)
}

fn read_run_record(path: &Path) -> Result<Option<SessionRecord>, RunStoreError> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(Some(decode_run_record(&raw)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("read", path, &e).into()),
    }
}

/// Verify the record has a live lease whose generation matches the presented
/// fencing token. Every mutation except `acquire_lease` and `release_lease`
/// must pass this guard.
fn require_live_lease(
    record: &SessionRecord,
    presented: FencingGeneration,
) -> Result<&Lease, RunStoreError> {
    if presented != record.generation {
        return Err(RunStoreError::Cas(CasError::GenerationMismatch {
            presented,
            current: record.generation,
        }));
    }
    let Some(lease) = record.lease.as_ref() else {
        return Err(RunStoreError::Cas(CasError::StateMismatch {
            actual: "unleased",
        }));
    };
    if lease.generation != presented {
        return Err(RunStoreError::Cas(CasError::GenerationMismatch {
            presented,
            current: lease.generation,
        }));
    }
    if lease.expires_at <= chrono::Utc::now() {
        return Err(RunStoreError::Cas(CasError::StateMismatch {
            actual: "expired",
        }));
    }
    Ok(lease)
}

fn mutate<T>(
    dir: &Path,
    session: SessionId,
    f: impl FnOnce(&mut SessionRecord) -> Result<T, RunStoreError>,
) -> Result<T, RunStoreError> {
    let path = run_record_path(dir, &session);
    let lock_path = run_record_lock_path(dir, &session);

    // The critical section is guarded by a stable sidecar lockfile, not by the
    // record file itself. Renaming the record file would replace its inode, so
    // a waiter that opened the pre-rename file could end up locking the old,
    // unlinked inode and miss the new state. The lockfile is never renamed, so
    // every process serializes on the same inode.
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .map_err(|e| io_err("open", &lock_path, &e))?;

    lock.lock_exclusive().map_err(|e| {
        RunStoreError::Store(SessionStoreError::Request {
            reason: format!("cannot lock {}: {e}", lock_path.display()),
        })
    })?;

    let raw = fs::read_to_string(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            RunStoreError::UnknownSession { session }
        } else {
            io_err("read", &path, &e).into()
        }
    })?;
    let mut record = decode_run_record(&raw)?;
    let result = f(&mut record)?;
    write_run_record(dir, &path, &record)?;
    Ok(result)
}

fn write_run_record(dir: &Path, path: &Path, record: &SessionRecord) -> Result<(), RunStoreError> {
    let payload = encode_run_record(record)?;
    let staged = dir.join(format!("{}.{}.tmp", record.session.id, Uuid::now_v7()));
    write_synced(&staged, payload.as_bytes()).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })?;

    fs::rename(&staged, path).map_err(|e| {
        let _ = fs::remove_file(&staged);
        io_err("commit", path, &e)
    })?;
    sync_dir(dir).map_err(Into::into)
}

fn record_path(dir: &Path, id: &DecisionId) -> PathBuf {
    dir.join(format!("{id}.{RECORD_EXTENSION}"))
}

/// Write to a uniquely named temporary file and rename it over the record
/// path: a concurrent reader sees either the previous record or the whole new
/// one, never a half-written file. A failed rename leaves no debris behind.
///
/// The rename implies neither sync. Without the file sync, a crash can leave
/// the record zero-length or torn; without the directory sync, the new name
/// itself may not survive the reboot. Either omission quietly downgrades a park
/// the caller was told was durable.
fn write_record(dir: &Path, record: &ParkedApprovalRecord) -> Result<(), SessionStoreError> {
    let payload = serde_json::to_vec(record).expect("approval record serializes to JSON");
    let staged = dir.join(format!("{}.{}.tmp", record.decision_id, Uuid::now_v7()));
    write_synced(&staged, &payload).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })?;

    let path = record_path(dir, &record.decision_id);
    fs::rename(&staged, &path).map_err(|e| {
        let _ = fs::remove_file(&staged);
        io_err("commit", &path, &e)
    })?;
    sync_dir(dir)
}

fn write_synced(path: &Path, payload: &[u8]) -> Result<(), SessionStoreError> {
    let mut file = fs::File::create(path).map_err(|e| io_err("create", path, &e))?;
    file.write_all(payload)
        .map_err(|e| io_err("write", path, &e))?;
    file.sync_all().map_err(|e| io_err("sync", path, &e))
}

/// Commit a name change in the directory itself, so the entry a rename created
/// or removed survives a host crash rather than only a process crash.
fn sync_dir(dir: &Path) -> Result<(), SessionStoreError> {
    fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|e| io_err("sync", dir, &e))
}

fn read_record(path: &Path) -> Result<Option<ParkedApproval>, SessionStoreError> {
    match fs::read(path) {
        Ok(bytes) => decode(&bytes).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("read", path, &e)),
    }
}

/// Take the record by renaming it to a name only this call knows, reporting
/// whether this call is the one that got it. Rename is the at-most-once
/// primitive: of any number of concurrent callers exactly one moves the file
/// and the rest see it already gone. Unlinking is not a substitute: on macOS,
/// concurrent unlinks of one path each report success, which would hand the
/// same approval to several consumers.
///
/// The claim is only a claim once the directory entry is durable. An unsynced
/// rename can be rolled back by a host crash, restoring the record for a
/// second consumer after the first already ran the gated call. A sync failure
/// is reported as a lost take rather than a won one.
///
/// Discarding the claimed file afterwards is best effort: the take has already
/// succeeded, so a failure there must not be reported as a lost race. What it
/// can leave behind is an inert file no read path considers.
fn take_record(dir: &Path, id: &DecisionId) -> Result<bool, SessionStoreError> {
    let path = record_path(dir, id);
    let claimed = dir.join(format!("{id}.{}.{TAKEN_EXTENSION}", Uuid::now_v7()));
    match fs::rename(&path, &claimed) {
        Ok(()) => {
            sync_dir(dir)?;
            let _ = fs::remove_file(&claimed);
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err("take", &path, &e)),
    }
}

/// The directories on the way down to `dir` that do not exist yet, shallowest
/// first. Collected before creation, since afterwards they are
/// indistinguishable from directories the deployment supplied.
///
/// `dir` must be absolute: a relative chain ends at the empty path, which names
/// the working directory but reports no parent to sync.
#[cfg(not(windows))]
fn uncreated_ancestors(dir: &Path) -> Vec<PathBuf> {
    let mut missing: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|path| !path.exists())
        .map(Path::to_path_buf)
        .collect();
    missing.reverse();
    missing
}

/// Commit each newly created directory by syncing the directory that names it.
/// Creating a directory is a namespace change like any other: without this, a
/// register can sync a record into an `approvals/` directory that a host crash
/// then takes away, losing the approval and the directory together.
///
/// Shallowest first, so an ancestor's entry is durable before the entry it
/// contains.
#[cfg(not(windows))]
fn commit_created_dirs(created: &[PathBuf]) -> Result<(), SessionStoreError> {
    for dir in created {
        let parent = dir.parent().ok_or_else(|| SessionStoreError::Connect {
            reason: format!("store path {} has no parent directory", dir.display()),
        })?;
        sync_dir(parent).map_err(|e| SessionStoreError::Connect {
            reason: format!("cannot commit {}: {}", dir.display(), detail(&e)),
        })?;
    }
    Ok(())
}

/// Prove the mount supports every step a park takes: create, write, sync the
/// file, sync the directory, remove. Permission alone is not the question — a
/// filesystem that rejects either sync leaves the store unable to make a single
/// approval durable, and it must say so at startup rather than at the first
/// park.
///
/// The probe runs the same helpers the write path does, so it cannot pass a
/// primitive production would fail on.
#[cfg(not(windows))]
fn probe_writable(dir: &Path) -> Result<(), SessionStoreError> {
    let probe = dir.join(format!("{}.{PROBE_EXTENSION}", Uuid::now_v7()));
    let exercised = write_synced(&probe, PROBE_PAYLOAD).and_then(|()| sync_dir(dir));
    // Unconditional, so a failed check leaves the directory as it found it.
    let removed = fs::remove_file(&probe);

    exercised
        .and_then(|()| removed.map_err(|e| io_err("remove", &probe, &e)))
        .map_err(|err| SessionStoreError::Connect {
            reason: format!(
                "{} is unusable as a store root: {}",
                dir.display(),
                detail(&err)
            ),
        })
}

/// Unwrap one layer of error prose, so a startup failure reads as a single
/// sentence naming the primitive that failed.
#[cfg(not(windows))]
fn detail(err: &SessionStoreError) -> String {
    match err {
        SessionStoreError::Request { reason } => reason.clone(),
        other => other.to_string(),
    }
}

/// Scan for the request's approvals rather than keeping an index: cancellation
/// is a request-teardown path over a human-scale number of parked approvals,
/// and an index would be a second write to keep consistent with the first.
fn remove_by_request(dir: &Path, request_id: &str) -> Result<(), SessionStoreError> {
    for path in record_paths(dir)? {
        let Some(parked) = read_record(&path)? else {
            continue;
        };
        if parked.request.request_id == request_id {
            take_record(dir, &parked.request.decision_id)?;
        }
    }
    Ok(())
}

fn record_paths(dir: &Path) -> Result<Vec<PathBuf>, SessionStoreError> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| io_err("list", dir, &e))? {
        let path = entry.map_err(|e| io_err("list", dir, &e))?.path();
        if path.extension().is_some_and(|ext| ext == RECORD_EXTENSION) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn decode(bytes: &[u8]) -> Result<ParkedApproval, SessionStoreError> {
    let record: ParkedApprovalRecord =
        serde_json::from_slice(bytes).map_err(|e| SessionStoreError::Decode {
            reason: e.to_string(),
        })?;
    ParkedApproval::try_from(record).map_err(|e| SessionStoreError::Decode {
        reason: e.to_string(),
    })
}

fn io_err(action: &str, path: &Path, e: &io::Error) -> SessionStoreError {
    SessionStoreError::Request {
        reason: format!("cannot {action} {}: {e}", path.display()),
    }
}

/// The backend refuses to open on windows, so every test that opens one is
/// POSIX-only; the contract for windows is pinned in `windows_tests`.
#[cfg(all(test, not(windows)))]
mod tests {
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
    use std::process::{Command, Stdio};

    use super::*;
    use crate::RunId;
    use crate::hitl::{
        AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, PROTOCOL_VERSION,
    };
    use crate::orchestration::park::{
        AgentInstanceId, CasError, ChatSessionId, CheckpointEnvelope, FencingGeneration, LeaseTtl,
        NonEmpty, ParkCommit, ParkReason, RunCheckpoint, RunEvent, RunState, Session, SessionId,
        SessionRecord,
    };
    use crate::session_store::conformance;

    /// Names the store root for the child half of the kill test.
    const CHILD_ROOT_ENV: &str = "AURA_TEST_FILE_STORE_ROOT";
    /// Names the working directory the relative-root child runs from.
    const CHILD_CWD_ENV: &str = "AURA_TEST_FILE_STORE_CWD";
    /// A single-component relative root, the shape that has no absolute
    /// ancestor chain to walk.
    const RELATIVE_ROOT: &str = "session-store";
    /// Prefixes the line the child prints once its approval is on disk.
    const CHILD_READY: &str = "registered-decision-id=";
    /// Upper bound on the killed child's life, so an aborted parent cannot
    /// leave a process running forever.
    const CHILD_MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(60);
    /// Names the session id the child half of the stale-generation test should
    /// operate on.
    const CHILD_SESSION_ENV: &str = "AURA_TEST_FILE_RUN_STORE_SESSION";
    /// Names the fencing generation the child half of the stale-generation test
    /// should attempt as stale.
    const CHILD_OLD_GEN_ENV: &str = "AURA_TEST_FILE_RUN_STORE_OLD_GEN";
    /// File the child half of the stale-generation test writes its acquired
    /// generation into, so the parent can read it without relying on stdout.
    const CHILD_ACQUIRED_FLAG: &str = "child-acquired.flag";
    /// Prefixes the line the child prints once it has parked a run.
    const CHILD_PARKED: &str = "parked-session=";
    /// Names the session id the child half of the inode-rename race test
    /// should operate on.
    const CHILD_INODE_SESSION_ENV: &str = "AURA_TEST_FILE_RUN_STORE_INODE_SESSION";
    /// Names the fencing generation the child half of the inode-rename race
    /// test opened the record under.
    const CHILD_INODE_OLD_GEN_ENV: &str = "AURA_TEST_FILE_RUN_STORE_INODE_OLD_GEN";
    /// File the child half of the inode-rename race test creates once it has
    /// opened the pre-rename record file.
    const CHILD_INODE_OPENED_FLAG: &str = "inode-opened.flag";
    /// File the child half of the inode-rename race test creates once it has
    /// verified the post-rename state.
    const CHILD_INODE_DONE_FLAG: &str = "inode-done.flag";

    fn parked(request_id: &str) -> ParkedApproval {
        let now = chrono::Utc::now();
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                decision_id: DecisionId::generate(),
                request_id: request_id.to_string(),
                scope: AgentScope::Single { session_id: None },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "test_*".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "test_tool".to_string(),
                    arguments: serde_json::json!({}),
                    tool_call_intent: None,
                }],
            },
            registered_at: now,
            expires_at: now + chrono::Duration::seconds(60),
        }
    }

    fn created_record() -> SessionRecord {
        SessionRecord {
            session: Session {
                id: SessionId::generate(),
                chat_session_id: Some(ChatSessionId::new("cs_file")),
                created_at: chrono::Utc::now(),
            },
            run_id: None,
            state: RunState::Created,
            lease: None,
            generation: FencingGeneration::INITIAL,
        }
    }

    fn run_id() -> RunId {
        "018f9d2e-7c3a-7000-8000-000000000271".parse().unwrap()
    }

    fn park_commit() -> ParkCommit {
        ParkCommit {
            checkpoint: CheckpointEnvelope::new(RunCheckpoint::test_minimal()),
            reason: ParkReason::ApprovalsBlocked {
                decisions: NonEmpty::new(vec![DecisionId::generate()]).unwrap(),
            },
            parked_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(300),
        }
    }

    fn lease_ttl() -> LeaseTtl {
        LeaseTtl::new(std::time::Duration::from_secs(300)).unwrap()
    }

    fn flag_path(root: &Path, name: &str) -> PathBuf {
        root.join(name)
    }

    fn create_flag(root: &Path, name: &str) {
        std::fs::write(flag_path(root, name), b"").unwrap();
    }

    fn wait_for_flag(root: &Path, name: &str) {
        let path = flag_path(root, name);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for flag {name}");
    }

    #[tokio::test]
    async fn conforms_to_the_approval_store_contract() {
        let root = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(root.path()).await.unwrap();
        conformance::assert_approval_store_conformance(std::sync::Arc::new(store)).await;
    }

    #[tokio::test]
    async fn conforms_to_the_run_store_contract() {
        let root = tempfile::tempdir().unwrap();
        let store = FileRunStore::open(root.path()).await.unwrap();
        conformance::assert_run_store_conformance(std::sync::Arc::new(store)).await;
    }

    #[tokio::test]
    async fn open_creates_a_missing_root() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("deep").join("store");
        FileApprovalStore::open(&nested).await.unwrap();
        assert!(nested.join(APPROVALS_DIR).is_dir());
    }

    /// Shallowest first is the crash-consistency order: an ancestor's entry has
    /// to be committed before the entry it contains.
    #[test]
    fn uncreated_ancestors_run_shallowest_first() {
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("deep");
        let inner = outer.join("store");
        let dir = inner.join(APPROVALS_DIR);

        assert_eq!(uncreated_ancestors(&dir), vec![outer, inner, dir]);
    }

    #[test]
    fn an_existing_directory_has_no_uncreated_ancestors() {
        let root = tempfile::tempdir().unwrap();
        assert!(uncreated_ancestors(root.path()).is_empty());
    }

    /// `AURA_SESSION_STORE_URL` accepts a relative path, whose ancestor chain
    /// ends at a path with no parent. The open runs in a child process so the
    /// working directory it needs cannot disturb the rest of the suite, and so
    /// the relative root lands in a temporary directory rather than the source
    /// tree.
    #[tokio::test]
    async fn a_missing_relative_root_opens_on_the_first_try() {
        let cwd = tempfile::tempdir().unwrap();
        // The exit status and the resulting directory carry the whole verdict,
        // so the child's own test output stays out of this run's.
        let status = Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--exact",
                "--ignored",
                "session_store::file::tests::child_opens_a_relative_root",
            ])
            .env(CHILD_CWD_ENV, cwd.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn the child test process");

        assert!(status.success(), "opening a relative root failed: {status}");
        assert!(
            cwd.path().join(RELATIVE_ROOT).join(APPROVALS_DIR).is_dir(),
            "the child's relative root must resolve under its working directory",
        );
    }

    /// The child half of [`a_missing_relative_root_opens_on_the_first_try`]:
    /// opens a single-component relative root once, from a working directory
    /// the parent owns, and parks an approval in it.
    #[tokio::test]
    #[ignore = "spawned as a child process by a_missing_relative_root_opens_on_the_first_try"]
    async fn child_opens_a_relative_root() {
        let cwd = std::env::var(CHILD_CWD_ENV).expect("the parent sets the working directory");
        std::env::set_current_dir(&cwd).expect("enter the parent's working directory");

        let store = FileApprovalStore::open(RELATIVE_ROOT)
            .await
            .expect("a missing relative root opens on the first try");
        let entry = parked("req-relative");
        let id = entry.request.decision_id;
        store.register(entry).await.expect("register succeeds");
        assert!(store.get(&id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn open_leaves_no_probe_behind() {
        let root = tempfile::tempdir().unwrap();
        FileApprovalStore::open(root.path()).await.unwrap();
        assert_eq!(
            std::fs::read_dir(root.path().join(APPROVALS_DIR))
                .unwrap()
                .count(),
            0,
        );
    }

    /// An existing directory satisfies `create_dir_all` whatever its mode, so
    /// the open check has to be a write.
    #[cfg(unix)]
    #[tokio::test]
    async fn open_rejects_a_directory_it_cannot_write() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(APPROVALS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Mode bits do not constrain a superuser, so only assert when the
        // directory is genuinely closed to this process.
        let closed = std::fs::write(dir.join("writability-check"), b"").is_err();
        let opened = FileApprovalStore::open(root.path()).await;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        if closed {
            let Err(err) = opened else {
                panic!("open must reject an approval directory it cannot write");
            };
            assert!(matches!(err, SessionStoreError::Connect { .. }), "{err:?}");
        }
    }

    #[tokio::test]
    async fn a_reopened_store_sees_earlier_approvals() {
        let root = tempfile::tempdir().unwrap();
        let entry = parked("req-reopen");
        let id = entry.request.decision_id;

        let first = FileApprovalStore::open(root.path()).await.unwrap();
        first.register(entry).await.unwrap();
        drop(first);

        let second = FileApprovalStore::open(root.path()).await.unwrap();
        assert!(second.get(&id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn ping_fails_once_the_root_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(root.path()).await.unwrap();
        store.ping().await.unwrap();

        std::fs::remove_dir_all(root.path()).unwrap();
        assert!(store.ping().await.is_err());
    }

    #[tokio::test]
    async fn a_corrupt_record_is_a_decode_error() {
        let root = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(root.path()).await.unwrap();
        let id = DecisionId::generate();
        std::fs::write(record_path(&store.dir, &id), b"{not json").unwrap();

        assert!(matches!(
            store.get(&id).await,
            Err(SessionStoreError::Decode { .. })
        ));
    }

    #[tokio::test]
    async fn an_interrupted_write_is_not_read_as_a_record() {
        let root = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(root.path()).await.unwrap();
        let kept = parked("req-scan");
        let kept_id = kept.request.decision_id;
        store.register(kept).await.unwrap();
        std::fs::write(store.dir.join("half-written.tmp"), b"{not json").unwrap();

        store.cancel_request("some-other-request").await.unwrap();
        assert!(store.get(&kept_id).await.unwrap().is_some());
    }

    /// The durability claim is about the filesystem, not about a graceful
    /// shutdown path: a process killed outright must leave the approval behind.
    #[tokio::test]
    async fn an_approval_survives_a_killed_process() {
        let root = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "session_store::file::tests::child_registers_then_waits",
            ])
            .env(CHILD_ROOT_ENV, root.path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the child test process");

        let id = read_registered_id(child.stdout.take().expect("piped child stdout"));
        child.kill().expect("kill the child process");
        child.wait().expect("reap the child process");

        let store = FileApprovalStore::open(root.path()).await.unwrap();
        assert!(
            store.get(&id).await.unwrap().is_some(),
            "the approval registered by the killed process must survive it",
        );
    }

    fn read_registered_id(stdout: std::process::ChildStdout) -> DecisionId {
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("read the child's stdout");
            if let Some(raw) = line.strip_prefix(CHILD_READY) {
                return DecisionId::parse(raw.trim()).expect("the child prints a decision id");
            }
        }
        panic!("the child exited before registering an approval");
    }

    /// The child half of [`an_approval_survives_a_killed_process`]: registers
    /// one approval into the root named by the environment, announces its id,
    /// then stays alive for the parent to kill.
    #[tokio::test]
    #[ignore = "spawned as a child process by an_approval_survives_a_killed_process"]
    async fn child_registers_then_waits() {
        let root = std::env::var(CHILD_ROOT_ENV).expect("the parent sets the store root");
        let store = FileApprovalStore::open(&root).await.unwrap();
        let entry = parked("req-killed");
        let id = entry.request.decision_id;
        store.register(entry).await.unwrap();

        let mut stdout = std::io::stdout();
        writeln!(stdout, "{CHILD_READY}{id}").unwrap();
        stdout.flush().unwrap();

        tokio::time::sleep(CHILD_MAX_LIFETIME).await;
    }

    /// A checkpoint and the lease that committed it must survive a real
    /// process boundary: the child parks a run, the parent kills the child,
    /// then the parent loads the record and proves the lease is still usable.
    #[tokio::test]
    async fn a_parked_run_survives_a_killed_process() {
        let root = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "session_store::file::tests::child_parks_then_waits",
            ])
            .env(CHILD_ROOT_ENV, root.path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the child test process");

        let (session, generation) = read_parked_session(child.stdout.take().expect("piped stdout"));
        child.kill().expect("kill the child process");
        child.wait().expect("reap the child process");

        let store = FileRunStore::open(root.path()).await.unwrap();
        let record = store
            .load(session)
            .await
            .unwrap()
            .expect("the parked record must survive the killed process");
        assert!(
            matches!(record.state, RunState::Parked { .. }),
            "the record must still be parked"
        );
        assert_eq!(
            record.generation, generation,
            "the parked generation must survive the kill"
        );
        let lease = record
            .lease
            .expect("the lease that committed the park must survive");
        assert_eq!(lease.generation, generation);

        // Prove the lease state is usable by heartbeating it.
        store
            .heartbeat_lease(session, generation, lease_ttl())
            .await
            .expect("the survived lease must accept a heartbeat");
    }

    fn read_parked_session(stdout: std::process::ChildStdout) -> (SessionId, FencingGeneration) {
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("read the child's stdout");
            if let Some(raw) = line.strip_prefix(CHILD_PARKED) {
                let mut parts = raw.trim().split(' ');
                let session =
                    SessionId::parse(parts.next().expect("the child prints a session id"))
                        .expect("the child prints a valid session id");
                let generation = parts
                    .next()
                    .expect("the child prints a generation")
                    .parse::<u64>()
                    .expect("the child prints a valid generation")
                    .into();
                return (session, generation);
            }
        }
        panic!("the child exited before parking a run");
    }

    /// The child half of [`a_parked_run_survives_a_killed_process`]: creates a
    /// session, acquires a lease, parks the run, announces the session id and
    /// generation, then stays alive for the parent to kill.
    #[tokio::test]
    #[ignore = "spawned as a child process by a_parked_run_survives_a_killed_process"]
    async fn child_parks_then_waits() {
        let root = std::env::var(CHILD_ROOT_ENV).expect("the parent sets the store root");
        let store = FileRunStore::open(&root).await.unwrap();
        let session = SessionId::generate();
        let mut record = created_record();
        record.session.id = session;
        store.create(record).await.expect("create succeeds");

        let lease = store
            .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
            .await
            .expect("acquire succeeds");
        let running = store
            .apply(
                session,
                lease.generation,
                RunEvent::Start { run_id: run_id() },
            )
            .await
            .expect("start succeeds");
        let parked = store
            .park(session, running.generation, park_commit())
            .await
            .expect("park succeeds");

        let mut stdout = std::io::stdout();
        writeln!(
            stdout,
            "{CHILD_PARKED}{} {}",
            session,
            u64::from(parked.generation)
        )
        .unwrap();
        stdout.flush().unwrap();

        tokio::time::sleep(CHILD_MAX_LIFETIME).await;
    }

    /// Two processes over one root must not corrupt state: process A acquires
    /// and releases a lease, process B acquires a fresh lease, and process A's
    /// subsequent write with its old generation is rejected.
    #[tokio::test]
    async fn run_store_rejects_stale_generation_across_processes() {
        let root = tempfile::tempdir().unwrap();
        let store = FileRunStore::open(root.path()).await.unwrap();
        let session = SessionId::generate();
        let mut record = created_record();
        record.session.id = session;
        store.create(record).await.unwrap();

        let lease = store
            .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
            .await
            .unwrap();
        let old_generation = lease.generation;
        store.release_lease(session, old_generation).await.unwrap();

        let child = Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "session_store::file::tests::child_acquires_then_applies_with_fresh_generation",
            ])
            .env(CHILD_ROOT_ENV, root.path())
            .env(CHILD_SESSION_ENV, session.to_string())
            .env(CHILD_OLD_GEN_ENV, u64::from(old_generation).to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the child test process");

        wait_for_flag(root.path(), CHILD_ACQUIRED_FLAG);
        let new_generation = read_acquired_generation(root.path());

        // Process A attempts a write with the generation it released. The child
        // has already advanced the record, so this must be rejected.
        let stale = store
            .apply(
                session,
                old_generation,
                RunEvent::Start { run_id: run_id() },
            )
            .await;
        assert!(
            matches!(
                stale,
                Err(RunStoreError::Cas(CasError::GenerationMismatch { .. }))
            ),
            "a stale generation must be rejected after another process acquired the lease, got {stale:?}"
        );

        create_flag(root.path(), "stale-attempted.flag");
        wait_for_flag(root.path(), "applied.flag");
        let output = child.wait_with_output().expect("reap the child process");
        assert!(
            output.status.success(),
            "the child process must exit cleanly: stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let final_record = store.load(session).await.unwrap().expect("record exists");
        assert!(
            matches!(final_record.state, RunState::Running),
            "the child's apply with the fresh generation must succeed"
        );
        assert_eq!(
            final_record.generation,
            new_generation.next(),
            "the child's apply must advance the generation coherently"
        );
    }

    fn read_acquired_generation(root: &Path) -> FencingGeneration {
        let raw = std::fs::read_to_string(flag_path(root, CHILD_ACQUIRED_FLAG))
            .expect("the child wrote its acquired generation");
        raw.trim()
            .parse::<u64>()
            .expect("the child wrote a valid generation")
            .into()
    }

    /// The child half of [`run_store_rejects_stale_generation_across_processes`]:
    /// acquires a fresh lease on the shared session, announces the generation,
    /// waits for the parent to attempt a stale write, then applies with the
    /// fresh generation.
    #[tokio::test]
    #[ignore = "spawned as a child process by run_store_rejects_stale_generation_across_processes"]
    async fn child_acquires_then_applies_with_fresh_generation() {
        let root = std::env::var(CHILD_ROOT_ENV).expect("the parent sets the store root");
        let session: SessionId = SessionId::parse(
            &std::env::var(CHILD_SESSION_ENV).expect("the parent sets the session id"),
        )
        .expect("valid session id");
        let old_generation: FencingGeneration = std::env::var(CHILD_OLD_GEN_ENV)
            .expect("the parent sets the old generation")
            .parse::<u64>()
            .expect("valid generation")
            .into();

        let store = FileRunStore::open(&root).await.unwrap();
        let lease = store
            .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
            .await
            .expect("acquire succeeds after the parent released");

        std::fs::write(
            flag_path(Path::new(&root), CHILD_ACQUIRED_FLAG),
            u64::from(lease.generation).to_string(),
        )
        .expect("write acquired generation flag");

        wait_for_flag(Path::new(&root), "stale-attempted.flag");

        // The stale generation from the parent must not be usable here either.
        let stale = store
            .apply(
                session,
                old_generation,
                RunEvent::Start { run_id: run_id() },
            )
            .await;
        assert!(
            matches!(
                stale,
                Err(RunStoreError::Cas(CasError::GenerationMismatch { .. }))
            ),
            "the stale generation must also be rejected in the child, got {stale:?}"
        );

        store
            .apply(
                session,
                lease.generation,
                RunEvent::Start { run_id: run_id() },
            )
            .await
            .expect("apply with the fresh generation succeeds");
        create_flag(Path::new(&root), "applied.flag");
    }

    /// A waiter that opened the record file before a rename must still see the
    /// new state once it acquires the stable sidecar lock, not the old content
    /// of the now-unlinked inode it originally opened.
    #[tokio::test]
    async fn sidecar_lock_synchronizes_across_record_renames() {
        let root = tempfile::tempdir().unwrap();
        let store = FileRunStore::open(root.path()).await.unwrap();
        let session = SessionId::generate();
        let mut record = created_record();
        record.session.id = session;
        store.create(record).await.unwrap();

        let lease = store
            .acquire_lease(session, AgentInstanceId::generate(), lease_ttl())
            .await
            .unwrap();
        let old_generation = lease.generation;

        let child = Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "session_store::file::tests::child_waits_on_sidecar_lock_after_opening_record",
            ])
            .env(CHILD_ROOT_ENV, root.path())
            .env(CHILD_INODE_SESSION_ENV, session.to_string())
            .env(
                CHILD_INODE_OLD_GEN_ENV,
                u64::from(old_generation).to_string(),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the child test process");

        wait_for_flag(root.path(), CHILD_INODE_OPENED_FLAG);

        store
            .apply(
                session,
                old_generation,
                RunEvent::Start { run_id: run_id() },
            )
            .await
            .expect("parent apply succeeds while the child waits on the sidecar lock");

        wait_for_flag(root.path(), CHILD_INODE_DONE_FLAG);
        let output = child.wait_with_output().expect("reap the child process");
        assert!(
            output.status.success(),
            "the child process must exit cleanly: stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let final_record = store.load(session).await.unwrap().expect("record exists");
        assert_eq!(
            final_record.generation,
            old_generation.next(),
            "the parent apply must advance the generation coherently"
        );
        assert!(matches!(final_record.state, RunState::Running));
    }

    /// The child half of [`sidecar_lock_synchronizes_across_record_renames`]:
    /// opens the pre-rename record file, waits on the sidecar lock while the
    /// parent mutates the record, then proves it observes the post-rename state
    /// after acquiring the lock.
    #[tokio::test]
    #[ignore = "spawned as a child process by sidecar_lock_synchronizes_across_record_renames"]
    async fn child_waits_on_sidecar_lock_after_opening_record() {
        use fs4::fs_std::FileExt;

        let root = std::env::var(CHILD_ROOT_ENV).expect("the parent sets the store root");
        let session: SessionId = SessionId::parse(
            &std::env::var(CHILD_INODE_SESSION_ENV).expect("the parent sets the session id"),
        )
        .expect("valid session id");
        let old_generation: FencingGeneration = std::env::var(CHILD_INODE_OLD_GEN_ENV)
            .expect("the parent sets the old generation")
            .parse::<u64>()
            .expect("valid generation")
            .into();

        let root = Path::new(&root);
        let runs_dir = root.join(super::RUNS_DIR);
        let record_path = super::run_record_path(&runs_dir, &session);
        let mut old_inode = fs::OpenOptions::new()
            .read(true)
            .open(&record_path)
            .expect("open the pre-rename record file");

        create_flag(root, CHILD_INODE_OPENED_FLAG);

        let lock_path = super::run_record_lock_path(&runs_dir, &session);
        let mut lock = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&lock_path)
            .expect("open the sidecar lockfile");

        // Wait until the parent has actually taken the sidecar lock, so the
        // next exclusive lock blocks until the rename is complete. If this
        // process grabs the lock first, drop it and retry.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => {
                    drop(lock);
                    if std::time::Instant::now() > deadline {
                        panic!("parent never took the sidecar lock");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    lock = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&lock_path)
                        .expect("reopen the sidecar lockfile");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("try_lock_exclusive failed: {e}"),
            }
        }

        lock.lock_exclusive()
            .expect("acquire the sidecar lock after the parent releases it");

        old_inode
            .seek(SeekFrom::Start(0))
            .expect("rewind the pre-rename inode");
        let mut old_raw = String::new();
        old_inode
            .read_to_string(&mut old_raw)
            .expect("read the pre-rename inode");
        let old_record = super::decode_run_record(&old_raw).expect("decode pre-rename record");
        assert_eq!(
            old_record.generation, old_generation,
            "the unlinked inode must still hold the pre-rename state"
        );

        let new_raw = fs::read_to_string(&record_path).expect("read the post-rename record path");
        let new_record = super::decode_run_record(&new_raw).expect("decode post-rename record");
        assert_eq!(
            new_record.generation,
            old_generation.next(),
            "the sidecar lock must expose the post-rename state, not the old one"
        );
        assert!(matches!(new_record.state, RunState::Running));

        create_flag(root, CHILD_INODE_DONE_FLAG);
    }

    /// `create` must serialize on the same stable sidecar lock that `mutate`
    /// uses, and it must not publish the record path until the payload is fully
    /// written. A second process that enters `mutate` while `create` is in
    /// progress blocks on the lock; it never sees an empty or partial record
    /// file.
    #[tokio::test]
    async fn create_uses_stable_sidecar_lock_and_publishes_complete_record() {
        let root = tempfile::tempdir().unwrap();
        let store = FileRunStore::open(root.path()).await.unwrap();
        let session = SessionId::generate();
        let mut record = created_record();
        record.session.id = session;

        let child = Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "session_store::file::tests::child_holds_sidecar_lock_during_parent_create",
            ])
            .env(CHILD_ROOT_ENV, root.path())
            .env(CHILD_SESSION_ENV, session.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the child test process");

        wait_for_flag(root.path(), "create-lock-held.flag");

        // Announce that we are about to call create, so the child can verify
        // the record path is not visible while we are blocked on the lock.
        create_flag(root.path(), "parent-will-create.flag");

        // This blocks until the child releases the sidecar lock.
        store
            .create(record)
            .await
            .expect("create succeeds after the child releases the lock");

        create_flag(root.path(), "create-done.flag");

        let output = child.wait_with_output().expect("reap the child process");
        assert!(
            output.status.success(),
            "the child process must exit cleanly: stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let loaded = store.load(session).await.unwrap().expect("record exists");
        assert_eq!(loaded.session.id, session);
        assert!(matches!(loaded.state, RunState::Created));
    }

    /// The child half of [`create_uses_stable_sidecar_lock_and_publishes_complete_record`]:
    /// holds the sidecar lock before the parent creates, verifies the record
    /// file is not published while the parent waits, then releases the lock
    /// and confirms the parent published a complete record.
    #[tokio::test]
    #[ignore = "spawned as a child process by create_uses_stable_sidecar_lock_and_publishes_complete_record"]
    async fn child_holds_sidecar_lock_during_parent_create() {
        use fs4::fs_std::FileExt;

        let root = std::env::var(CHILD_ROOT_ENV).expect("the parent sets the store root");
        let session: SessionId = SessionId::parse(
            &std::env::var(CHILD_SESSION_ENV).expect("the parent sets the session id"),
        )
        .expect("valid session id");

        let root = Path::new(&root);
        let runs_dir = root.join(super::RUNS_DIR);
        let record_path = super::run_record_path(&runs_dir, &session);
        let lock_path = super::run_record_lock_path(&runs_dir, &session);

        let lock = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&lock_path)
            .expect("open the sidecar lockfile");
        lock.lock_exclusive()
            .expect("hold the sidecar lock before the parent creates");

        create_flag(root, "create-lock-held.flag");

        // Wait until the parent is about to create; while we still hold the
        // lock, the record file must not be visible.
        wait_for_flag(root, "parent-will-create.flag");
        assert!(
            !record_path.exists(),
            "record must not be published while the parent waits on the sidecar lock"
        );

        create_flag(root, "lock-released.flag");
        drop(lock);

        // Wait for the parent to finish creating, then confirm the record is
        // present and decodes cleanly.
        wait_for_flag(root, "create-done.flag");
        let raw = fs::read_to_string(&record_path).expect("read the created record");
        let loaded = super::decode_run_record(&raw).expect("record must be valid JSON");
        assert_eq!(loaded.session.id, session);
        assert!(matches!(loaded.state, RunState::Created));
    }
}

/// Runs only on a windows builder, where it is the whole contract: the backend
/// refuses rather than opening a store it cannot make durable.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[tokio::test]
    async fn open_refuses_to_run_on_windows() {
        let Err(err) = FileApprovalStore::open(std::env::temp_dir()).await else {
            panic!("the file session store must refuse to open on windows");
        };
        let SessionStoreError::Connect { reason } = err else {
            panic!("expected a Connect error, got {err:?}");
        };
        assert!(reason.contains("not supported on windows"), "{reason}");
        assert!(reason.contains("AURA_SESSION_STORE=memory"), "{reason}");
    }
}
