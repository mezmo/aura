//! Durability harness for the #271 HITL park/reify acceptance frames.
//!
//! This test drives a real `aura-web-server` process with a stub Ollama LLM
//! and a gated MCP tool, then records a frame transcript. Production durable
//! parking is not yet wired, so the first failing frame is `park_at_quiescence`.
//!
//! Run with the integration feature and infrastructure:
//!
//! ```text
//! make test-integration-durability-local
//! ```
//!
//! To intentionally regenerate the golden snapshots:
//!
//! ```text
//! make test-integration-durability-bless
//! ```
//!
//! Or directly (set `INSTA_UPDATE=always` to generate snapshots the first time):
//!
//! ```text
//! INSTA_UPDATE=always cargo test -p aura-web-server --features integration-durability --test durability_harness_test
//! ```

#![cfg(feature = "integration-durability")]

use std::path::PathBuf;
use std::time::Duration;

use aura::orchestration::park::RunState;
use tokio::sync::mpsc;
use tokio::time::Instant;

use aura_test_utils::durability::{
    AuraServerProcess, Frame, FrameTranscript, RedFrames, ServerConfig, SessionStoreBackend,
    StubLlm, normalize::scrub_nondeterminism, render_red_frames,
};
use aura_test_utils::sse::SseEvent;
use serde_json::json;

const MCP_URL_ENV: &str = "AURA_TEST_MCP_URL";
const DEFAULT_MCP_URL: &str = "http://127.0.0.1:9999/mcp";
const REDIS_URL_ENV: &str = "AURA_TEST_REDIS_URL";

