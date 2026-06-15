//! Tri-state consent behaviour at the `TelemetryHandle` level.
//!
//! Covers the runtime `Unknown → Enabled` transition (`enable()`), the
//! **no-backfill** guarantee (events captured while `Unknown` are never
//! sent, even after enabling), and that `set_disabled` holds.

use std::path::PathBuf;
use std::time::Duration;

use aura_telemetry::events::{CliSessionStarted, ServerStarted};
use aura_telemetry::properties::{DeploymentMethod, OsFamily, Source};
use aura_telemetry::{
    init, DisableReason, EnableOutcome, TelemetryConfig, TelemetryHandle, TelemetryState,
};
use serde_json::Value;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Fixture {
    handle: TelemetryHandle,
    server: MockServer,
    log_path: PathBuf,
    _dir: tempfile::TempDir,
}

async fn fixture(state: TelemetryState) -> Fixture {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/batch/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let cfg = TelemetryConfig {
        endpoint: server.uri(),
        api_key: "phc_test".into(),
        install_id: Uuid::new_v4(),
        install_id_path: None,
        session_id: Uuid::new_v4(),
        source: Source::Cli,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::Local,
        aura_version: "9.9.9-test",
        inspection_log_path: Some(log_path.clone()),
        state,
        channel_capacity: 16,
        batch_size: 1,
        flush_interval: Duration::from_millis(50),
        post_timeout: Duration::from_millis(500),
        http_client: None,
    };
    Fixture {
        handle: init(cfg),
        server,
        log_path,
        _dir: dir,
    }
}

fn log_lines(path: &PathBuf) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// Unknown holds: events are written to the inspection log with the
/// `"Unknown"` label, and nothing is POSTed.
#[tokio::test]
async fn unknown_holds_and_inspects_would_send() {
    let f = fixture(TelemetryState::Unknown).await;
    assert!(matches!(f.handle.state(), TelemetryState::Unknown));

    f.handle.capture(ServerStarted {
        default_agent_set: true,
    });
    // Drain deterministically instead of sleeping. Held events are
    // written to the inspection log synchronously inside `capture`, and
    // `Unknown` spawns no sink — so `shutdown` returns at once. If a bug
    // *had* spawned a sink and POSTed, `shutdown` would join it and the
    // flushed request would be observable below.
    f.handle.shutdown(Duration::from_secs(2)).await;

    let rows = log_lines(&f.log_path);
    assert_eq!(rows.len(), 1, "the held event is written for inspection");
    assert_eq!(rows[0]["event"], "server_started");
    assert_eq!(rows[0]["sent"], false);
    assert_eq!(rows[0]["not_sent_reason"], "Unknown");
    // The full envelope is recorded so the user can inspect what *would*
    // be sent.
    assert_eq!(rows[0]["properties"]["aura_source"], "cli");

    assert!(
        f.server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "Unknown must not POST"
    );
}

/// The no-backfill guarantee: an event captured while Unknown is NOT
/// sent after `enable()`; only events captured *after* enabling go out.
#[tokio::test]
async fn enable_does_not_backfill_unknown_events() {
    let f = fixture(TelemetryState::Unknown).await;

    // Captured while Unknown — must never be sent.
    f.handle.capture(ServerStarted {
        default_agent_set: false,
    });

    assert_eq!(f.handle.enable(), aura_telemetry::EnableOutcome::Enabled);
    assert!(matches!(f.handle.state(), TelemetryState::Enabled));

    // Captured after enabling — must be sent.
    f.handle.capture(CliSessionStarted {
        interactive: true,
        standalone_mode: false,
        client_tools_enabled: false,
    });

    f.handle.shutdown(Duration::from_secs(2)).await;

    let reqs = f.server.received_requests().await.unwrap_or_default();
    // Exactly one batch, containing only the post-enable event.
    let events: Vec<Value> = reqs
        .iter()
        .flat_map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["batch"].as_array().cloned().unwrap_or_default()
        })
        .collect();
    assert_eq!(
        events.len(),
        1,
        "only the post-enable event should be sent; got {events:?}"
    );
    assert_eq!(events[0]["event"], "cli_session_started");
    // The held server_started must NOT appear on the wire.
    assert!(
        events.iter().all(|e| e["event"] != "server_started"),
        "Unknown-era event was backfilled — forbidden"
    );
}

