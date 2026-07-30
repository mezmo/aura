//! The approval decision route: how a gated call gets its decision.
//!
//! A closed two-variant enum chosen by the `[hitl.route]` config table. Replaces
//! the spike's `ApprovalDispatch` trait: the variant set is known, and
//! [`DecisionRoute::decide`] holds the shared semantics (deadline, fail-closed
//! mapping, event emission) in one place instead of per-impl.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_config::{DecisionRouteConfig, GlobPattern, HitlConfig, WebhookUrl};

use super::decision::{ApprovalDecision, ApprovalOutcome};
use super::events;
use super::protocol::{ApprovalDecisionWire, ApprovalRequest, ApprovalRequestWire};
use super::registry::PendingApprovals;
use super::signing::{SigningContext, WebhookHmac, authorize_ingress};
use crate::approval_event_broker::{self, ApprovalLifecycleEvent};

/// Maximum time to wait for a TCP connection to the approval webhook before
/// failing closed. Without this, an unreachable host can hang the connect
/// phase for the full route timeout (e.g. 300s).
const WEBHOOK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Request-stable HITL state shared by the config gate and the agent-callable
/// tool: the compiled glob patterns and the resolved decision route. Built once
/// per request in the builder and shared (by `Arc`) across orchestration
/// workers; the per-agent [`AgentScope`] and request id are supplied at gate
/// construction rather than stored here.
///
/// [`AgentScope`]: super::decision::AgentScope
#[derive(Clone)]
pub struct HitlRuntime {
    pub patterns: Arc<[GlobPattern]>,
    pub route: Arc<DecisionRoute>,
}

impl HitlRuntime {
    /// Resolve the request-stable runtime from parsed `[hitl]` config: share the
    /// compiled globs and build the decision route once (the webhook client and
    /// its connection pool are created here).
    ///
    /// `hmac` is the startup-loaded webhook HMAC shared by every agent build;
    /// `None` leaves the webhook route unsigned.
    #[must_use]
    pub fn from_config(
        config: &HitlConfig,
        pending_approvals: &PendingApprovals,
        hmac: Option<&WebhookHmac>,
    ) -> Self {
        let route = match &config.route {
            DecisionRouteConfig::Webhook { url, timeout_secs } => {
                let signing = match hmac {
                    None => EgressSigning::Disabled,
                    Some(hmac) => EgressSigning::Enabled(hmac.clone()),
                };
                DecisionRoute::Webhook {
                    client: WebhookClient::with_signing(
                        build_webhook_client(),
                        url.clone(),
                        signing,
                    ),
                    timeout: Duration::from_secs(*timeout_secs),
                }
            }
            DecisionRouteConfig::Conversational { timeout_secs } => DecisionRoute::Conversational {
                registry: pending_approvals.clone(),
                timeout: Duration::from_secs(*timeout_secs),
            },
        };
        Self {
            patterns: Arc::from(config.require_approval.clone()),
            route: Arc::new(route),
        }
    }
}

/// Errors that can occur while asking a webhook for an approval decision.
///
/// These are channel faults (transport, bad HTTP status, unparsable body): the
/// route never obtained a decision. A denial is not an error — it arrives as a
/// successful `Ok(ApprovalOutcome::Decided(Denied { .. }))`.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval webhook transport error: {0}")]
    Transport(String),
    #[error("approval webhook returned status {status}")]
    BadStatus { status: u16 },
    #[error("approval webhook response parse error: {0}")]
    Parse(String),
    /// An unusable webhook signing configuration, with the reason.
    #[error("approval webhook signing misconfigured: {0}")]
    Misconfigured(String),
    /// Egress signing failed (system clock unavailable).
    #[error("approval webhook egress signing failed: {0}")]
    Signing(String),
    /// The webhook's HTTP response did not carry a valid signature over its
    /// body (Route A response leg, verified with the same primitive as
    /// ingress). The decision inside is untrusted and discarded.
    #[error("approval webhook response failed signature verification: {0}")]
    ResponseUnverified(String),
}

/// Where an approval decision comes from. Fixed per deployment by config.
pub enum DecisionRoute {
    /// Attended: park in-process, decision returns via `POST /v1/approvals/{id}`.
    Conversational {
        registry: PendingApprovals,
        timeout: Duration,
    },
    /// Unattended: one synchronous HTTP round-trip to a webhook.
    Webhook {
        client: WebhookClient,
        timeout: Duration,
    },
}