#[tokio::test(flavor = "multi_thread")]
async fn durability_harness_file_frames() {
    let mcp_url = std::env::var(MCP_URL_ENV).unwrap_or_else(|_| DEFAULT_MCP_URL.to_string());
    let (mut transcript, mut red, mut server, events, setup_failure) =
        drive_to_park(SessionStoreBackend::File, mcp_url, None).await;

    if let Some(err) = setup_failure {
        let mut frame = Frame::new("harness_setup_failure");
        frame.record_state("error", json!(err));
        transcript.push(frame);
        finish("file_frames", transcript, red, &mut server).await;
        return;
    }

    let client = reqwest::Client::new();

    let session_id = extract_session_id(&events);
    let decision_id = extract_decision_id(&events);

    // Frame: checkpoint_commit_crash — kill the server at the park window,
    // restart, and assert that at least one decodable run record is Parked
    // with its checkpoint. Orchestration artifacts (plan.json, etc.) do not
    // count; only files that decode as a SessionRecord via decode_run_record
    // are run records, and a Created/Running/Completed/Failed/Cancelled
    // record does not satisfy the crash-recovery guarantee this frame checks.
    let mut checkpoint_frame = Frame::new("checkpoint_commit_crash");
    let memory_dir = server.memory_dir().to_path_buf();
    let (run_records, artifact_count) = find_run_records(&memory_dir).await;
    let approval_files: Vec<PathBuf> =
        glob::glob(&format!("{}/approvals/*.json", memory_dir.display()))
            .expect("approval glob")
            .filter_map(Result::ok)
            .collect();
    checkpoint_frame.record_state("memory_dir", json!(memory_dir.display().to_string()));
    checkpoint_frame.record_state("run_record_count", json!(run_records.len()));
    checkpoint_frame.record_state("artifact_count", json!(artifact_count));
    checkpoint_frame.record_state("approval_file_count", json!(approval_files.len()));
    checkpoint_frame.record_state(
        "run_record_paths",
        json!(
            run_records
                .iter()
                .map(|(p, _)| p.display().to_string())
                .collect::<Vec<_>>()
        ),
    );
    checkpoint_frame.record_state(
        "run_record_states",
        json!(
            run_records
                .iter()
                .map(|(_, state)| run_state_name(state))
                .collect::<Vec<_>>()
        ),
    );
    let has_parked_checkpoint = run_records
        .iter()
        .any(|(_, state)| matches!(state, RunState::Parked { .. }));
    if !has_parked_checkpoint {
        red.push("checkpoint_commit_crash");
    }
    transcript.push(checkpoint_frame);

    // Frame: process_restart — real process boundary (the previous kill already
    // stopped the process; restart it and confirm it is healthy).
    let mut restart_frame = Frame::new("process_restart");
    server.restart().await.expect("server restarts after crash");
    record_health(&client, &server, &mut restart_frame).await;
    transcript.push(restart_frame);

    // Frame: approval_resolution_by_handle — the parked approval must be
    // resolvable by its decision id after restart. This is honestly green
    // today because the conversational approval record survives a SIGKILL.
    let mut approval_frame = Frame::new("approval_resolution_by_handle");
    if let Some(id) = &decision_id {
        let resolve_url = format!("{}/v1/approvals/{}", server.api_url(), id);
        let resolve_body = json!({"approved": true});
        match client.post(&resolve_url).json(&resolve_body).send().await {
            Ok(resp) => {
                approval_frame.record_state("resolve_status", json!(resp.status().as_u16()));
                if resp.status() != reqwest::StatusCode::NO_CONTENT {
                    red.push("approval_resolution_by_handle");
                }
            }
            Err(e) => {
                approval_frame.record_state("resolve_error", json!(e.to_string()));
                red.push("approval_resolution_by_handle");
            }
        }
    } else {
        approval_frame.record_state("resolve_error", json!("no decision_id captured"));
        red.push("approval_resolution_by_handle");
    }
    transcript.push(approval_frame);

    // Remaining frames exercise the post-park surface. Each one drives the
    // expected V1 endpoint and fails today because the endpoint/behavior does
    // not exist.
    drive_post_park_frames(
        &client,
        &server,
        session_id.as_deref(),
        &mut transcript,
        &mut red,
    )
    .await;

    finish("file_frames", transcript, red, &mut server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn durability_harness_redis_frames() {
    let redis_url = match std::env::var(REDIS_URL_ENV) {
        Ok(url) => url,
        Err(_) => {
            eprintln!("warning: {} not set; skipping redis frames", REDIS_URL_ENV);
            return;
        }
    };
    let mcp_url = std::env::var(MCP_URL_ENV).unwrap_or_else(|_| DEFAULT_MCP_URL.to_string());
    let (mut transcript, mut red, mut server, events, setup_failure) =
        drive_to_park(SessionStoreBackend::Redis, mcp_url, Some(redis_url.clone())).await;

    if let Some(err) = setup_failure {
        let mut frame = Frame::new("harness_setup_failure");
        frame.record_state("error", json!(err));
        transcript.push(frame);
        finish("redis_frames", transcript, red, &mut server).await;
        return;
    }

    let client = reqwest::Client::new();

    let session_id = extract_session_id(&events);
    let decision_id = extract_decision_id(&events);

    // Frame: process_restart — real process boundary with the Redis backend.
    let mut restart_frame = Frame::new("process_restart");
    server.restart().await.expect("server restarts after crash");
    record_health(&client, &server, &mut restart_frame).await;
    transcript.push(restart_frame);

    // Frame: publish_loss_wake_discovery — the approval is still parked (not
    // resolved yet). Kill the server again and publish the decision directly to
    // the Redis event bus while it is down. Redis drops the message because no
    // subscriber is connected; that is the lost-publish fact the frame records.
    // The durable decision state that a real resolve_durable would persist is a
    // staged hole, so the frame names that gap. After restart the server must
    // discover the decision from the store and wake the parked run; today the
    // store-based discovery path does not exist, so the claim fails.
    let mut wake_frame = Frame::new("publish_loss_wake_discovery");
    server
        .stop()
        .await
        .expect("server stops for publish-loss test");
    if let Some(id) = &decision_id {
        match publish_decision_to_redis(&redis_url, id, true).await {
            Ok(subscribers) => {
                wake_frame.record_state("publish_subscribers", json!(subscribers));
            }
            Err(e) => {
                wake_frame.record_state("publish_error", json!(e.to_string()));
                red.push("publish_loss_wake_discovery");
            }
        }
    } else {
        wake_frame.record_state("publish_error", json!("no decision_id captured"));
        red.push("publish_loss_wake_discovery");
    }
    wake_frame.record_state(
        "durable_decision_state",
        json!("unrepresentable: resolve_durable is a staged hole"),
    );
    server
        .start()
        .await
        .expect("server restarts after publish-loss");
    if let Some(id) = &session_id {
        let claim_url = format!("{}/v1/sessions/{}/claim", server.api_url(), id);
        match client.post(&claim_url).send().await {
            Ok(resp) => {
                wake_frame.record_state("claim_status", json!(resp.status().as_u16()));
                if !resp.status().is_success() {
                    red.push("publish_loss_wake_discovery");
                }
            }
            Err(e) => {
                wake_frame.record_state("claim_error", json!(e.to_string()));
                red.push("publish_loss_wake_discovery");
            }
        }
    } else {
        wake_frame.record_state("claim_error", json!("no session_id captured"));
        red.push("publish_loss_wake_discovery");
    }
    transcript.push(wake_frame);

    // Frame: approval_resolution_by_handle — the Redis-backed approval record
    // survives process death and resolves by handle.
    let mut approval_frame = Frame::new("approval_resolution_by_handle");
    if let Some(id) = &decision_id {
        let resolve_url = format!("{}/v1/approvals/{}", server.api_url(), id);
        let resolve_body = json!({"approved": true});
        match client.post(&resolve_url).json(&resolve_body).send().await {
            Ok(resp) => {
                approval_frame.record_state("resolve_status", json!(resp.status().as_u16()));
                if resp.status() != reqwest::StatusCode::NO_CONTENT {
                    red.push("approval_resolution_by_handle");
                }
            }
            Err(e) => {
                approval_frame.record_state("resolve_error", json!(e.to_string()));
                red.push("approval_resolution_by_handle");
            }
        }
    } else {
        approval_frame.record_state("resolve_error", json!("no decision_id captured"));
        red.push("approval_resolution_by_handle");
    }
    transcript.push(approval_frame);

    finish("redis_frames", transcript, red, &mut server).await;
}

/// Drive a run up to the first HITL approval request, kill the server at the
/// park window, and return the captured events. The approval record is left
/// in the backend because the crash prevents the request teardown from
/// cancelling it.
async fn drive_to_park(
    backend: SessionStoreBackend,
    mcp_url: String,
    redis_url: Option<String>,
) -> (
    FrameTranscript,
    RedFrames,
    AuraServerProcess,
    Vec<SseEvent>,
    Option<String>,
) {
    let stub = StubLlm::start().await;
    let memory_dir = tempfile::tempdir().expect("temp memory dir").keep();

    let port = portpicker::pick_unused_port().expect("unused port");
    let config = ServerConfig {
        llm_base_url: stub.base_url(),
        mcp_url,
        memory_dir: memory_dir.clone(),
        port,
        backend,
        redis_url: redis_url.clone(),
    };

    let mut server = AuraServerProcess::new(config).expect("server config writes");
    server.start().await.expect("server starts");

    let mut transcript = FrameTranscript::new();
    let mut red = RedFrames::default();
    let client = reqwest::Client::new();

    // Frame: start
    let mut start_frame = Frame::new("start");
    record_health(&client, &server, &mut start_frame).await;
    transcript.push(start_frame);

    // Drive the run until the approval request, then crash the server.
    let request_body = json!({
        "model": "durability",
        "messages": [{"role": "user", "content": "Run the durability harness."}],
        "stream": true,
    });
    let (events, setup_failure) =
        drive_to_approval(&client, &mut server, request_body, redis_url.as_ref()).await;
    let memory_dir = server.memory_dir().to_path_buf();

    // Frame: planning — the coordinator must emit a plan.
    let mut planning_frame = Frame::new("planning");
    for evt in &events {
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&evt.data) {
            scrub_nondeterminism(&mut value, &memory_dir);
            planning_frame.push_event(value);
        }
    }
    if !has_event(&events, "aura.orchestrator.plan_created") {
        red.push("planning");
    }
    transcript.push(planning_frame);

    // Frame: worker_execution — the worker task must start.
    let mut worker_frame = Frame::new("worker_execution");
    for evt in &events {
        if (evt.event_type.as_deref() == Some("aura.orchestrator.task_started")
            || evt.event_type.as_deref() == Some("aura.orchestrator.tool_call_started"))
            && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&evt.data)
        {
            scrub_nondeterminism(&mut value, &memory_dir);
            worker_frame.push_event(value);
        }
    }
    if !has_event(&events, "aura.orchestrator.task_started") {
        red.push("worker_execution");
    }
    transcript.push(worker_frame);

    // Frame: park_at_quiescence — after the approval is requested, the run
    // must durably park (emit an orchestrator.run_parked event). Production
    // does not wire run_store_for_parking, so this frame is red.
    let mut park_frame = Frame::new("park_at_quiescence");
    for evt in &events {
        if (evt.event_type.as_deref() == Some("aura.approval_requested")
            || evt.event_type.as_deref() == Some("aura.approval_pending"))
            && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&evt.data)
        {
            scrub_nondeterminism(&mut value, &memory_dir);
            park_frame.push_event(value);
        }
    }
    let approval_requested = has_event(&events, "aura.approval_requested");
    let run_parked = events
        .iter()
        .any(|e| e.event_type.as_deref() == Some("aura.orchestrator.run_parked"));
    if !approval_requested || !run_parked {
        red.push("park_at_quiescence");
    }
    transcript.push(park_frame);

    (transcript, red, server, events, setup_failure)
}

