//! File-backed session-store capabilities: durable parked approvals with no
//! infrastructure beyond a writable directory.
//!
//! On-disk layout under the store root — a persisted contract shared by every
//! process reading the store:
//!
//! | Path                                  | Contents                            |
//! | ------------------------------------- | ----------------------------------- |
//! | `{root}/approvals/{decision_id}.json` | one [`ParkedApprovalRecord`]        |
//! | `{root}/approvals/*.tmp`              | an interrupted write, ignored       |
//! | `{root}/approvals/*.taken`            | an interrupted take, ignored        |
//! | `{root}/approvals/*.probe`            | an interrupted open check, ignored  |
//!
//! There is no file-backed [`EventBus`](super::EventBus): a decision published
//! to a process that is no longer awaiting it has no reader, so the file
//! backend pairs durable approvals with the process-local bus
//! (`docs/adr/2026-07-21-hitl-park-reify.md` decision 14).
//!
//! Every guarantee rests on a filesystem primitive rather than a process-local
//! lock, so a root on shared storage behaves the same as one on a local disk.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use uuid::Uuid;

use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};

use super::{ApprovalStore, ParkedApprovalRecord, SessionStoreError};

const APPROVALS_DIR: &str = "approvals";
const RECORD_EXTENSION: &str = "json";
const TAKEN_EXTENSION: &str = "taken";
const PROBE_EXTENSION: &str = "probe";

/// Parked approvals as one JSON file per decision, under a directory root.
pub struct FileApprovalStore {
    dir: PathBuf,
}

impl FileApprovalStore {
    /// Open the approval directory under `root`, creating it if absent, and
    /// prove it is writable before returning.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let dir = root.as_ref().join(APPROVALS_DIR);
        fs::create_dir_all(&dir).map_err(|e| SessionStoreError::Connect {
            reason: format!("cannot create {}: {e}", dir.display()),
        })?;
        // `create_dir_all` is satisfied by an existing directory this process
        // cannot write to, and only an actual write proves otherwise. Deferring
        // that discovery to the first park is not equivalent: a park failing to
        // persist is not fatal to the park, so the fault would surface as a
        // caller waiting out its whole timeout instead of as a startup error.
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

fn record_path(dir: &Path, id: &DecisionId) -> PathBuf {
    dir.join(format!("{id}.{RECORD_EXTENSION}"))
}

/// Write to a uniquely named temporary file and rename it over the record
/// path: a concurrent reader sees either the previous record or the whole new
/// one, never a half-written file. A failed rename leaves no debris behind.
///
/// Both syncs are load-bearing, and neither is implied by the rename. Without
/// the file sync a crash can resurrect the record as a zero-length or torn
/// file; without the directory sync the new name itself can be lost, and a
/// park the caller was told is durable disappears across a reboot.
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
/// and the rest see it already gone. Unlinking is not a substitute — on macOS,
/// concurrent unlinks of one path each report success, which would hand the
/// same approval to several consumers.
///
/// The claim is only a claim once the directory entry is durable. An unsynced
/// rename can be rolled back by a host crash, restoring the record for a
/// second consumer after the first already ran the gated call — so a sync
/// failure is reported as a lost take rather than a won one.
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

/// Prove the directory accepts writes, by making one.
fn probe_writable(dir: &Path) -> Result<(), SessionStoreError> {
    let probe = dir.join(format!("{}.{PROBE_EXTENSION}", Uuid::now_v7()));
    fs::write(&probe, b"").map_err(|e| SessionStoreError::Connect {
        reason: format!("cannot write to {}: {e}", dir.display()),
    })?;
    fs::remove_file(&probe).map_err(|e| SessionStoreError::Connect {
        reason: format!("cannot remove {}: {e}", probe.display()),
    })
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    use super::*;
    use crate::hitl::{
        AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, PROTOCOL_VERSION,
    };
    use crate::session_store::conformance;

    /// Names the store root for the child half of the kill test.
    const CHILD_ROOT_ENV: &str = "AURA_TEST_FILE_STORE_ROOT";
    /// Prefixes the line the child prints once its approval is on disk.
    const CHILD_READY: &str = "registered-decision-id=";
    /// Upper bound on the killed child's life, so an aborted parent cannot
    /// leave a process running forever.
    const CHILD_MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(60);

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
                }],
            },
            registered_at: now,
            expires_at: now + chrono::Duration::seconds(60),
        }
    }

    #[tokio::test]
    async fn conforms_to_the_approval_store_contract() {
        let root = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(root.path()).unwrap();
        conformance::assert_approval_store_conformance(std::sync::Arc::new(store)).await;
    }

    #[tokio::test]
    async fn open_creates_a_missing_root() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("deep").join("store");
        FileApprovalStore::open(&nested).unwrap();
        assert!(nested.join(APPROVALS_DIR).is_dir());
    }

    #[tokio::test]
    async fn open_leaves_no_probe_behind() {
        let root = tempfile::tempdir().unwrap();
        FileApprovalStore::open(root.path()).unwrap();
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
        let opened = FileApprovalStore::open(root.path());
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

        let first = FileApprovalStore::open(root.path()).unwrap();
        first.register(entry).await.unwrap();
        drop(first);

        let second = FileApprovalStore::open(root.path()).unwrap();
        assert!(second.get(&id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn ping_fails_once_the_root_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(root.path()).unwrap();
        store.ping().await.unwrap();

        std::fs::remove_dir_all(root.path()).unwrap();
        assert!(store.ping().await.is_err());
    }

    #[tokio::test]
    async fn a_corrupt_record_is_a_decode_error() {
        let root = tempfile::tempdir().unwrap();
        let store = FileApprovalStore::open(root.path()).unwrap();
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
        let store = FileApprovalStore::open(root.path()).unwrap();
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

        let store = FileApprovalStore::open(root.path()).unwrap();
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
        let store = FileApprovalStore::open(&root).unwrap();
        let entry = parked("req-killed");
        let id = entry.request.decision_id;
        store.register(entry).await.unwrap();

        let mut stdout = std::io::stdout();
        writeln!(stdout, "{CHILD_READY}{id}").unwrap();
        stdout.flush().unwrap();

        tokio::time::sleep(CHILD_MAX_LIFETIME).await;
    }
}
