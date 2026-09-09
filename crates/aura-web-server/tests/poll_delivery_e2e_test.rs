#![cfg(feature = "integration-hitl-header-forwarding")]

//! End-to-end poll-delivery flow: park -> notify -> poll -> durable resolve,
//! over a real `aura-web-server` child, the real model, and an in-process mock
//! governance receiver, on the actual `/v1/chat/completions` path.
//!
//! The gate parks the gated `echo_headers` call, the startup reconciler
//! POSTs the ack-only notification (whose decision-shaped body must NEVER be
//! read as a decision), then polls the status GET until the receiver answers
//! decided. No HMAC secret is configured — the rig runs unsigned; the signed
//! legs are unit-proven in `hitl::route`.
//!
//! The flow is proven through durable resolve with the run still parked: the
//! parked run ends with the orchestrator's parked message and no tool output,
//! and the decision landing in the store (carrying the captured approver
//! identity) is the terminal state asserted here. Re-execution belongs to
//! the resume endpoint, which this rig does not start.
//!
//! The park arm requires an orchestration worker scope, so the rig config
//! enables `[orchestration]` (a single-agent config would fail the gated call
//! closed). The approval store is the file backend pointed at a per-test
//! temp dir, which is what lets the restart test read the same rows from a
//! rebooted server.
//!
//! # Run recipe
//!
//! `make test-integration-hitl-local` starts the shared `mock-mcp` fixture
//! for the sibling suite; this suite runs directly with the same feature
//! flag (equivalent wiring, needs no env beyond `OPENAI_API_KEY`):
//!
//! ```sh
//! cargo test -p aura-web-server --features integration-hitl-header-forwarding \
//!     --test poll_delivery_e2e_test
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
mod common;

use common::AuraServer;

const CHAT_TIMEOUT: Duration = Duration::from_secs(90);

/// The reboot's resolve must land within ~two poll intervals of the health
/// check — a reconciler that only resolves on tick N > 1 fails this budget.
const FIRST_TICK_BUDGET: Duration = Duration::from_secs(3);
/// Wall-clock budget for one reconciler side effect (the notify POST, a run
/// of status GETs, the durable resolve). `poll_interval_secs` is 1, so this
/// tolerates ~20 missed ticks before failing.
const TICK_BUDGET: Duration = Duration::from_secs(20);

/// The chat request header mapped onto the webhook egress; the receiver must
/// see its value on every notify POST (the parked approval row's own header,
/// applied by the reconciler).
const EGRESS_NAME: &str = "x-tenant-egress";
const EGRESS_VALUE: &str = "Bearer rig-egress-sentinel";
/// The identity header the decided status GET carries, docked onto the
/// decision record by `tool_headers_from_response`.
const IDENTITY_NAME: &str = "x-approver-id";
const IDENTITY_VALUE: &str = "approver-mike";

/// The prompt that has the model call `echo_headers` and relay its output.
const ECHO_PROMPT: &str = "Call the echo_headers tool now and reply with only its raw JSON output.";

// ---------------------------------------------------------------------------
// A mock governance receiver: POST /notify + GET /status on one port
// ---------------------------------------------------------------------------

/// The receiver's mutable state, shared across its connection tasks.
struct ReceiverShared {
    decided: bool,
    /// Every request captured verbatim (request line, headers, body).
    requests: Vec<String>,
}

/// An in-process governance receiver: the notification POST is answered 200
/// with a DECISION-SHAPED body (`{"approved": true}`) — an ack whose body
/// must never be read as a decision — and the status GET answers 404 until
/// the test flips `decided`, then 200 with the status envelope
/// (`{"status": "approved"}`) and the approver identity header. Unsigned:
/// aura's notify/poll legs run without HMAC.
#[derive(Clone)]
struct MockGovernanceReceiver {
    base_url: String,
    shared: Arc<Mutex<ReceiverShared>>,
}