impl DecisionRoute {
    /// Obtain a decision for `request`, applying the shared semantics (deadline,
    /// fail-closed mapping, event emission) in one place.
    pub async fn decide(
        &self,
        request: ApprovalRequest,
        cancel: &crate::request_cancellation::RequestCancelToken,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        let started = Instant::now();
        let request_id = request.request_id.clone();
        let decision_id = request.decision_id;
        let scope = request.scope.clone();

        match self {
            Self::Conversational { registry, timeout } => {
                let requested_event = ApprovalLifecycleEvent::Requested((&request).into());
                let expires_at = chrono::Utc::now()
                    + chrono::Duration::from_std(*timeout)
                        .expect("approval timeout fits in chrono");
                let pending_event = events::pending(&request, &expires_at);

                // Register before publishing anything: both events carry the
                // decision id off-process (SSE), and an approver reacting to
                // either must find the parked record already resolvable.
                let handle = registry.register(request, *timeout).await;

                approval_event_broker::publish(&request_id, requested_event).await;
                approval_event_broker::publish(
                    &request_id,
                    ApprovalLifecycleEvent::Pending(pending_event),
                )
                .await;

                let outcome = handle.outcome(cancel).await;
                if matches!(
                    outcome,
                    ApprovalOutcome::TimedOut { .. } | ApprovalOutcome::Cancelled(_)
                ) {
                    registry.remove(&decision_id).await;
                }

                let completed_event =
                    events::completed(decision_id, &outcome, &scope, started.elapsed());
                approval_event_broker::publish(
                    &request_id,
                    ApprovalLifecycleEvent::Completed(completed_event),
                )
                .await;

                Ok(outcome)
            }
            Self::Webhook { client, timeout } => {
                approval_event_broker::publish(
                    &request_id,
                    ApprovalLifecycleEvent::Requested((&request).into()),
                )
                .await;

                let result = client.request_approval(&request, *timeout).await;
                let completed = match &result {
                    Ok(outcome) => {
                        events::completed(decision_id, outcome, &scope, started.elapsed())
                    }
                    Err(err) => events::completed_error(
                        decision_id,
                        err.to_string(),
                        &scope,
                        started.elapsed(),
                    ),
                };
                approval_event_broker::publish(
                    &request_id,
                    ApprovalLifecycleEvent::Completed(completed),
                )
                .await;
                result
            }
        }
    }
}

/// Build the reqwest client used for approval webhook calls. Sets a short
/// connect timeout so an unreachable host fails fast instead of hanging for
/// the full route timeout.
pub(crate) fn build_webhook_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(WEBHOOK_CONNECT_TIMEOUT)
        .build()
        .expect("reqwest client builder only fails on TLS backend init")
}

/// HMAC signing state for the webhook route.
enum EgressSigning {
    /// Unsigned egress.
    Disabled,
    /// Signed egress under the configured HMAC secret.
    Enabled(WebhookHmac),
    /// An unusable signing configuration, with the reason.
    Misconfigured(String),
}

/// HTTP client for the webhook route. Carried over from the spike's
/// `HttpApprovalDispatch`.
pub struct WebhookClient {
    client: reqwest::Client,
    url: WebhookUrl,
    signing: EgressSigning,
}

impl WebhookClient {
    /// Builds an unsigned client. Deliberately does NOT read the environment
    /// (so constructing one in a test never races env-mutating tests):
    /// production construction goes through [`HitlRuntime::from_config`],
    /// which receives the startup-loaded HMAC and calls `with_signing`.
    #[must_use]
    pub fn new(client: reqwest::Client, url: WebhookUrl) -> Self {
        Self::with_signing(client, url, EgressSigning::Disabled)
    }

    fn with_signing(client: reqwest::Client, url: WebhookUrl, signing: EgressSigning) -> Self {
        // A plaintext response channel would defeat response-leg verification,
        // so http:// with a secret configured is itself a misconfiguration
        // (DESIGN.md §4, Route A).
        let signing = match signing {
            EgressSigning::Enabled(_) if url.as_str().starts_with("http://") => {
                EgressSigning::Misconfigured(format!(
                    "webhook url {} uses plaintext http:// while an HMAC secret is configured; \
                     use https://",
                    url.as_str()
                ))
            }
            other => other,
        };
        if let EgressSigning::Misconfigured(reason) = &signing {
            tracing::error!(
                reason,
                "HITL webhook HMAC misconfigured; every approval request on this route \
                 will fail closed"
            );
        }
        Self {
            client,
            url,
            signing,
        }
    }