/// Post-park frames that exercise the expected durable-park surface. Each
/// frame drives a V1 endpoint; today they return 404 or otherwise fail
/// because the production behavior is not implemented.
async fn drive_post_park_frames(
    client: &reqwest::Client,
    server: &AuraServerProcess,
    session_id: Option<&str>,
    transcript: &mut FrameTranscript,
    red: &mut RedFrames,
) {
    // Frame: dispatch_claim_crash — after approval, the run should claim its
    // lease and dispatch the blocked worker. Today the claim endpoint does not
    // exist.
    let mut dispatch_frame = Frame::new("dispatch_claim_crash");
    if let Some(id) = session_id {
        let claim_url = format!("{}/v1/sessions/{}/claim", server.api_url(), id);
        match client.post(&claim_url).send().await {
            Ok(resp) => {
                dispatch_frame.record_state("claim_status", json!(resp.status().as_u16()));
                if !resp.status().is_success() {
                    red.push("dispatch_claim_crash");
                }
            }
            Err(e) => {
                dispatch_frame.record_state("claim_error", json!(e.to_string()));
                red.push("dispatch_claim_crash");
            }
        }
    } else {
        dispatch_frame.record_state("claim_error", json!("no session_id captured"));
        red.push("dispatch_claim_crash");
    }
    transcript.push(dispatch_frame);

    // Frame: headless_reify — a headless request should be able to reify the
    // parked session. Today the reify endpoint does not exist.
    let mut reify_frame = Frame::new("headless_reify");
    if let Some(id) = session_id {
        let reify_url = format!("{}/v1/sessions/{}/reify", server.api_url(), id);
        match client.post(&reify_url).send().await {
            Ok(resp) => {
                reify_frame.record_state("reify_status", json!(resp.status().as_u16()));
                if !resp.status().is_success() {
                    red.push("headless_reify");
                }
            }
            Err(e) => {
                reify_frame.record_state("reify_error", json!(e.to_string()));
                red.push("headless_reify");
            }
        }
    } else {
        reify_frame.record_state("reify_error", json!("no session_id captured"));
        red.push("headless_reify");
    }
    transcript.push(reify_frame);

    // Frame: completion — the run should eventually complete. Today there is
    // no session status endpoint to poll.
    let mut completion_frame = Frame::new("completion");
    if let Some(id) = session_id {
        let status_url = format!("{}/v1/sessions/{}", server.api_url(), id);
        match client.get(&status_url).send().await {
            Ok(resp) => {
                completion_frame.record_state("status_status", json!(resp.status().as_u16()));
                let completed = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|body| {
                        body.get("status")
                            .and_then(|s| s.as_str().map(|s| s == "completed"))
                    })
                    .unwrap_or(false);
                completion_frame.record_state("status_completed", json!(completed));
                if !completed {
                    red.push("completion");
                }
            }
            Err(e) => {
                completion_frame.record_state("status_error", json!(e.to_string()));
                red.push("completion");
            }
        }
    } else {
        completion_frame.record_state("status_error", json!("no session_id captured"));
        red.push("completion");
    }
    transcript.push(completion_frame);

    // Frame: retrieval_by_handle — the run should be retrievable by its
    // session handle. Today the retrieval endpoint does not exist.
    let mut retrieval_frame = Frame::new("retrieval_by_handle");
    if let Some(id) = session_id {
        let retrieve_url = format!("{}/v1/sessions/{}", server.api_url(), id);
        match client.get(&retrieve_url).send().await {
            Ok(resp) => {
                retrieval_frame.record_state("retrieve_status", json!(resp.status().as_u16()));
                let has_run = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .map(|body| body.get("run_id").is_some())
                    .unwrap_or(false);
                retrieval_frame.record_state("has_run_id", json!(has_run));
                if !has_run {
                    red.push("retrieval_by_handle");
                }
            }
            Err(e) => {
                retrieval_frame.record_state("retrieve_error", json!(e.to_string()));
                red.push("retrieval_by_handle");
            }
        }
    } else {
        retrieval_frame.record_state("retrieve_error", json!("no session_id captured"));
        red.push("retrieval_by_handle");
    }
    transcript.push(retrieval_frame);
}

