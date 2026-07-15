//! End-to-end test of the wire format.
//!
//! Boots a wiremock, points the telemetry sink at it, captures one
//! `chat_request_completed` event, shuts down so the background task
//! flushes, and asserts the bytes that landed in the mock.
//!
//! This test is the "fully inspectable in implementation **and** tests"
//! guarantee made concrete: a reader can `cargo test -p aura-telemetry
//! --test wire_format -- --nocapture` to see the literal payload Aura
//! would send to PostHog.

use std::path::PathBuf;
use std::time::Duration;

use aura_telemetry::events::ChatRequestCompleted;
use aura_telemetry::events::CliSessionEnded;
use aura_telemetry::properties::ExitReason;
use aura_telemetry::properties::{DeploymentMethod, OsFamily, Source};
use aura_telemetry::{init, TelemetryConfig};
use serde_json::Value;
use tempfile::tempdir;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

async fn capture_one_and_get_body(disable: bool) -> (Vec<Request>, PathBuf, tempfile::TempDir) {
    let server = MockServer::start().await;
    let captured = Mock::given(method("POST"))
        .and(path("/batch/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(if disable { 0 } else { 1 })
        .mount_as_scoped(&server)
        .await;

    let dir = tempdir().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let install_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let cfg = TelemetryConfig {
        endpoint: server.uri(),
        api_key: "phc_test_key".into(),
        install_id,
        install_id_path: None,
        session_id,
        source: Source::Cli,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::Local,
        aura_version: "9.9.9-test",
        inspection_log_path: Some(log_path.clone()),
        state: if disable {
            aura_telemetry::TelemetryState::Disabled(aura_telemetry::DisableReason::DoNotTrack)
        } else {
            aura_telemetry::TelemetryState::Enabled
        },
        channel_capacity: 16,
        batch_size: 1,
        flush_interval: Duration::from_millis(50),
        post_timeout: Duration::from_millis(500),
        http_client: None,
    };
    let handle = init(cfg);

    handle.capture(ChatRequestCompleted { success: true });

    handle.shutdown(Duration::from_secs(2)).await;
    // Avoid moving `captured` out of the future before the assertion.
    drop(captured);
    (
        server.received_requests().await.unwrap_or_default(),
        log_path,
        dir,
    )
}

#[tokio::test]
async fn chat_request_payload_shape_is_correct() {
    let (requests, _log, _dir) = capture_one_and_get_body(false).await;
    assert_eq!(requests.len(), 1, "wiremock should have received one batch");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("body is json");
    assert_eq!(body["api_key"], "phc_test_key");
    let batch = body["batch"].as_array().expect("batch is array");
    assert_eq!(batch.len(), 1);
    let event = &batch[0];
    assert_eq!(event["event"], "chat_request_completed");
    assert!(event["distinct_id"].is_string());
    assert!(event["timestamp"].is_string());

    let props = &event["properties"];
    // Envelope.
    assert_eq!(props["aura_version"], "9.9.9-test");
    assert_eq!(props["aura_source"], "cli");
    assert_eq!(props["os_family"], "linux");
    assert_eq!(props["deployment_method"], "local");
    assert!(props["session_id"].is_string());
    // Anonymity guards.
    assert_eq!(props["$ip"], "");
    assert_eq!(props["$geoip_disable"], true);
    // Per-event property.
    assert_eq!(props["success"], true);

    // Strictly NO host identifiers in the wire payload — fingerprinty
    // values are forbidden by the spec.
    for forbidden in [
        "host",
        "hostname",
        "ip",
        "mac",
        "ipv4",
        "ipv6",
        "username",
        "user",
        "cwd",
        "path",
        "arch",
        "kernel",
        "distro",
        "container",
        "k8s_namespace",
        "namespace",
        "model",
        "prompt",
    ] {
        assert!(
            props.get(forbidden).is_none(),
            "wire payload must not contain forbidden field `{forbidden}`"
        );
    }
}

#[tokio::test]
async fn disabled_mode_writes_to_inspection_log_and_sends_nothing() {
    let (requests, log_path, _dir) = capture_one_and_get_body(true).await;
    assert!(
        requests.is_empty(),
        "no batches should be sent when disabled"
    );
    let contents = std::fs::read_to_string(&log_path).expect("inspection log exists");
    let lines: Vec<&str> = contents.lines().collect();
    // First line is the synthetic telemetry_opt_out, second is the
    // captured chat_request_completed with sent:false.
    assert_eq!(lines.len(), 2, "expected opt-out + captured record");
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["event"], "telemetry_opt_out");
    assert_eq!(first["sent"], false);
    assert_eq!(first["not_sent_reason"], "DoNotTrack");
    assert_eq!(first["properties"]["reason"], "DoNotTrack");

    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["event"], "chat_request_completed");
    assert_eq!(second["sent"], false);
    assert_eq!(second["not_sent_reason"], "DoNotTrack");
    // Inspection log still records the envelope so the user sees
    // exactly what *would* have been sent.
    assert_eq!(second["properties"]["aura_source"], "cli");
    assert_eq!(second["properties"]["$ip"], "");
}