    /// POST the request and resolve a decision, failing closed on timeout or
    /// transport/parse error. With a secret configured the POST carries the
    /// `X-Aura-*` signature headers (context `approval-request:{decision_id}`)
    /// and the HTTP response must verify under
    /// `approval-decision:{decision_id}` before its body is parsed.
    async fn request_approval(
        &self,
        request: &ApprovalRequest,
        timeout: Duration,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        let hmac = match &self.signing {
            EgressSigning::Misconfigured(reason) => {
                return Err(ApprovalError::Misconfigured(reason.clone()));
            }
            EgressSigning::Disabled => None,
            EgressSigning::Enabled(hmac) => Some(hmac),
        };
        // Serialize the wire view, not the domain request: it keeps `scope` /
        // `origin` as the flat `aura_events` DTOs instead of leaking Rust enum
        // variant names onto the webhook contract.
        let wire = ApprovalRequestWire::from(request);
        let Some(hmac) = hmac else {
            return self.request_approval_unsigned(&wire, timeout).await;
        };

        // Signing requires the exact bytes that go on the wire, so serialize
        // once and send that buffer instead of `.json(&wire)`.
        let body = serde_json::to_vec(&wire).expect("approval request wire view serializes");
        let egress_context =
            SigningContext::new(&format!("approval-request:{}", request.decision_id))
                .expect("decision id renders as dot-free ASCII");
        let headers = hmac
            .sign(&egress_context, &body)
            .map_err(|e| ApprovalError::Signing(e.to_string()))?;
        let mut post = self
            .client
            .post(self.url.as_str())
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        for (name, value) in headers.into_pairs() {
            post = post.header(name, value);
        }
        match post.body(body).timeout(timeout).send().await {
            Err(e) if e.is_timeout() => Ok(ApprovalOutcome::TimedOut { waited: timeout }),
            Err(e) => Err(ApprovalError::Transport(e.to_string())),
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    return Err(ApprovalError::BadStatus {
                        status: status.as_u16(),
                    });
                }
                // Route A response leg: the decision arrives as this HTTP
                // response, so it is verified with the same primitive as
                // ingress before any parse.
                let signature = header_value(resp.headers(), super::signing::SIGNATURE_HEADER);
                let timestamp = header_value(resp.headers(), super::signing::TIMESTAMP_HEADER);
                let body = match resp.bytes().await {
                    Ok(body) => body,
                    // A timeout firing mid-body download is still a timeout, not
                    // a transport fault — keep the classification honest.
                    Err(e) if e.is_timeout() => {
                        return Ok(ApprovalOutcome::TimedOut { waited: timeout });
                    }
                    Err(e) => return Err(ApprovalError::Transport(e.to_string())),
                };
                let response_context =
                    SigningContext::new(&format!("approval-decision:{}", request.decision_id))
                        .expect("decision id renders as dot-free ASCII");
                let verified = authorize_ingress(
                    Some(hmac),
                    &response_context,
                    signature.as_deref(),
                    timestamp.as_deref(),
                    body,
                )
                .map_err(|e| ApprovalError::ResponseUnverified(e.to_string()))?;
                match serde_json::from_slice::<ApprovalDecisionWire>(verified.as_ref()) {
                    Ok(wire) => Ok(ApprovalOutcome::Decided(ApprovalDecision::from(wire))),
                    Err(e) => Err(ApprovalError::Parse(e.to_string())),
                }
            }
        }
    }

    /// Send the approval POST unsigned and trust the response.
    async fn request_approval_unsigned(
        &self,
        wire: &ApprovalRequestWire<'_>,
        timeout: Duration,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        match self
            .client
            .post(self.url.as_str())
            .json(wire)
            .timeout(timeout)
            .send()
            .await
        {
            Err(e) if e.is_timeout() => Ok(ApprovalOutcome::TimedOut { waited: timeout }),
            Err(e) => Err(ApprovalError::Transport(e.to_string())),
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    return Err(ApprovalError::BadStatus {
                        status: status.as_u16(),
                    });
                }
                match resp.json::<ApprovalDecisionWire>().await {
                    Ok(wire) => Ok(ApprovalOutcome::Decided(ApprovalDecision::from(wire))),
                    // A timeout firing mid-body download is still a timeout, not a
                    // parse fault — keep the error-vs-decision classification honest.
                    Err(e) if e.is_timeout() => Ok(ApprovalOutcome::TimedOut { waited: timeout }),
                    Err(e) => Err(ApprovalError::Parse(e.to_string())),
                }
            }
        }
    }
}

/// A configured webhook URL that would carry signed traffic in plaintext.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "webhook url {url} uses plaintext http:// while an HMAC secret is configured; use https://"
)]
pub struct PlaintextWebhookUrlError {
    url: String,
}