impl MockGovernanceReceiver {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock governance receiver");
        let base_url = format!(
            "http://{}",
            listener.local_addr().expect("listener local addr")
        );
        let shared = Arc::new(Mutex::new(ReceiverShared {
            decided: false,
            requests: Vec::new(),
        }));
        let sink = Arc::clone(&shared);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                // Serve each connection on its own task so one slow or
                // half-dead client (a killed server mid-request) never
                // head-of-line-blocks the reconciler's next attempt.
                let sink = Arc::clone(&sink);
                tokio::spawn(async move { serve_one(socket, sink).await });
            }
        });
        Self { base_url, shared }
    }

    fn notify_url(&self) -> String {
        format!("{}/notify", self.base_url)
    }

    fn status_url(&self) -> String {
        format!("{}/status", self.base_url)
    }

    /// Flip the receiver to decided: status GETs start answering 200 with
    /// the approver identity header.
    fn set_decided(&self) {
        self.shared.lock().expect("receiver state mutex").decided = true;
    }

    /// A snapshot of every captured request so far.
    fn requests(&self) -> Vec<String> {
        self.shared
            .lock()
            .expect("receiver state mutex")
            .requests
            .clone()
    }
}

/// Read one request off `socket`, record it verbatim, and answer per the
/// receiver's protocol. A peer that hangs up mid-request is dropped without
/// recording a partial capture.
async fn serve_one(mut socket: tokio::net::TcpStream, shared: Arc<Mutex<ReceiverShared>>) {
    let Some(captured) = read_full_request(&mut socket).await else {
        return;
    };
    let response = {
        let mut state = shared.lock().expect("receiver state mutex");
        state.requests.push(captured.clone());
        build_receiver_response(&captured, state.decided)
    };
    socket.write_all(response.as_bytes()).await.ok();
    socket.shutdown().await.ok();
}