#[tokio::test]
async fn active_mode_inspection_log_marks_sent_true() {
    let (_requests, log_path, _dir) = capture_one_and_get_body(false).await;
    let contents = std::fs::read_to_string(&log_path).expect("inspection log exists");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 1, "no opt-out, just the captured event");
    let evt: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(evt["event"], "chat_request_completed");
    assert_eq!(evt["sent"], true);
    assert!(evt["not_sent_reason"].is_null());
}

/// Regression: a slow endpoint must not let an in-flight POST outlive
/// the shutdown budget. If it did, the bg task would be cancelled
/// before writing its post-flush inspection-log rows, leaving the
/// captured event neither delivered nor recorded — a violation of the
/// "line is written for every captured event" contract.
///
/// We simulate the bad case with a wiremock that delays its 200
/// response for longer than the shutdown budget; `post_timeout` is
/// configured shorter than the budget so the bg task returns Err
/// from `post_batch`, writes a `PostFailed(timeout)` row, and exits
/// before the outer shutdown timeout fires.
#[tokio::test]
async fn slow_endpoint_does_not_swallow_inspection_log_row() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/batch/"))
        // 5s delay — much longer than the shutdown budget below.
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let cfg = TelemetryConfig {
        endpoint: server.uri(),
        api_key: "phc_test_key".into(),
        install_id: Uuid::new_v4(),
        install_id_path: None,
        session_id: Uuid::new_v4(),
        source: Source::Cli,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::Local,
        aura_version: "9.9.9-test",
        inspection_log_path: Some(log_path.clone()),
        state: aura_telemetry::TelemetryState::Enabled,
        channel_capacity: 16,
        batch_size: 1,
        flush_interval: Duration::from_millis(50),
        // Shorter than the 2s shutdown budget below. The bg task gets
        // ~300ms to give up on the POST and write the row before
        // shutdown's timeout fires.
        post_timeout: Duration::from_millis(300),
        http_client: None,
    };
    let handle = init(cfg);
    handle.capture(ChatRequestCompleted { success: true });
    // Shutdown budget intentionally larger than post_timeout — the
    // contract is "post_timeout < shutdown_budget", and this is the
    // production pairing.
    handle.shutdown(Duration::from_secs(2)).await;

    let contents = std::fs::read_to_string(&log_path).expect("inspection log exists");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected the in-flight event to be recorded"
    );
    let evt: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(evt["event"], "chat_request_completed");
    assert_eq!(evt["sent"], false);
    let reason = evt["not_sent_reason"]
        .as_str()
        .expect("not_sent_reason set");
    assert!(
        reason.contains("PostFailed(timeout)"),
        "expected PostFailed(timeout) for a request that exceeded post_timeout, got {reason:?}"
    );
}

/// Regression: the inspection log used to write `sent: true` as soon
/// as the event was enqueued, which lied to the user when the POST
/// later failed. The background-task now finalises the row only
/// after the POST returns, so a 5xx from the endpoint produces an
/// honest `sent: false` with `not_sent_reason: "PostFailed(...)"`.
#[tokio::test]
async fn post_failure_marks_inspection_log_not_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/batch/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let cfg = TelemetryConfig {
        endpoint: server.uri(),
        api_key: "phc_test_key".into(),
        install_id: Uuid::new_v4(),
        install_id_path: None,
        session_id: Uuid::new_v4(),
        source: Source::Cli,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::Local,
        aura_version: "9.9.9-test",
        inspection_log_path: Some(log_path.clone()),
        state: aura_telemetry::TelemetryState::Enabled,
        channel_capacity: 16,
        batch_size: 1,
        flush_interval: Duration::from_millis(50),
        post_timeout: Duration::from_millis(500),
        http_client: None,
    };
    let handle = init(cfg);
    handle.capture(ChatRequestCompleted { success: true });
    handle.shutdown(Duration::from_secs(2)).await;

    let contents = std::fs::read_to_string(&log_path).expect("inspection log exists");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 1, "expected one inspection row");
    let evt: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(evt["event"], "chat_request_completed");
    assert_eq!(
        evt["sent"], false,
        "POST failure must surface as sent:false"
    );
    let reason = evt["not_sent_reason"]
        .as_str()
        .expect("not_sent_reason set on failure");
    assert!(
        reason.starts_with("PostFailed(") && reason.ends_with(')'),
        "expected PostFailed(<category>), got {reason:?}"
    );
    // The category for a 500 is `http_5xx` per the sink classifier.
    assert!(
        reason.contains("http_5xx"),
        "expected http_5xx classification for a 500 response, got {reason:?}"
    );
}