/// Boot-time guard: with an HMAC secret configured, a plaintext `http://`
/// webhook URL must fail startup, not the first approval request. Call this
/// for every `[hitl]` config once the secret has been loaded; the request-time
/// `Misconfigured` rejection inside [`WebhookClient`] acts as defense in
/// depth for paths that skip startup validation.
pub fn validate_webhook_signing_config(
    config: &HitlConfig,
    hmac: Option<&WebhookHmac>,
) -> Result<(), PlaintextWebhookUrlError> {
    if hmac.is_none() {
        return Ok(());
    }
    match &config.route {
        DecisionRouteConfig::Webhook { url, .. } if url.as_str().starts_with("http://") => {
            Err(PlaintextWebhookUrlError {
                url: url.as_str().to_string(),
            })
        }
        DecisionRouteConfig::Webhook { .. } | DecisionRouteConfig::Conversational { .. } => Ok(()),
    }
}

/// First value of `name`, treating a non-UTF-8 header value as absent
/// (DESIGN.md residual risk 8).
fn header_value(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::decision::{
        AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalOutcome, DecisionId,
    };
    use super::super::protocol::{
        ApprovalDecisionWire, ApprovalItem, ApprovalRequest, ApprovalRequestWire, PROTOCOL_VERSION,
    };
    use super::super::registry::PendingApprovals;
    use super::DecisionRoute;
    use std::time::Duration;

    fn conv_route(timeout: Duration) -> (PendingApprovals, DecisionRoute) {
        let registry = PendingApprovals::new();
        let route = DecisionRoute::Conversational {
            registry: registry.clone(),
            timeout,
        };
        (registry, route)
    }

    fn single_request(request_id: &str, origin: ApprovalOrigin) -> ApprovalRequest {
        ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: request_id.into(),
            scope: AgentScope::Single { session_id: None },
            origin,
            items: vec![],
        }
    }

    #[test]
    fn single_agent_request_wire_shape() {
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: "req-123".to_string(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "shell*".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "shell_exec".to_string(),
                arguments: json!({ "cmd": "ls -la" }),
            }],
        };

        let value =
            serde_json::to_value(ApprovalRequestWire::from(&request)).expect("serializable");

        assert_eq!(value["version"], PROTOCOL_VERSION);
        assert_eq!(value["request_id"], "req-123");
        assert!(value["decision_id"].is_string());
        // scope/origin are flat, `kind`-tagged DTOs: no Rust variant names leak.
        assert_eq!(value["scope"]["kind"], "single");
        // a sessionless single-agent request omits session_id entirely (no null).
        assert!(value["scope"].get("session_id").is_none());
        assert_eq!(value["origin"]["kind"], "config_gate");
        assert_eq!(value["origin"]["matched_pattern"], "shell*");
        // regression guard: the externally-tagged domain variant keys must not appear.
        assert!(value["scope"].get("Single").is_none());
        assert!(value["origin"].get("ConfigGate").is_none());

        let items = value["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["tool_name"], "shell_exec");
        assert_eq!(items[0]["arguments"]["cmd"], "ls -la");
    }

    #[test]
    fn worker_request_wire_shape_flattens_task_and_keeps_session() {
        let run_id: crate::orchestration::RunId =
            "0191e8c0-1111-7000-8000-000000000000".parse().unwrap();
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: "req-9".to_string(),
            scope: AgentScope::Worker {
                run_id,
                task: crate::orchestration::TaskIdentity::new(2, Some("k8s-agent".to_string())),
                session_id: Some(crate::config::SessionId::new("sess-abc".to_string())),
            },
            origin: ApprovalOrigin::AgentRequested {
                reason: "deleting prod ns".to_string(),
            },
            items: vec![],
        };

        let value =
            serde_json::to_value(ApprovalRequestWire::from(&request)).expect("serializable");

        assert_eq!(value["scope"]["kind"], "worker");
        assert_eq!(value["request_id"], "req-9");
        assert_eq!(value["scope"]["task_id"], 2);
        assert_eq!(value["scope"]["worker"], "k8s-agent");
        assert_eq!(value["scope"]["session_id"], "sess-abc");
        assert!(value["scope"]["run_id"].is_string());
        // task is flattened to task_id/worker siblings, not a nested object.
        assert!(value["scope"].get("task").is_none());
        assert_eq!(value["origin"]["kind"], "agent_requested");
        // regression guard: no externally-tagged domain variant keys.
        assert!(value["scope"].get("Worker").is_none());
        assert!(value["origin"].get("AgentRequested").is_none());
    }

    #[test]
    fn wire_to_outcome_approved() {
        let wire = ApprovalDecisionWire {
            approved: true,
            reason: None,
        };
        assert_eq!(ApprovalDecision::from(wire), ApprovalDecision::Approved);
    }

    #[test]
    fn wire_to_outcome_denied() {
        let wire = ApprovalDecisionWire {
            approved: false,
            reason: Some("x".into()),
        };
        assert_eq!(
            ApprovalDecision::from(wire),
            ApprovalDecision::Denied {
                reason: Some("x".to_string())
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn conversational_decide_approved() {
        let (registry, route) = conv_route(Duration::from_secs(60));
        let request = single_request(
            "conv-req-1",
            ApprovalOrigin::AgentRequested {
                reason: "test".into(),
            },
        );
        let decision_id = request.decision_id;
        let cancel = crate::request_cancellation::RequestCancelToken::unbound();

        let decide_handle: tokio::task::JoinHandle<Result<ApprovalOutcome, super::ApprovalError>> =
            tokio::spawn({
                let cancel = cancel.clone();
                async move { route.decide(request, &cancel).await }
            });

        loop {
            tokio::task::yield_now().await;
            if registry
                .resolve(&decision_id, ApprovalDecision::Approved)
                .await
                .is_ok()
            {
                break;
            }
        }

        let result = decide_handle.await.unwrap();
        assert_eq!(
            result.unwrap(),
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn conversational_decide_denied() {
        let (registry, route) = conv_route(Duration::from_secs(60));
        let request = single_request(
            "conv-req-2",
            ApprovalOrigin::ConfigGate {
                matched_pattern: "rm_*".into(),
            },
        );
        let decision_id = request.decision_id;
        let cancel = crate::request_cancellation::RequestCancelToken::unbound();

        let decide_handle: tokio::task::JoinHandle<Result<ApprovalOutcome, super::ApprovalError>> =
            tokio::spawn({
                let cancel = cancel.clone();
                async move { route.decide(request, &cancel).await }
            });

        loop {
            tokio::task::yield_now().await;
            if registry
                .resolve(
                    &decision_id,
                    ApprovalDecision::Denied {
                        reason: Some("too risky".into()),
                    },
                )
                .await
                .is_ok()
            {
                break;
            }
        }

        let result = decide_handle.await.unwrap();
        assert_eq!(
            result.unwrap(),
            ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: Some("too risky".into())
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn conversational_decide_times_out() {
        use super::super::registry::ResolveError;

        let (registry, route) = conv_route(Duration::from_secs(5));
        let request = single_request(
            "conv-req-3",
            ApprovalOrigin::AgentRequested {
                reason: "test".into(),
            },
        );
        let decision_id = request.decision_id;
        let cancel = crate::request_cancellation::RequestCancelToken::unbound();

        let decide_handle: tokio::task::JoinHandle<Result<ApprovalOutcome, super::ApprovalError>> =
            tokio::spawn(async move { route.decide(request, &cancel).await });
        tokio::time::advance(Duration::from_secs(6)).await;

        let result = decide_handle.await.unwrap().unwrap();
        match result {
            ApprovalOutcome::TimedOut { .. } => {}
            other => panic!("expected TimedOut, got {:?}", other),
        }
        assert_eq!(
            registry
                .resolve(&decision_id, ApprovalDecision::Approved)
                .await,
            Err(ResolveError::NotFound),
            "late decisions for timed-out approvals must be rejected as expired",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn conversational_decide_cancelled_on_disconnect() {
        let (_, route) = conv_route(Duration::from_secs(60));
        let request = single_request(
            "conv-req-4",
            ApprovalOrigin::AgentRequested {
                reason: "test".into(),
            },
        );
        let cancel = crate::request_cancellation::RequestCancelToken::unbound();

        let decide_handle: tokio::task::JoinHandle<Result<ApprovalOutcome, super::ApprovalError>> =
            tokio::spawn({
                let cancel = cancel.clone();
                async move { route.decide(request, &cancel).await }
            });

        tokio::task::yield_now().await;
        cancel.cancel();

        let result = decide_handle.await.unwrap().unwrap();
        assert_eq!(
            result,
            ApprovalOutcome::Cancelled(super::super::decision::CancelReason::ClientDisconnected)
        );
    }

    #[tokio::test]
    async fn conversational_resolve_at_requested_event_succeeds() {
        let request_id = format!("req_test_{}", uuid::Uuid::new_v4().simple());
        let mut rx = crate::approval_event_broker::subscribe(&request_id).await;

        let (registry, route) = conv_route(Duration::from_secs(60));
        let request = single_request(
            &request_id,
            ApprovalOrigin::ConfigGate {
                matched_pattern: "dangerous_*".into(),
            },
        );
        let decision_id = request.decision_id;

        let cancel = crate::request_cancellation::RequestCancelToken::unbound();
        let decide_handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { route.decide(request, &cancel).await }
        });

        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("requested event should arrive")
            .expect("requested event channel open");
        assert!(matches!(
            first,
            crate::approval_event_broker::ApprovalLifecycleEvent::Requested(_)
        ));

        // An approver reacting to `Requested` immediately must find the
        // record already parked — resolving here may not race registration.
        registry
            .resolve(&decision_id, ApprovalDecision::Approved)
            .await
            .expect("record must be resolvable once Requested is observable");

        let outcome = decide_handle.await.unwrap().unwrap();
        assert_eq!(
            outcome,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );

        crate::approval_event_broker::unsubscribe(&request_id).await;
    }

    mod webhook_signing {
        use std::time::Duration;

        use bytes::Bytes;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use super::super::super::decision::{
            AgentScope, ApprovalDecision, ApprovalOrigin, DecisionId,
        };
        use super::super::super::protocol::{ApprovalRequest, PROTOCOL_VERSION};
        use super::super::super::signing::{
            PrimarySecret, SIGNATURE_HEADER, SigningContext, TIMESTAMP_HEADER, Tolerance,
            WebhookHmac, authorize_ingress,
        };
        use super::super::{
            ApprovalError, ApprovalOutcome, EgressSigning, WebhookClient, build_webhook_client,
        };

        fn test_hmac() -> WebhookHmac {
            WebhookHmac::new(
                PrimarySecret::new(b"0123456789abcdef0123456789abcdef"),
                None,
                Tolerance::new(300).unwrap(),
            )
            .unwrap()
        }

        fn test_request(decision_id: DecisionId) -> ApprovalRequest {
            ApprovalRequest {
                version: PROTOCOL_VERSION,
                decision_id,
                request_id: "req-signed".into(),
                scope: AgentScope::Single { session_id: None },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "dangerous_*".into(),
                },
                items: vec![],
            }
        }

        struct ReceivedRequest {
            headers: Vec<(String, String)>,
            body: Vec<u8>,
        }

        impl ReceivedRequest {
            fn header(&self, name: &str) -> Option<&str> {
                self.headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.as_str())
            }
        }

        /// One-shot HTTP/1.1 receiver: accepts a single POST, hands the
        /// captured request back over the channel, and replies 200 with the
        /// given extra headers and body.
        async fn one_shot_receiver(
            response_headers: Vec<(String, String)>,
            response_body: String,
        ) -> (String, tokio::sync::oneshot::Receiver<ReceivedRequest>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "peer closed before request completed");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos;
                    }
                };
                let header_text = String::from_utf8(buf[..header_end].to_vec()).unwrap();
                let headers: Vec<(String, String)> = header_text
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();
                let content_length: usize = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or(0);
                let mut body = buf[header_end + 4..].to_vec();
                while body.len() < content_length {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "peer closed mid-body");
                    body.extend_from_slice(&chunk[..n]);
                }

                let mut response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n",
                    response_body.len()
                );
                for (name, value) in &response_headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                response.push_str(&response_body);
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
                tx.send(ReceivedRequest { headers, body }).ok();
            });
            (url, rx)
        }

        /// Builds a signing-enabled client directly against a loopback
        /// `http://` receiver. Bypasses the https-only policy deliberately:
        /// the policy is exercised by `http_url_with_secret_fails_closed`.
        fn loopback_signed_client(url: &str, hmac: WebhookHmac) -> WebhookClient {
            WebhookClient {
                client: build_webhook_client(),
                url: aura_config::WebhookUrl::new(url).unwrap(),
                signing: EgressSigning::Enabled(hmac),
            }
        }

        #[tokio::test]
        async fn signed_egress_verified_by_receiver_and_signed_response_accepted() {
            let hmac = test_hmac();
            let decision_id = DecisionId::generate();

            // The mock receiver answers with a decision signed over its own
            // response body under the approval-decision context.
            let response_body = r#"{"approved":true}"#.to_string();
            let response_context =
                SigningContext::new(&format!("approval-decision:{decision_id}")).unwrap();
            let response_headers = hmac
                .sign(&response_context, response_body.as_bytes())
                .unwrap()
                .into_pairs()
                .map(|(name, value)| (name.to_string(), value));
            let (url, received) = one_shot_receiver(response_headers.to_vec(), response_body).await;

            let client = loopback_signed_client(&url, hmac.clone());
            let outcome = client
                .request_approval(&test_request(decision_id), Duration::from_secs(5))
                .await
                .expect("signed round trip must succeed");
            assert_eq!(
                outcome,
                ApprovalOutcome::Decided(ApprovalDecision::Approved)
            );

            // The receiver independently verifies the egress signature over
            // the raw bytes it saw, bound to the approval-request context.
            let received = received.await.unwrap();
            assert_eq!(received.header("content-type"), Some("application/json"));
            let signature = received.header(SIGNATURE_HEADER).map(str::to_owned);
            let timestamp = received.header(TIMESTAMP_HEADER).map(str::to_owned);
            assert!(signature.is_some(), "egress POST must carry the signature");
            assert!(timestamp.is_some(), "egress POST must carry the timestamp");
            let egress_context =
                SigningContext::new(&format!("approval-request:{decision_id}")).unwrap();
            authorize_ingress(
                Some(&hmac),
                &egress_context,
                signature.as_deref(),
                timestamp.as_deref(),
                Bytes::from(received.body),
            )
            .expect("receiver must be able to verify the egress signature");
        }

        #[tokio::test]
        async fn unsigned_response_rejected_when_secret_configured() {
            let hmac = test_hmac();
            let decision_id = DecisionId::generate();
            let (url, _received) =
                one_shot_receiver(vec![], r#"{"approved":true}"#.to_string()).await;

            let client = loopback_signed_client(&url, hmac);
            let err = client
                .request_approval(&test_request(decision_id), Duration::from_secs(5))
                .await
                .expect_err("an unsigned response must not become a decision");
            assert!(
                matches!(err, ApprovalError::ResponseUnverified(_)),
                "expected ResponseUnverified, got {err:?}"
            );
        }

        #[tokio::test]
        async fn response_signed_for_other_decision_rejected() {
            let hmac = test_hmac();
            let decision_id = DecisionId::generate();

            // Signed response, but bound to a different decision's context:
            // the A1 context binding must reject the cross-decision replay.
            let response_body = r#"{"approved":true}"#.to_string();
            let other_context =
                SigningContext::new(&format!("approval-decision:{}", DecisionId::generate()))
                    .unwrap();
            let response_headers = hmac
                .sign(&other_context, response_body.as_bytes())
                .unwrap()
                .into_pairs()
                .map(|(name, value)| (name.to_string(), value));
            let (url, _received) =
                one_shot_receiver(response_headers.to_vec(), response_body).await;

            let client = loopback_signed_client(&url, hmac);
            let err = client
                .request_approval(&test_request(decision_id), Duration::from_secs(5))
                .await
                .expect_err("a cross-decision response signature must be rejected");
            assert!(matches!(err, ApprovalError::ResponseUnverified(_)));
        }

        #[tokio::test]
        async fn http_url_with_secret_fails_closed() {
            let client = WebhookClient::with_signing(
                build_webhook_client(),
                aura_config::WebhookUrl::new("http://approvals.example.com/aura").unwrap(),
                EgressSigning::Enabled(test_hmac()),
            );
            let err = client
                .request_approval(
                    &test_request(DecisionId::generate()),
                    Duration::from_secs(1),
                )
                .await
                .expect_err("plaintext url with a secret must fail closed");
            assert!(
                matches!(err, ApprovalError::Misconfigured(_)),
                "expected Misconfigured, got {err:?}"
            );
        }

        #[test]
        fn boot_validation_rejects_plaintext_url_only_with_secret() {
            use super::super::validate_webhook_signing_config;

            let webhook = |url: &str| aura_config::HitlConfig {
                require_approval: vec![],
                route: aura_config::DecisionRouteConfig::Webhook {
                    url: aura_config::WebhookUrl::new(url).unwrap(),
                    timeout_secs: 300,
                },
            };
            let hmac = test_hmac();

            // Secret + http:// fails at boot.
            let err = validate_webhook_signing_config(
                &webhook("http://approvals.example.com/aura"),
                Some(&hmac),
            )
            .unwrap_err();
            assert!(err.to_string().contains("plaintext http://"));

            // Secret + https:// passes.
            validate_webhook_signing_config(
                &webhook("https://approvals.example.com/aura"),
                Some(&hmac),
            )
            .unwrap();

            // Without a secret, http:// is allowed.
            validate_webhook_signing_config(&webhook("http://approvals.example.com/aura"), None)
                .unwrap();

            // Conversational route has no URL to validate.
            let conversational = aura_config::HitlConfig {
                require_approval: vec![],
                route: aura_config::DecisionRouteConfig::Conversational { timeout_secs: 60 },
            };
            validate_webhook_signing_config(&conversational, Some(&hmac)).unwrap();
        }

        #[test]
        fn from_config_threads_hmac_into_webhook_route() {
            use super::super::{DecisionRoute, HitlRuntime};

            let config = |url: &str| aura_config::HitlConfig {
                require_approval: vec![],
                route: aura_config::DecisionRouteConfig::Webhook {
                    url: aura_config::WebhookUrl::new(url).unwrap(),
                    timeout_secs: 300,
                },
            };
            let pending = crate::hitl::PendingApprovals::new();
            let hmac = test_hmac();

            fn signing_of(runtime: &HitlRuntime) -> &EgressSigning {
                match &*runtime.route {
                    DecisionRoute::Webhook { client, .. } => &client.signing,
                    DecisionRoute::Conversational { .. } => panic!("expected webhook route"),
                }
            }

            let signed = HitlRuntime::from_config(
                &config("https://approvals.example.com/aura"),
                &pending,
                Some(&hmac),
            );
            assert!(
                matches!(signing_of(&signed), EgressSigning::Enabled(_)),
                "a threaded secret must produce signed egress"
            );

            let unsigned = HitlRuntime::from_config(
                &config("http://approvals.example.com/aura"),
                &pending,
                None,
            );
            assert!(
                matches!(signing_of(&unsigned), EgressSigning::Disabled),
                "no threaded secret must produce unsigned egress"
            );

            let plaintext = HitlRuntime::from_config(
                &config("http://approvals.example.com/aura"),
                &pending,
                Some(&hmac),
            );
            assert!(
                matches!(signing_of(&plaintext), EgressSigning::Misconfigured(_)),
                "plaintext http:// with a secret must fail closed"
            );
        }

        #[tokio::test]
        async fn no_secret_sends_unsigned_and_trusts_response() {
            let decision_id = DecisionId::generate();
            let (url, received) =
                one_shot_receiver(vec![], r#"{"approved":true}"#.to_string()).await;

            let client = WebhookClient::with_signing(
                build_webhook_client(),
                aura_config::WebhookUrl::new(&url).unwrap(),
                EgressSigning::Disabled,
            );
            let outcome = client
                .request_approval(&test_request(decision_id), Duration::from_secs(5))
                .await
                .expect("unsigned round trip succeeds");
            assert_eq!(
                outcome,
                ApprovalOutcome::Decided(ApprovalDecision::Approved)
            );

            let received = received.await.unwrap();
            assert!(
                received.header(SIGNATURE_HEADER).is_none(),
                "no secret configured must mean no signature header"
            );
            assert!(received.header(TIMESTAMP_HEADER).is_none());
        }
    }

    #[tokio::test]
    async fn webhook_route_emits_requested_and_completed_on_channel_error() {
        let request_id = format!("req_test_{}", uuid::Uuid::new_v4().simple());
        let mut rx = crate::approval_event_broker::subscribe(&request_id).await;
        let route = super::DecisionRoute::Webhook {
            client: super::WebhookClient::new(
                super::build_webhook_client(),
                aura_config::WebhookUrl::new("http://127.0.0.1:9").unwrap(),
            ),
            timeout: std::time::Duration::from_secs(1),
        };
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: request_id.clone(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "dangerous_*".into(),
            },
            items: vec![ApprovalItem {
                tool_name: "dangerous_apply".into(),
                arguments: serde_json::json!({}),
            }],
        };

        let cancel = crate::request_cancellation::RequestCancelToken::unbound();
        let result = route.decide(request, &cancel).await;
        assert!(result.is_err(), "discard-port webhook should fail closed");

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("requested event should arrive")
            .expect("requested event channel open");
        assert!(matches!(
            first,
            crate::approval_event_broker::ApprovalLifecycleEvent::Requested(_)
        ));

        let second = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("completed event should arrive")
            .expect("completed event channel open");
        match second {
            crate::approval_event_broker::ApprovalLifecycleEvent::Completed(completed) => {
                assert!(matches!(
                    completed.outcome,
                    aura_events::ApprovalOutcomeWire::Errored { .. }
                ));
            }
            other => panic!("expected completed event, got {:?}", other),
        }

        crate::approval_event_broker::unsubscribe(&request_id).await;
    }
}
