#![cfg(feature = "integration-hitl-header-forwarding")]

//! Integration tests for HITL approver header forwarding (aura#496).
//!
//! Proves the override path end to end, through a real approval webhook and
//! a real gated MCP call, over the actual `/v1/chat/completions` path — the
//! surface `crates/aura/tests/approver_header_forwarding_coverage.md` marks
//! as excluded from unit coverage.
//!
//! Each test builds its own `aura-web-server` child process against its own
//! generated config: the shared server behind `header_forwarding_tests.rs`
//! cannot carry `[hitl]` gating, because a `require_approval` glob would
//! gate every suite's calls to the same tool. All instances share the one
//! `mock-mcp` fixture `header_forwarding_tests.rs` already depends on.
//!
//! # Run recipe
//!
//! 1. Start the shared MCP fixture: `docker compose -f compose/base.yml -f
//!    compose/dev.yml up -d mock-mcp` (FastMCP at
//!    `${MCP_MOCK_HOST:-127.0.0.1}:9999`).
//! 2. Export `OPENAI_API_KEY` (each generated config resolves
//!    `{{ env.OPENAI_API_KEY }}`, exactly like `test-config.toml`).
//! 3. `cargo test -p aura-web-server --features integration-hitl-header-forwarding`.
//!    Cargo builds `aura-web-server` as a side effect (via
//!    `CARGO_BIN_EXE_aura-web-server`); each test spawns it fresh, waits on
//!    `/health`, drives one chat completion, and kills it on drop.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

const CHAT_TIMEOUT: Duration = Duration::from_secs(90);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// The prompt that has the model call `echo_headers` and relay its output.
const ECHO_PROMPT: &str = "Call the echo_headers tool now and reply with only its raw JSON output.";

// ---------------------------------------------------------------------------
// A dedicated aura-web-server, spawned fresh per test case
// ---------------------------------------------------------------------------

/// A freshly spawned `aura-web-server`, bound to its own port and reading a
/// config generated for exactly one test case. Killed and its config file
/// removed on drop.
struct AuraServer {
    port: u16,
    child: Child,
    config_path: PathBuf,
    /// Accumulated stderr, drained continuously so the child's pipe never
    /// blocks; read back to explain a health-check timeout.
    stderr_log: Arc<Mutex<String>>,
}

impl AuraServer {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Spawn `aura-web-server` (built as a side effect of this test binary,
    /// via `CARGO_BIN_EXE_aura-web-server`) against `config_toml`, and wait
    /// until it answers `/health`. `free_port`'s bind-then-drop leaves a
    /// window for something else to grab the port before the child binds it;
    /// one retry on a fresh port covers that without masking a genuine
    /// startup failure, which still panics on the second attempt.
    async fn start(config_toml: &str) -> Self {
        match Self::try_start(config_toml).await {
            Ok(server) => server,
            Err(failed) => {
                let log = failed.stderr_log.lock().expect("stderr log mutex").clone();
                eprintln!(
                    "aura-web-server on port {} never answered /health within {HEALTH_TIMEOUT:?}; \
                     retrying once on a fresh port. stderr:\n{log}",
                    failed.port
                );
                failed.stop().await;
                match Self::try_start(config_toml).await {
                    Ok(server) => server,
                    Err(failed) => {
                        let log = failed.stderr_log.lock().expect("stderr log mutex").clone();
                        let port = failed.port;
                        failed.stop().await;
                        panic!(
                            "aura-web-server never answered /health, on a fresh port either; \
                             last tried port {port}; stderr:\n{log}"
                        );
                    }
                }
            }
        }
    }

    /// One spawn-and-wait attempt. `Err` carries the (still-running) server
    /// so the caller can log its stderr and stop it before retrying.
    async fn try_start(config_toml: &str) -> Result<Self, Self> {
        let port = free_port();
        let config_path =
            std::env::temp_dir().join(format!("aura-hitl-test-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, config_toml).expect("write generated test config");

        let mut child = Command::new(env!("CARGO_BIN_EXE_aura-web-server"))
            .env("CONFIG_PATH", &config_path)
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn aura-web-server (did `cargo build -p aura-web-server` succeed?)");

        let stderr_log = Arc::new(Mutex::new(String::new()));
        let stderr = child.stderr.take().expect("stderr was piped");
        let log_sink = Arc::clone(&stderr_log);
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut log = log_sink.lock().expect("stderr log mutex");
                log.push_str(&line);
                log.push('\n');
            }
        });

        let server = Self {
            port,
            child,
            config_path,
            stderr_log,
        };
        if server.is_healthy_within(HEALTH_TIMEOUT).await {
            Ok(server)
        } else {
            Err(server)
        }
    }

