//! Wire-payload construction and outbound HTTP to the PostHog `/batch`
//! endpoint.
//!
//! This module is the **only** code in `aura` that builds the JSON sent
//! to PostHog, and the only code that opens an outbound HTTP connection
//! for telemetry. Keeping the surface small means the
//! `docs/telemetry.md` audit guide can point a reader at exactly one
//! source file to verify what goes on the wire.
//!
//! Network errors are intentionally swallowed at the `tracing::debug!`
//! level — telemetry must never alter Aura's behaviour. The
//! `aura.telemetry.dropped` counter tracks events lost to a full
//! channel; nothing tracks HTTP failures because they are not a user
//! concern (the user can inspect the local log to see what was queued).

use std::time::Duration;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::properties::{DeploymentMethod, OsFamily, Properties, Source};
use crate::EventPayload;

/// Identity that every batch carries; built once at telemetry init.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub install_id: Uuid,
    pub session_id: Uuid,
    pub source: Source,
    pub os_family: OsFamily,
    pub deployment_method: DeploymentMethod,
    pub aura_version: &'static str,
}

/// Build the JSON for one event ready to be embedded in a PostHog
/// `/batch` array. The envelope fields are merged in here; per-event
/// fields override nothing (envelope keys are namespaced so collisions
/// are not possible).
pub fn build_event_json(envelope: &Envelope, payload: &EventPayload, ts_iso: &str) -> Value {
    let mut properties = json!({
        "aura_version": envelope.aura_version,
        "aura_source": envelope.source.as_str(),
        "os_family": envelope.os_family.as_str(),
        "deployment_method": envelope.deployment_method.as_str(),
        "session_id": envelope.session_id.to_string(),
        // Suppress server-side IP and geoip enrichment. PostHog respects
        // both keys (see PostHog docs on "anonymous events"). The empty
        // `$ip` is the documented way to prevent the ingest pipeline
        // from filling it in from the TCP socket.
        "$ip": "",
        "$geoip_disable": true,
    });
    // Per-event properties last, so anything the envelope provides is
    // not silently overwritten by a future event variant.
    merge_properties(&mut properties, &payload.properties);

    json!({
        "event": payload.name,
        "distinct_id": envelope.install_id.to_string(),
        "timestamp": ts_iso,
        "properties": properties,
    })
}

fn merge_properties(into: &mut Value, props: &Properties) {
    let map = into.as_object_mut().expect("envelope properties is object");
    for (k, v) in props.iter() {
        map.insert(k.to_string(), v.to_json());
    }
}

/// Wrap a batch of events with the API key for a PostHog `/batch` POST.
pub fn build_batch(api_key: &str, events: &[Value]) -> Value {
    json!({
        "api_key": api_key,
        "batch": events,
    })
}

/// POST a batch. Returns `Err` only so the caller can `tracing::debug!`
/// the result — never propagated upstream.
///
/// `timeout` is the per-request budget. The default in
/// `TelemetryConfig::default_for` is intentionally **shorter** than the
/// 2s shutdown budget callers use in production, so the background
/// task always reaches its post-flush inspection-log writes before
/// shutdown cancels it. Callers on slow networks who would rather wait
/// longer can raise it via `TelemetryConfig::post_timeout` — at the
/// cost of more events surfacing as `PostFailed(...)` in the local
/// log on shutdown.
pub async fn post_batch(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    events: &[Value],
    timeout: Duration,
) -> reqwest::Result<()> {
    let url = batch_url(endpoint);
    let body = build_batch(api_key, events);
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(timeout)
        .send()
        .await?;
    // Drop the body explicitly; we don't need it but want to release
    // the connection. `error_for_status` returns Err for 4xx/5xx so
    // those surface as logged failures rather than silent OKs.
    resp.error_for_status()?;
    Ok(())
}

fn batch_url(endpoint: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    format!("{base}/batch/")
}

