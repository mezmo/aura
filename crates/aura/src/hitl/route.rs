//! The approval decision route: how a gated call gets its decision.
//!
//! A closed two-variant enum chosen by the `[hitl.route]` config table. Replaces
//! the spike's `ApprovalDispatch` trait: the variant set is known, and
//! [`DecisionRoute::decide`] holds the shared semantics (deadline, fail-closed
//! mapping, event emission) in one place instead of per-impl.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_config::{DecisionRouteConfig, GlobPattern, HitlConfig, WebhookUrl};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

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
    ///
    /// `req_headers` is the inbound client request's HTTP headers, used to
    /// resolve `[hitl.route]` `headers_from_request` mappings. Pass `None`
    /// outside an HTTP request context (e.g. CLI standalone mode).
    #[must_use]
    pub fn from_config(
        config: &HitlConfig,
        pending_approvals: &PendingApprovals,
        hmac: Option<&WebhookHmac>,
        req_headers: Option<&HashMap<String, String>>,
    ) -> Self {
        let route = match &config.route {
            DecisionRouteConfig::Webhook {
                url,
                timeout_secs,
                headers,
                headers_from_request,
            } => {
                let signing = match hmac {
                    None => EgressSigning::Disabled,
                    Some(hmac) => EgressSigning::Enabled(hmac.clone()),
                };
                DecisionRoute::Webhook {
                    client: WebhookClient::with_headers_and_signing(
                        build_webhook_client(),
                        url.clone(),
                        resolve_webhook_headers(headers, headers_from_request, req_headers),
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

/// Resolve operator-configured webhook headers into a validated [`HeaderMap`]:
/// static `headers` overlaid with `headers_from_request` values from the
/// inbound client request. Invalid header names or values are skipped with
/// a warning (matching `mcp_streamable_http.rs`).
///
/// Opt-in only: nothing is forwarded unless the operator configures it.
/// Classification is deliberately absent from this surface — see
/// [`apply_request_header_mappings`](crate::rig_builder::apply_request_header_mappings).
fn resolve_webhook_headers(
    static_headers: &HashMap<String, String>,
    headers_from_request: &HashMap<String, String>,
    req_headers: Option<&HashMap<String, String>>,
) -> HeaderMap {
    let empty = HashMap::new();
    let req_headers = req_headers.unwrap_or(&empty);

    let mut resolved: HashMap<String, String> = static_headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .collect();
    let resolved_count = crate::rig_builder::apply_request_header_mappings(
        &mut resolved,
        headers_from_request,
        req_headers,
    );
    if resolved_count > 0 {
        tracing::info!("Webhook route: resolved {resolved_count} header(s) from request");
    }

    let mut header_map = HeaderMap::new();
    for (key, value) in &resolved {
        match (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(val)) => {
                header_map.insert(name, val);
            }
            _ => {
                tracing::warn!(
                    "Skipping invalid webhook header '{}' (failed to convert)",
                    key
                );
            }
        }
    }
    header_map
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
    /// Resolved webhook headers.
    headers: HeaderMap,
    signing: EgressSigning,
}

impl WebhookClient {
    /// Builds an unsigned client. Deliberately does NOT read the environment
    /// (so constructing one in a test never races env-mutating tests):
    /// production construction goes through [`HitlRuntime::from_config`],
    /// which receives the startup-loaded HMAC and calls `with_signing`.
    #[must_use]
    pub fn new(client: reqwest::Client, url: WebhookUrl) -> Self {
        Self::with_headers_and_signing(client, url, HeaderMap::new(), EgressSigning::Disabled)
    }

    /// Create a webhook client with resolved operator-configured headers
    /// applied to every approval POST.
    #[must_use]
    pub fn new_with_headers(client: reqwest::Client, url: WebhookUrl, headers: HeaderMap) -> Self {
        Self::with_headers_and_signing(client, url, headers, EgressSigning::Disabled)
    }

    fn with_headers_and_signing(
        client: reqwest::Client,
        url: WebhookUrl,
        headers: HeaderMap,
        signing: EgressSigning,
    ) -> Self {
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
            headers,
            signing,
        }
    }

    /// Apply operator-configured headers to a request builder. Called before
    /// any signature headers so a mapped header can never displace them.
    fn apply_operator_headers(
        &self,
        mut builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        for (name, value) in self.headers.iter() {
            builder = builder.header(name.clone(), value.clone());
        }
        builder
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
        let mut post = self.apply_operator_headers(
            self.client
                .post(self.url.as_str())
                .header(reqwest::header::CONTENT_TYPE, "application/json"),
        );
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
        let builder = self
            .apply_operator_headers(self.client.post(self.url.as_str()).json(wire))
            .timeout(timeout);
        match builder.send().await {
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
    use super::DecisionRoute;

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
                tool_call_intent: None,
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
        assert!(items[0].get("tool_call_intent").is_none());
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
    fn config_gate_item_tool_call_intent_present_on_wire() {
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: "req-cg-intent".to_string(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "kubectl_delete".to_string(),
                arguments: json!({ "namespace": "prod" }),
                tool_call_intent: Some("rollout restart to pick up the new config map".to_string()),
            }],
        };

        let value =
            serde_json::to_value(ApprovalRequestWire::from(&request)).expect("serializable");
        let items = value["items"].as_array().expect("items array");
        assert_eq!(
            items[0]["tool_call_intent"],
            "rollout restart to pick up the new config map"
        );
    }

    #[test]
    fn agent_requested_item_tool_call_intent_omitted_when_absent() {
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: "req-ar-none".to_string(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::AgentRequested {
                reason: "touches prod".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "request_approval".to_string(),
                arguments: json!({
                    "action_description": "Delete namespace",
                    "risk_rationale": "touches prod"
                }),
                tool_call_intent: None,
            }],
        };

        let value =
            serde_json::to_value(ApprovalRequestWire::from(&request)).expect("serializable");
        let items = value["items"].as_array().expect("items array");
        assert!(items[0].get("tool_call_intent").is_none());
    }

    #[test]
    fn agent_requested_item_tool_call_intent_present_on_wire() {
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: "req-ar-intent".to_string(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::AgentRequested {
                reason: "touches prod".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "request_approval".to_string(),
                arguments: json!({
                    "action_description": "Delete namespace",
                    "risk_rationale": "touches prod"
                }),
                tool_call_intent: Some(
                    "namespace cleanup is the fastest path to unblock".to_string(),
                ),
            }],
        };

        let value =
            serde_json::to_value(ApprovalRequestWire::from(&request)).expect("serializable");
        let items = value["items"].as_array().expect("items array");
        assert_eq!(
            items[0]["tool_call_intent"],
            "namespace cleanup is the fastest path to unblock"
        );
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
        use super::super::registry::PendingApprovals;
        use std::time::Duration;

        let registry = PendingApprovals::new();
        let route = DecisionRoute::Conversational {
            registry: registry.clone(),
            timeout: Duration::from_secs(60),
        };
        let decision_id = DecisionId::generate();
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id,
            request_id: "conv-req-1".into(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::AgentRequested {
                reason: "test".into(),
            },
            items: vec![],
        };
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
        use super::super::registry::PendingApprovals;
        use std::time::Duration;

        let registry = PendingApprovals::new();
        let route = DecisionRoute::Conversational {
            registry: registry.clone(),
            timeout: Duration::from_secs(60),
        };
        let decision_id = DecisionId::generate();
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id,
            request_id: "conv-req-2".into(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "rm_*".into(),
            },
            items: vec![],
        };
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
        use super::super::registry::{PendingApprovals, ResolveError};
        use std::time::Duration;

        let registry = PendingApprovals::new();
        let route = DecisionRoute::Conversational {
            registry: registry.clone(),
            timeout: Duration::from_secs(5),
        };
        let decision_id = DecisionId::generate();
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id,
            request_id: "conv-req-3".into(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::AgentRequested {
                reason: "test".into(),
            },
            items: vec![],
        };
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
        use super::super::registry::PendingApprovals;
        use std::time::Duration;

        let registry = PendingApprovals::new();
        let route = DecisionRoute::Conversational {
            registry,
            timeout: Duration::from_secs(60),
        };
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: "conv-req-4".into(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::AgentRequested {
                reason: "test".into(),
            },
            items: vec![],
        };
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
        use super::super::decision::ApprovalDecision;
        use super::super::registry::PendingApprovals;

        let request_id = format!("req_test_{}", uuid::Uuid::new_v4().simple());
        let mut rx = crate::approval_event_broker::subscribe(&request_id).await;

        let registry = PendingApprovals::new();
        let route = DecisionRoute::Conversational {
            registry: registry.clone(),
            timeout: std::time::Duration::from_secs(60),
        };
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: request_id.clone(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "dangerous_*".into(),
            },
            items: vec![],
        };
        let decision_id = request.decision_id;

        let cancel = crate::request_cancellation::RequestCancelToken::unbound();
        let decide_handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { route.decide(request, &cancel).await }
        });

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
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
        use std::collections::HashMap;
        use std::time::Duration;

        use bytes::Bytes;
        use reqwest::header::HeaderMap;
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
                headers: HeaderMap::new(),
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
            let client = WebhookClient::with_headers_and_signing(
                build_webhook_client(),
                aura_config::WebhookUrl::new("http://approvals.example.com/aura").unwrap(),
                HeaderMap::new(),
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
                    headers: HashMap::new(),
                    headers_from_request: HashMap::new(),
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
                    headers: HashMap::new(),
                    headers_from_request: HashMap::new(),
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
                None,
            );
            assert!(
                matches!(signing_of(&signed), EgressSigning::Enabled(_)),
                "a threaded secret must produce signed egress"
            );

            let unsigned = HitlRuntime::from_config(
                &config("http://approvals.example.com/aura"),
                &pending,
                None,
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
                None,
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

            let client = WebhookClient::with_headers_and_signing(
                build_webhook_client(),
                aura_config::WebhookUrl::new(&url).unwrap(),
                HeaderMap::new(),
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
                tool_call_intent: None,
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

    // -----------------------------------------------------------------
    // resolve_webhook_headers unit tests
    // -----------------------------------------------------------------

    fn make_req_headers(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn webhook_headers_static_only() {
        let static_headers = make_req_headers(&[("x-tenant", "sre-prod")]);
        let headers_from_request = std::collections::HashMap::new();
        let header_map =
            super::resolve_webhook_headers(&static_headers, &headers_from_request, None);
        assert_eq!(header_map.get("x-tenant").unwrap(), "sre-prod");
        assert_eq!(header_map.len(), 1);
    }

    #[test]
    fn webhook_headers_mapped_only() {
        let static_headers = std::collections::HashMap::new();
        let headers_from_request = make_req_headers(&[("x-tenant", "x-incoming-tenant")]);
        let req_headers = make_req_headers(&[("x-incoming-tenant", "forwarded-value")]);

        let header_map = super::resolve_webhook_headers(
            &static_headers,
            &headers_from_request,
            Some(&req_headers),
        );
        assert_eq!(header_map.get("x-tenant").unwrap(), "forwarded-value");
        assert_eq!(header_map.len(), 1);
    }

    #[test]
    fn webhook_headers_fallback_precedence() {
        // When the mapped request header IS present, it overrides the static value.
        let static_headers = make_req_headers(&[("authorization", "static-token")]);
        let headers_from_request = make_req_headers(&[("authorization", "x-incoming-auth")]);
        let req_headers = make_req_headers(&[("x-incoming-auth", "dynamic-token")]);

        let header_map = super::resolve_webhook_headers(
            &static_headers,
            &headers_from_request,
            Some(&req_headers),
        );
        assert_eq!(
            header_map.get("authorization").unwrap(),
            "dynamic-token",
            "request header should override static"
        );

        // When the mapped request header is ABSENT, the static value is the fallback.
        let req_headers_empty = std::collections::HashMap::new();
        let header_map_fallback = super::resolve_webhook_headers(
            &static_headers,
            &headers_from_request,
            Some(&req_headers_empty),
        );
        assert_eq!(
            header_map_fallback.get("authorization").unwrap(),
            "static-token",
            "static header should be used when request header is absent"
        );
    }

    #[test]
    fn webhook_headers_case_insensitive_lookup() {
        // TOML config uses "Authorization" (capitalized) but the inbound
        // request header arrives lowercased. The lookup must be case-insensitive.
        let static_headers = std::collections::HashMap::new();
        let headers_from_request = make_req_headers(&[("Authorization", "Authorization")]);
        let req_headers = make_req_headers(&[("authorization", "Token my-token")]);

        let header_map = super::resolve_webhook_headers(
            &static_headers,
            &headers_from_request,
            Some(&req_headers),
        );
        assert_eq!(
            header_map.get("authorization").unwrap(),
            "Token my-token",
            "case-insensitive lookup should resolve lowercased request header"
        );
    }

    #[test]
    fn webhook_headers_mixed_casing_static_and_dynamic_keys() {
        // Static headers use "Authorization" (capitalized) while the
        // headers_from_request outbound key uses "authorization" (lowercase).
        // Both refer to the same HTTP header; the dynamic value must override
        // the static fallback deterministically, not non-deterministically
        // based on HashMap iteration order.
        let static_headers = make_req_headers(&[("Authorization", "static-token")]);
        let headers_from_request = make_req_headers(&[("authorization", "x-incoming-auth")]);
        let req_headers = make_req_headers(&[("x-incoming-auth", "dynamic-token")]);

        let header_map = super::resolve_webhook_headers(
            &static_headers,
            &headers_from_request,
            Some(&req_headers),
        );
        assert_eq!(
            header_map.get("authorization").unwrap(),
            "dynamic-token",
            "dynamic header must override static fallback regardless of key casing"
        );
    }

    #[test]
    fn webhook_headers_invalid_skipped() {
        // Header name with a space is invalid per RFC 7230; the entry must
        // be skipped with a warning, not panic.
        let static_headers = make_req_headers(&[("invalid header!", "value"), ("x-valid", "ok")]);
        let headers_from_request = std::collections::HashMap::new();

        let header_map =
            super::resolve_webhook_headers(&static_headers, &headers_from_request, None);
        assert_eq!(header_map.get("x-valid").unwrap(), "ok");
        assert_eq!(
            header_map.len(),
            1,
            "invalid header must be skipped, valid one must remain"
        );
    }

    /// Minimal tracing-capture facility: a shared buffer that implements
    /// `io::Write` by reference, so `Arc<CapturedLog>` satisfies
    /// `tracing_subscriber`'s `MakeWriter` (`impl MakeWriter for Arc<W>` when
    /// `&W: Write`). The workspace has no existing fmt-log-text capture helper,
    /// so this is the minimal in-test subscriber.
    struct CapturedLog(std::sync::Mutex<Vec<u8>>);

    impl std::io::Write for &CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn webhook_headers_invalid_value_skipped_with_warning_and_no_leak() {
        // A header VALUE with forbidden control characters must be skipped,
        // a warning must be emitted, and the value must NOT appear in the
        // captured log (the header name may).
        use std::sync::Arc;

        let buf = Arc::new(CapturedLog(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();

        // A valid header name paired with a value containing CR/LF (forbidden
        // in field values per RFC 7230) alongside a valid entry that must
        // survive. HashMap iteration order is nondeterministic, but the invalid
        // entry warns exactly once regardless of order.
        let static_headers =
            make_req_headers(&[("x-bad", "unique-secret\r\ninvalid"), ("x-valid", "ok")]);
        let headers_from_request = std::collections::HashMap::new();

        let header_map = tracing::subscriber::with_default(subscriber, || {
            super::resolve_webhook_headers(&static_headers, &headers_from_request, None)
        });
        let log = String::from_utf8_lossy(&buf.0.lock().unwrap()).to_string();

        // (a) the invalid entry is skipped; the valid one remains.
        assert_eq!(header_map.get("x-valid").unwrap(), "ok");
        assert!(
            header_map.get("x-bad").is_none(),
            "invalid-value header must be skipped"
        );
        assert_eq!(header_map.len(), 1);

        // (b) the warning is emitted and names the offending header.
        assert!(
            log.contains("Skipping invalid webhook header"),
            "warning must be emitted, got log: {log}"
        );
        assert!(
            log.contains("x-bad"),
            "warning must name the skipped header, got log: {log}"
        );

        // (c) the forbidden value must not leak into the log. The secret
        // substring survives debug escaping, so its absence proves the value
        // itself was not logged — not just that the exact raw byte sequence
        // was absent.
        assert!(
            !log.contains("unique-secret"),
            "secret substring from the invalid header value must not appear in the log, got: {log:?}"
        );
        assert!(
            !log.contains("unique-secret\r\ninvalid"),
            "invalid header value must not appear in the log, got: {log:?}"
        );
    }

    #[test]
    fn webhook_headers_empty_config_produces_bare_post() {
        // Empty config (no static, no from_request, no req_headers) must
        // produce an empty HeaderMap, so the header loop in request_approval
        // adds nothing to the POST.
        let static_headers = std::collections::HashMap::new();
        let headers_from_request = std::collections::HashMap::new();

        let header_map =
            super::resolve_webhook_headers(&static_headers, &headers_from_request, None);
        assert!(
            header_map.is_empty(),
            "empty config must produce no headers (bare POST)"
        );
    }

    // -----------------------------------------------------------------
    // Integration tests: mock webhook asserts headers arrive on the POST
    // -----------------------------------------------------------------

    /// Spawn a mock webhook on a random port that captures the COMPLETE raw
    /// HTTP request (request line, headers, and body) for each of `count`
    /// sequential connections, responding to each with `{"approved": true}`.
    /// Returns the port and an mpsc receiver yielding one captured request
    /// text per connection, in connection order.
    ///
    /// Capturing the full request (not just the header section) lets callers
    /// assert byte-for-byte request equivalence between two client
    /// constructors run sequentially against the SAME server — sequential
    /// captures avoid Host/port drift between two separately-bound servers.
    async fn spawn_capturing_webhook(count: usize) -> (u16, tokio::sync::mpsc::Receiver<String>) {
        use tokio::io::AsyncWriteExt;
        use tokio::sync::mpsc;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel::<String>(count);

        tokio::spawn(async move {
            for _ in 0..count {
                let (mut socket, _) = listener.accept().await.unwrap();
                let captured = read_full_request(&mut socket).await;
                let body = "{\"approved\": true}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = tx.send(captured).await;
            }
        });

        (port, rx)
    }

    /// Read one complete HTTP request (request line + headers + the
    /// Content-Length-declared body) from `socket` and return it as a
    /// lossy-UTF-8 string. Reading the body before responding prevents reqwest
    /// from seeing a connection reset while it is still sending the POST body.
    async fn read_full_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until we have the complete header section (\r\n\r\n).
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        // Parse Content-Length so we can consume the request body before
        // responding — otherwise reqwest may get a connection reset while
        // still sending the POST body.
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
            socket.read_exact(&mut body_buf).await.unwrap();
            buf.extend_from_slice(&body_buf);
        }

        String::from_utf8_lossy(&buf).to_string()
    }

    fn make_approval_request() -> ApprovalRequest {
        ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: "hdr-test".into(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "dangerous_*".into(),
            },
            items: vec![ApprovalItem {
                tool_name: "dangerous_apply".into(),
                arguments: serde_json::json!({}),
                tool_call_intent: None,
            }],
        }
    }

    #[tokio::test]
    async fn webhook_forwards_configured_headers_to_mock_server() {
        let (port, mut rx) = spawn_capturing_webhook(1).await;

        // Build a webhook client with a static header.
        let mut header_map = reqwest::header::HeaderMap::new();
        header_map.insert(
            "x-tenant",
            reqwest::header::HeaderValue::from_static("sre-prod"),
        );
        let client = super::WebhookClient::new_with_headers(
            super::build_webhook_client(),
            aura_config::WebhookUrl::new(format!("http://127.0.0.1:{port}")).unwrap(),
            header_map,
        );

        let route = super::DecisionRoute::Webhook {
            client,
            timeout: std::time::Duration::from_secs(5),
        };
        let cancel = crate::request_cancellation::RequestCancelToken::unbound();
        let result = route.decide(make_approval_request(), &cancel).await;
        assert!(result.is_ok(), "mock webhook should return a decision");

        let captured = rx.recv().await.expect("mock webhook capture");
        assert!(
            captured.contains("x-tenant: sre-prod"),
            "configured header must appear in the webhook POST, got: {captured}"
        );
    }

    #[tokio::test]
    async fn webhook_empty_config_sends_no_custom_headers() {
        let (port, mut rx) = spawn_capturing_webhook(1).await;

        // Empty HeaderMap: the POST carries no custom headers.
        let client = super::WebhookClient::new_with_headers(
            super::build_webhook_client(),
            aura_config::WebhookUrl::new(format!("http://127.0.0.1:{port}")).unwrap(),
            reqwest::header::HeaderMap::new(),
        );

        let route = super::DecisionRoute::Webhook {
            client,
            timeout: std::time::Duration::from_secs(5),
        };
        let cancel = crate::request_cancellation::RequestCancelToken::unbound();
        let result = route.decide(make_approval_request(), &cancel).await;
        assert!(result.is_ok(), "mock webhook should return a decision");

        let captured = rx.recv().await.expect("mock webhook capture");
        assert!(
            !captured.contains("x-tenant"),
            "no custom headers should appear with empty config, got: {captured}"
        );
    }

    #[tokio::test]
    async fn webhook_empty_headers_match_bare_client_byte_for_byte() {
        // Empty configuration must produce a request byte-for-byte identical
        // to the bare constructor (`WebhookClient::new`). Here we
        // capture the COMPLETE raw request (headers AND body) from both
        // constructors run sequentially against the SAME mock server (so Host
        // and port are identical) and assert the captured request texts are
        // equal.
        //
        // A single request is cloned for both calls so every wire field
        // (notably decision_id) is identical — the body must not differ
        // between the two captures. Each capture is checked for the body's
        // decision_id before comparing, so a dropped body can never pass
        // silently.
        let (port, mut rx) = spawn_capturing_webhook(2).await;
        let url = aura_config::WebhookUrl::new(format!("http://127.0.0.1:{port}")).unwrap();
        let cancel = crate::request_cancellation::RequestCancelToken::unbound();
        let request = make_approval_request();
        // The decision_id appears only in the JSON body, not in the request
        // line or headers. Checking each capture for it before comparing
        // guarantees the body was actually captured.
        let decision_id = request.decision_id.to_string();

        // Capture 1: the bare constructor (no headers configured).
        let bare = super::DecisionRoute::Webhook {
            client: super::WebhookClient::new(super::build_webhook_client(), url.clone()),
            timeout: std::time::Duration::from_secs(5),
        };
        let result = bare.decide(request.clone(), &cancel).await;
        assert!(result.is_ok(), "bare client should get a decision");

        // Capture 2: the headers constructor with an empty `HeaderMap` — the
        // documented "empty config" path. Same server, sequential, so the
        // Host header and port do not drift between captures.
        let with_empty = super::DecisionRoute::Webhook {
            client: super::WebhookClient::new_with_headers(
                super::build_webhook_client(),
                url,
                reqwest::header::HeaderMap::new(),
            ),
            timeout: std::time::Duration::from_secs(5),
        };
        let result = with_empty.decide(request, &cancel).await;
        assert!(result.is_ok(), "empty-headers client should get a decision");

        let captured_bare = rx.recv().await.expect("first capture");
        let captured_empty = rx.recv().await.expect("second capture");

        // Each capture must contain the body's decision_id before we compare
        // the two — otherwise a dropped body could produce two identical
        // header-only captures and pass silently.
        assert!(
            captured_bare.contains(&decision_id),
            "first capture must contain the request body's decision_id ({decision_id}), got: {captured_bare}"
        );
        assert!(
            captured_empty.contains(&decision_id),
            "second capture must contain the request body's decision_id ({decision_id}), got: {captured_empty}"
        );

        assert_eq!(
            captured_bare, captured_empty,
            "WebhookClient::new and new_with_headers(HeaderMap::new()) must produce \
             byte-for-byte identical requests\n--- bare ---\n{captured_bare}\n--- empty ---\n{captured_empty}"
        );
    }
}