/// Open a streaming chat request, collect events until the first
/// `aura.approval_requested`, then wait for the approval record to be persisted
/// in the backend before SIGKILL-ing the server. The response is held open
/// until the process dies, so the request teardown never runs and the approval
/// record survives.
///
/// Returns the collected events and an optional harness-setup failure message.
/// A setup failure means the crash window was never reached, which is
/// distinguishable from a product-red frame in the output.
async fn drive_to_approval(
    client: &reqwest::Client,
    server: &mut AuraServerProcess,
    body: serde_json::Value,
    redis_url: Option<&String>,
) -> (Vec<SseEvent>, Option<String>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let url = format!("{}/v1/chat/completions", server.api_url());
    let client = client.clone();
    let handle = tokio::spawn(async move {
        let _ = sse_collect(client, url, body, tx).await;
    });

    let overall_deadline = Instant::now() + Duration::from_secs(20);
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel();
    let (fail_tx, mut fail_rx) = mpsc::unbounded_channel();
    let mut events = Vec::new();
    let mut setup_failure: Option<String> = None;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(overall_deadline) => break,
            _ = kill_rx.recv() => {
                let _ = server.stop().await;
                break;
            }
            maybe_fail = fail_rx.recv() => {
                if let Some(err) = maybe_fail {
                    setup_failure = Some(err);
                }
                break;
            }
            maybe = rx.recv() => {
                match maybe {
                    Some(evt) => {
                        if evt.event_type.as_deref() == Some("aura.approval_requested")
                            && let Some(id) = decision_id_from_event(&evt)
                        {
                            let memory_dir = server.memory_dir().to_path_buf();
                            let redis_url = redis_url.cloned();
                            let kill_tx = kill_tx.clone();
                            let fail_tx = fail_tx.clone();
                            tokio::spawn(async move {
                                match wait_for_approval_record(
                                    &memory_dir,
                                    &id,
                                    redis_url.as_ref(),
                                )
                                .await
                                {
                                    Ok(()) => {
                                        let _ = kill_tx.send(());
                                    }
                                    Err(e) => {
                                        let _ = fail_tx.send(e.to_string());
                                    }
                                }
                            });
                        }
                        events.push(evt);
                    }
                    None => break,
                }
            }
        }
    }

    // Drain any events that arrived between the kill and connection close.
    while let Ok(Some(evt)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        events.push(evt);
    }
    let _ = handle.await;
    (events, setup_failure)
}

