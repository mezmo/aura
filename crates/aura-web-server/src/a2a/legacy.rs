//! A2A v0.3 JSON-RPC binding, served at the server root.
//!
//! v0.3 clients address an agent by a single base URL and POST JSON-RPC to it,
//! so they never reach the versioned `/a2a/v1/rpc` mount. This module accepts
//! those requests at `/`, translates the payloads (see [`types`]), and hands
//! them to the same [`a2a_server::RequestHandler`] as the v1.0 binding.
//!
//! Only the methods a v0.3 client can reach given aura's advertised
//! capabilities are dispatched: `message/send`, `message/stream`, `tasks/get`,
//! `tasks/cancel` and `tasks/resubscribe`. Push-notification and extended-card
//! methods answer `method not found`, matching the card's
//! `pushNotifications: false` and absent extended-card capability.

mod types;

use std::sync::Arc;

use a2a::{
    A2AError, CancelTaskRequest, GetTaskRequest, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    SendMessageRequest, SendMessageResponse, StreamResponse, SubscribeToTaskRequest, Task,
};
use a2a_server::sse::sse_jsonrpc_stream;
use a2a_server::{RequestHandler, ServiceParams};
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub use types::{
    LegacyArtifact, LegacyEvent, LegacyFile, LegacyMessage, LegacyMessageSendParams, LegacyPart,
    LegacyRole, LegacyTask, LegacyTaskIdParams, LegacyTaskQueryParams, LegacyTaskState,
    LegacyTaskStatus, LegacyTaskStatusUpdateEvent,
};

/// The A2A protocol version this binding implements.
pub const LEGACY_PROTOCOL_VERSION: &str = "0.3";

mod methods {
    pub const MESSAGE_SEND: &str = "message/send";
    pub const MESSAGE_STREAM: &str = "message/stream";
    pub const TASKS_GET: &str = "tasks/get";
    pub const TASKS_CANCEL: &str = "tasks/cancel";
    pub const TASKS_RESUBSCRIBE: &str = "tasks/resubscribe";

    pub fn is_streaming(method: &str) -> bool {
        matches!(method, MESSAGE_STREAM | TASKS_RESUBSCRIBE)
    }
}

/// Serve the v0.3 JSON-RPC binding at `/`.
pub fn legacy_jsonrpc_router<H: RequestHandler>(handler: Arc<H>) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::post(handle_jsonrpc::<H>))
        .with_state(handler)
}

/// Parse the JSON-RPC envelope by hand rather than through the `Json`
/// extractor, so a malformed body answers `-32700` instead of a bare HTTP 422
/// that a JSON-RPC client cannot interpret.
async fn handle_jsonrpc<H: RequestHandler>(
    State(handler): State<Arc<H>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return error_response(JsonRpcId::Null, A2AError::parse_error(err.to_string()));
        }
    };

    let id = request.id.clone();
    if request.jsonrpc != "2.0" {
        return error_response(id, A2AError::invalid_request("invalid jsonrpc version"));
    }

    let params = service_params_from_headers(&headers);
    let raw_params = request.params.clone().unwrap_or(Value::Null);
    let method = request.method.as_str();

    if methods::is_streaming(method) {
        return handle_streaming(handler.as_ref(), &params, method, raw_params, id).await;
    }

    let result = handle_unary(handler.as_ref(), &params, method, raw_params).await;
    match result {
        Ok(value) => (StatusCode::OK, Json(JsonRpcResponse::success(id, value))).into_response(),
        Err(err) => error_response(id, err),
    }
}

async fn handle_unary<H: RequestHandler>(
    handler: &H,
    params: &a2a_server::ServiceParams,
    method: &str,
    raw_params: Value,
) -> Result<Value, A2AError> {
    match method {
        methods::MESSAGE_SEND => {
            let request: SendMessageRequest =
                parse_params::<LegacyMessageSendParams>(raw_params)?.into();
            let response: SendMessageResponse = handler.send_message(params, request).await?;
            to_value(LegacyEvent::from(response))
        }
        methods::TASKS_GET => {
            let request: GetTaskRequest = parse_params::<LegacyTaskQueryParams>(raw_params)?.into();
            let task: Task = handler.get_task(params, request).await?;
            to_value(LegacyTask::from(task))
        }
        methods::TASKS_CANCEL => {
            let request: CancelTaskRequest = parse_params::<LegacyTaskIdParams>(raw_params)?.into();
            let task: Task = handler.cancel_task(params, request).await?;
            to_value(LegacyTask::from(task))
        }
        "" => Err(A2AError::invalid_request("method is required")),
        other => Err(A2AError::method_not_found(other)),
    }
}