    async fn is_healthy_within(&self, timeout: Duration) -> bool {
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(resp) = client
                .get(format!("{}/health", self.base_url()))
                .send()
                .await
                && resp.status().is_success()
            {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Kill the child and await its exit, reaping the process, then remove
    /// its generated config file. Call this explicitly at the end of a
    /// test; `Drop`'s `start_kill` is only the fallback for a test that
    /// panics before reaching it, since a non-blocking kill on drop cannot
    /// await the reap.
    async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = std::fs::remove_file(&self.config_path);
    }
}

impl Drop for AuraServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.config_path);
    }
}

/// An OS-assigned free port, read and released before the caller uses it.
/// The bind-then-drop race is the standard tolerance for test-local ports.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

// ---------------------------------------------------------------------------
// A hand-rolled mock webhook approver
// ---------------------------------------------------------------------------

/// How the mock approver answers every decision request it receives for the
/// life of one test.
#[derive(Clone, Copy)]
enum ApproverReply {
    /// Approve and return one extra response header, for capture.
    ApproveWithHeader {
        name: &'static str,
        value: &'static str,
    },
    /// Approve with no extra headers — the mapped header (if any) is missing.
    ApproveBare,
    /// Deny every request.
    Deny,
}

/// An in-process webhook approver bound to a loopback port, answering every
/// POST it receives the same way for the life of the test. Mirrors the
/// one-shot receiver idiom in `hitl::route`'s `webhook_signing` tests, minus
/// the one-shot restriction: a chat turn may retry or the harness may want
/// more than one decision, so this loops `accept`.
struct MockApprover {
    url: String,
    hits: Arc<AtomicUsize>,
}

impl MockApprover {
    async fn start(reply: ApproverReply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock approver");
        let url = format!("http://{}/decision", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let hit_sink = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                // Counted on accept, before the decision is served: a test
                // asserting `hits() == 0` is asserting the gate never even
                // opened a connection, not that a slow reply raced the
                // assertion.
                hit_sink.fetch_add(1, Ordering::SeqCst);
                serve_one_decision(socket, reply).await;
            }
        });
        Self { url, hits }
    }

    /// How many decision requests this approver has accepted so far. The
    /// direct proof that a call did, or did not, consult the route at all —
    /// stronger than inferring it from the reply's shape.
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// Read one HTTP request off `socket` and answer it per `reply`. Ignores the
/// request body: which decision to hand back is fixed per test, not derived
/// from the payload.
async fn serve_one_decision(mut socket: tokio::net::TcpStream, reply: ApproverReply) {
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
    let content_length: usize = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
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

    let (payload, extra_header) = match reply {
        ApproverReply::ApproveWithHeader { name, value } => {
            (json!({"approved": true}).to_string(), Some((name, value)))
        }
        ApproverReply::ApproveBare => (json!({"approved": true}).to_string(), None),
        ApproverReply::Deny => (
            json!({"approved": false, "reason": "integration test denial"}).to_string(),
            None,
        ),
    };

    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        payload.len()
    );
    if let Some((name, value)) = extra_header {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&payload);
    socket.write_all(response.as_bytes()).await.ok();
    socket.shutdown().await.ok();
}

// ---------------------------------------------------------------------------
// Generated config
// ---------------------------------------------------------------------------

