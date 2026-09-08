//! Single-instance A2A task persistence for the existing file session backend.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use a2a::{A2AError, ListTasksRequest, ListTasksResponse, Task, TaskState};
use a2a_server::{InMemoryTaskStore, TaskStore, task_store::TaskVersion};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    task: Task,
    version: TaskVersion,
}

pub(super) struct FileTaskStore {
    directory: PathBuf,
    entries: Arc<Mutex<BTreeMap<String, Entry>>>,
    _claim: Arc<File>,
}

impl FileTaskStore {
    pub(super) fn open(root: &str) -> io::Result<Self> {
        let directory = Path::new(root).join("tasks");
        std::fs::create_dir_all(&directory)?;
        let claim = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("writer.lock"))?;
        claim.try_lock().map_err(io::Error::other)?;
        let mut entries = BTreeMap::new();
        for file in std::fs::read_dir(&directory)? {
            let path = file?.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let mut entry: Entry =
                serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
            // A transport stream cannot survive a server restart. The workflow
            // record remains authoritative and resume reattaches fresh credentials.
            if matches!(
                entry.task.status.state,
                TaskState::Working | TaskState::Submitted
            ) {
                entry.task.status.state = TaskState::InputRequired;
            }
            if entries.insert(entry.task.id.clone(), entry).is_some() {
                return Err(io::Error::other("duplicate persisted A2A task"));
            }
        }
        Ok(Self {
            directory,
            entries: Arc::new(Mutex::new(entries)),
            _claim: Arc::new(claim),
        })
    }

    async fn write(&self, task: Task, create: bool) -> Result<TaskVersion, A2AError> {
        let entries = Arc::clone(&self.entries);
        let claim = Arc::clone(&self._claim);
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            let _claim = claim;
            let mut entries = entries
                .lock()
                .map_err(|_| A2AError::internal("task writer poisoned"))?;
            let version = match (entries.get(&task.id), create) {
                (None, true) => 1,
                (Some(_), true) => return Err(A2AError::invalid_params("task already exists")),
                (None, false) => return Err(A2AError::task_not_found(&task.id)),
                (Some(entry), false) => entry
                    .version
                    .checked_add(1)
                    .ok_or_else(|| A2AError::internal("task version exhausted"))?,
            };
            let entry = Entry { task, version };
            let publish = || -> io::Result<()> {
                let bytes = serde_json::to_vec(&entry).map_err(io::Error::other)?;
                let id =
                    aura::orchestration::workflow::WorkflowRequest::start(&entry.task.id).run_id;
                let path = directory.join(format!("{id}.json"));
                let tmp = path.with_extension("tmp");
                let mut file = File::create(&tmp)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                std::fs::rename(tmp, &path)?;
                File::open(&directory)?.sync_all()
            };
            publish().map_err(|e| A2AError::internal(e.to_string()))?;
            entries.insert(entry.task.id.clone(), entry);
            Ok(version)
        })
        .await
        .map_err(|e| A2AError::internal(e.to_string()))?
    }
}

#[async_trait]
impl TaskStore for FileTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.write(task, true).await
    }
    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.write(task, false).await
    }
    async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| A2AError::internal("task store poisoned"))?
            .get(id)
            .map(|e| e.task.clone()))
    }
    async fn list(&self, request: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        // Reuse upstream filtering/history rules. Clamp pagination before the
        // upstream slice operation, which assumes a token inside the result set.
        let snapshot = InMemoryTaskStore::new();
        let tasks: Vec<_> = self
            .entries
            .lock()
            .map_err(|_| A2AError::internal("task store poisoned"))?
            .values()
            .map(|e| e.task.clone())
            .collect();
        for task in tasks {
            snapshot.create(task).await?;
        }
        let mut query = request.clone();
        query.page_token = None;
        query.page_size = Some(i32::MAX);
        let mut result = snapshot.list(&query).await?;
        let start = request
            .page_token
            .as_deref()
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| A2AError::invalid_params("invalid page token"))?
            .min(result.tasks.len());
        let size = request.page_size.filter(|n| *n > 0).unwrap_or(50) as usize;
        let end = start.saturating_add(size).min(result.tasks.len());
        result.next_page_token = if end < result.tasks.len() {
            end.to_string()
        } else {
            String::new()
        };
        result.tasks = result.tasks[start..end].to_vec();
        result.page_size = size as i32;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn restart_preserves_task_and_requires_reattachment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_str().unwrap();
        let store = FileTaskStore::open(path).unwrap();
        assert!(FileTaskStore::open(path).is_err());
        let task = Task {
            id: "../opaque-id".into(),
            context_id: "session".into(),
            status: a2a::TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        assert_eq!(store.create(task).await.unwrap(), 1);
        drop(store);
        let reopened = FileTaskStore::open(path).unwrap();
        let task = reopened.get("../opaque-id").await.unwrap().unwrap();
        assert_eq!(task.status.state, TaskState::InputRequired);
        assert_eq!(reopened.update(task).await.unwrap(), 2);
    }
}