async fn handle_streaming<H: RequestHandler>(
    handler: &H,
    params: &a2a_server::ServiceParams,
    method: &str,
    raw_params: Value,
    id: JsonRpcId,
) -> Response {
    let stream = match method {
        methods::MESSAGE_STREAM => match parse_params::<LegacyMessageSendParams>(raw_params) {
            Ok(parsed) => handler.send_streaming_message(params, parsed.into()).await,
            Err(err) => Err(err),
        },
        methods::TASKS_RESUBSCRIBE => match parse_params::<LegacyTaskIdParams>(raw_params) {
            Ok(parsed) => {
                let request: SubscribeToTaskRequest = parsed.into();
                handler.subscribe_to_task(params, request).await
            }
            Err(err) => Err(err),
        },
        other => Err(A2AError::method_not_found(other)),
    };

    match stream {
        Ok(stream) => sse_jsonrpc_stream(id, legacy_event_stream(stream)).into_response(),
        Err(err) => error_response(id, err),
    }
}

fn legacy_event_stream(
    stream: BoxStream<'static, Result<StreamResponse, A2AError>>,
) -> BoxStream<'static, Result<LegacyEvent, A2AError>> {
    Box::pin(stream.map(|item| item.map(LegacyEvent::from)))
}

/// Mirror the v1.0 binding's header handling: lowercase names (axum already
/// normalizes them), non-ASCII values dropped, multi-valued headers preserved
/// in order. `AuraAgentExecutor` reads `x-aura-model` out of the result.
fn service_params_from_headers(headers: &HeaderMap) -> ServiceParams {
    let mut params = ServiceParams::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            params
                .entry(name.as_str().to_owned())
                .or_default()
                .push(value.to_owned());
        }
    }
    params
}

fn parse_params<T: DeserializeOwned>(raw_params: Value) -> Result<T, A2AError> {
    serde_json::from_value(raw_params)
        .map_err(|err| A2AError::invalid_params(format!("invalid params: {err}")))
}

fn to_value<T: Serialize>(value: T) -> Result<Value, A2AError> {
    serde_json::to_value(value)
        .map_err(|err| A2AError::internal(format!("failed to serialize v0.3 payload: {err}")))
}

