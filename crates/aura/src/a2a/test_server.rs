//! A loopback A2A JSON-RPC server for unit tests: one handler closure
//! answers every method, and every request is kept so a test can inspect
//! what reached the wire.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A JSON-RPC handler: `Ok(result)` or `Err((code, message))`.
pub(crate) type Handler =
    dyn Fn(&str, &Value) -> Result<Value, (i32, String)> + Send + Sync + 'static;

#[derive(Clone)]
pub(crate) struct RecordedRequest {
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl RecordedRequest {
    pub(crate) fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub(crate) fn body_json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("client sends JSON")
    }

    pub(crate) fn method(&self) -> String {
        self.body_json()["method"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

enum Mode {
    Rpc(Arc<Handler>),
    FixedStatus(u16, String),
}

pub(crate) struct LoopbackA2aServer {
    /// Origin the client is pointed at.
    pub(crate) url: String,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl LoopbackA2aServer {
    pub(crate) async fn start<H>(handler: H) -> Self
    where
        H: Fn(&str, &Value) -> Result<Value, (i32, String)> + Send + Sync + 'static,
    {
        Self::start_mode(Mode::Rpc(Arc::new(handler))).await
    }

    /// Answer every request with `status` and `body`, as a gateway would.
    pub(crate) async fn start_with_status(status: u16, body: &str) -> Self {
        Self::start_mode(Mode::FixedStatus(status, body.to_owned())).await
    }

    async fn start_mode(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let mode = Arc::new(mode);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve_one_request(
                    socket,
                    Arc::clone(&sink),
                    Arc::clone(&mode),
                ));
            }
        });
        Self { url, received }
    }

    pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
        self.received.lock().unwrap().clone()
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(RecordedRequest::method)
            .collect()
    }
}

async fn serve_one_request(
    mut socket: TcpStream,
    sink: Arc<Mutex<Vec<RecordedRequest>>>,
    mode: Arc<Mode>,
) {
    let mut buf = Vec::new();
    let head_end = loop {
        let mut chunk = [0u8; 4096];
        let Ok(n) = socket.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let headers: Vec<(String, String)> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let Ok(n) = socket.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        body.extend_from_slice(&chunk[..n]);
    }

    let (status, payload) = match &*mode {
        Mode::FixedStatus(status, payload) => (*status, payload.clone()),
        Mode::Rpc(handler) => {
            let message: Value = serde_json::from_slice(&body).expect("client sends JSON-RPC");
            let id = message["id"].clone();
            let method = message["method"].as_str().unwrap_or_default();
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let envelope = match handler(method, &params) {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err((code, msg)) => json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": code, "message": msg }
                }),
            };
            (200, envelope.to_string())
        }
    };

    sink.lock().unwrap().push(RecordedRequest { headers, body });

    let response = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    socket.write_all(response.as_bytes()).await.ok();
    socket.shutdown().await.ok();
}

/// A task in `TASK_STATE_WORKING`, as the AURA server returns from
/// `SendMessage`.
pub(crate) fn working_task(id: &str, context_id: &str) -> Value {
    json!({
        "id": id,
        "contextId": context_id,
        "status": { "state": "TASK_STATE_WORKING" }
    })
}

/// A completed task carrying the AURA server's `response` and `final`
/// artifacts.
pub(crate) fn completed_task(id: &str, context_id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "contextId": context_id,
        "status": { "state": "TASK_STATE_COMPLETED" },
        "artifacts": [
            { "artifactId": "response", "name": "Response",
              "parts": [ { "text": "chunk one " }, { "text": "chunk two" } ] },
            { "artifactId": "final", "name": "Final Info",
              "parts": [ { "text": text } ],
              "metadata": { "input_tokens": 10, "output_tokens": 5 } }
        ]
    })
}