/// The shared mock-mcp fixture's URL — the same server
/// `header_forwarding_tests.rs` depends on, honoring the same `MCP_MOCK_HOST`
/// override for container-network runs.
fn mcp_url() -> String {
    let host = std::env::var("MCP_MOCK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    format!("http://{host}:9999/mcp")
}

/// A minimal single-agent config: one MCP server with a frozen static
/// `Authorization` header (standing in for the pre-approval requester
/// identity, as `test-config.toml` does for the base suite), plus whatever
/// `[hitl]` table the caller supplies.
fn config_toml(mcp_url: &str, frozen_authorization: &str, hitl_toml: &str) -> String {
    format!(
        r#"
[mcp]
sanitize_schemas = true

[mcp.servers.mock_test_server]
transport = "http_streamable"
url = "{mcp_url}"
description = "Mock MCP server for HITL header-forwarding integration tests"

[mcp.servers.mock_test_server.headers]
Authorization = "{frozen_authorization}"

[agent]
name = "HITL Header Forwarding Test Assistant"
alias = "test-assistant"
system_prompt = """
You are a test assistant. Call tools immediately when requested, with no
explanation, confirmation, or promise to call them later.

If a tool call succeeds, reply with only its raw output - no commentary.

If a tool call returns an error, reply with only the exact error message
text - no apology, no extra commentary.

AVAILABLE TOOLS (from mock_test_server):
- echo_headers: Return HTTP headers as JSON (no params)
"""
turn_depth = 3

[agent.llm]
provider = "openai"
api_key = "{{{{ env.OPENAI_API_KEY }}}}"
model = "gpt-5.1"
temperature = 0.0

{hitl_toml}
"#
    )
}

/// `[hitl]` gating `echo_headers`, routed to `approver_url`, with
/// `tool_headers_from_response` mapping the approver token onto
/// `authorization`.
fn gated_hitl_toml(approver_url: &str) -> String {
    let mapping = r#"tool_headers_from_response = { "authorization" = "x-approver-token" }"#;
    format!(
        r#"
[hitl]
require_approval = ["echo_headers"]

[hitl.route]
mode = "webhook"
url = "{approver_url}"
{mapping}
"#
    )
}

/// `[hitl]` present and pointed at a real (denying) approver, but its glob
/// matches no tool this suite calls — proves an unmatched call never
/// consults the route at all.
fn ungated_hitl_toml(approver_url: &str) -> String {
    format!(
        r#"
[hitl]
require_approval = ["not_a_real_tool_*"]

[hitl.route]
mode = "webhook"
url = "{approver_url}"
tool_headers_from_response = {{ "authorization" = "x-approver-token" }}
"#
    )
}

// ---------------------------------------------------------------------------
// Chat helpers
// ---------------------------------------------------------------------------

async fn send_chat(server: &AuraServer, prompt: &str) -> Value {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url()))
        .json(&json!({
            "model": "test-assistant",
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("hitl-test-{}", uuid::Uuid::new_v4())
            }
        }))
        .timeout(CHAT_TIMEOUT)
        .send()
        .await
        .expect("chat completion request reaches the server");

    assert_eq!(
        response.status(),
        200,
        "expected 200 OK from /v1/chat/completions"
    );
    response.json().await.expect("response body is valid JSON")
}

fn assistant_text(response_json: &Value) -> &str {
    response_json["choices"][0]["message"]["content"]
        .as_str()
        .expect("response carries assistant message content")
}

/// The `echo_headers` JSON blob out of a chat response's assistant prose,
/// with `context` naming what the calling test expected, so a miss names
/// both the expectation and what the assistant actually said.
fn headers_from_response(response_json: &Value, context: &str) -> Value {
    let text = assistant_text(response_json);
    extract_json_object(text).unwrap_or_else(|| panic!("{context}; got: {text:?}"))
}