/// A2A carries protocol failures in the JSON-RPC envelope, so the HTTP status
/// stays 200 and the client reads `error`.
fn error_response(id: JsonRpcId, err: A2AError) -> Response {
    (
        StatusCode::OK,
        Json(JsonRpcResponse::error(id, err.to_jsonrpc_error())),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Message, Part, Role, TaskState, TaskStatus, TaskStatusUpdateEvent, error_code};
    use a2a_server::ServiceParams;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    /// Answers each method with a fixed payload so the tests assert on the
    /// translation layer rather than on agent behavior.
    struct StubHandler;

    fn stub_task(state: TaskState) -> Task {
        Task {
            id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    #[async_trait]
    impl RequestHandler for StubHandler {
        async fn send_message(
            &self,
            _params: &ServiceParams,
            req: SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            // Echo the request text back so the test can assert the inbound
            // translation landed.
            Ok(SendMessageResponse::Message(Message::new(
                Role::Agent,
                vec![Part::text(req.message.text().unwrap_or_default())],
            )))
        }

        async fn send_streaming_message(
            &self,
            _params: &ServiceParams,
            _req: SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            let events = vec![
                Ok(StreamResponse::Task(stub_task(TaskState::Working))),
                Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: "t1".into(),
                    context_id: "c1".into(),
                    status: TaskStatus {
                        state: TaskState::Completed,
                        message: None,
                        timestamp: None,
                    },
                    metadata: None,
                })),
            ];
            Ok(Box::pin(futures_util::stream::iter(events)))
        }

        async fn get_task(
            &self,
            _params: &ServiceParams,
            _req: GetTaskRequest,
        ) -> Result<Task, A2AError> {
            Ok(stub_task(TaskState::Completed))
        }

        async fn list_tasks(
            &self,
            _params: &ServiceParams,
            _req: a2a::ListTasksRequest,
        ) -> Result<a2a::ListTasksResponse, A2AError> {
            Err(A2AError::unsupported_operation("list"))
        }

        async fn cancel_task(
            &self,
            _params: &ServiceParams,
            _req: CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            Ok(stub_task(TaskState::Canceled))
        }

        async fn subscribe_to_task(
            &self,
            _params: &ServiceParams,
            _req: SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Err(A2AError::task_not_found("t1"))
        }

        async fn create_push_config(
            &self,
            _params: &ServiceParams,
            _req: a2a::TaskPushNotificationConfig,
        ) -> Result<a2a::TaskPushNotificationConfig, A2AError> {
            Err(A2AError::push_notification_not_supported())
        }

        async fn get_push_config(
            &self,
            _params: &ServiceParams,
            _req: a2a::GetTaskPushNotificationConfigRequest,
        ) -> Result<a2a::TaskPushNotificationConfig, A2AError> {
            Err(A2AError::push_notification_not_supported())
        }

        async fn list_push_configs(
            &self,
            _params: &ServiceParams,
            _req: a2a::ListTaskPushNotificationConfigsRequest,
        ) -> Result<a2a::ListTaskPushNotificationConfigsResponse, A2AError> {
            Err(A2AError::push_notification_not_supported())
        }

        async fn delete_push_config(
            &self,
            _params: &ServiceParams,
            _req: a2a::DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            Err(A2AError::push_notification_not_supported())
        }

        async fn get_extended_agent_card(
            &self,
            _params: &ServiceParams,
            _req: a2a::GetExtendedAgentCardRequest,
        ) -> Result<a2a::AgentCard, A2AError> {
            Err(A2AError::unsupported_operation("extended card"))
        }
    }

    fn app() -> axum::Router {
        legacy_jsonrpc_router(Arc::new(StubHandler))
    }

    async fn post(body: Value) -> (StatusCode, String) {
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn post_json(body: Value) -> Value {
        let (status, text) = post(body).await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_str(&text).unwrap()
    }

    /// The exact request kagent's v0.3 transport sends to a BYO agent's root.
    fn message_send_request() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": "t1",
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "say hello"}],
                    "messageId": "m1",
                    "kind": "message",
                },
            },
        })
    }

    #[tokio::test]
    async fn message_send_at_the_root_returns_a_v0_3_result() {
        let response = post_json(message_send_request()).await;

        assert_eq!(response["jsonrpc"], json!("2.0"));
        assert_eq!(response["id"], json!("t1"));
        assert_eq!(response["result"]["kind"], json!("message"));
        assert_eq!(response["result"]["role"], json!("agent"));
        assert_eq!(
            response["result"]["parts"],
            json!([{"kind": "text", "text": "say hello"}])
        );
        assert!(response.get("error").is_none());
    }

    #[tokio::test]
    async fn tasks_get_returns_a_kebab_case_state() {
        let response = post_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/get",
            "params": {"id": "t1"},
        }))
        .await;

        assert_eq!(response["result"]["kind"], json!("task"));
        assert_eq!(response["result"]["id"], json!("t1"));
        assert_eq!(response["result"]["status"]["state"], json!("completed"));
    }

    #[tokio::test]
    async fn tasks_cancel_returns_the_canceled_task() {
        let response = post_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/cancel",
            "params": {"id": "t1"},
        }))
        .await;

        assert_eq!(response["result"]["status"]["state"], json!("canceled"));
    }

    #[tokio::test]
    async fn message_stream_emits_v0_3_sse_frames() {
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": "s1",
                    "method": "message/stream",
                    "params": {
                        "message": {
                            "role": "user",
                            "parts": [{"kind": "text", "text": "hi"}],
                            "messageId": "m1",
                        },
                    },
                })
                .to_string(),
            ))
            .unwrap();

        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let frames: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], json!("s1"));
        assert_eq!(frames[0]["result"]["kind"], json!("task"));
        assert_eq!(frames[0]["result"]["status"]["state"], json!("working"));
        assert_eq!(frames[1]["result"]["kind"], json!("status-update"));
        assert_eq!(frames[1]["result"]["final"], json!(true));
    }

    #[tokio::test]
    async fn v1_method_names_are_rejected_at_the_v0_3_binding() {
        let response = post_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "SendMessage",
            "params": {},
        }))
        .await;

        assert_eq!(
            response["error"]["code"],
            json!(error_code::METHOD_NOT_FOUND)
        );
    }

    #[tokio::test]
    async fn handler_errors_surface_as_jsonrpc_errors_with_http_200() {
        let response = post_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/resubscribe",
            "params": {"id": "t1"},
        }))
        .await;

        assert_eq!(response["error"]["code"], json!(error_code::TASK_NOT_FOUND));
    }

    #[tokio::test]
    async fn malformed_params_answer_invalid_params_not_http_422() {
        let response = post_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/get",
            "params": {},
        }))
        .await;

        assert_eq!(response["error"]["code"], json!(error_code::INVALID_PARAMS));
    }

    #[tokio::test]
    async fn malformed_json_answers_a_parse_error_not_http_422() {
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .unwrap();

        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], json!(error_code::PARSE_ERROR));
    }

    #[tokio::test]
    async fn a_wrong_jsonrpc_version_is_rejected() {
        let response = post_json(json!({
            "jsonrpc": "1.0",
            "id": 1,
            "method": "tasks/get",
            "params": {"id": "t1"},
        }))
        .await;

        assert_eq!(
            response["error"]["code"],
            json!(error_code::INVALID_REQUEST)
        );
    }
}
