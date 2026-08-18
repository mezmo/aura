//! The approval decision route: how a gated call gets its decision.
//!
//! A closed two-variant enum chosen by the `[hitl.route]` config table. Replaces
//! the spike's `ApprovalDispatch` trait: the variant set is known, and
//! [`DecisionRoute::decide`] holds the shared semantics (deadline, fail-closed
//! mapping, event emission) in one place instead of per-impl.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_config::{DecisionRouteConfig, GlobPattern, HitlConfig, ToolHeaderMappings, WebhookUrl};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::decision::{ApprovalDecision, ApprovalOutcome, DecisionId};
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
                tool_headers_from_response,
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
                        tool_headers_from_response.clone(),
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
    /// Approver identity capture failed closed: the approved response was
    /// missing mapped headers. Names only, never values; the message is
    /// the event-level audit signal. Wraps only the capture kind, so
    /// application-time failures cannot be mislabeled as capture failures.
    #[error("{0}")]
    CaptureFailed(#[from] crate::approver_headers::CaptureError),
}

/// A decision as seen by the config gate.
///
/// An enum, not a product type: only the [`GateDecision::Approved`] variant
/// can carry approver header overrides, so a denied, timed-out, or
/// cancelled decision holding credentials is unrepresentable. Only
/// [`DecisionRoute::decide_for_gate`] produces it, so the route-wide
/// [`ApprovalOutcome`] consumed by the `request_approval` tool stays
/// unit-shaped and that surface never holds approver credentials.
#[derive(Debug)]
pub enum GateDecision {
    /// Approved; the only variant that may carry captured overrides.
    Approved {
        overrides: Option<crate::approver_headers::ApproverHeaders>,
    },
    Denied {
        reason: Option<String>,
    },
    TimedOut {
        waited: Duration,
    },
    Cancelled(super::decision::CancelReason),
}

impl GateDecision {
    /// Total mapping from a route outcome, carrying no overrides: the shape
    /// every conversational decision and every webhook decision without
    /// captured identity produces.
    pub(crate) fn without_overrides(outcome: ApprovalOutcome) -> Self {
        match outcome {
            ApprovalOutcome::Decided(ApprovalDecision::Approved) => {
                Self::Approved { overrides: None }
            }
            ApprovalOutcome::Decided(ApprovalDecision::Denied { reason }) => {
                Self::Denied { reason }
            }
            ApprovalOutcome::TimedOut { waited } => Self::TimedOut { waited },
            ApprovalOutcome::Cancelled(reason) => Self::Cancelled(reason),
        }
    }

    /// Event-projection of the decision. Identity material never enters
    /// events; a projection method, unlike a product type, cannot
    /// construct a denied-with-overrides state.
    fn to_outcome(&self) -> ApprovalOutcome {
        match self {
            Self::Approved { .. } => ApprovalOutcome::Decided(ApprovalDecision::Approved),
            Self::Denied { reason } => ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: reason.clone(),
            }),
            Self::TimedOut { waited } => ApprovalOutcome::TimedOut { waited: *waited },
            Self::Cancelled(reason) => ApprovalOutcome::Cancelled(*reason),
        }
    }
}

/// Where an approval decision comes from. Fixed per deployment by config.
#[expect(
    clippy::large_enum_variant,
    reason = "built once per request and held behind Arc, never copied; \
              boxing the webhook client would churn every construction site \
              for no runtime benefit"
)]
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

/// Stamp `decision_id` on the current tracing span.
///
/// `set_span_attribute` appends rather than overwrites by key, so one span
/// stamped twice would carry a duplicate attribute entry. The entry points
/// stamp once each and delegate through the unstamped
/// [`DecisionRoute::decide_inner`].
fn stamp_decision_id(decision_id: DecisionId) {
    crate::logging::set_span_attribute(
        &tracing::Span::current(),
        crate::logging::ATTR_DECISION_ID,
        decision_id.to_string(),
    );
}

/// Run one webhook approval round trip behind the choreography both webhook
/// arms share: publish `Requested`, race the round trip against the cancel
/// token, then publish `Completed`.
///
/// Two phases, both failing closed: the race is `biased` so a pending
/// cancellation wins, and the result is rechecked against the token before
/// it becomes the decision, so a disconnect landing just as the decision
/// arrives is caught too. The arms differ only in the round trip itself and
/// in how their decision shape projects into the completed event.
async fn webhook_round_trip<T>(
    request: &ApprovalRequest,
    cancel: &crate::request_cancellation::RequestCancelToken,
    round_trip: impl Future<Output = Result<T, ApprovalError>>,
    cancelled: impl FnOnce() -> T,
    event_outcome: impl FnOnce(&T) -> ApprovalOutcome,
) -> Result<T, ApprovalError> {
    let started = Instant::now();
    let request_id = request.request_id.clone();
    let decision_id = request.decision_id;
    let scope = request.scope.clone();

    approval_event_broker::publish(
        &request_id,
        ApprovalLifecycleEvent::Requested(request.into()),
    )
    .await;

    let raced = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        decision = round_trip => Some(decision),
    };
    let result = match raced {
        Some(decision) if !cancel.is_cancelled() => decision,
        _ => {
            tracing::warn!(%decision_id, "approval cancelled: client disconnected");
            Ok(cancelled())
        }
    };
    let completed = match &result {
        Ok(decision) => events::completed(
            decision_id,
            &event_outcome(decision),
            &scope,
            started.elapsed(),
        ),
        Err(err) => {
            events::completed_error(decision_id, err.to_string(), &scope, started.elapsed())
        }
    };
    approval_event_broker::publish(&request_id, ApprovalLifecycleEvent::Completed(completed)).await;
    result
}