/// The first `{...}` JSON object embedded in `text`, if any — mirrors
/// `header_forwarding_tests.rs`'s extraction of the `echo_headers` JSON blob
/// out of the model's prose.
fn extract_json_object(text: &str) -> Option<Value> {
    // The assistant relays the tool output in whatever quoting it fancies:
    // bare, wrapped as a JSON string literal, or with the headers JSON
    // stringified inside a `result` field - sometimes several layers deep.
    // Unwrap until an object with plain fields remains.
    if let Ok(Value::String(inner)) = serde_json::from_str::<Value>(text.trim()) {
        return extract_json_object(&inner);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let mut value: Value = serde_json::from_str(&text[start..=end]).ok()?;
    while let Some(inner) = value.get("result").and_then(Value::as_str) {
        match serde_json::from_str::<Value>(inner) {
            Ok(unwrapped) => value = unwrapped,
            Err(_) => break,
        }
    }
    Some(value)
}

// ---------------------------------------------------------------------------
// Cases (spec Phase 3)
// ---------------------------------------------------------------------------

/// An approver answering with `reply`, plus a freshly spawned server whose
/// `[hitl]` route points at it and whose client carries the frozen identity.
async fn gated_server(reply: ApproverReply) -> (MockApprover, AuraServer) {
    let approver = MockApprover::start(reply).await;
    let server = AuraServer::start(&config_toml(
        &mcp_url(),
        "Bearer legacy-frozen-identity",
        &gated_hitl_toml(&approver.url),
    ))
    .await;
    (approver, server)
}

/// The approver saw exactly `expected` decision requests; `context` says why.
fn assert_hits(approver: &MockApprover, expected: usize, context: &str) {
    assert_eq!(approver.hits(), expected, "{context}");
}

/// Case 1: an approved decision carrying the mapped header replaces the
/// frozen requester identity on the one gated call.
#[tokio::test]
async fn override_forwarded_to_the_gated_call() {
    let (approver, server) = gated_server(ApproverReply::ApproveWithHeader {
        name: "x-approver-token",
        value: "Bearer approver-issued-identity",
    })
    .await;

    let response = send_chat(&server, ECHO_PROMPT).await;
    let headers = headers_from_response(
        &response,
        "the assistant relays the echo_headers JSON output",
    );

    assert_eq!(
        headers.get("authorization").and_then(Value::as_str),
        Some("Bearer approver-issued-identity"),
        "the approver's identity must replace the frozen requester identity, got: {headers}"
    );
    assert_hits(
        &approver,
        1,
        "exactly one decision request for the one gated call",
    );
    server.stop().await;
}

/// Case 2: an approved decision missing the mapped header fails the gated
/// call closed; the error names the header, never any value.
#[tokio::test]
async fn missing_mapped_header_fails_the_call_by_name_only() {
    let (approver, server) = gated_server(ApproverReply::ApproveBare).await;

    let response = send_chat(
        &server,
        "Call the echo_headers tool now. If it returns an error, reply with \
         only the exact error message text.",
    )
    .await;
    let text = assistant_text(&response);

    assert!(
        text.to_lowercase().contains("authorization"),
        "the error must name the missing header, got: {text}"
    );
    assert!(
        !text.contains("legacy-frozen-identity"),
        "the error must never leak a header value, got: {text}"
    );
    assert_hits(
        &approver,
        1,
        "exactly one decision request for the one gated call",
    );
    server.stop().await;
}

/// Case 3: `[hitl]` is configured, but the glob does not match this tool —
/// the call never consults the route, so a denying approver never fires.
#[tokio::test]
async fn a_tool_the_glob_does_not_match_is_unaffected() {
    let approver = MockApprover::start(ApproverReply::Deny).await;
    let server = AuraServer::start(&config_toml(
        &mcp_url(),
        "Bearer legacy-frozen-identity",
        &ungated_hitl_toml(&approver.url),
    ))
    .await;

    let response = send_chat(&server, ECHO_PROMPT).await;
    let headers = headers_from_response(
        &response,
        "a tool the glob does not match runs with no approval step at all",
    );

    assert_eq!(
        headers.get("authorization").and_then(Value::as_str),
        Some("Bearer legacy-frozen-identity"),
        "an unmatched call must carry no approver override, got: {headers}"
    );
    assert_hits(
        &approver,
        0,
        "a call the glob does not match must never consult the approver at all",
    );
    server.stop().await;
}
