use a2a::{A2AError, Artifact, ListTasksRequest, ListTasksResponse, Task};
use a2a_server::task_store::TaskVersion;
use a2a_server::{InMemoryTaskStore, TaskStore};
use async_trait::async_trait;
use std::sync::Arc;

/// Wrapper over the backing [`TaskStore`] adding AURA's artifact-merge fix to
/// `update`.
#[derive(Clone)]
pub struct SharedTaskStore(Arc<dyn TaskStore>);

impl SharedTaskStore {
    pub fn new() -> Self {
        Self::from_store(Arc::new(InMemoryTaskStore::new()))
    }

    pub fn from_store(store: Arc<dyn TaskStore>) -> Self {
        Self(store)
    }
}

impl Default for SharedTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Collapse duplicate artifact entries by `artifact_id`, concatenating their `parts` in
/// arrival order. Name, description, metadata, and extensions are taken from the first
/// occurrence. This compensates for the upstream `a2a-server` handler ignoring the `append`
/// field and always pushing artifact updates as new Vec entries.
fn merge_artifacts(mut task: Task) -> Task {
    let Some(artifacts) = task.artifacts.take() else {
        return task;
    };

    let mut merged: Vec<Artifact> = Vec::new();
    for artifact in artifacts {
        if let Some(existing) = merged
            .iter_mut()
            .find(|a| a.artifact_id == artifact.artifact_id)
        {
            existing.parts.extend(artifact.parts);
        } else {
            merged.push(artifact);
        }
    }

    task.artifacts = Some(merged);
    task
}

#[async_trait]
impl TaskStore for SharedTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.0.create(task).await
    }
    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.0.update(merge_artifacts(task)).await
    }
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        self.0.get(task_id).await
    }
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.0.list(req).await
    }
}
