//! Durable configured-run records and typed controls shared with A2A.
//!
//! Files contain workflow data and approval references, never credentials.
//! An OS-held lock excludes concurrent writers; it is released after a crash.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::park::ParkedRun;

/// Parsed from an A2A data part before any text reaches a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRequest {
    pub run_id: uuid::Uuid,
    pub command: WorkflowCommand,
    pub message_id: String,
}

impl WorkflowRequest {
    /// Stable start identity for retries of an A2A message in the same scope.
    pub fn start(message_id: &str) -> Self {
        let digest = Sha256::digest(message_id.as_bytes());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self {
            run_id: uuid::Uuid::from_bytes(bytes),
            command: WorkflowCommand::Start,
            message_id: message_id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCommand {
    Start,
    Resume,
    Inspect,
    Takeover,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RunState {
    Ready,
    Running,
    Received {
        output: String,
    },
    AwaitingApproval {
        checkpoint: Box<ParkedRun>,
    },
    Waiting {
        wake_at: DateTime<Utc>,
        deadline: Option<DateTime<Utc>>,
    },
    HumanOwned {
        suspended: Box<RunState>,
    },
    Cancelled {
        execution_uncertain: bool,
    },
    Failed {
        reason: String,
    },
    Inconclusive,
    Uncertain,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunEvent {
    pub sequence: usize,
    pub at: DateTime<Utc>,
    pub stage: usize,
    pub event: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingNotification {
    pub key: String,
    pub suspended: Box<RunState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub version: u32,
    pub run_id: uuid::Uuid,
    pub agent: String,
    pub fingerprint: String,
    pub input: Value,
    pub stage: usize,
    pub results: BTreeMap<String, Value>,
    pub receipts: Vec<Value>,
    pub notifications: Vec<String>,
    pub pending_notification: Option<PendingNotification>,
    pub state: RunState,
    pub events: Vec<RunEvent>,
    pub messages: BTreeMap<String, String>,
    /// The verification deadline survives polling, approval and process restarts.
    pub verification_deadline: Option<DateTime<Utc>>,
}

impl RunRecord {
    pub(crate) fn transition(&mut self, state: RunState, event: &str) {
        self.state = state;
        self.events.push(RunEvent {
            sequence: self.events.len() + 1,
            at: Utc::now(),
            stage: self.stage,
            event: event.into(),
        });
    }
}

/// A run claim held for the lifetime of execution, including durable waits.
/// Separate runs can proceed concurrently; the same run cannot execute twice.
pub(crate) struct RunStore {
    path: PathBuf,
    _claim: Option<Arc<File>>,
    writes: Arc<Mutex<u64>>,
    next_write: AtomicU64,
}

impl RunStore {
    pub(crate) fn reader(root: &str, scope: &str, run_id: uuid::Uuid) -> Self {
        let scope = hex::encode(Sha256::digest(scope.as_bytes()));
        Self {
            path: Path::new(root)
                .join("workflows")
                .join(scope)
                .join(format!("{run_id}.json")),
            _claim: None,
            writes: Arc::new(Mutex::new(0)),
            next_write: AtomicU64::new(1),
        }
    }

    pub(crate) async fn open(root: &str, scope: &str, run_id: uuid::Uuid) -> io::Result<Self> {
        let scope = hex::encode(Sha256::digest(scope.as_bytes()));
        let directory = Path::new(root).join("workflows").join(scope);
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let claim = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join(format!("{run_id}.lock")))?;
            claim.try_lock().map_err(|error| match error {
                std::fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
                std::fs::TryLockError::Error(error) => error,
            })?;
            Ok(Self {
                path: directory.join(format!("{run_id}.json")),
                _claim: Some(Arc::new(claim)),
                writes: Arc::new(Mutex::new(0)),
                next_write: AtomicU64::new(1),
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(crate) async fn load(&self) -> io::Result<Option<RunRecord>> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => {
                let record: RunRecord = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if record.version != 1 {
                    return Err(io::Error::other("unsupported workflow record version"));
                }
                Ok(Some(record))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn save(&self, record: &RunRecord) -> io::Result<()> {
        if self._claim.is_none() {
            return Err(io::Error::other("workflow write requires a claim"));
        }
        let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
        let path = self.path.clone();
        let claim = self._claim.clone();
        let writes = Arc::clone(&self.writes);
        let ticket = self.next_write.fetch_add(1, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let _claim = claim;
            let mut published = writes
                .lock()
                .map_err(|_| io::Error::other("workflow writer poisoned"))?;
            if ticket < *published {
                return Ok(());
            }
            *published = ticket;
            let tmp = path.with_extension("tmp");
            let mut file = File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(tmp, &path)?;
            File::open(
                path.parent()
                    .ok_or_else(|| io::Error::other("missing workflow directory"))?,
            )?
            .sync_all()
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(crate) fn resume_path(&self) -> PathBuf {
        self.path.with_extension("resuming.json")
    }
}

pub(crate) fn target_fingerprint(mcp: &Option<aura_config::McpConfig>) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_value(mcp)
            .expect("MCP config is serializable")
            .to_string(),
    ))
}

pub(crate) struct RunCancellation(pub(crate) crate::request_cancellation::RequestCancellation);
impl Drop for RunCancellation {
    fn drop(&mut self) {
        crate::request_cancellation::RequestCancellation::unregister(&self.0.request_id);
    }
}

impl RunState {
    pub(crate) fn execution_uncertain(&self) -> bool {
        match self {
            Self::Running | Self::Uncertain => true,
            Self::HumanOwned { suspended } => suspended.execution_uncertain(),
            Self::Cancelled {
                execution_uncertain,
            } => *execution_uncertain,
            _ => false,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn claim_survives_reopen_and_excludes_a_second_writer() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_str().unwrap();
        let id = uuid::Uuid::new_v4();
        let store = RunStore::open(root, "session", id).await.unwrap();
        assert!(RunStore::open(root, "session", id).await.is_err());
        assert!(RunStore::open(root, "another session", id).await.is_ok());
        let record = RunRecord {
            version: 1,
            run_id: id,
            agent: "test".into(),
            fingerprint: "config".into(),
            input: Value::Null,
            stage: 0,
            results: BTreeMap::new(),
            receipts: Vec::new(),
            notifications: Vec::new(),
            pending_notification: None,
            state: RunState::Running,
            events: Vec::new(),
            messages: BTreeMap::new(),
            verification_deadline: None,
        };
        store.save(&record).await.unwrap();
        drop(store);
        let reopened = RunStore::open(root, "session", id).await.unwrap();
        assert!(matches!(
            reopened.load().await.unwrap().unwrap().state,
            RunState::Running
        ));
    }
}