impl DecisionRoute {
    /// Obtain a decision for a config-gated call, carrying any captured
    /// approver header overrides on the approved arm.
    ///
    /// The webhook arm goes through the capture-bearing client seam
    /// ([`WebhookClient::request_approval_for_gate`]), the only path that
    /// reads approver identity off an approval response. The conversational
    /// arm delegates to the unstamped [`Self::decide_inner`]: that route has
    /// no distinct identity source and never captures.
    pub async fn decide_for_gate(
        &self,
        request: ApprovalRequest,
        cancel: &crate::request_cancellation::RequestCancelToken,
    ) -> Result<GateDecision, ApprovalError> {
        stamp_decision_id(request.decision_id);
        match self {
            Self::Conversational { .. } => {
                let outcome = self.decide_inner(request, cancel).await?;
                Ok(GateDecision::without_overrides(outcome))
            }
            Self::Webhook { client, timeout } => {
                webhook_round_trip(
                    &request,
                    cancel,
                    client.request_approval_for_gate(&request, *timeout),
                    || GateDecision::Cancelled(super::decision::CancelReason::ClientDisconnected),
                    GateDecision::to_outcome,
                )
                .await
            }
        }
    }

    /// Obtain a decision for `request`, stamping the decision id on the
    /// current span before delegating to [`Self::decide_inner`].
    pub async fn decide(
        &self,
        request: ApprovalRequest,
        cancel: &crate::request_cancellation::RequestCancelToken,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        stamp_decision_id(request.decision_id);
        self.decide_inner(request, cancel).await
    }

