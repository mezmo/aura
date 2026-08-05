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
//! Or directly (set `INSTA_UPDATE=always` the first time to generate the
//! golden snapshots):
//!
//! ```text
//! INSTA_UPDATE=always cargo test -p aura-web-server --features integration-durability --test durability_harness_test
//! ```

#![cfg(feature = "integration-durability")]

use std::path::PathBuf;
use std::time::Duration;

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
    let (transcript, red, mut server) = run_harness(SessionStoreBackend::File, mcp_url, None).await;
    finish(transcript, red, &mut server).await;
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
    let (transcript, red, mut server) =
        run_harness(SessionStoreBackend::Redis, mcp_url, Some(redis_url)).await;
    finish(transcript, red, &mut server).await;
}

async fn run_harness(
    backend: SessionStoreBackend,
    mcp_url: String,
    redis_url: Option<String>,
) -> (FrameTranscript, RedFrames, AuraServerProcess) {
    let stub = StubLlm::start().await;
    let memory_dir = tempfile::tempdir().expect("temp memory dir").keep();

    let port = portpicker::pick_unused_port().expect("unused port");
    let config = ServerConfig {
        llm_base_url: stub.base_url(),
        mcp_url,
        memory_dir: memory_dir.clone(),
        port,
        backend,
        redis_url,
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

    // Frame: planning — drive the run until the first plan is created.
    let chat_url = format!("{}/v1/chat/completions", server.api_url());
    let request_body = json!({
        "model": "durability",
        "messages": [{"role": "user", "content": "Run the durability harness."}],
        "stream": true,
    });

    let events =
        match collect_sse_events(&client, &chat_url, request_body, Duration::from_secs(15)).await {
            Ok(events) => events,
            Err(_e) => {
                red.push("planning");
                transcript.push(Frame::new("planning"));
                return (transcript, red, server);
            }
        };

    let mut planning_frame = Frame::new("planning");
    for evt in &events {
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&evt.data) {
            scrub_nondeterminism(&mut value);
            planning_frame.push_event(value);
        }
    }
    let plan_created = has_event(&events, "aura.orchestrator.plan_created");
    if !plan_created {
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
            scrub_nondeterminism(&mut value);
            worker_frame.push_event(value);
        }
    }
    let task_started = has_event(&events, "aura.orchestrator.task_started");
    if !task_started {
        red.push("worker_execution");
    }
    transcript.push(worker_frame);

    // Frame: park_at_quiescence — after the approval is requested, the run
    // must durably park (emit an orchestrator.run_parked event) within a short
    // window. Production does not wire run_store_for_parking, so this frame is
    // red.
    let mut park_frame = Frame::new("park_at_quiescence");
    for evt in &events {
        if (evt.event_type.as_deref() == Some("aura.approval_requested")
            || evt.event_type.as_deref() == Some("aura.approval_pending"))
            && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&evt.data)
        {
            scrub_nondeterminism(&mut value);
            park_frame.push_event(value);
        }
    }
    let approval_requested = has_event(&events, "aura.approval_requested");
    let run_parked = events.iter().any(|e| {
        e.data.contains("orchestrator.run_parked")
            || e.event_type.as_deref() == Some("aura.orchestrator.run_parked")
    });
    if !approval_requested || !run_parked {
        red.push("park_at_quiescence");
    }
    transcript.push(park_frame);

    // Disconnect the client. With durable parking absent, the conversational
    // route tears down its parked approvals.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Frame: checkpoint_commit_crash — no checkpoint marker exists.
    let mut checkpoint_frame = Frame::new("checkpoint_commit_crash");
    let checkpoint_dir = memory_dir.join("checkpoints");
    checkpoint_frame.record_state("checkpoint_dir_exists", json!(checkpoint_dir.is_dir()));
    if !checkpoint_dir.is_dir() {
        red.push("checkpoint_commit_crash");
    }
    transcript.push(checkpoint_frame);

    // Frame: process_restart — stop and restart the server.
    let mut restart_frame = Frame::new("process_restart");
    server.stop().await.expect("server stops");
    server.restart().await.expect("server restarts");
    record_health(&client, &server, &mut restart_frame).await;
    transcript.push(restart_frame);

    // Frame: approval_by_handle — the parked approval should be resolvable by
    // its decision id after restart. Because the run did not durably park, the
    // approval was torn down with the request, so this frame is red.
    let mut approval_frame = Frame::new("approval_by_handle");
    let decision_id = events.iter().find_map(|e| {
        if e.event_type.as_deref() == Some("aura.approval_requested") {
            serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .and_then(|v| v.get("decision_id")?.as_str().map(|s| s.to_string()))
        } else {
            None
        }
    });
    approval_frame.record_state("decision_id_present", json!(decision_id.is_some()));

    if let Some(id) = decision_id {
        let resolve_url = format!("{}/v1/approvals/{}", server.api_url(), id);
        let resolve_body = json!({"approved": true});
        match client.post(&resolve_url).json(&resolve_body).send().await {
            Ok(resp) => {
                approval_frame.record_state("resolve_status", json!(resp.status().as_u16()));
                if resp.status() != reqwest::StatusCode::NO_CONTENT {
                    red.push("approval_by_handle");
                }
            }
            Err(e) => {
                approval_frame.record_state("resolve_error", json!(e.to_string()));
                red.push("approval_by_handle");
            }
        }
    } else {
        red.push("approval_by_handle");
    }

    // Also record the file-store approval count as a secondary signal.
    let approval_files: Vec<PathBuf> =
        glob::glob(&format!("{}/approvals/*.json", memory_dir.display()))
            .expect("approval glob")
            .filter_map(Result::ok)
            .collect();
    approval_frame.record_state("approval_file_count", json!(approval_files.len()));
    transcript.push(approval_frame);

    // Remaining frames depend on durable park/reify, which is unimplemented.
    red.push("dispatch_claim_crash");
    red.push("headless_reify");
    red.push("completion");
    red.push("retrieval_by_handle");

    // Record the remaining frames as empty placeholders so the transcript
    // captures the full acceptance surface.
    transcript.push(Frame::new("dispatch_claim_crash"));
    transcript.push(Frame::new("headless_reify"));
    transcript.push(Frame::new("completion"));
    transcript.push(Frame::new("retrieval_by_handle"));

    (transcript, red, server)
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

async fn collect_sse_events(
    client: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
    timeout: Duration,
) -> Result<Vec<SseEvent>, Box<dyn std::error::Error>> {
    let mut response = client
        .post(url)
        .json(&body)
        .timeout(timeout + Duration::from_secs(5))
        .send()
        .await?;

    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut current_event_type: Option<String> = None;

    loop {
        match tokio::time::timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
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
                            return Ok(events);
                        }
                        events.push(SseEvent {
                            event_type: current_event_type.take(),
                            data: data.to_string(),
                        });
                    }
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => break,
        }
    }

    // Drain any trailing line without a newline.
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf).trim_end().to_string();
        if let Some(data) = line.strip_prefix("data: ")
            && data != "[DONE]"
        {
            events.push(SseEvent {
                event_type: current_event_type.take(),
                data: data.to_string(),
            });
        }
    }

    Ok(events)
}

async fn finish(transcript: FrameTranscript, red: RedFrames, server: &mut AuraServerProcess) {
    let mut snapshot = transcript.to_snapshot();
    scrub_nondeterminism(&mut snapshot);
    insta::assert_json_snapshot!(snapshot);

    let _ = server.stop().await;

    if !red.is_empty() {
        panic!("{}", render_red_frames(&red));
    }
}
