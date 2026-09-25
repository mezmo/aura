//! File-backed skill-invocation store: one JSON-lines file per session under
//! `{root}/skills/`, so a session's log survives a process restart on a
//! single host.
//!
//! Layout:
//!
//! | Path                                      | Content                                   |
//! | ----------------------------------------- | ----------------------------------------- |
//! | `{root}/skills/{uuid5(session_id)}.jsonl` | one `SkillInvocationRecord` JSON per line |
//!
//! The filename is the v5 UUID of the session id (OID namespace), so any
//! client-supplied id — including one carrying path separators or `..` —
//! maps to a fixed-shape name that cannot address outside the directory.
//!
//! Store contract:
//!
//! - `record` is idempotent per dedup key (the first record for a key wins),
//!   refuses a new key once the file holds [`MAX_SKILL_RECORDS_PER_SESSION`]
//!   decodable records, and rewrites the whole file through temp-file plus
//!   rename, so a crash mid-write leaves the previous log intact. Lines that
//!   fail to decode are carried over verbatim rather than dropped.
//! - `list` skips lines that fail to decode, including records on another
//!   schema version, with a warning.
//! - Expiry is mtime-based against the configured TTL: a session file past
//!   it is removed when next touched, and `open` sweeps every expired file so
//!   an idle host does not accumulate them. Each successful `record` rewrites
//!   the file, refreshing its mtime.
//!
//! Concurrency and blocking-pool handling follow the approval store in the
//! parent module, including why the operations are sync.

use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio::task::spawn_blocking;

use crate::config::SessionId;
use crate::session_store::{
    MAX_SKILL_RECORDS_PER_SESSION, SessionStoreError, SkillInvocationRecord, SkillInvocationStore,
};

use super::{connect_err, join_err, publish, request_err};

/// Per-session skill logs, one `{uuid}.jsonl` file per session.
const SKILLS_DIR: &str = "skills";
/// Extension of a session log file.
const FILE_EXT: &str = "jsonl";

/// A file-backed [`SkillInvocationStore`] over one root directory.
pub struct FileSkillInvocationStore {
    inner: Arc<Inner>,
}

/// The shared store state: the skills directory, the TTL, and the operation
/// lock.
struct Inner {
    dir: PathBuf,
    ttl: Option<Duration>,
    lock: Mutex<()>,
}

impl FileSkillInvocationStore {
    /// Open (or initialize) the store under `root`: create the skills
    /// directory, probe it for writes, and sweep session files past
    /// `ttl_secs`. Fails fast, so a store that cannot hold files fails at
    /// startup rather than on the first skill load.
    pub fn open(
        root: impl AsRef<Path>,
        ttl_secs: Option<NonZeroU64>,
    ) -> Result<Self, SessionStoreError> {
        let dir = root.as_ref().join(SKILLS_DIR);
        fs::create_dir_all(&dir).map_err(connect_err)?;
        let inner = Arc::new(Inner {
            dir,
            ttl: ttl_secs.map(|secs| Duration::from_secs(secs.get())),
            lock: Mutex::new(()),
        });
        inner.probe_writable_sync().map_err(connect_err)?;
        inner.sweep_expired_sync().map_err(connect_err)?;
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
    fn session_path(&self, session_id: &SessionId) -> PathBuf {
        let name = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, session_id.as_str().as_bytes());
        self.dir.join(format!("{name}.{FILE_EXT}"))
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().expect("file skill store lock poisoned")
    }

