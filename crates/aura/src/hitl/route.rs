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
    #[must_use]
    pub fn from_config(config: &HitlConfig, pending_approvals: &PendingApprovals) -> Self {
        let route = match &config.route {
            DecisionRouteConfig::Webhook { url, timeout_secs } => DecisionRoute::Webhook {
                client: WebhookClient::new(build_webhook_client(), url.clone()),
                timeout: Duration::from_secs(*timeout_secs),
            },
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
                approval_event_broker::publish(
                    &request_id,
                    ApprovalLifecycleEvent::Requested((&request).into()),
                )
                .await;

                let expires_at = chrono::Utc::now()
                    + chrono::Duration::from_std(*timeout)
                        .expect("approval timeout fits in chrono");
                let pending_event = events::pending(&request, &expires_at);
                let handle = registry.register(request, *timeout);

                approval_event_broker::publish(
                    &request_id,
                    ApprovalLifecycleEvent::Pending(pending_event),
                )
                .await;

                let outcome = handle.outcome(cancel).await;

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

/// HTTP client for the webhook route. Carried over from the spike's
/// `HttpApprovalDispatch`.
pub struct WebhookClient {
    client: reqwest::Client,
    url: WebhookUrl,
}

impl WebhookClient {
    #[must_use]
    pub fn new(client: reqwest::Client, url: WebhookUrl) -> Self {
        Self { client, url }
    }

    /// POST the request and resolve a decision, failing closed on timeout or
    /// transport/parse error.
    async fn request_approval(
        &self,
        request: &ApprovalRequest,
        timeout: Duration,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        // Serialize the wire view, not the domain request: it keeps `scope` /
        // `origin` as the flat `aura_events` DTOs instead of leaking Rust enum
        // variant names onto the webhook contract.
        let wire = ApprovalRequestWire::from(request);
        match self
            .client
            .post(self.url.as_str())
            .json(&wire)
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
        use super::super::registry::PendingApprovals;
        use std::time::Duration;

        let registry = PendingApprovals::new();
        let route = DecisionRoute::Conversational {
            registry,
            timeout: Duration::from_secs(5),
        };
        let request = ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
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
