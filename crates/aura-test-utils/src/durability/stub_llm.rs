//! A deterministic Ollama-compatible stub LLM for the durability harness.
//!
//! The harness needs scripted coordinator and worker responses, not a real
//! model. This server speaks enough of the Ollama `/api/chat` streaming
//! endpoint to satisfy `rig::providers::ollama`.

use std::net::SocketAddr;

use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{oneshot, Mutex};

/// Scripted Ollama chat server.
pub struct StubLlm {
    addr: SocketAddr,
    _shutdown: oneshot::Sender<()>,
}

impl StubLlm {
    /// Start the stub on a random loopback port and return its handle.
    pub async fn start() -> Self {
        let state = std::sync::Arc::new(Mutex::new(0usize));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let app = Router::new()
            .route("/api/chat", post(chat_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("stub LLM binds a loopback port");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            let server = axum::serve(listener, app);
            let guarded = server.with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            });
            if let Err(e) = guarded.await {
                eprintln!("stub LLM server exited: {e}");
            }
        });

        Self {
            addr,
            _shutdown: shutdown_tx,
        }
    }

    /// Base URL to pass to the agent config's `base_url`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

#[derive(Deserialize)]
struct ChatRequest {
    #[allow(dead_code)]
    model: String,
    #[serde(default)]
    stream: bool,
}

#[derive(Serialize)]
struct ChatChunk {
    model: String,
    created_at: String,
    message: serde_json::Value,
    done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_eval_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    eval_count: Option<u64>,
}

async fn chat_handler(
    axum::extract::State(counter): axum::extract::State<std::sync::Arc<Mutex<usize>>>,
    Json(req): Json<ChatRequest>,
) -> Response {
    if !req.stream {
        return (StatusCode::BAD_REQUEST, "only streaming is supported").into_response();
    }

    let count = {
        let mut c = counter.lock().await;
        let n = *c;
        *c += 1;
        n
    };

    let tool_call = match count {
        0 => coordinator_plan(),
        1 => worker_tool_call(),
        _ => final_answer(),
    };

    let created = chrono::Utc::now().to_rfc3339();
    let model = "aura-stub".to_string();

    let chunks = vec![
        ChatChunk {
            model: model.clone(),
            created_at: created.clone(),
            message: tool_call,
            done: false,
            prompt_eval_count: Some(10),
            eval_count: None,
        },
        ChatChunk {
            model,
            created_at: created.clone(),
            message: json!({"role": "assistant", "content": ""}),
            done: true,
            prompt_eval_count: Some(10),
            eval_count: Some(5),
        },
    ];

    let stream = async_stream::stream! {
        for chunk in chunks {
            let line = serde_json::to_string(&chunk).expect("chunk serializes");
            yield Ok::<_, std::convert::Infallible>(line + "\n");
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        .body(Body::from_stream(stream))
        .expect("response builds")
}

fn coordinator_plan() -> serde_json::Value {
    json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [{
            "type": "function",
            "function": {
                "name": "create_plan",
                "arguments": {
                    "goal": "Exercise the HITL gate",
                    "steps": [{
                        "type": "task",
                        "task": "Call the gated mock_tool",
                        "worker": "gated"
                    }],
                    "routing_rationale": "The query requires a gated tool execution.",
                    "planning_summary": "One worker task calls the gated tool."
                }
            }
        }]
    })
}

fn worker_tool_call() -> serde_json::Value {
    json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [{
            "type": "function",
            "function": {
                "name": "mock_tool",
                "arguments": { "message": "exercise the HITL gate" }
            }
        }]
    })
}

fn final_answer() -> serde_json::Value {
    json!({
        "role": "assistant",
        "content": "The gated tool was approved and the run completed.",
        "tool_calls": []
    })
}
