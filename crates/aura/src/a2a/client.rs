//! Minimal A2A v1.0 JSON-RPC client over the crate's `reqwest` dependency.
//!
//! Only the three calls the remote-agent tool needs are implemented:
//! `SendMessage`, `GetTask`, and `CancelTask`. Wire types come from the
//! `a2a` crate so the request and response shapes match the server binding
//! the web server crate hosts.

use std::collections::HashMap;
use std::time::Duration;

use a2a::jsonrpc::methods;
use a2a::{
    CancelTaskRequest, GetTaskRequest, JsonRpcId, JsonRpcRequest, JsonRpcResponse, Message, Part,
    Role, SendMessageConfiguration, SendMessageRequest, SendMessageResponse, Task, VERSION,
};
use aura_config::a2a::{MODEL_HEADER, jsonrpc_endpoint, parse_header};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Header carrying the protocol version on every request.
const VERSION_HEADER: &str = "a2a-version";

/// Bytes of a non-2xx body kept in the error message.
const MAX_ERROR_BODY_BYTES: usize = 512;

/// Connect timeout for the underlying HTTP client.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-request timeout for one JSON-RPC round trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum A2aClientError {
    #[error("invalid remote agent URL {url:?}: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("invalid header {name:?} for remote agent: {reason}")]
    InvalidHeader { name: String, reason: String },
    #[error("failed to build HTTP client: {0}")]
    HttpClient(reqwest::Error),
    #[error("request to {endpoint} failed: {source}")]
    Http {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{endpoint} answered HTTP {status}: {body}")]
    Status {
        endpoint: String,
        status: u16,
        body: String,
    },
    #[error("remote agent returned JSON-RPC error {code}: {message}")]
    Rpc { code: i32, message: String },
    #[error("malformed A2A response from {endpoint}: {reason}")]
    Protocol { endpoint: String, reason: String },
}

/// One remote agent's JSON-RPC endpoint plus the headers every call carries.
#[derive(Clone, Debug)]
pub struct A2aClient {
    http: reqwest::Client,
    endpoint: String,
    headers: HeaderMap,
}

impl A2aClient {
    /// Build a client for the remote at `base_url` (an `http(s)` origin; the
    /// JSON-RPC path is appended). `headers` ride on every request under
    /// lowercased names.
    pub fn new(
        base_url: &str,
        headers: &HashMap<String, String>,
        user_agent: &str,
    ) -> Result<Self, A2aClientError> {
        let endpoint = jsonrpc_endpoint(base_url).map_err(|reason| A2aClientError::InvalidUrl {
            url: base_url.to_owned(),
            reason,
        })?;
        let mut header_map = HeaderMap::with_capacity(headers.len() + 1);
        for (name, value) in headers {
            let (name, value) = header_for_wire(name, value)?;
            header_map.insert(name, value);
        }
        header_map.insert(
            HeaderName::from_static(VERSION_HEADER),
            HeaderValue::from_static(VERSION),
        );
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(A2aClientError::HttpClient)?;
        Ok(Self {
            http,
            endpoint,
            headers: header_map,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Send one user text message. `context_id` continues an earlier
    /// exchange; `model` selects an agent on the remote. The remote answers
    /// with either a task to poll or a finished message.
    pub async fn send_message(
        &self,
        text: &str,
        context_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<SendMessageResponse, A2aClientError> {
        let mut message = Message::new(Role::User, vec![Part::text(text)]);
        message.context_id = context_id.map(str::to_owned);
        let request = SendMessageRequest {
            message,
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: None,
                task_push_notification_config: None,
                history_length: Some(0),
                return_immediately: Some(true),
            }),
            metadata: None,
            tenant: None,
        };
        let mut extra = HeaderMap::new();
        if let Some(model) = model {
            let (name, value) = header_for_wire(MODEL_HEADER, model)?;
            extra.insert(name, value);
        }
        self.call(methods::SEND_MESSAGE, &request, extra).await
    }

    pub async fn get_task(&self, task_id: &str) -> Result<Task, A2aClientError> {
        let request = GetTaskRequest {
            id: task_id.to_owned(),
            history_length: Some(0),
            tenant: None,
        };
        self.call(methods::GET_TASK, &request, HeaderMap::new())
            .await
    }

    pub async fn cancel_task(&self, task_id: &str) -> Result<Task, A2aClientError> {
        let request = CancelTaskRequest {
            id: task_id.to_owned(),
            metadata: None,
            tenant: None,
        };
        self.call(methods::CANCEL_TASK, &request, HeaderMap::new())
            .await
    }

    /// One JSON-RPC round trip. A non-2xx status is reported with a bounded
    /// body excerpt because a gateway in front of the remote (auth, routing)
    /// answers in its own format, not as a JSON-RPC envelope.
    async fn call<P: serde::Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        extra_headers: HeaderMap,
    ) -> Result<R, A2aClientError> {
        let endpoint = self.endpoint.clone();
        let params = serde_json::to_value(params).map_err(|e| A2aClientError::Protocol {
            endpoint: endpoint.clone(),
            reason: format!("failed to serialize {method} params: {e}"),
        })?;
        let request = JsonRpcRequest::new(
            JsonRpcId::String(uuid::Uuid::now_v7().to_string()),
            method,
            Some(params),
        );

        let response = self
            .http
            .post(&endpoint)
            .headers(self.headers.clone())
            .headers(extra_headers)
            .json(&request)
            .send()
            .await
            .map_err(|source| A2aClientError::Http {
                endpoint: endpoint.clone(),
                source,
            })?;

        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|source| A2aClientError::Http {
                endpoint: endpoint.clone(),
                source,
            })?;
        if !status.is_success() {
            return Err(A2aClientError::Status {
                endpoint,
                status: status.as_u16(),
                body: bounded_text(&body),
            });
        }

        let envelope: JsonRpcResponse =
            serde_json::from_slice(&body).map_err(|e| A2aClientError::Protocol {
                endpoint: endpoint.clone(),
                reason: format!("{method} response is not a JSON-RPC envelope: {e}"),
            })?;
        if let Some(error) = envelope.error {
            return Err(A2aClientError::Rpc {
                code: error.code,
                message: error.message,
            });
        }
        let result: Value = envelope.result.ok_or_else(|| A2aClientError::Protocol {
            endpoint: endpoint.clone(),
            reason: format!("{method} response carries neither result nor error"),
        })?;
        serde_json::from_value(result).map_err(|e| A2aClientError::Protocol {
            endpoint,
            reason: format!("{method} result did not deserialize: {e}"),
        })
    }
}

