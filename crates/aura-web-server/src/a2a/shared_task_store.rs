use a2a::{A2AError, ListTasksRequest, ListTasksResponse, Task};
use a2a_server::task_store::TaskVersion;
use a2a_server::{InMemoryTaskStore, TaskStore};
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Clone)]
pub struct SharedTaskStore(Arc<InMemoryTaskStore>);

impl SharedTaskStore {
    pub fn new() -> Self {
        Self(Arc::new(InMemoryTaskStore::new()))
    }

    pub fn inner_store(&self) -> Arc<InMemoryTaskStore> {
        self.0.clone()
    }
}

impl Default for SharedTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskStore for SharedTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.0.create(task).await
    }
    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.0.update(task).await
    }
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        self.0.get(task_id).await
    }
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.0.list(req).await
    }
}