/// The receiver's HTTP/1.1 answer: the POST is ack-only with a deliberately
/// decision-shaped body; an undecided GET is a 404; a decided GET carries the
/// status envelope and the identity header.
fn build_receiver_response(captured: &str, decided: bool) -> String {
    let request_line = captured.lines().next().unwrap_or_default();
    if request_line.starts_with("POST ") {
        let body = json!({ "approved": true }).to_string();
        return format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
             {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    if request_line.starts_with("GET ") && decided {
        let body = json!({ "status": "approved" }).to_string();
        return format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{IDENTITY_NAME}: \
             {IDENTITY_VALUE}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
}

/// Read one full HTTP/1.1 request (head plus content-length body) off
/// `socket`, or `None` if the peer hangs up first. Mirrors the scripted
/// receiver in `hitl::poller`'s tests.
async fn read_full_request(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let header_section = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length: usize = header_section
        .lines()
        .find(|line| line.to_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|val| val.trim().parse().ok())
        .unwrap_or(0);
    let body_already_read = buf.len() - header_end;
    let remaining = content_length.saturating_sub(body_already_read);
    if remaining > 0 {
        let mut body_buf = vec![0u8; remaining];
        socket.read_exact(&mut body_buf).await.ok()?;
        buf.extend_from_slice(&body_buf);
    }
    Some(String::from_utf8_lossy(&buf).to_string())
}

// ---------------------------------------------------------------------------
// Generated config
// ---------------------------------------------------------------------------

/// The shared mock-mcp fixture's URL, the same server the sibling HITL suite
/// depends on, honoring the same `MCP_MOCK_HOST` override.
fn mcp_url() -> String {
    let host = std::env::var("MCP_MOCK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    format!("http://{host}:9999/mcp")
}

/// Spawn a rig server: the rig config plus the store/instance env the
/// reconciler identity depends on.
async fn spawn_rig_server(
    receiver: &MockGovernanceReceiver,
    store_root: &std::path::Path,
    instance_id: &str,
) -> common::AuraServer {
    let config_toml = rig_config_toml(&mcp_url(), &store_root.join("memory"), receiver);
    AuraServer::start(
        &config_toml,
        "aura-poll-e2e-",
        &[
            ("AURA_INSTANCE_ID", instance_id.to_string()),
            ("AURA_SESSION_STORE", "file".to_string()),
            (
                "AURA_SESSION_STORE_PATH",
                store_root
                    .to_str()
                    .expect("store root is UTF-8")
                    .to_string(),
            ),
        ],
    )
    .await
}

/// A minimal orchestrated single-worker rig: `memory_dir` (the park commit
/// refuses to publish a checkpoint without it), orchestration on with direct
/// answers off (the park arm needs a worker scope), and `[hitl]` gating
/// `echo_headers` on the poll-mode webhook route. Byte-identical across the
/// restart test's two boots, so the rebooted server resolves the same rows.
fn rig_config_toml(mcp_url: &str, memory_dir: &Path, receiver: &MockGovernanceReceiver) -> String {
    format!(
        r#"
memory_dir = "{memory_dir}"

[mcp]
sanitize_schemas = true

[mcp.servers.mock_test_server]
transport = "http_streamable"
url = "{mcp_url}"
description = "Mock MCP server for the poll-delivery e2e rig"

[agent]
name = "Poll Delivery E2E Assistant"
alias = "poll-e2e-assistant"
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

[orchestration]
enabled = true
allow_direct_answers = false

[hitl]
require_approval = ["echo_headers"]

[hitl.park]
enabled = true

[hitl.route]
mode = "webhook"
url = "{notify}"
poll_url = "{status}"
delivery = "poll"
poll_interval_secs = 1
poll_request_timeout_secs = 5
headers_from_request = {{ "{EGRESS_NAME}" = "{EGRESS_NAME}" }}
tool_headers_from_response = {{ "{IDENTITY_NAME}" = "{IDENTITY_NAME}" }}
"#,
        memory_dir = memory_dir.display(),
        mcp_url = mcp_url,
        notify = receiver.notify_url(),
        status = receiver.status_url(),
    )
}

// ---------------------------------------------------------------------------
// Chat, store, and wait helpers
// ---------------------------------------------------------------------------

/// Drive one chat completion with the egress header attached — the request
/// the gate's egress capture reads `x-tenant-egress` from.
async fn send_chat(server: &AuraServer) -> Value {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/v1/chat/completions", server.base_url()))
        .header(EGRESS_NAME, EGRESS_VALUE)
        .json(&json!({
            "model": "poll-e2e-assistant",
            "messages": [{"role": "user", "content": ECHO_PROMPT}],
            "stream": false,
            "metadata": {
                "account_id": "test-account",
                "chat_session_id": format!("poll-e2e-{}", uuid::Uuid::new_v4())
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

/// `{root}/approvals` and `{root}/decisions`: the file backend's layout, read
/// directly from the test so the store's durable state is proven on disk.
fn store_json_files(store_root: &Path, subdir: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(store_root.join(subdir)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
}

fn decision_files(store_root: &Path) -> Vec<PathBuf> {
    store_json_files(store_root, "decisions")
}

fn approval_files(store_root: &Path) -> Vec<PathBuf> {
    store_json_files(store_root, "approvals")
}

/// Poll until exactly one parked approval exists and return its decision id
/// (the approval file's stem) plus the file's content.
async fn wait_for_single_approval(store_root: &Path) -> (String, String) {
    let deadline = tokio::time::Instant::now() + TICK_BUDGET;
    loop {
        let files = approval_files(store_root);
        if let [only] = files.as_slice() {
            let content = std::fs::read_to_string(only).expect("read approval file");
            let id = only
                .file_stem()
                .expect("approval file has a stem")
                .to_string_lossy()
                .to_string();
            return (id, content);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected exactly one parked approval within {TICK_BUDGET:?}, found {} file(s): {:?}",
            files.len(),
            files
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll until a captured receiver request matching `matches` shows up; on
/// timeout, dump everything captured so the failure is diagnosable.
async fn wait_for_captured_request(
    receiver: &MockGovernanceReceiver,
    context: &str,
    matches: impl Fn(&str) -> bool,
) -> String {
    let deadline = tokio::time::Instant::now() + TICK_BUDGET;
    loop {
        if let Some(captured) = receiver.requests().into_iter().find(|r| matches(r)) {
            return captured;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{context} within {TICK_BUDGET:?}; captured requests so far:\n{}",
            receiver.requests().join("\n---\n")
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll until at least `minimum` captured receiver requests match `matches`;
/// on timeout, dump everything captured so the failure is diagnosable.
async fn wait_for_request_count(
    receiver: &MockGovernanceReceiver,
    minimum: usize,
    context: &str,
    matches: impl Fn(&str) -> bool,
) {
    let deadline = tokio::time::Instant::now() + TICK_BUDGET;
    loop {
        let count = receiver
            .requests()
            .iter()
            .filter(|captured| matches(captured))
            .count();
        if count >= minimum {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{context} within {TICK_BUDGET:?} (wanted {minimum}, saw {count}); captured \
             requests so far:\n{}",
            receiver.requests().join("\n---\n")
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll until the store holds exactly one decision file and return its
/// content.
async fn wait_for_decision_file(store_root: &Path) -> String {
    wait_for_decision_file_within(store_root, TICK_BUDGET).await
}

/// [`wait_for_decision_file`] with a caller-owned budget, for asserts whose
/// claim bounds the tick count (the reboot's first-tick resolve).
async fn wait_for_decision_file_within(store_root: &Path, budget: Duration) -> String {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let files = decision_files(store_root);
        if let [only] = files.as_slice() {
            return std::fs::read_to_string(only).expect("read decision file");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected exactly one durable decision within {budget:?}, found {} file(s): {:?}",
            files.len(),
            files
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The rig runs unsigned by design; a secret in the environment would flip
/// the poll legs into signed mode the mock receiver cannot satisfy.
fn ensure_unsigned_mode() {
    for var in [
        "AURA_HITL_WEBHOOK_SECRET",
        "AURA_HITL_WEBHOOK_SECRET_SECONDARY",
    ] {
        assert!(
            std::env::var(var).is_err(),
            "{var} must not be set: the rig's receiver is unsigned by design"
        );
    }
}

/// The parked-notification assertions both cases share: the parked run's
/// terminal completion carries the orchestrator's parked message and no
/// tool output; the approval row is at rest with its resolved egress value;
/// the notify POST names the row's decision id and authenticates with its
/// own egress value. Returns the decision id of the parked approval.
async fn park_and_notify(
    receiver: &MockGovernanceReceiver,
    server: &AuraServer,
    store_root: &Path,
) -> String {
    // The parked run's terminal completion: the orchestrator's parked
    // message replaces any tool relay, so the gated call's output never
    // reaches the client.
    let response = send_chat(server).await;
    let content = assistant_text(&response);
    assert!(
        content.contains("parked") && content.contains("awaiting human approval"),
        "the parked run must end with the orchestrator's parked message, got: {content}"
    );
    assert!(
        content.starts_with("Run ") && !content.contains('{'),
        "the parked message is exclusive - no tool output may ride alongside it, got: {content}"
    );
    assert!(
        !content.contains(EGRESS_VALUE),
        "no request-scoped credential value may reach the client, got: {content}"
    );

    // The parked approval row is at rest in the store, carrying its
    // request-scoped resolved egress value.
    let (decision_id, approval_record) = wait_for_single_approval(store_root).await;
    assert!(
        approval_record.contains(EGRESS_VALUE),
        "the parked approval record must persist the resolved egress value at \
         rest, got: {approval_record}"
    );

    // The reconciler's notify POST, authenticated with the parked row's own
    // egress value.
    let notify = wait_for_captured_request(
        receiver,
        "the reconciler must notify the parked approval",
        |captured| captured.starts_with("POST /notify"),
    )
    .await;
    let wire_body = notify
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_else(|| panic!("the notify capture carries a body: {notify}"));
    let wire: Value = serde_json::from_str(wire_body)
        .unwrap_or_else(|err| panic!("the notify body is valid JSON: {err}; capture: {notify}"));
    assert_eq!(
        wire["decision_id"].as_str(),
        Some(decision_id.as_str()),
        "the notify names the parked approval's decision id: {notify}"
    );
    let egress_line = notify
        .lines()
        .find(|line| line.to_lowercase().starts_with(&format!("{EGRESS_NAME}:")))
        .unwrap_or_else(|| panic!("the notify POST carries the row's egress header: {notify}"));
    assert_eq!(
        egress_line.split_once(':').expect("header line").1.trim(),
        EGRESS_VALUE,
        "the reconciler applies the parked row's own egress value: {notify}"
    );

    decision_id
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// The full poll flow with the run still parked: the decision-shaped notify
/// ack is ignored (nothing resolves, the status GETs keep coming), and a
/// decision landing at the receiver later resolves durably with the approver
/// identity docked — while the run's completion stays the parked message.
#[tokio::test]
async fn poll_flow_parks_notifies_and_resolves_with_the_run_still_parked() {
    ensure_unsigned_mode();
    let store_dir = tempfile::tempdir().expect("temp store dir");
    let store_root = store_dir.path().to_path_buf();
    let receiver = MockGovernanceReceiver::start().await;
    let server = spawn_rig_server(&receiver, &store_root, "poll-e2e-single").await;

    let decision_id = park_and_notify(&receiver, &server, &store_root).await;

    // (3) The ack body is never read as a decision: past the 200 ack, the
    // reconciler keeps polling the undecided status endpoint, and nothing
    // resolves. Two polls is past any single-tick misread.
    wait_for_request_count(
        &receiver,
        2,
        "status polls past the ack (the reconciler must keep polling)",
        |captured| captured.starts_with("GET /status"),
    )
    .await;
    let status_get = receiver
        .requests()
        .into_iter()
        .find(|captured| captured.starts_with("GET /status"))
        .expect("at least one status poll was captured");
    assert!(
        status_get.contains(&format!("decision_id={decision_id}")),
        "the status GET names the parked decision id: {status_get}"
    );
    assert!(
        decision_files(&store_root).is_empty(),
        "the decision-shaped notify ack must never resolve anything: {:?}",
        decision_files(&store_root)
    );
    assert!(
        !approval_files(&store_root).is_empty(),
        "the approval row stays parked while undecided"
    );

    // (4)+(5) The receiver decides; the reconciler's next poll resolves
    // durably with the captured approver identity beside the decision, and
    // the approval record (with its egress value) carried into the resolved
    // entry.
    receiver.set_decided();
    let decision_record = wait_for_decision_file(&store_root).await;
    assert!(
        decision_record.contains(IDENTITY_VALUE),
        "the decision record docks the poll-200's approver identity, got: {decision_record}"
    );
    assert!(
        decision_record.contains(EGRESS_VALUE),
        "the resolved entry carries the approval row's egress value, got: {decision_record}"
    );

    server.stop().await;
}

/// Restart: notify, kill the server, decide at the receiver, reboot onto the
/// same store — the rebooted server's first tick re-notifies (at-least-once,
/// idempotent by decision id) and resolves durably: exactly one decision,
/// at most one duplicate notify.
#[tokio::test]
async fn restart_resolves_the_parked_approval_on_a_rebooted_server() {
    ensure_unsigned_mode();
    let store_dir = tempfile::tempdir().expect("temp store dir");
    let store_root = store_dir.path().to_path_buf();
    let receiver = MockGovernanceReceiver::start().await;
    let instance_id = "poll-e2e-restart";

    let first_boot = spawn_rig_server(&receiver, &store_root, instance_id).await;
    park_and_notify(&receiver, &first_boot, &store_root).await;
    let notifies_before_kill = receiver
        .requests()
        .iter()
        .filter(|captured| captured.starts_with("POST /notify"))
        .count();
    assert_eq!(
        notifies_before_kill, 1,
        "exactly one notify before the kill (the ack marker short-circuits retries)"
    );

    // Kill: process death with the ticket parked in the file store. No
    // teardown runs, so nothing may sweep the approval row.
    first_boot.stop().await;
    assert!(
        !approval_files(&store_root).is_empty(),
        "the parked approval row must survive the process death"
    );

    // The decision lands at the receiver while no server is running.
    receiver.set_decided();

    // Reboot onto the SAME store and config: the in-memory notified marker
    // is gone, so the first tick re-notifies (one duplicate, idempotent at
    // the receiver) and then polls the now-decided status endpoint.
    let second_boot = spawn_rig_server(&receiver, &store_root, instance_id).await;
    let decision_record = wait_for_decision_file_within(&store_root, FIRST_TICK_BUDGET).await;
    assert!(
        decision_record.contains(IDENTITY_VALUE),
        "the rebooted server's resolve docks the approver identity, got: {decision_record}"
    );

    let notifies = receiver
        .requests()
        .iter()
        .filter(|captured| captured.starts_with("POST /notify"))
        .count();
    assert!(
        notifies >= 2,
        "the reboot must re-notify (its marker died with the process); \
         captured requests:\n{}",
        receiver.requests().join("\n---\n")
    );
    assert!(
        notifies <= 2,
        "at most one duplicate notify across the reboot (at-least-once, \
         idempotent by decision_id); captured requests:\n{}",
        receiver.requests().join("\n---\n")
    );

    second_boot.stop().await;
}