fn header_for_wire(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), A2aClientError> {
    parse_header(name, value).map_err(|reason| A2aClientError::InvalidHeader {
        name: name.to_owned(),
        reason,
    })
}

fn bounded_text(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if text.len() <= MAX_ERROR_BODY_BYTES {
        return text.into_owned();
    }
    let cut = text.floor_char_boundary(MAX_ERROR_BODY_BYTES);
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::test_server::{LoopbackA2aServer, working_task};
    use a2a::TaskState;

    #[test]
    fn rejects_headers_that_cannot_travel() {
        let headers = HashMap::from([("bad header".to_owned(), "x".to_owned())]);
        assert!(matches!(
            A2aClient::new("http://spoke", &headers, "aura/test"),
            Err(A2aClientError::InvalidHeader { .. })
        ));
        let headers = HashMap::from([("x-ok".to_owned(), "line\nbreak".to_owned())]);
        assert!(matches!(
            A2aClient::new("http://spoke", &headers, "aura/test"),
            Err(A2aClientError::InvalidHeader { .. })
        ));
    }

    #[tokio::test]
    async fn send_message_carries_headers_version_and_model() {
        let server = LoopbackA2aServer::start(|method, _params| match method {
            "SendMessage" => Ok(serde_json::json!({ "task": working_task("t1", "ctx-1") })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let headers = HashMap::from([("Authorization".to_owned(), "Bearer key".to_owned())]);
        let client = A2aClient::new(&server.url, &headers, "aura/test").unwrap();

        let reply = client
            .send_message("hello", Some("ctx-1"), Some("verifier"))
            .await
            .unwrap();
        let SendMessageResponse::Task(task) = reply else {
            panic!("expected a task");
        };
        assert_eq!(task.id, "t1");
        assert_eq!(task.status.state, TaskState::Working);

        let request = server.requests().remove(0);
        assert_eq!(request.header_values("authorization"), vec!["Bearer key"]);
        assert_eq!(request.header_values("a2a-version"), vec!["1.0"]);
        assert_eq!(request.header_values("x-aura-model"), vec!["verifier"]);
        assert_eq!(request.header_values("user-agent"), vec!["aura/test"]);
        let body = request.body_json();
        assert_eq!(body["method"], "SendMessage");
        assert_eq!(body["params"]["message"]["role"], "ROLE_USER");
        assert_eq!(body["params"]["message"]["contextId"], "ctx-1");
        assert_eq!(body["params"]["message"]["parts"][0]["text"], "hello");
        assert_eq!(body["params"]["configuration"]["returnImmediately"], true);
    }

    #[tokio::test]
    async fn rpc_errors_and_http_failures_are_distinguished() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "GetTask" => Err((-32001, "task not found".to_owned())),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let client = A2aClient::new(&server.url, &HashMap::new(), "aura/test").unwrap();
        let err = client.get_task("nope").await.unwrap_err();
        assert!(
            matches!(err, A2aClientError::Rpc { code: -32001, ref message } if message == "task not found"),
            "{err}"
        );

        let server = LoopbackA2aServer::start_with_status(401, "{\"message\":\"no key\"}").await;
        let client = A2aClient::new(&server.url, &HashMap::new(), "aura/test").unwrap();
        let err = client.get_task("t").await.unwrap_err();
        assert!(
            matches!(err, A2aClientError::Status { status: 401, ref body, .. } if body.contains("no key")),
            "{err}"
        );
    }
}
