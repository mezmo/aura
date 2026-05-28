//! End-to-end test of the wire format.
//!
//! Boots a wiremock, points the telemetry sink at it, captures one
//! `server_started` event, shuts down so the background task flushes,
//! and asserts the bytes that landed in the mock.
//!
//! This test is the "fully inspectable in implementation **and** tests"
//! guarantee made concrete: a reader can `cargo test -p aura-telemetry
//! --test wire_format -- --nocapture` to see the literal payload Aura
//! would send to PostHog.

use std::path::PathBuf;
use std::time::Duration;

use aura_telemetry::events::ServerStarted;
use aura_telemetry::properties::{DeploymentMethod, OsFamily, Source};
use aura_telemetry::{init, TelemetryConfig};
use serde_json::Value;
use tempfile::tempdir;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

async fn capture_one_and_get_body(
    disable: bool,
) -> (Vec<Request>, PathBuf, tempfile::TempDir) {
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
        session_id,
        source: Source::WebServer,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::Local,
        aura_version: "9.9.9-test",
        inspection_log_path: Some(log_path.clone()),
        disable_reason: if disable {
            Some(aura_telemetry::DisableReason::DoNotTrack)
        } else {
            None
        },
        channel_capacity: 16,
        batch_size: 1,
        flush_interval: Duration::from_millis(50),
        http_client: None,
    };
    let handle = init(cfg);

    handle.capture(ServerStarted {
        default_agent_set: true,
    });

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
async fn server_started_payload_shape_is_correct() {
    let (requests, _log, _dir) = capture_one_and_get_body(false).await;
    assert_eq!(requests.len(), 1, "wiremock should have received one batch");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("body is json");
    assert_eq!(body["api_key"], "phc_test_key");
    let batch = body["batch"].as_array().expect("batch is array");
    assert_eq!(batch.len(), 1);
    let event = &batch[0];
    assert_eq!(event["event"], "server_started");
    assert!(event["distinct_id"].is_string());
    assert!(event["timestamp"].is_string());

    let props = &event["properties"];
    // Envelope.
    assert_eq!(props["aura_version"], "9.9.9-test");
    assert_eq!(props["aura_source"], "web-server");
    assert_eq!(props["os_family"], "linux");
    assert_eq!(props["deployment_method"], "local");
    assert!(props["session_id"].is_string());
    // Anonymity guards.
    assert_eq!(props["$ip"], "");
    assert_eq!(props["$geoip_disable"], true);
    // Per-event property.
    assert_eq!(props["default_agent_set"], true);

    // Strictly NO host identifiers in the wire payload — fingerprinty
    // values are forbidden by the spec.
    for forbidden in [
        "host", "hostname", "ip", "mac", "ipv4", "ipv6", "username", "user",
        "cwd", "path", "arch", "kernel", "distro", "container",
        "k8s_namespace", "namespace",
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
    assert!(requests.is_empty(), "no batches should be sent when disabled");
    let contents = std::fs::read_to_string(&log_path).expect("inspection log exists");
    let lines: Vec<&str> = contents.lines().collect();
    // First line is the synthetic telemetry_opt_out, second is the
    // captured server_started with sent:false.
    assert_eq!(lines.len(), 2, "expected opt-out + captured record");
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["event"], "telemetry_opt_out");
    assert_eq!(first["sent"], false);
    assert_eq!(first["disable_reason"], "DoNotTrack");
    assert_eq!(first["properties"]["reason"], "DoNotTrack");

    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["event"], "server_started");
    assert_eq!(second["sent"], false);
    assert_eq!(second["disable_reason"], "DoNotTrack");
    // Inspection log still records the envelope so the user sees
    // exactly what *would* have been sent.
    assert_eq!(second["properties"]["aura_source"], "web-server");
    assert_eq!(second["properties"]["$ip"], "");
}

#[tokio::test]
async fn active_mode_inspection_log_marks_sent_true() {
    let (_requests, log_path, _dir) = capture_one_and_get_body(false).await;
    let contents = std::fs::read_to_string(&log_path).expect("inspection log exists");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 1, "no opt-out, just the captured event");
    let evt: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(evt["event"], "server_started");
    assert_eq!(evt["sent"], true);
    assert!(evt["disable_reason"].is_null());
}