/// Classify a `reqwest::Error` into one of a small set of stable
/// labels for the local inspection log. Users debugging "why didn't
/// PostHog see this event" should see at a glance whether the failure
/// was the network, a timeout, or a status response — without needing
/// the full error string (which can leak the endpoint URL into the
/// audit log, which we want to keep tidy).
pub fn classify_post_error(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        return "timeout";
    }
    if let Some(status) = err.status() {
        return if status.is_client_error() {
            "http_4xx"
        } else if status.is_server_error() {
            "http_5xx"
        } else {
            "http_other"
        };
    }
    if err.is_connect() || err.is_request() {
        return "network";
    }
    "other"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::properties::{DeploymentMethod, OsFamily, PropertyValue, Source};

    fn fake_envelope() -> Envelope {
        Envelope {
            install_id: Uuid::nil(),
            session_id: Uuid::nil(),
            source: Source::Cli,
            os_family: OsFamily::Linux,
            deployment_method: DeploymentMethod::Local,
            aura_version: "9.9.9-test",
        }
    }

    fn payload(name: &'static str, extras: &[(&'static str, PropertyValue)]) -> EventPayload {
        let mut props = Properties::new();
        for (k, v) in extras {
            props.insert(k, v.clone());
        }
        EventPayload {
            name,
            properties: props,
        }
    }

    #[test]
    fn batch_url_appends_slash() {
        assert_eq!(
            batch_url("https://us.i.posthog.com"),
            "https://us.i.posthog.com/batch/"
        );
        assert_eq!(
            batch_url("https://us.i.posthog.com/"),
            "https://us.i.posthog.com/batch/"
        );
    }

    #[test]
    fn envelope_carries_required_fields() {
        let env = fake_envelope();
        let evt = build_event_json(
            &env,
            &payload("cli_session_started", &[]),
            "2026-05-28T00:00:00Z",
        );
        let props = &evt["properties"];
        assert_eq!(props["aura_version"], "9.9.9-test");
        assert_eq!(props["aura_source"], "cli");
        assert_eq!(props["os_family"], "linux");
        assert_eq!(props["deployment_method"], "local");
        assert_eq!(props["session_id"], Uuid::nil().to_string());
        assert_eq!(evt["distinct_id"], Uuid::nil().to_string());
        assert_eq!(evt["event"], "cli_session_started");
        assert_eq!(evt["timestamp"], "2026-05-28T00:00:00Z");
    }

    #[test]
    fn geoip_suppression_keys_present() {
        let env = fake_envelope();
        let evt = build_event_json(&env, &payload("cli_session_started", &[]), "now");
        assert_eq!(evt["properties"]["$ip"], "");
        assert_eq!(evt["properties"]["$geoip_disable"], true);
    }

    #[test]
    fn per_event_properties_layered_after_envelope() {
        let env = fake_envelope();
        let evt = build_event_json(
            &env,
            &payload(
                "cli_session_started",
                &[("interactive", PropertyValue::Bool(true))],
            ),
            "now",
        );
        assert_eq!(evt["properties"]["interactive"], true);
        // Envelope still intact.
        assert_eq!(evt["properties"]["aura_source"], "cli");
    }

    #[test]
    fn build_batch_wraps_with_api_key() {
        let env = fake_envelope();
        let events = vec![build_event_json(
            &env,
            &payload("cli_session_started", &[]),
            "now",
        )];
        let batch = build_batch("phc_test", &events);
        assert_eq!(batch["api_key"], "phc_test");
        assert_eq!(batch["batch"].as_array().unwrap().len(), 1);
    }

    mod classify {
        use super::*;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// Drive a real POST against `endpoint` and return the resulting
        /// error so `classify_post_error` is tested on genuine
        /// `reqwest::Error`s, not hand-built ones (which can't be).
        async fn post_error(endpoint: &str, timeout: Duration) -> reqwest::Error {
            post_batch(&reqwest::Client::new(), endpoint, "phc_test", &[], timeout)
                .await
                .expect_err("the POST is set up to fail")
        }

        async fn server_returning(status: u16) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            server
        }

        #[tokio::test]
        async fn client_error_status_is_http_4xx() {
            let server = server_returning(404).await;
            let err = post_error(&server.uri(), Duration::from_secs(2)).await;
            assert_eq!(classify_post_error(&err), "http_4xx");
        }

        #[tokio::test]
        async fn server_error_status_is_http_5xx() {
            let server = server_returning(503).await;
            let err = post_error(&server.uri(), Duration::from_secs(2)).await;
            assert_eq!(classify_post_error(&err), "http_5xx");
        }

        #[tokio::test]
        async fn slow_response_is_timeout() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                // Delay far exceeds the per-request budget below, so the
                // client times out deterministically before the response.
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
                .mount(&server)
                .await;
            let err = post_error(&server.uri(), Duration::from_millis(100)).await;
            assert!(err.is_timeout(), "precondition: timed out, got {err:?}");
            assert_eq!(classify_post_error(&err), "timeout");
        }

        #[tokio::test]
        async fn connection_refused_is_network() {
            // Port 1 on loopback is not listening -> connect error.
            let err = post_error("http://127.0.0.1:1", Duration::from_secs(2)).await;
            assert!(
                !err.is_timeout() && err.status().is_none(),
                "precondition: a transport error, not a timeout/status: {err:?}"
            );
            assert_eq!(classify_post_error(&err), "network");
        }
    }
}