    /// Create and unlink an empty probe file. This catches permission and
    /// mount faults, not a full disk.
    fn probe_writable_sync(&self) -> io::Result<()> {
        let probe = self.dir.join(format!(".{}.probe", uuid::Uuid::new_v4()));
        fs::write(&probe, b"")
            .and_then(|()| fs::remove_file(&probe))
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("{} not writable: {err}", self.dir.display()),
                )
            })
    }

    /// Whether `path` is a session file older than the TTL. Without a TTL
    /// nothing expires, and a missing file is not expired (its caller reads
    /// it as empty).
    fn expired(&self, path: &Path) -> io::Result<bool> {
        let Some(ttl) = self.ttl else {
            return Ok(false);
        };
        let modified = match fs::metadata(path) {
            Ok(meta) => meta.modified()?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        Ok(SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age > ttl))
    }

    /// Remove every expired session file. A fault on one file is logged and
    /// skipped: a stale file is a cleanup miss, not a store fault.
    fn sweep_expired_sync(&self) -> io::Result<()> {
        if self.ttl.is_none() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(FILE_EXT) {
                continue;
            }
            match self.expired(&path) {
                Ok(true) => {
                    if let Err(err) = fs::remove_file(&path) {
                        tracing::warn!(
                            path = %path.display(), error = %err,
                            "expired skill session file not removed by sweep"
                        );
                    }
                }
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    path = %path.display(), error = %err,
                    "skill session file skipped by sweep"
                ),
            }
        }
        Ok(())
    }

    /// Read a session file into its non-empty lines, removing it first if
    /// expired. A missing file reads as empty.
    fn read_lines(&self, path: &Path) -> Result<Vec<String>, SessionStoreError> {
        if self.expired(path).map_err(request_err)? {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(request_err(err)),
            }
            return Ok(Vec::new());
        }
        match fs::read_to_string(path) {
            Ok(text) => Ok(text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(request_err(err)),
        }
    }

    fn record_sync(
        &self,
        session_id: &SessionId,
        record: SkillInvocationRecord,
    ) -> Result<(), SessionStoreError> {
        let _guard = self.lock();
        let path = self.session_path(session_id);
        let mut lines = self.read_lines(&path)?;

        let dedup_key = record.invocation.dedup_key();
        let mut held = 0usize;
        for line in &lines {
            if let Ok(existing) = SkillInvocationRecord::decode(line) {
                held += 1;
                if existing.invocation.dedup_key() == dedup_key {
                    // First write wins; the rewrite refreshes the file's mtime.
                    return publish(&path, rejoin(&lines).as_bytes());
                }
            }
        }
        if held >= MAX_SKILL_RECORDS_PER_SESSION {
            tracing::warn!(
                session_id = session_id.as_str(),
                cap = MAX_SKILL_RECORDS_PER_SESSION,
                "skill invocation store at per-session capacity; dropping new record"
            );
            return Ok(());
        }

        lines.push(
            serde_json::to_string(&record).expect("skill invocation record serializes to JSON"),
        );
        publish(&path, rejoin(&lines).as_bytes())
    }

    fn list_sync(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SkillInvocationRecord>, SessionStoreError> {
        let _guard = self.lock();
        let path = self.session_path(session_id);
        let mut records: Vec<SkillInvocationRecord> = self
            .read_lines(&path)?
            .iter()
            .filter_map(|line| match SkillInvocationRecord::decode(line) {
                Ok(record) => Some(record),
                Err(e) => {
                    tracing::warn!(
                        session_id = session_id.as_str(),
                        "skipping skill invocation record: {e}"
                    );
                    None
                }
            })
            .collect();
        records.sort_by_key(|r| (r.anchor, r.seq));
        Ok(records)
    }
}

/// One record per line, newline-terminated.
fn rejoin(lines: &[String]) -> String {
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[async_trait]
impl SkillInvocationStore for FileSkillInvocationStore {
    async fn record(
        &self,
        session_id: &SessionId,
        record: SkillInvocationRecord,
    ) -> Result<(), SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.clone();
        spawn_blocking(move || inner.record_sync(&session_id, record))
            .await
            .map_err(join_err)?
    }

    async fn list(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SkillInvocationRecord>, SessionStoreError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.clone();
        spawn_blocking(move || inner.list_sync(&session_id))
            .await
            .map_err(join_err)?
    }
}