    /// Obtain a decision for `request`, applying the shared semantics
    /// (deadline, fail-closed mapping, event emission) in one place.
    ///
    /// Does not stamp: both callers stamp on their own entry.
    async fn decide_inner(
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

                let mut outcome = handle.outcome(cancel).await;
                if matches!(
                    outcome,
                    ApprovalOutcome::TimedOut { .. } | ApprovalOutcome::Cancelled(_)
                ) {
                    registry.remove(&decision_id).await;
                }
                // The deadline backstop consults the store before failing
                // closed: a decision durably recorded but whose wake was lost
                // (down to the final poll gap) still takes effect.
                // Cancellation deliberately does not — a disconnected request
                // must not execute a buffered approval.
                if matches!(outcome, ApprovalOutcome::TimedOut { .. })
                    && let Some(decision) = registry.recorded_decision(&decision_id).await
                {
                    outcome = ApprovalOutcome::Decided(decision);
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
                webhook_round_trip(
                    &request,
                    cancel,
                    client.request_approval(&request, *timeout),
                    || {
                        ApprovalOutcome::Cancelled(
                            super::decision::CancelReason::ClientDisconnected,
                        )
                    },
                    |outcome| outcome.clone(),
                )
                .await
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

/// HTTP client for the webhook route.
pub struct WebhookClient {
    client: reqwest::Client,
    url: WebhookUrl,
    /// Resolved webhook headers.
    headers: HeaderMap,
    signing: EgressSigning,
    /// Validated `tool_headers_from_response` mappings.
    tool_header_mappings: ToolHeaderMappings,
}

/// One webhook round trip's answer, with the HTTP response headers it
/// arrived alongside.
enum WebhookReply {
    Decided {
        decision: ApprovalDecision,
        response_headers: HeaderMap,
    },
    TimedOut {
        waited: Duration,
    },
}

impl WebhookReply {
    /// Project to the route-wide outcome, dropping the response headers.
    fn into_outcome(self) -> ApprovalOutcome {
        match self {
            Self::Decided { decision, .. } => ApprovalOutcome::Decided(decision),
            Self::TimedOut { waited } => ApprovalOutcome::TimedOut { waited },
        }
    }
}

impl WebhookClient {
    /// Builds an unsigned client. Deliberately does NOT read the environment
    /// (so constructing one in a test never races env-mutating tests):
    /// production construction goes through [`HitlRuntime::from_config`],
    /// which receives the startup-loaded HMAC and calls `with_signing`.
    #[must_use]
    pub fn new(client: reqwest::Client, url: WebhookUrl) -> Self {
        Self::with_headers_and_signing(
            client,
            url,
            HeaderMap::new(),
            EgressSigning::Disabled,
            ToolHeaderMappings::default(),
        )
    }

    /// Create a webhook client with resolved operator-configured headers
    /// applied to every approval POST.
    #[must_use]
    pub fn new_with_headers(client: reqwest::Client, url: WebhookUrl, headers: HeaderMap) -> Self {
        Self::with_headers_and_signing(
            client,
            url,
            headers,
            EgressSigning::Disabled,
            ToolHeaderMappings::default(),
        )
    }

    fn with_headers_and_signing(
        client: reqwest::Client,
        url: WebhookUrl,
        headers: HeaderMap,
        signing: EgressSigning,
        tool_header_mappings: ToolHeaderMappings,
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
        // The cleartext-capture warning lives at the boot-time seam, not
        // here: see [`warn_on_cleartext_capture`].
        Self {
            client,
            url,
            headers,
            signing,
            tool_header_mappings,
        }
    }

    /// Gate-scoped approval request: the config-gate path through the
    /// webhook, capturing approver identity from the reply's response
    /// headers on the approved arm only.
    pub(crate) async fn request_approval_for_gate(
        &self,
        request: &ApprovalRequest,
        timeout: Duration,
    ) -> Result<GateDecision, ApprovalError> {
        match self.request_approval_with_headers(request, timeout).await? {
            WebhookReply::Decided {
                decision: ApprovalDecision::Approved,
                response_headers,
            } => {
                let overrides = if self.tool_header_mappings.is_empty() {
                    None
                } else {
                    Some(crate::approver_headers::ApproverHeaders::from_captured(
                        &self.tool_header_mappings,
                        &response_headers,
                    )?)
                };
                Ok(GateDecision::Approved { overrides })
            }
            other => Ok(GateDecision::without_overrides(other.into_outcome())),
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

    /// Resolve a decision without its response headers: the route-wide path,
    /// which never captures approver identity.
    async fn request_approval(
        &self,
        request: &ApprovalRequest,
        timeout: Duration,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        Ok(self
            .request_approval_with_headers(request, timeout)
            .await?
            .into_outcome())
    }

    /// POST the request and resolve a decision, failing closed on timeout or
    /// transport/parse error. With a secret configured the POST carries the
    /// `X-Aura-*` signature headers (context `approval-request:{decision_id}`)
    /// and the HTTP response must verify under
    /// `approval-decision:{decision_id}` before its body is parsed.
    ///
    /// The reply carries the response headers, cloned off the response
    /// before its body is consumed. On the signed path they reach a caller
    /// only inside a [`WebhookReply::Decided`], which is built after
    /// verification and parse both succeed.
    async fn request_approval_with_headers(
        &self,
        request: &ApprovalRequest,
        timeout: Duration,
    ) -> Result<WebhookReply, ApprovalError> {
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
            Err(e) if e.is_timeout() => Ok(WebhookReply::TimedOut { waited: timeout }),
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
                // Cloned here because `bytes()` consumes the response.
                let response_headers = resp.headers().clone();
                let body = match resp.bytes().await {
                    Ok(body) => body,
                    // A timeout firing mid-body download is still a timeout, not
                    // a transport fault — keep the classification honest.
                    Err(e) if e.is_timeout() => {
                        return Ok(WebhookReply::TimedOut { waited: timeout });
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
                    Ok(wire) => Ok(WebhookReply::Decided {
                        decision: ApprovalDecision::from(wire),
                        response_headers,
                    }),
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
    ) -> Result<WebhookReply, ApprovalError> {
        let builder = self
            .apply_operator_headers(self.client.post(self.url.as_str()).json(wire))
            .timeout(timeout);
        match builder.send().await {
            Err(e) if e.is_timeout() => Ok(WebhookReply::TimedOut { waited: timeout }),
            Err(e) => Err(ApprovalError::Transport(e.to_string())),
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    return Err(ApprovalError::BadStatus {
                        status: status.as_u16(),
                    });
                }
                // Cloned here because `json()` consumes the response.
                let response_headers = resp.headers().clone();
                match resp.json::<ApprovalDecisionWire>().await {
                    Ok(wire) => Ok(WebhookReply::Decided {
                        decision: ApprovalDecision::from(wire),
                        response_headers,
                    }),
                    // A timeout firing mid-body download is still a timeout, not a
                    // parse fault — keep the error-vs-decision classification honest.
                    Err(e) if e.is_timeout() => Ok(WebhookReply::TimedOut { waited: timeout }),
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

/// Boot-time warning for a webhook route that captures approver response
/// headers over plain `http://`. Call this once per `[hitl]` config at
/// startup, alongside `validate_webhook_signing_config`. Do not call from
/// [`WebhookClient`] construction: that runs fresh per request via
/// `HitlRuntime::from_config` and would turn one misconfiguration into a
/// warning per chat request.
pub fn warn_on_cleartext_capture(config: &HitlConfig) {
    let DecisionRouteConfig::Webhook {
        url,
        tool_headers_from_response,
        ..
    } = &config.route
    else {
        return;
    };
    if tool_headers_from_response.is_empty() || url.as_str().starts_with("https://") {
        return;
    }
    tracing::warn!(
        origin = %redact_to_origin(url.as_str()),
        "HITL webhook route captures approver response headers over cleartext http, so \
         this route's tool_headers_from_response values are readable by any network \
         observer; intended for trusted-gateway or service-to-service deployments"
    );
}

/// `scheme://host[:port]` of `url`, dropping userinfo, path, query, and
/// fragment — the parts of a webhook URL a log line must never carry,
/// since a webhook URL may embed a token in any of them.
fn redact_to_origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if scheme.is_empty() {
        host_port.to_string()
    } else {
        format!("{scheme}://{host_port}")
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

    /// A decision durably recorded whose wake never arrives still takes
    /// effect at the deadline backstop instead of failing closed. The route
    /// timeout sits below the wake task's poll interval, so only the
    /// backstop can observe the recorded decision.
    #[tokio::test(start_paused = true)]
    async fn conversational_timeout_backstop_recovers_recorded_decision() {
        use crate::session_store::{
            ApprovalStore, EventBus, InMemoryApprovalStore, InMemoryEventBus, SessionStoreError,
            Subscription,
        };
        use std::sync::Arc;

        /// Bus double whose `publish` drops every payload.
        struct LossyBus(InMemoryEventBus);

        #[async_trait::async_trait]
        impl EventBus for LossyBus {
            async fn publish(
                &self,
                _topic: &str,
                _payload: bytes::Bytes,
            ) -> Result<(), SessionStoreError> {
                Ok(())
            }

            async fn subscribe(&self, topic: &str) -> Result<Subscription, SessionStoreError> {
                self.0.subscribe(topic).await
            }
        }

        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let bus: Arc<dyn EventBus> = Arc::new(LossyBus(InMemoryEventBus::new()));
        let registry = PendingApprovals::with_backend(store, bus);
        let route = DecisionRoute::Conversational {
            registry: registry.clone(),
            timeout: Duration::from_secs(2),
        };
        let request = single_request(
            "conv-req-backstop",
            ApprovalOrigin::AgentRequested {
                reason: "test".into(),
            },
        );
        let decision_id = request.decision_id;
        let cancel = crate::request_cancellation::RequestCancelToken::unbound();

        let decide_handle = tokio::spawn({
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

        let result = decide_handle.await.unwrap().unwrap();
        assert_eq!(result, ApprovalOutcome::Decided(ApprovalDecision::Approved));
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
        use std::collections::HashMap;
        use std::time::Duration;

        use bytes::Bytes;
        use reqwest::header::HeaderMap;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use super::super::super::decision::{
            AgentScope, ApprovalDecision, ApprovalOrigin, CancelReason, DecisionId,
        };
        use super::super::super::protocol::{ApprovalRequest, PROTOCOL_VERSION};
        use super::super::super::signing::{
            PrimarySecret, SIGNATURE_HEADER, SigningContext, TIMESTAMP_HEADER, Tolerance,
            WebhookHmac, authorize_ingress,
        };
        use super::super::{
            ApprovalError, ApprovalOutcome, EgressSigning, GateDecision, WebhookClient,
            build_webhook_client,
        };
        use crate::approver_headers::CaptureError;

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
            one_shot_receiver_with_status("200 OK", response_headers, response_body).await
        }

        /// [`one_shot_receiver`] with the response status line chosen by the
        /// caller, for the non-2xx path.
        async fn one_shot_receiver_with_status(
            status: &'static str,
            response_headers: Vec<(String, String)>,
            response_body: String,
        ) -> (String, tokio::sync::oneshot::Receiver<ReceivedRequest>) {
            one_shot_receiver_cancelling_after_read(status, response_headers, response_body, None)
                .await
        }

        /// [`one_shot_receiver`] that fires `cancel_after_read` once it has
        /// the whole request and before it writes a byte of the response, so
        /// the caller meets a cancelled token and an arriving approval at the
        /// same time.
        async fn cancelling_receiver(
            cancel: crate::request_cancellation::RequestCancelToken,
            response_headers: Vec<(String, String)>,
            response_body: String,
        ) -> (String, tokio::sync::oneshot::Receiver<ReceivedRequest>) {
            one_shot_receiver_cancelling_after_read(
                "200 OK",
                response_headers,
                response_body,
                Some(cancel),
            )
            .await
        }

        async fn one_shot_receiver_cancelling_after_read(
            status: &'static str,
            response_headers: Vec<(String, String)>,
            response_body: String,
            cancel_after_read: Option<crate::request_cancellation::RequestCancelToken>,
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

                if let Some(cancel) = cancel_after_read {
                    cancel.cancel();
                }

                let mut response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n",
                    response_body.len()
                );
                for (name, value) in &response_headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                response.push_str(&response_body);
                // A cancelling caller may already have dropped the connection,
                // so a failed reply is not a test failure; every test that
                // cares asserts on what the client resolved to.
                socket.write_all(response.as_bytes()).await.ok();
                socket.shutdown().await.ok();
                tx.send(ReceivedRequest { headers, body }).ok();
            });
            (url, rx)
        }

        /// A receiver that accepts one connection, signals over the returned
        /// channel that it has, and then never answers — so the caller's own
        /// timeout or cancellation is what resolves the round trip. Awaiting
        /// the signal before acting proves the round trip is in flight. The
        /// spawned task holds the accepted socket open for the life of the
        /// test; closing it would surface as a transport error instead.
        async fn stalled_receiver() -> (String, tokio::sync::oneshot::Receiver<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _socket = listener.accept().await.unwrap();
                accepted_tx.send(()).ok();
                std::future::pending::<()>().await;
            });
            (url, accepted_rx)
        }

        /// Builds a client directly against a loopback `http://` receiver.
        /// Bypasses the https-only policy deliberately: the policy is
        /// exercised by `http_url_with_secret_fails_closed`.
        fn loopback_client(
            url: &str,
            signing: EgressSigning,
            tool_header_mappings: aura_config::ToolHeaderMappings,
        ) -> WebhookClient {
            WebhookClient {
                client: build_webhook_client(),
                url: aura_config::WebhookUrl::new(url).unwrap(),
                headers: HeaderMap::new(),
                signing,
                tool_header_mappings,
            }
        }

        fn loopback_signed_client(url: &str, hmac: WebhookHmac) -> WebhookClient {
            loopback_client(
                url,
                EgressSigning::Enabled(hmac),
                aura_config::ToolHeaderMappings::default(),
            )
        }

        /// The one mapping every gate test below configures.
        fn user_mapping() -> aura_config::ToolHeaderMappings {
            crate::approver_headers::tests::mappings(&[("x-forwarded-user", "x-approver-id")])
        }

        /// Sign `body` under the approval-decision context for `decision_id`,
        /// as a webhook answering that decision must.
        fn signed_response_headers(
            hmac: &WebhookHmac,
            decision_id: DecisionId,
            body: &str,
        ) -> Vec<(String, String)> {
            let context = SigningContext::new(&format!("approval-decision:{decision_id}")).unwrap();
            hmac.sign(&context, body.as_bytes())
                .unwrap()
                .into_pairs()
                .map(|(name, value)| (name.to_string(), value))
                .to_vec()
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
                aura_config::ToolHeaderMappings::default(),
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

            let webhook =
                |url: &str| webhook_config(url, aura_config::ToolHeaderMappings::default());
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

        fn webhook_config(
            url: &str,
            tool_headers_from_response: aura_config::ToolHeaderMappings,
        ) -> aura_config::HitlConfig {
            aura_config::HitlConfig {
                require_approval: vec![],
                route: aura_config::DecisionRouteConfig::Webhook {
                    url: aura_config::WebhookUrl::new(url).unwrap(),
                    timeout_secs: 300,
                    headers: HashMap::new(),
                    headers_from_request: HashMap::new(),
                    tool_headers_from_response,
                },
            }
        }

        fn signing_of(runtime: &super::super::HitlRuntime) -> &EgressSigning {
            match &*runtime.route {
                super::super::DecisionRoute::Webhook { client, .. } => &client.signing,
                super::super::DecisionRoute::Conversational { .. } => {
                    panic!("expected webhook route")
                }
            }
        }

        #[test]
        fn from_config_threads_hmac_into_webhook_route() {
            use super::super::HitlRuntime;

            let config =
                |url: &str| webhook_config(url, aura_config::ToolHeaderMappings::default());
            let pending = crate::hitl::PendingApprovals::new();
            let hmac = test_hmac();

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

        /// Capture does not require https: TLS may be terminated ahead of the
        /// process (trusted gateway, service-to-service), so a cleartext url
        /// with mappings builds a usable unsigned route. Only the HMAC-secret
        /// rule rejects plaintext, and it is unchanged by capture.
        #[test]
        fn from_config_allows_capture_over_plaintext_url() {
            use super::super::HitlRuntime;

            let pending = crate::hitl::PendingApprovals::new();

            let plaintext = HitlRuntime::from_config(
                &webhook_config("http://approvals.example.com/aura", user_mapping()),
                &pending,
                None,
                None,
            );
            assert!(
                matches!(signing_of(&plaintext), EgressSigning::Disabled),
                "plaintext http:// with capture and no secret must stay usable"
            );

            let secure = HitlRuntime::from_config(
                &webhook_config("https://approvals.example.com/aura", user_mapping()),
                &pending,
                None,
                None,
            );
            assert!(
                matches!(signing_of(&secure), EgressSigning::Disabled),
                "https:// with capture and no secret is a usable unsigned route"
            );

            let legacy = HitlRuntime::from_config(
                &webhook_config(
                    "http://approvals.example.com/aura",
                    aura_config::ToolHeaderMappings::default(),
                ),
                &pending,
                None,
                None,
            );
            assert!(
                matches!(signing_of(&legacy), EgressSigning::Disabled),
                "plaintext http:// without capture is unchanged"
            );

            // The HMAC-secret rule is the one that still rejects plaintext,
            // and capture being configured does not soften it.
            let signed_plaintext = HitlRuntime::from_config(
                &webhook_config("http://approvals.example.com/aura", user_mapping()),
                &pending,
                Some(&test_hmac()),
                None,
            );
            assert!(
                matches!(
                    signing_of(&signed_plaintext),
                    EgressSigning::Misconfigured(_)
                ),
                "plaintext http:// with a secret must still fail closed"
            );
        }

        /// Capture what `body` logs at WARN and above, as text.
        fn captured_warn_log(body: impl FnOnce()) -> String {
            let buf = std::sync::Arc::new(super::CapturedLog(std::sync::Mutex::new(Vec::new())));
            let subscriber = tracing_subscriber::fmt()
                .with_writer(buf.clone())
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .finish();
            tracing::subscriber::with_default(subscriber, body);
            String::from_utf8_lossy(&buf.0.lock().unwrap()).to_string()
        }

        /// Construction (`HitlRuntime::from_config`, and so `WebhookClient`
        /// construction) runs once per chat request, not once at startup —
        /// so it must never warn on cleartext capture itself, or every
        /// request on the route would repeat the warning.
        /// [`warn_on_cleartext_capture`] is the boot-time seam for it.
        #[test]
        fn capture_over_plaintext_url_stays_silent_at_construction() {
            let pending = crate::hitl::PendingApprovals::new();
            let log = captured_warn_log(|| {
                let _ = super::super::HitlRuntime::from_config(
                    &webhook_config("http://approvals.example.com/aura", user_mapping()),
                    &pending,
                    None,
                    None,
                );
            });
            assert!(log.is_empty(), "construction must not log, got: {log}");
        }

        /// The boot-time warning names the risk and the webhook's origin,
        /// but never the userinfo, path, or query a webhook URL may carry —
        /// any of which can hold a secret. An https route with the same
        /// mappings stays quiet, so the warning is attributable to the
        /// cleartext scheme alone.
        #[test]
        fn warn_on_cleartext_capture_warns_once_and_redacts_the_url() {
            let log = captured_warn_log(|| {
                super::super::warn_on_cleartext_capture(&webhook_config(
                    "http://token:secret@approvals.example.com:8443/aura/hook?key=shh",
                    user_mapping(),
                ));
                super::super::warn_on_cleartext_capture(&webhook_config(
                    "https://approvals.example.com/aura",
                    user_mapping(),
                ));
            });
            assert!(
                log.contains("cleartext http"),
                "the warning must name the risk, got log: {log}"
            );
            assert!(
                log.contains("http://approvals.example.com:8443"),
                "the warning must name the origin, got log: {log}"
            );
            for secret in ["token", "secret", "aura/hook", "key=shh"] {
                assert!(
                    !log.contains(secret),
                    "the warning must never carry userinfo, path, or query, got: {log}"
                );
            }
            assert!(
                !log.contains("https://approvals.example.com"),
                "an https route must not warn, got log: {log}"
            );
        }

        /// An absent or empty `tool_headers_from_response` map is the
        /// legacy path: no capture, so no exposure to warn about, over
        /// either scheme.
        #[test]
        fn warn_on_cleartext_capture_is_quiet_with_no_map() {
            let log = captured_warn_log(|| {
                super::super::warn_on_cleartext_capture(&webhook_config(
                    "http://approvals.example.com/aura",
                    aura_config::ToolHeaderMappings::default(),
                ));
            });
            assert!(
                log.is_empty(),
                "no map means nothing to warn about, got: {log}"
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
                aura_config::ToolHeaderMappings::default(),
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

        /// Capture composes with the response-leg signature check: the same
        /// response both verifies and yields identity.
        #[tokio::test]
        async fn signed_gate_approved_captures_mapped_headers() {
            let hmac = test_hmac();
            let decision_id = DecisionId::generate();
            let response_body = r#"{"approved":true}"#.to_owned();

            let mut response_headers = signed_response_headers(&hmac, decision_id, &response_body);
            response_headers.push(("x-approver-id".to_owned(), "alice".to_owned()));
            let (url, received) = one_shot_receiver(response_headers, response_body).await;

            let client = loopback_client(&url, EgressSigning::Enabled(hmac), user_mapping());
            let decision = client
                .request_approval_for_gate(&test_request(decision_id), Duration::from_secs(5))
                .await
                .expect("signed gate round trip succeeds");

            match decision {
                GateDecision::Approved {
                    overrides: Some(overrides),
                } => assert_eq!(
                    overrides.captured_names().collect::<Vec<_>>(),
                    vec!["x-forwarded-user"]
                ),
                other => panic!("expected Approved carrying overrides, got {other:?}"),
            }

            let received = received.await.unwrap();
            assert!(
                received.header(SIGNATURE_HEADER).is_some(),
                "the gate path must still sign its egress POST"
            );
        }

        /// The route-wide `request_approval` surface — what the
        /// agent-callable tool uses — never captures identity, whatever the
        /// mapping says: an approved, identity-bearing response resolves to
        /// a plain decision, and the outcome type has no override channel
        /// that could carry the headers anywhere.
        #[tokio::test]
        async fn route_wide_approval_discards_identity_headers() {
            let (url, _received) = one_shot_receiver(
                vec![("x-approver-id".to_owned(), "alice".to_owned())],
                r#"{"approved":true}"#.to_owned(),
            )
            .await;

            let client = loopback_client(&url, EgressSigning::Disabled, user_mapping());
            let outcome = client
                .request_approval(
                    &test_request(DecisionId::generate()),
                    Duration::from_secs(5),
                )
                .await
                .expect("the route-wide round trip succeeds");

            assert_eq!(
                outcome,
                ApprovalOutcome::Decided(ApprovalDecision::Approved),
                "the identity headers must be discarded, not captured"
            );
        }

        /// The gate path's response matrix, one loopback receiver per row:
        /// identity exists only on an approved response whose mapped headers
        /// are all present. Every other row — a denial, a missing mapped
        /// header, an empty mapping, a non-2xx status, a malformed body, an
        /// unverified response, a timeout — yields no override object, each
        /// asserted at its own shape rather than by omission.
        #[tokio::test]
        async fn gate_response_matrix_yields_overrides_only_on_a_complete_approval() {
            enum Reply {
                Respond {
                    status: &'static str,
                    headers: Vec<(String, String)>,
                    body: String,
                },
                Stall,
            }
            enum Expected {
                ApprovedWithOverrides,
                ApprovedWithoutOverrides,
                Denied(&'static str),
                TimedOut,
                BadStatus(u16),
                Parse,
                Unverified,
                CaptureFailed(&'static str),
            }

            let identity = || ("x-approver-id".to_owned(), "alice".to_owned());
            let approved = || r#"{"approved":true}"#.to_owned();
            let cases: Vec<(&str, Reply, EgressSigning, bool, Expected)> = vec![
                (
                    "an unsigned approval carrying the mapped header",
                    Reply::Respond {
                        status: "200 OK",
                        headers: vec![identity()],
                        body: approved(),
                    },
                    EgressSigning::Disabled,
                    true,
                    Expected::ApprovedWithOverrides,
                ),
                (
                    "an approval missing the mapped header fails closed",
                    Reply::Respond {
                        status: "200 OK",
                        headers: vec![],
                        body: approved(),
                    },
                    EgressSigning::Disabled,
                    true,
                    Expected::CaptureFailed("x-forwarded-user"),
                ),
                (
                    "a denial short-circuits before capture",
                    Reply::Respond {
                        status: "200 OK",
                        headers: vec![],
                        body: r#"{"approved":false,"reason":"not today"}"#.to_owned(),
                    },
                    EgressSigning::Disabled,
                    true,
                    Expected::Denied("not today"),
                ),
                (
                    "an empty mapping yields no override object at all",
                    Reply::Respond {
                        status: "200 OK",
                        headers: vec![identity()],
                        body: approved(),
                    },
                    EgressSigning::Disabled,
                    false,
                    Expected::ApprovedWithoutOverrides,
                ),
                (
                    "a non-2xx status is a channel fault",
                    Reply::Respond {
                        status: "503 Service Unavailable",
                        headers: vec![identity()],
                        body: approved(),
                    },
                    EgressSigning::Disabled,
                    true,
                    Expected::BadStatus(503),
                ),
                (
                    "a malformed body is a channel fault",
                    Reply::Respond {
                        status: "200 OK",
                        headers: vec![identity()],
                        body: "not json".to_owned(),
                    },
                    EgressSigning::Disabled,
                    true,
                    Expected::Parse,
                ),
                (
                    "an unsigned response under a configured HMAC is rejected",
                    Reply::Respond {
                        status: "200 OK",
                        headers: vec![identity()],
                        body: approved(),
                    },
                    EgressSigning::Enabled(test_hmac()),
                    true,
                    Expected::Unverified,
                ),
                (
                    "a webhook that never answers times out",
                    Reply::Stall,
                    EgressSigning::Disabled,
                    true,
                    Expected::TimedOut,
                ),
            ];

            for (case, reply, signing, mapped, expected) in cases {
                let (url, timeout) = match reply {
                    Reply::Respond {
                        status,
                        headers,
                        body,
                    } => {
                        let (url, _received) =
                            one_shot_receiver_with_status(status, headers, body).await;
                        (url, Duration::from_secs(5))
                    }
                    Reply::Stall => {
                        let (url, _accepted) = stalled_receiver().await;
                        (url, Duration::from_millis(200))
                    }
                };
                let mapping = if mapped {
                    user_mapping()
                } else {
                    aura_config::ToolHeaderMappings::default()
                };
                let decision = loopback_client(&url, signing, mapping)
                    .request_approval_for_gate(&test_request(DecisionId::generate()), timeout)
                    .await;

                match (expected, decision) {
                    (
                        Expected::ApprovedWithOverrides,
                        Ok(GateDecision::Approved {
                            overrides: Some(overrides),
                        }),
                    ) => assert_eq!(
                        overrides.captured_names().collect::<Vec<_>>(),
                        vec!["x-forwarded-user"],
                        "{case}"
                    ),
                    (
                        Expected::ApprovedWithoutOverrides,
                        Ok(GateDecision::Approved { overrides: None }),
                    ) => {}
                    (Expected::Denied(reason), Ok(GateDecision::Denied { reason: got })) => {
                        assert_eq!(got.as_deref(), Some(reason), "{case}")
                    }
                    (Expected::TimedOut, Ok(GateDecision::TimedOut { .. })) => {}
                    (
                        Expected::BadStatus(status),
                        Err(ApprovalError::BadStatus { status: got }),
                    ) => {
                        assert_eq!(got, status, "{case}")
                    }
                    (Expected::Parse, Err(ApprovalError::Parse(_))) => {}
                    (Expected::Unverified, Err(ApprovalError::ResponseUnverified(_))) => {}
                    (
                        Expected::CaptureFailed(name),
                        Err(
                            ref err @ ApprovalError::CaptureFailed(CaptureError::MissingHeaders {
                                ref names,
                            }),
                        ),
                    ) => {
                        assert_eq!(names, &[name.to_owned()], "{case}");
                        assert!(
                            err.to_string().contains(name),
                            "{case}: the audit message must name the missing header: {err}"
                        );
                    }
                    (_, got) => panic!("{case}: unexpected gate outcome {got:?}"),
                }
            }
        }

        /// Cancelling a round trip the receiver has already accepted — so it
        /// is provably in flight — resolves the cancelled decision without
        /// waiting out the timeout. Both entrypoints honour the same token:
        /// `decide_for_gate` and the route-wide `decide`.
        #[tokio::test]
        async fn webhook_cancellation_short_circuits_the_round_trip_on_both_paths() {
            for route_wide in [false, true] {
                let (url, accepted) = stalled_receiver().await;
                let route = super::super::DecisionRoute::Webhook {
                    client: loopback_client(&url, EgressSigning::Disabled, user_mapping()),
                    timeout: Duration::from_secs(300),
                };
                let cancel = crate::request_cancellation::RequestCancelToken::unbound();
                let request = test_request(DecisionId::generate());

                let cancelled = async {
                    if route_wide {
                        match route.decide(request, &cancel).await {
                            Ok(ApprovalOutcome::Cancelled(CancelReason::ClientDisconnected)) => {}
                            other => {
                                panic!("route_wide={route_wide}: expected Cancelled, got {other:?}")
                            }
                        }
                    } else {
                        match route.decide_for_gate(request, &cancel).await {
                            Ok(GateDecision::Cancelled(CancelReason::ClientDisconnected)) => {}
                            other => {
                                panic!("route_wide={route_wide}: expected Cancelled, got {other:?}")
                            }
                        }
                    }
                };
                let ((), ()) = tokio::join!(cancelled, async {
                    accepted
                        .await
                        .expect("the round trip must reach the receiver first");
                    cancel.cancel();
                });
            }
        }

        /// Cancellation beats an approval that is already on the wire: the
        /// receiver cancels the token before writing an approved,
        /// identity-bearing response, so the decision and the disconnect are
        /// both live. Whichever the race sees first, the recheck makes
        /// `Cancelled` the answer and no override object is produced.
        #[tokio::test]
        async fn webhook_gate_cancellation_beats_a_ready_approval() {
            let cancel = crate::request_cancellation::RequestCancelToken::unbound();
            let (url, _received) = cancelling_receiver(
                cancel.clone(),
                vec![("x-approver-id".to_owned(), "alice".to_owned())],
                r#"{"approved":true}"#.to_owned(),
            )
            .await;
            let route = super::super::DecisionRoute::Webhook {
                client: loopback_client(&url, EgressSigning::Disabled, user_mapping()),
                timeout: Duration::from_secs(300),
            };

            let decision = route
                .decide_for_gate(test_request(DecisionId::generate()), &cancel)
                .await
                .expect("cancellation is an outcome, not a channel fault");

            assert!(
                matches!(
                    decision,
                    GateDecision::Cancelled(CancelReason::ClientDisconnected)
                ),
                "expected Cancelled to win over the ready approval, got {decision:?}"
            );
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
