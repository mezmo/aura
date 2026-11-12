use a2a::*;
use a2a_server::middleware::ServiceParams;
use a2a_server::{AgentExecutor, DefaultRequestHandler, RequestHandler};
use async_trait::async_trait;
use aura::session_store::EventBus;
use futures_util::stream::BoxStream;
use std::sync::Arc;

use super::bus_bridge::relay_subscription;
use super::shared_task_store::SharedTaskStore;
use crate::auth::AuthContext;

/// Reserved service-params key carrying the authenticated principal from
/// [`AuraRequestHandler`] to [`super::AuraAgentExecutor`].
///
/// The auth middleware stores the principal in a task-local, but the a2a
/// framework spawns execution into a fresh tokio task and task-locals do
/// not cross a spawn — the executor would always see `None`. The request
/// handler, in contrast, runs inside the request task (the middleware
/// scope), so it copies the principal into `service_params`, which the
/// framework propagates to `ExecutorContext`.
///
/// `ServiceParams` is built from client headers, so the wrapper ALWAYS
/// overwrites (or removes) this key: a client cannot forge a principal.
const A2A_AUTH_PARAM: &str = "x-aura-auth";

#[derive(serde::Serialize, serde::Deserialize)]
struct PrincipalWire {
    name: String,
    agents: Vec<String>,
}

/// Return `params` with the reserved principal key set to the current
/// task's principal (or removed when there is none, so a forged header
/// cannot survive on an unauthenticated or auth-disabled server).
fn with_principal(mut params: ServiceParams) -> ServiceParams {
    params.remove(A2A_AUTH_PARAM);
    if let Some(ctx) = crate::auth::current_auth() {
        let wire = PrincipalWire {
            name: ctx.name,
            agents: ctx.allowed_agents,
        };
        match serde_json::to_string(&wire) {
            Ok(json) => {
                params.insert(A2A_AUTH_PARAM.to_string(), vec![json]);
            }
            Err(e) => {
                tracing::error!("failed to serialize auth principal for A2A execution: {e}");
            }
        }
    }
    params
}

/// Recover the principal injected by [`with_principal`] from an execution
/// context's service params. Malformed or absent values yield `None`
/// (unauthenticated), never a partial trust.
pub(super) fn principal_from_service_params(params: &ServiceParams) -> Option<AuthContext> {
    let raw = params.get(A2A_AUTH_PARAM)?.first()?;
    let wire: PrincipalWire = serde_json::from_str(raw).ok()?;
    if wire.name.is_empty() {
        return None;
    }
    Some(AuthContext {
        name: wire.name,
        allowed_agents: wire.agents,
    })
}

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
        // Capture the principal while still in the request task; execution
        // is spawned away and cannot see the task-local (see A2A_AUTH_PARAM).
        let params = with_principal(params.clone());
        self.inner.send_message(&params, req).await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let params = with_principal(params.clone());
        self.inner.send_streaming_message(&params, req).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, with_current_auth};
    use a2a_server::ExecutorContext;
    use tokio::sync::mpsc;

    /// Captures the `ServiceParams` the framework propagates to the
    /// executor — which runs inside the framework's spawned task, so this
    /// test only passes if the principal truly crosses the spawn boundary.
    struct CapturingExecutor {
        seen: mpsc::UnboundedSender<ServiceParams>,
    }

    #[async_trait]
    impl AgentExecutor for CapturingExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let _ = self.seen.send(ctx.service_params);
            Box::pin(futures_util::stream::empty())
        }

        fn cancel(
            &self,
            _ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            Box::pin(futures_util::stream::empty())
        }
    }

    fn send_request() -> SendMessageRequest {
        SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("hi")]),
            configuration: None,
            metadata: None,
            tenant: None,
        }
    }

    fn handler_with_forged_params() -> (AuraRequestHandler, mpsc::UnboundedReceiver<ServiceParams>)
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let session_store: Arc<dyn crate::session_store::SessionStore> =
            Arc::new(crate::session_store::InMemorySessionStore::new());
        let task_store = SharedTaskStore::from_store(session_store.tasks());
        let handler = AuraRequestHandler::new(
            CapturingExecutor { seen: tx },
            task_store,
            session_store.bus(),
        );
        (handler, rx)
    }

    fn forged_params() -> ServiceParams {
        ServiceParams::from([(
            A2A_AUTH_PARAM.to_string(),
            vec![r#"{"name":"forge","agents":["*"]}"#.to_string()],
        )])
    }

    #[tokio::test]
    async fn principal_reaches_executor_across_the_framework_spawn() {
        let (handler, mut rx) = handler_with_forged_params();
        let ctx = AuthContext {
            name: "alice".to_string(),
            allowed_agents: vec!["alpha".to_string()],
        };
        with_current_auth(ctx, async {
            handler
                .send_message(&forged_params(), send_request())
                .await
                .expect("send_message");
        })
        .await;

        let seen = rx.recv().await.expect("executor invoked");
        let principal = principal_from_service_params(&seen).expect("principal injected");
        assert_eq!(principal.name, "alice");
        assert_eq!(principal.allowed_agents, vec!["alpha".to_string()]);
    }

    #[tokio::test]
    async fn forged_principal_is_stripped_without_auth() {
        let (handler, mut rx) = handler_with_forged_params();
        handler
            .send_message(&forged_params(), send_request())
            .await
            .expect("send_message");

        let seen = rx.recv().await.expect("executor invoked");
        assert!(principal_from_service_params(&seen).is_none());
    }
}