async fn sse_collect(
    client: reqwest::Client,
    url: String,
    body: serde_json::Value,
    tx: mpsc::UnboundedSender<SseEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut response = client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(25))
        .send()
        .await?;

    let mut buf: Vec<u8> = Vec::new();
    let mut current_event_type: Option<String> = None;

    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line_bytes).trim_end().to_string();
                    if line.is_empty() {
                        current_event_type = None;
                        continue;
                    }
                    if let Some(event) = line.strip_prefix("event: ") {
                        current_event_type = Some(event.to_string());
                    } else if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            return Ok(());
                        }
                        let _ = tx.send(SseEvent {
                            event_type: current_event_type.take(),
                            data: data.to_string(),
                        });
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    Ok(())
}

async fn record_health(client: &reqwest::Client, server: &AuraServerProcess, frame: &mut Frame) {
    let url = format!("{}/health", server.api_url());
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            frame.record_state("health_status", json!(status));
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                frame.record_state("health_body", body);
            }
        }
        Err(e) => {
            frame.record_state("health_error", json!(e.to_string()));
        }
    }
}

fn has_event(events: &[SseEvent], event_type: &str) -> bool {
    events
        .iter()
        .any(|e| e.event_type.as_deref() == Some(event_type))
}

fn extract_session_id(events: &[SseEvent]) -> Option<String> {
    events.iter().find_map(|e| {
        if e.event_type.as_deref() == Some("aura.session_info") {
            serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .and_then(|v| v.get("session_id")?.as_str().map(|s| s.to_string()))
        } else {
            None
        }
    })
}

fn extract_decision_id(events: &[SseEvent]) -> Option<String> {
    events.iter().find_map(|e| {
        if e.event_type.as_deref() == Some("aura.approval_requested") {
            decision_id_from_event(e)
        } else {
            None
        }
    })
}

