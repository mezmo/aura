//! A real server process and A2A file backend; no model or external MCP needed.
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start(root: &std::path::Path, port: u16) -> Server {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aura-web-server"));
    command.env_clear();
    for name in [
        "PATH",
        "HOME",
        "TMPDIR",
        "DYLD_LIBRARY_PATH",
        "LD_LIBRARY_PATH",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let child = command
        .args(["--enable-a2a", "--port", &port.to_string(), "--config"])
        .arg(root.join("config.toml"))
        .env("AURA_SESSION_STORE", "file")
        .env("AURA_SESSION_STORE_PATH", root.join("sessions"))
        .env("OTEL_SDK_DISABLED", "true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Server(child);
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if client
                .get(format!(
                    "http://127.0.0.1:{port}/.well-known/agent-card.json"
                ))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("server did not start");
    server
}

async fn send(client: &reqwest::Client, base: &str, message: Value) -> Value {
    let response = client
        .post(format!("{base}/a2a/v1/message:send"))
        .header("A2A-Version", "1.0")
        .json(&json!({"message": message}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{body}");
    body["task"].clone()
}

async fn task(client: &reqwest::Client, base: &str, id: &str) -> Value {
    client
        .get(format!("{base}/a2a/v1/tasks/{id}"))
        .header("A2A-Version", "1.0")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a2a_wait_survives_process_kill_and_resumes_with_its_original_deadline() {
    exercise_restart(false).await;
}

#[tokio::test]
async fn a2a_cancel_after_restart_persists_workflow_cancellation() {
    exercise_restart(true).await;
}

async fn exercise_restart(cancel: bool) {
    let root = tempfile::tempdir().unwrap();
    let config = format!(
        r#"
memory_dir = "{}"
[agent]
name = "restart-test"
system_prompt = "unused in configured wait"
[agent.llm]
provider = "openai"
api_key = "unused-test-key"
model = "unused-test-model"
[orchestration]
enabled = true
stage_order = ["wait"]
[orchestration.worker.worker]
description = "test"
preamble = "test"
mcp_filter = []
[orchestration.stages.wait]
worker = "worker"
output_schema = {{ type = "object", required = ["waited"] }}
operation = {{ kind = "wait", seconds = 5 }}
"#,
        root.path().join("runs").display()
    );
    std::fs::write(root.path().join("config.toml"), config).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let base = format!("http://127.0.0.1:{port}");
    let server = start(root.path(), port).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let started = send(
        &client,
        &base,
        json!({"messageId":"start", "role":"ROLE_USER", "parts":[{"text":"input"}]}),
    )
    .await;
    let id = started["id"].as_str().expect("start returned task");
    let context = started["contextId"].as_str().unwrap();
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = task(&client, &base, id).await;
            if let Some(run) = current["artifacts"]
                .as_array()
                .and_then(|artifacts| artifacts.iter().find(|a| a["artifactId"] == "workflow"))
                .and_then(|a| a.pointer("/parts/0/data"))
                && run.pointer("/state/state") == Some(&json!("waiting"))
            {
                break run.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no durable waiting snapshot");
    drop(server); // SIGKILL: no graceful shutdown checkpoint.
    let _restarted = start(root.path(), port).await;
    let restored = task(&client, &base, id).await;
    assert_eq!(
        restored.pointer("/status/state"),
        Some(&json!("TASK_STATE_INPUT_REQUIRED"))
    );
    if cancel {
        let response = client
            .post(format!("{base}/a2a/v1/tasks/{id}/cancel"))
            .header("A2A-Version", "1.0")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert!(status.is_success(), "{body}");
        let current = task(&client, &base, id).await;
        assert_eq!(
            current.pointer("/status/state"),
            Some(&json!("TASK_STATE_CANCELED")),
            "{current}"
        );
        let run = current["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["artifactId"] == "workflow")
            .unwrap();
        assert_eq!(
            run.pointer("/parts/0/data/state/state"),
            Some(&json!("cancelled")),
            "{run}"
        );
        return;
    }
    let resumed = send(&client, &base, json!({"messageId":"resume", "contextId":context, "role":"ROLE_USER", "parts":[{"data":{"aura.workflow":{"command":"resume", "run_id":snapshot["run_id"]}}}]})).await;
    let resumed_id = resumed["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = task(&client, &base, resumed_id).await;
            if current.pointer("/status/state") == Some(&json!("TASK_STATE_COMPLETED")) {
                break;
            }
            assert_ne!(
                current.pointer("/status/state"),
                Some(&json!("TASK_STATE_FAILED")),
                "{current}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("resumed wait did not finish");
}

#[tokio::test]
async fn exact_mcp_action_waits_for_approval_and_verifies_without_a_model() {
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mutations = Arc::new(AtomicUsize::new(0));
    let notifications = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&mutations);
    let notices = Arc::clone(&notifications);
    let router = Router::new().route("/mcp", post(move |Json(request): Json<Value>| {
        let count = Arc::clone(&count);
        let notices = Arc::clone(&notices);
        async move {
            let result = match request["method"].as_str().unwrap_or("") {
                "initialize" => json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"workflow-test","version":"1"}}),
                "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
                "tools/list" => json!({"tools":[
                    {"name":"mutate","inputSchema":{"type":"object"}},
                    {"name":"probe","inputSchema":{"type":"object"}},
                    {"name":"notify","inputSchema":{"type":"object"}}
                ]}),
                "tools/call" => {
                    let value = match request.pointer("/params/name").and_then(Value::as_str).unwrap() {
                        "mutate" => { count.fetch_add(1, Ordering::SeqCst); json!({"applied":true}) },
                        "notify" => { notices.fetch_add(1, Ordering::SeqCst); json!({"delivered":true}) },
                        "probe" => json!({"healthy":count.load(Ordering::SeqCst) == 1}),
                        name => panic!("unexpected tool {name}"),
                    };
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                _ => json!({}),
            };
            Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result})).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp_port = listener.local_addr().unwrap().port();
    let mcp = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let config = format!(
        r#"
memory_dir = "{}"
[agent]
name = "approval-test"
system_prompt = "unused"
[agent.llm]
provider = "openai"
api_key = "unused-test-key"
model = "unused-test-model"
[mcp.servers.test]
transport = "http_streamable"
url = "http://127.0.0.1:{mcp_port}/mcp"
[hitl]
require_approval = ["mutate"]
[hitl.route]
mode = "conversational"
timeout_secs = 30
[hitl.park]
enabled = true
[orchestration]
enabled = true
stage_order = ["act", "verify"]
[orchestration.worker.action]
description = "test"
preamble = "test"
mcp_filter = ["mutate", "probe", "notify"]
[orchestration.stages.act]
worker = "action"
output_schema = {{ type = "object", required = ["applied"] }}
operation = {{ kind = "tool", tool = "mutate", arguments = {{}} }}
on_approval_wait = [{{after_secs=0, worker="action", tool="notify", arguments={{}}, inputs={{payload="/run"}}}}]
[orchestration.stages.verify]
worker = "action"
output_schema = {{ type = "object", required = ["healthy"] }}
operation = {{ kind = "verify", tool = "probe", arguments = {{}}, read_only = true, success_schema = {{ required = ["healthy"], properties = {{ healthy = {{ const = true }} }} }}, interval_secs = 1, timeout_secs = 10 }}
"#,
        root.path().join("runs").display()
    );
    std::fs::write(root.path().join("config.toml"), config).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let _server = start(root.path(), port).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let started = send(
        &client,
        &base,
        json!({"messageId":"approval-start", "role":"ROLE_USER", "parts":[{"text":"input"}]}),
    )
    .await;
    let id = started["id"].as_str().unwrap();
    let decision = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = task(&client, &base, id).await;
            assert_ne!(
                current.pointer("/status/state"),
                Some(&json!("TASK_STATE_FAILED")),
                "{current}"
            );
            if let Some(run) = current["artifacts"]
                .as_array()
                .and_then(|a| a.iter().find(|a| a["artifactId"] == "workflow"))
                .and_then(|a| a.pointer("/parts/0/data"))
                && let Some(decision) = run
                    .pointer("/state/checkpoint/plan/tasks/0/pending/0/decision_id")
                    .and_then(Value::as_str)
            {
                break decision.to_string();
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("action did not park");
    assert_eq!(mutations.load(Ordering::SeqCst), 0);
    client
        .post(format!("{base}/v1/approvals/{decision}"))
        .json(&json!({"approved":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = task(&client, &base, id).await;
            assert_ne!(
                current.pointer("/status/state"),
                Some(&json!("TASK_STATE_FAILED")),
                "{current}"
            );
            if current.pointer("/status/state") == Some(&json!("TASK_STATE_COMPLETED")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("approved action did not finish");
    assert_eq!(mutations.load(Ordering::SeqCst), 1);
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    mcp.abort();
}
