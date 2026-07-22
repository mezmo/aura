use a2a::*;
use a2a_server::middleware::ServiceParams;
use a2a_server::{AgentExecutor, DefaultRequestHandler, RequestHandler};
use async_trait::async_trait;
use aura::session_store::EventBus;
use futures_util::stream::BoxStream;
use std::sync::Arc;

use super::bus_bridge::relay_subscription;
use super::shared_task_store::SharedTaskStore;

/// Wraps [`DefaultRequestHandler`] and forces `return_immediately = true` on every
/// `message:send` request so the HTTP response returns as soon as the task is
/// queued (`Working` state), without waiting for the agent to finish.
///
/// Callers can poll `tasks/{id}` or subscribe via `message:stream` to check
/// the status of the task and view completion events.
///
/// `subscribe_to_task` additionally falls back to a bus relay when the task
/// is executing on another instance (see `bus_bridge`).
pub struct AuraRequestHandler {
    inner: Arc<DefaultRequestHandler>,
    task_store: SharedTaskStore,
    bus: Arc<dyn EventBus>,
}

impl AuraRequestHandler {
    pub fn new(
        executor: impl AgentExecutor,
        task_store: SharedTaskStore,
        bus: Arc<dyn EventBus>,
    ) -> Self {
        Self {
            inner: Arc::new(DefaultRequestHandler::new(executor, task_store.clone())),
            task_store,
            bus,
        }
    }
}

#[async_trait]
impl RequestHandler for AuraRequestHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        mut req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        match req.configuration {
            Some(ref mut c) => c.return_immediately = Some(true),
            None => {
                req.configuration = Some(SendMessageConfiguration {
                    // force the execution to return the task immediately be default
                    return_immediately: Some(true),
                    accepted_output_modes: None,
                    task_push_notification_config: None,
                    history_length: None,
                })
            }
        }
        self.inner.send_message(params, req).await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.inner.send_streaming_message(params, req).await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.inner.get_task(params, req).await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.inner.list_tasks(params, req).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.inner.cancel_task(params, req).await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let task_id = req.id.clone();
        match self.inner.subscribe_to_task(params, req).await {
            Ok(stream) => Ok(stream),
            // Not executing on this instance — relay the fan-out topic if the
            // task is live somewhere else (`task_not_found` again otherwise).
            // Every other error kind is the caller's answer as-is.
            Err(err) if err.code == error_code::TASK_NOT_FOUND => {
                relay_subscription(self.bus.clone(), self.task_store.clone(), &task_id).await
            }
            Err(err) => Err(err),
        }
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.create_push_config(params, req).await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.get_push_config(params, req).await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.inner.list_push_configs(params, req).await
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.inner.delete_push_config(params, req).await
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.inner.get_extended_agent_card(params, req).await
    }
}