/// `set_disabled` from Unknown holds and never sends.
#[tokio::test]
async fn set_disabled_holds() {
    let f = fixture(TelemetryState::Unknown).await;
    f.handle.set_disabled(DisableReason::AuraDisabled);
    assert!(matches!(
        f.handle.state(),
        TelemetryState::Disabled(DisableReason::AuraDisabled)
    ));

    f.handle.capture(ServerStarted {
        default_agent_set: true,
    });
    // Deterministic drain (see `unknown_holds_*`): no sink runs while
    // Disabled, so this returns immediately; a stray POST would surface.
    f.handle.shutdown(Duration::from_secs(2)).await;

    let rows = log_lines(&f.log_path);
    assert_eq!(rows.last().unwrap()["not_sent_reason"], "AuraDisabled");
    assert!(
        f.server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "disabled must not POST"
    );
}

/// `enable()` is a no-op when Disabled **at init** — a startup kill
/// switch can't be resurrected, and the outcome says so honestly.
#[tokio::test]
async fn enable_is_noop_when_disabled() {
    let f = fixture(TelemetryState::Disabled(DisableReason::DoNotTrack)).await;
    assert_eq!(f.handle.enable(), EnableOutcome::HeldUntilRestart);
    assert!(
        matches!(f.handle.state(), TelemetryState::Disabled(_)),
        "enable() must not revive a Disabled handle"
    );
    f.handle.capture(ServerStarted {
        default_agent_set: true,
    });
    // Deterministic drain: a Disabled-at-init handle has no sink, so
    // `shutdown` returns at once; any erroneous POST would surface.
    f.handle.shutdown(Duration::from_secs(2)).await;
    assert!(
        f.server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "Disabled must not POST even after enable()"
    );
}

/// `/telemetry disable` then `/telemetry enable` in one session: a
/// **runtime** opt-out can be undone immediately because the sink was
/// captured at init. Contrast `enable_is_noop_when_disabled`, where a
/// startup kill switch left no sink to resume. This is the behaviour the
/// docs promise ("take effect immediately for the running session").
#[tokio::test]
async fn runtime_disable_then_enable_resumes_sending() {
    let f = fixture(TelemetryState::Unknown).await;

    // Enable from Unknown (spawns the sink), then opt out at runtime.
    assert_eq!(f.handle.enable(), EnableOutcome::Enabled);
    f.handle.set_disabled(DisableReason::AuraDisabled);
    assert!(matches!(f.handle.state(), TelemetryState::Disabled(_)));

    // Re-enable: the captured sink resumes; the state flips back.
    assert_eq!(f.handle.enable(), EnableOutcome::Enabled);
    assert!(matches!(f.handle.state(), TelemetryState::Enabled));
    // A second call is idempotent.
    assert_eq!(f.handle.enable(), EnableOutcome::AlreadyEnabled);

    f.handle.capture(CliSessionStarted {
        interactive: true,
        standalone_mode: false,
        client_tools_enabled: false,
    });
    f.handle.shutdown(Duration::from_secs(2)).await;

    let reqs = f.server.received_requests().await.unwrap_or_default();
    let events: Vec<Value> = reqs
        .iter()
        .flat_map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["batch"].as_array().cloned().unwrap_or_default()
        })
        .collect();
    assert!(
        events.iter().any(|e| e["event"] == "cli_session_started"),
        "an event captured after re-enable must be sent; got {events:?}"
    );
}