fn decision_id_from_event(event: &SseEvent) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&event.data)
        .ok()
        .and_then(|v| v.get("decision_id")?.as_str().map(|s| s.to_string()))
}

fn redis_key_prefix() -> String {
    std::env::var("AURA_SESSION_STORE_PREFIX")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "aura".to_string())
}

/// Poll the backend until the approval record for `decision_id` is persisted,
/// or until a generous timeout expires. This replaces the fixed post-approval
/// sleep and removes the timing-flake class from the crash window.
async fn wait_for_approval_record(
    memory_dir: &std::path::Path,
    decision_id: &str,
    redis_url: Option<&String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let interval = Duration::from_millis(50);

    if let Some(url) = redis_url {
        let client = redis::Client::open(url.as_str())?;
        let mut conn = client.get_connection_manager().await?;
        let key = format!("{}:approval:{decision_id}", redis_key_prefix());
        while Instant::now() < deadline {
            let exists: bool = redis::AsyncCommands::exists(&mut conn, &key).await?;
            if exists {
                return Ok(());
            }
            tokio::time::sleep(interval).await;
        }
    } else {
        let path = memory_dir
            .join("approvals")
            .join(format!("{decision_id}.json"));
        while Instant::now() < deadline {
            if path.exists() {
                return Ok(());
            }
            tokio::time::sleep(interval).await;
        }
    }

    Err("approval record did not appear within deadline".into())
}

/// Publish an approval decision directly to the Redis event bus channel that
/// the server's `PendingApprovals` subscribes to. This simulates a decision
/// event that is published while the server is down and would otherwise be lost.
async fn publish_decision_to_redis(
    redis_url: &str,
    decision_id: &str,
    approved: bool,
) -> Result<usize, Box<dyn std::error::Error>> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_connection_manager().await?;
    let channel = format!("{}:bus:approval:{decision_id}", redis_key_prefix());
    let payload = if approved {
        r#"{"Approved":null}"#.to_string()
    } else {
        r#"{"Denied":{"reason":"harness test denial"}}"#.to_string()
    };
    let subscribers: usize = redis::AsyncCommands::publish(&mut conn, channel, payload).await?;
    Ok(subscribers)
}

/// Candidate files under `memory_dir` that decode as a v1 [`RunState`]
/// record, paired with their decoded state, plus a count of candidates that
/// did not decode (orchestration artifacts such as `plan.json`).
async fn find_run_records(memory_dir: &std::path::Path) -> (Vec<(PathBuf, RunState)>, usize) {
    let pattern = format!("{}/**/*.json", memory_dir.display());
    let candidates: Vec<PathBuf> = glob::glob(&pattern)
        .expect("glob pattern is valid")
        .filter_map(Result::ok)
        .filter(|p| !p.starts_with(memory_dir.join("approvals")))
        .collect();

    let mut records = Vec::new();
    for candidate in &candidates {
        if let Ok(raw) = tokio::fs::read_to_string(candidate).await {
            // decode_run_record returns Err for JSON that is not a run
            // record; orchestration artifacts live alongside run records
            // under memory_dir and are counted, not decoded.
            if let Ok(record) = aura::session_store::decode_run_record(&raw) {
                records.push((candidate.clone(), record.state));
            }
        }
    }

    let artifact_count = candidates.len().saturating_sub(records.len());
    (records, artifact_count)
}

fn run_state_name(state: &RunState) -> &'static str {
    match state {
        RunState::Created => "created",
        RunState::Running => "running",
        RunState::Parked { .. } => "parked",
        RunState::Completed => "completed",
        RunState::Failed { .. } => "failed",
        RunState::Cancelled => "cancelled",
    }
}

async fn finish(
    snapshot_name: &str,
    transcript: FrameTranscript,
    red: RedFrames,
    server: &mut AuraServerProcess,
) {
    let has_harness_failure = transcript
        .frames()
        .iter()
        .any(|f| f.name == "harness_setup_failure");
    let mut snapshot = transcript.to_snapshot();
    scrub_nondeterminism(&mut snapshot, server.memory_dir());
    insta::assert_json_snapshot!(snapshot_name, snapshot);

    let _ = server.stop().await;

    if has_harness_failure {
        panic!("harness setup failure (see harness_setup_failure frame in snapshot)");
    }

    if !red.is_empty() {
        panic!("{}", render_red_frames(&red));
    }
}