/// When `capture` outruns the background task, events get dropped at
/// the channel boundary. The drop must show up in the inspection log
/// as `sent: false, not_sent_reason: "ChannelFull"`, not as silence —
/// that is the whole point of the local audit trail.
#[tokio::test]
async fn channel_full_drops_record_to_inspection_log() {
    let server = MockServer::start().await;
    // Block the POST forever so the background task can never drain
    // the channel; subsequent captures fill the channel and must drop.
    Mock::given(method("POST"))
        .and(path("/batch/"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let cfg = TelemetryConfig {
        endpoint: server.uri(),
        api_key: "phc_test_key".into(),
        install_id: Uuid::new_v4(),
        install_id_path: None,
        session_id: Uuid::new_v4(),
        source: Source::Cli,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::Local,
        aura_version: "9.9.9-test",
        inspection_log_path: Some(log_path.clone()),
        state: aura_telemetry::TelemetryState::Enabled,
        channel_capacity: 1,
        batch_size: 1,
        // High flush interval — flush is gated by batch_size=1 hitting
        // and the first event is held in-flight by the wiremock delay.
        flush_interval: Duration::from_secs(30),
        post_timeout: Duration::from_secs(30),
        http_client: None,
    };
    let handle = init(cfg);
    // First capture takes the slot. Subsequent captures drop until
    // the bg task starts a flush, which itself is suspended by the
    // wiremock delay — so all overflow events drop with ChannelFull.
    for _ in 0..32 {
        handle.capture(ChatRequestCompleted { success: true });
    }
    // Give the bg task a moment to dequeue the first event so the
    // remaining captures definitely see a full channel.
    tokio::time::sleep(Duration::from_millis(50)).await;
    for _ in 0..32 {
        handle.capture(ChatRequestCompleted { success: true });
    }

    assert!(
        handle.dropped_count() > 0,
        "expected at least one channel-full drop"
    );

    // Best-effort shutdown: the wiremock holds the POST for 30s, so
    // we cannot wait for it. The inspection log writes for
    // ChannelFull happen synchronously inside `capture`, so they are
    // already on disk by now.
    handle.shutdown(Duration::from_millis(50)).await;

    let contents = std::fs::read_to_string(&log_path).expect("inspection log exists");
    let drops: Vec<Value> = contents
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["not_sent_reason"] == "ChannelFull")
        .collect();
    assert!(
        !drops.is_empty(),
        "expected at least one ChannelFull row in inspection log"
    );
    for drop in &drops {
        assert_eq!(drop["sent"], false);
        assert_eq!(drop["event"], "chat_request_completed");
    }
}

#[tokio::test]
async fn cli_session_ended_payload_shape_is_correct() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/batch/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cfg = TelemetryConfig {
        endpoint: server.uri(),
        api_key: "phc_test_key".into(),
        install_id: Uuid::new_v4(),
        install_id_path: None,
        session_id: Uuid::new_v4(),
        source: Source::Cli,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::Local,
        aura_version: "9.9.9-test",
        inspection_log_path: Some(dir.path().join("events.jsonl")),
        state: aura_telemetry::TelemetryState::Enabled,
        channel_capacity: 16,
        batch_size: 1,
        flush_interval: Duration::from_millis(50),
        post_timeout: Duration::from_millis(500),
        http_client: None,
    };
    let handle = init(cfg);
    handle.capture(CliSessionEnded {
        exit_reason: ExitReason::Quit,
    });
    handle.shutdown(Duration::from_secs(2)).await;

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1, "one batch should be sent");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("body is json");
    let event = &body["batch"][0];
    assert_eq!(event["event"], "cli_session_ended");
    assert_eq!(event["properties"]["exit_reason"], "quit");
    // Envelope still present.
    assert_eq!(event["properties"]["aura_source"], "cli");
    assert!(event["properties"]["session_id"].is_string());
}
