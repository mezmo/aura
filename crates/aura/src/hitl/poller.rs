//! The poll-delivery reconciler: drives a parked webhook-poll approval from
//! notification to durable resolution.
//!
//! Each tick the reconciler lists the undecided, non-expired approvals from
//! the shared [`ApprovalStore`], keeps its own `instance_id`'s rows, and per
//! id POSTs the ack-only notification until the receiver answers 2xx while
//! polling the status endpoint every tick in parallel — the status read does
//! not wait for the ack, so a receiver that holds the POST open cannot
//! starve it. A decided 200 resolves durably through the same
//! [`PendingApprovals::resolve`] path the ingress handler uses. The
//! reconciler never terminalizes an approval — expiry stays fail-closed at
//! resolve time — and its notified markers are in-memory only, self-pruning
//! when an id leaves `list_pending`.
//!
//! HA posture: single-writer — parked documents are pod-local, so two
//! instances' reconcilers never see each other's runs. Notify delivery is
//! at-least-once: a crash between a 2xx ack and the marker being observed
//! produces one duplicate, idempotent by `decision_id` at the receiver.
//! Poll-claim is exactly-once within an instance; a shared-store,
//! multi-instance deployment has no cross-instance claim guarantee.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use aura_config::{DecisionRouteConfig, HitlConfig, ToolHeaderMappings, WebhookDelivery};

use super::decision::{ApprovalDecision, DecisionId, ResolvedDecision};
use super::registry::{PendingApprovals, ResolveError};
use super::route::{PollOutcome, WebhookClient, webhook_client_from_config};
use super::signing::WebhookHmac;
use crate::approver_headers::ApproverHeaders;
use crate::session_store::ApprovalStore;

/// The poll-delivery reconciler for one process: a private webhook client,
/// the shared approval store it scans, the ingress registry it resolves
/// through, and the identity mapping its poll-200 captures against. Built
/// once at startup from the `[hitl.route]` config; [`Self::spawn`] runs the
/// tick loop until its shutdown token cancels.
pub struct PollReconciler {
    client: WebhookClient,
    store: Arc<dyn ApprovalStore>,
    registry: PendingApprovals,
    instance_id: String,
    interval: Duration,
    tool_header_mappings: ToolHeaderMappings,
}

impl PollReconciler {
    /// Build the reconciler for a `[hitl]` config, or `None` for every
    /// configuration the reconciler does not drive: the conversational arm,
    /// and the webhook arm under sync delivery. The client is built by the
    /// same construction [`super::route::HitlRuntime::from_config`] uses, so
    /// the reconciler's notify/poll legs carry the exact wire shape of the
    /// per-request routes. Its operator headers are the static set only —
    /// `headers_from_request` values are per-row: each parked approval
    /// carries its own request-scoped resolved values, which the notify
    /// overlays without mutating this client.
    #[must_use]
    pub fn from_config(
        config: &HitlConfig,
        hmac: Option<&WebhookHmac>,
        instance_id: String,
        store: Arc<dyn ApprovalStore>,
        registry: &PendingApprovals,
    ) -> Option<Self> {
        let DecisionRouteConfig::Webhook {
            delivery: WebhookDelivery::Poll,
            poll_interval_secs,
            tool_headers_from_response,
            ..
        } = &config.route
        else {
            return None;
        };
        Some(Self {
            client: webhook_client_from_config(&config.route, hmac, None)?,
            store,
            registry: registry.clone(),
            instance_id,
            interval: Duration::from_secs(*poll_interval_secs),
            tool_header_mappings: tool_headers_from_response.clone(),
        })
    }

    /// Spawn the tick loop on the current runtime. The loop stops when
    /// `shutdown` cancels (the web server registers its shutdown token
    /// here) or when the returned handle's [`Self::stop`] runs.
    /// Dropping the handle detaches the loop — the cancel-sweep task's
    /// caller convention (`park::cancel_run_approvals`): await it for
    /// ordered teardown, or drop it and let the loop keep running until
    /// shutdown.
    pub fn spawn(self, shutdown: &CancellationToken) -> PollerHandle {
        let token = shutdown.child_token();
        let task = tokio::spawn(self.run(token.clone()));
        PollerHandle { token, task }
    }

    async fn run(self, token: CancellationToken) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut notified = HashSet::new();
        loop {
            tokio::select! {
                () = token.cancelled() => break,
                _ = tick.tick() => self.tick(&mut notified).await,
            }
        }
    }

    /// One reconcile pass. The notified set self-prunes first: an id that
    /// left `list_pending` (resolved, cancelled, expired) needs no marker.
    async fn tick(&self, notified: &mut HashSet<DecisionId>) {
        let pending = match self.store.list_pending().await {
            Ok(pending) => pending,
            Err(err) => {
                warn!(error = %err, "approval list_pending failed; retrying next tick");
                return;
            }
        };
        notified.retain(|id| {
            pending
                .iter()
                .any(|parked| parked.request.decision_id == *id)
        });

        for parked in pending {
            if parked.request.instance_id != self.instance_id {
                continue;
            }
            let id = parked.request.decision_id;
            if !notified.contains(&id) {
                match self
                    .client
                    .notify(&parked.request, parked.egress_headers.as_ref())
                    .await
                {
                    Ok(()) => {
                        notified.insert(id);
                    }
                    Err(err) => {
                        // The status read below runs regardless: a receiver
                        // that holds the POST open must not starve it.
                        warn!(
                            decision_id = %id,
                            error = %err,
                            "approval notify failed; retrying next tick"
                        );
                    }
                }
            }
            match self.client.poll_decision(id).await {
                Ok(PollOutcome::NotYet) => {}
                Ok(PollOutcome::Decided {
                    decision,
                    response_headers,
                }) => {
                    let resolved = match decision {
                        // Approver identity rides these headers: capture
                        // against the route's mapping the same way the sync
                        // gate does. A capture failure records the decision
                        // WITHOUT identity — reify blocks the approved
                        // execution later if identity is required
                        // (record-then-block, per the approver identity
                        // ADR).
                        ApprovalDecision::Approved if !self.tool_header_mappings.is_empty() => {
                            match ApproverHeaders::from_captured(
                                &self.tool_header_mappings,
                                &response_headers,
                            ) {
                                Ok(identity) => ResolvedDecision::approved(Some(identity)),
                                Err(err) => {
                                    warn!(
                                        decision_id = %id,
                                        error = %err,
                                        "approver identity capture failed; recording the \
                                         decision without identity",
                                    );
                                    ResolvedDecision::approved(None)
                                }
                            }
                        }
                        other => ResolvedDecision::from(other),
                    };
                    match self.registry.resolve(&id, resolved).await {
                        Ok(()) => {}
                        // The ticket expired or was swept between
                        // list_pending and resolve; the decision is
                        // discarded with it, as the ingress 404 would.
                        Err(ResolveError::NotFound) => debug!(
                            decision_id = %id,
                            "polled decision arrived after the approval left the store"
                        ),
                        Err(ResolveError::Store(err)) => warn!(
                            decision_id = %id,
                            error = %err,
                            "polled decision could not be recorded; it may be re-polled"
                        ),
                    }
                }
                Err(err) => warn!(
                    decision_id = %id,
                    error = %err,
                    "approval poll failed; retrying next tick"
                ),
            }
        }
    }
}

/// Stop/join handle for a spawned [`PollReconciler`].
pub struct PollerHandle {
    token: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl PollerHandle {
    /// Cancel the loop and await its exit. The in-flight tick completes
    /// first — a notify or poll request already under way is never cut.
    pub async fn stop(self) {
        self.token.cancel();
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use super::super::decision::{AgentScope, ApprovalDecision, ApprovalOrigin};
    use super::super::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
    use super::super::read_full_request;
    use super::super::registry::ParkedApproval;
    use super::*;
    use crate::session_store::{FileApprovalStore, InMemoryApprovalStore};

    const INSTANCE_ID: &str = "poll-instance";

    fn poll_config() -> aura_config::HitlConfig {
        aura_config::HitlConfig {
            require_approval: vec![],
            park: aura_config::ParkConfig::default(),
            route: aura_config::DecisionRouteConfig::Webhook {
                url: aura_config::WebhookUrl::new("http://127.0.0.1:1").unwrap(),
                timeout_secs: 300,
                headers: std::collections::HashMap::new(),
                headers_from_request: std::collections::HashMap::new(),
                tool_headers_from_response: aura_config::ToolHeaderMappings::default(),
                delivery: aura_config::WebhookDelivery::Poll,
                poll_url: None,
                poll_interval_secs: 10,
                poll_request_timeout_secs: 30,
            },
        }
    }

    fn parked_request(decision_id: DecisionId, instance_id: &str) -> ApprovalRequest {
        ApprovalRequest {
            version: PROTOCOL_VERSION,
            instance_id: instance_id.to_string(),
            decision_id,
            request_id: format!("req_{decision_id}"),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "kubectl_apply".to_string(),
                arguments: serde_json::json!({ "namespace": "prod" }),
                tool_call_intent: None,
            }],
        }
    }

    fn reconciler_with(store: Arc<dyn ApprovalStore>, url: &str) -> PollReconciler {
        let mut config = poll_config();
        let aura_config::DecisionRouteConfig::Webhook { url: route_url, .. } = &mut config.route
        else {
            unreachable!("poll_config builds a webhook route");
        };
        *route_url = aura_config::WebhookUrl::new(url).unwrap();
        let registry = PendingApprovals::with_backend(
            store.clone(),
            Arc::new(crate::session_store::InMemoryEventBus::new()),
        );
        PollReconciler::from_config(&config, None, INSTANCE_ID.to_string(), store, &registry)
            .expect("poll config builds a reconciler")
    }

    /// A pending approval parked directly in the store, expiring far out.
    async fn park_pending(store: &Arc<dyn ApprovalStore>, instance_id: &str) -> DecisionId {
        let request = parked_request(DecisionId::generate(), instance_id);
        let id = request.decision_id;
        store
            .register(ParkedApproval {
                request,
                registered_at: chrono::Utc::now(),
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
                egress_headers: None,
            })
            .await
            .expect("pending approval registers");
        id
    }

    /// Sequential-connection mock receiver: connection `i` gets
    /// `responses[i]`, every captured request text lands on the channel.
    /// Each response closes the connection, so every request is its own.
    async fn scripted_receiver(
        responses: Vec<(&'static str, String)>,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel(responses.len());
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let captured = read_full_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.ok();
                socket.shutdown().await.ok();
                tx.send(captured).await.expect("capture channel open");
            }
        });
        (url, rx)
    }

    fn ack_ok() -> (&'static str, String) {
        ("200 OK", String::new())
    }

    fn poll_pending() -> (&'static str, String) {
        ("204 No Content", String::new())
    }

    fn poll_decided(decision: &str) -> (&'static str, String) {
        ("200 OK", decision.to_string())
    }

    async fn store_decision(
        store: &Arc<dyn ApprovalStore>,
        id: &DecisionId,
    ) -> Option<ResolvedDecision> {
        store.decision(id).await.unwrap()
    }

    /// The tick loop against a scripted receiver: a failed notify retries
    /// the POST next tick without delaying the status read, an acked id
    /// skips the POST (the marker short-circuits it) and keeps polling,
    /// and the decided 200 resolves durably through the ingress registry.
    #[tokio::test]
    async fn tick_polls_while_unacked_and_the_marker_skips_the_post() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx) = scripted_receiver(vec![
            ("503 Service Unavailable", String::new()),
            poll_pending(),
            ack_ok(),
            poll_pending(),
            poll_decided(r#"{"status":"approved"}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        let first = rx.recv().await.unwrap();
        assert!(first.starts_with("POST "), "first tick notifies: {first}");
        let same_tick = rx.recv().await.unwrap();
        assert!(
            same_tick.starts_with("GET "),
            "a failed notify must not delay the status read: {same_tick}"
        );
        assert!(
            same_tick.contains(&format!("decision_id={id}")),
            "the poll query carries the decision id: {same_tick}"
        );

        reconciler.tick(&mut notified).await;
        let retried = rx.recv().await.unwrap();
        assert!(
            retried.starts_with("POST "),
            "an unacked id retries the POST: {retried}"
        );
        assert!(rx.recv().await.unwrap().starts_with("GET "));

        reconciler.tick(&mut notified).await;
        let marked = rx.recv().await.unwrap();
        assert!(
            marked.starts_with("GET "),
            "the notified marker must skip the POST: {marked}"
        );

        assert_eq!(
            store_decision(&store, &id).await,
            Some(ResolvedDecision::from(ApprovalDecision::Approved)),
            "the decided 200 resolves durably"
        );
        assert!(
            store.get(&id).await.unwrap().is_none(),
            "resolve removed the pending ticket"
        );
    }

    /// A receiver whose POSTs never succeed cannot starve the status read:
    /// the approval still resolves through the GET alone.
    #[tokio::test]
    async fn tick_resolves_via_the_status_get_while_notify_never_acks() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx) = scripted_receiver(vec![
            ("503 Service Unavailable", String::new()),
            poll_pending(),
            ("503 Service Unavailable", String::new()),
            poll_decided(r#"{"status":"approved"}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("POST "));
        assert!(rx.recv().await.unwrap().starts_with("GET "));

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("POST "));
        assert!(rx.recv().await.unwrap().starts_with("GET "));

        assert_eq!(
            store_decision(&store, &id).await,
            Some(ResolvedDecision::from(ApprovalDecision::Approved)),
            "the status GET alone must carry the resolution"
        );
    }

    /// A 200 whose body is outside the status envelope keeps the approval
    /// pending: nothing is recorded, and the reconciler polls again.
    #[tokio::test]
    async fn tick_unparsable_poll_200_records_no_decision() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx) = scripted_receiver(vec![
            ack_ok(),
            poll_decided("<html>upstream error page</html>"),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("POST "));
        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));

        assert_eq!(
            store_decision(&store, &id).await,
            None,
            "an out-of-envelope body must never become a decision"
        );
        assert!(store.get(&id).await.unwrap().is_some());
    }

    /// A policy-instant approval on the notify POST is never read as a
    /// decision: the ack body is dropped, and only the status GET resolves.
    #[tokio::test]
    async fn policy_instant_approve_on_the_notify_ack_waits_for_the_status_get() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx) = scripted_receiver(vec![
            poll_decided(r#"{"status":"approved"}"#), // the approving ack body
            poll_pending(),
            poll_decided(r#"{"status":"approved"}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("POST "));
        let first_poll = rx.recv().await.unwrap();
        assert!(
            first_poll.starts_with("GET "),
            "an acked id polls the same tick: {first_poll}"
        );

        assert!(
            store_decision(&store, &id).await.is_none(),
            "an approving ack body must never mint a decision"
        );

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));
        assert_eq!(
            store_decision(&store, &id).await,
            Some(ResolvedDecision::from(ApprovalDecision::Approved)),
            "the decided status GET resolves durably"
        );
    }

    /// A pending row belonging to another instance is never notified,
    /// polled, or resolved: the own-instance filter keeps a shared
    /// backend's reconcilers to their own approvals.
    #[tokio::test]
    async fn tick_skips_pending_approvals_of_other_instances() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let own = park_pending(&store, INSTANCE_ID).await;
        let other = park_pending(&store, "another-instance").await;
        let (url, mut rx) =
            scripted_receiver(vec![ack_ok(), poll_decided(r#"{"status":"approved"}"#)]).await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        let first = rx.recv().await.unwrap();
        assert!(first.starts_with("POST "));
        assert!(
            first.contains(&own.to_string()) && !first.contains(&other.to_string()),
            "the notify belongs to the own-instance row only"
        );
        let second = rx.recv().await.unwrap();
        assert!(second.starts_with("GET "));
        assert!(second.contains(&format!("decision_id={own}")));

        assert_eq!(
            store_decision(&store, &own).await,
            Some(ResolvedDecision::from(ApprovalDecision::Approved)),
        );
        assert!(
            store.get(&other).await.unwrap().is_some(),
            "the other instance's row stays untouched"
        );
    }

    /// `from_config` gates the production spawn on webhook poll delivery.
    #[test]
    fn from_config_gates_on_poll_delivery() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let registry = PendingApprovals::new();
        let build = |config| {
            PollReconciler::from_config(
                &config,
                None,
                INSTANCE_ID.to_string(),
                store.clone(),
                &registry,
            )
        };

        assert!(build(poll_config()).is_some());

        let mut sync = poll_config();
        if let aura_config::DecisionRouteConfig::Webhook { delivery, .. } = &mut sync.route {
            *delivery = aura_config::WebhookDelivery::Sync;
        }
        assert!(build(sync).is_none(), "sync delivery has no reconciler");

        let conversational = aura_config::HitlConfig {
            require_approval: vec![],
            park: aura_config::ParkConfig::default(),
            route: aura_config::DecisionRouteConfig::Conversational { timeout_secs: 60 },
        };
        assert!(build(conversational).is_none());
    }

    /// The restart path through the real production wiring (from_config +
    /// spawn over a persistent file-backed store): boot one, the notify
    /// reaches the receiver; stop the runtime; the receiver answers the
    /// reboot's re-notify (idempotent by decision id) with a decided poll —
    /// and the first tick after the reboot resolves durably.
    #[tokio::test]
    async fn restart_resolves_on_the_first_tick_after_reboot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let (url, mut rx) = scripted_receiver(vec![
            ack_ok(),
            poll_pending(),
            ack_ok(),
            poll_decided(r#"{"status":"approved"}"#),
        ])
        .await;

        let shutdown = CancellationToken::new();

        // Boot one.
        let store_a: Arc<dyn ApprovalStore> = Arc::new(FileApprovalStore::open(&path).unwrap());
        let registry_a = PendingApprovals::with_backend(
            store_a.clone(),
            Arc::new(crate::session_store::InMemoryEventBus::new()),
        );
        let id = {
            let request = parked_request(DecisionId::generate(), INSTANCE_ID);
            let id = request.decision_id;
            registry_a
                .register_durable(ParkedApproval {
                    request,
                    registered_at: chrono::Utc::now(),
                    expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
                    egress_headers: None,
                })
                .await
                .expect("pending approval parks durably");
            id
        };
        let handle_a = reconciler_with(store_a.clone(), &url).spawn(&shutdown);
        let first = rx.recv().await.unwrap();
        assert!(first.starts_with("POST "), "boot one notifies: {first}");
        handle_a.stop().await;

        // The second and third scripted responses belong to boot one's
        // in-flight poll; answer the poll pending so the ticket survives
        // the stop undecided.
        let _pending = rx.recv().await.unwrap();

        // Boot two: a fresh reconciler over the same store root — notified
        // markers are gone, so the notify repeats (idempotent) and the
        // first tick picks up the decision. Its registry resolves over the
        // same store, as the ingress handler's would.
        let store_b: Arc<dyn ApprovalStore> = Arc::new(FileApprovalStore::open(&path).unwrap());
        let _handle_b = reconciler_with(store_b.clone(), &url).spawn(&shutdown);
        let third = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("boot two re-notifies")
            .unwrap();
        assert!(
            third.starts_with("POST "),
            "the reboot re-notifies: {third}"
        );

        let recorded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(decision) = store_decision(&store_b, &id).await {
                    break decision;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the first tick after the reboot resolves");
        assert_eq!(recorded, ResolvedDecision::from(ApprovalDecision::Approved));
        // The file backend retains the record behind `get` (resolve moves
        // the ticket into the decision file); the pending scan is what the
        // reconciler consumes, so that is what must be empty now.
        let still_pending = store_b
            .list_pending()
            .await
            .unwrap()
            .into_iter()
            .any(|parked| parked.request.decision_id == id);
        assert!(
            !still_pending,
            "the resolved ticket leaves the pending scan"
        );

        shutdown.cancel();
    }

    /// The handle stops the loop cleanly: no tick is cut mid-request and
    /// the joined task ends without hanging.
    #[tokio::test]
    async fn stop_stops_the_loop_and_joins_without_hanging() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let (url, _rx) = scripted_receiver(vec![ack_ok()]).await;
        let reconciler = reconciler_with(store, &url);
        let handle = reconciler.spawn(&CancellationToken::new());

        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), handle.stop())
            .await
            .expect("stop must not hang");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stop returned promptly"
        );
    }

    /// Cancelling the token the loop was spawned under — the web server's
    /// shutdown path — ends the loop.
    #[tokio::test]
    async fn shutdown_token_cancel_ends_the_loop() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let (url, _rx) = scripted_receiver(vec![ack_ok()]).await;
        let shutdown = CancellationToken::new();
        let reconciler = reconciler_with(store, &url);
        let handle = reconciler.spawn(&shutdown);

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !handle.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the loop must end when the shutdown token cancels");
    }

    // ====================================================================
    // Egress headers at rest + identity docking on the decision record
    // ====================================================================

    mod at_rest {
        use std::collections::HashMap;
        use std::sync::Arc;

        use futures::StreamExt;
        use reqwest::header::{HeaderMap, HeaderValue};
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;
        use tokio::sync::mpsc;

        use super::super::super::decision::ResolvedDecision;
        use super::super::super::registry::ParkedApproval;
        use super::*;
        use crate::session_store::EventBus;
        use crate::session_store::FileApprovalStore;

        const EGRESS_ALPHA: &str = "Bearer egress-sentinel-alpha";
        const EGRESS_BETA: &str = "Bearer egress-sentinel-beta";
        const IDENTITY_SENTINEL: &str = "approver-identity-sentinel";

        /// Minimal tracing-capture buffer, shared as the writer (the
        /// route.rs `CapturedLog` pattern).
        struct CaptureLog(std::sync::Mutex<Vec<u8>>);

        impl std::io::Write for &CaptureLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        /// A poll config with static client headers and an optional identity
        /// mapping, built the production way.
        fn poll_config_with(
            static_headers: HashMap<String, String>,
            identity_mapping: bool,
        ) -> aura_config::HitlConfig {
            aura_config::HitlConfig {
                require_approval: vec![],
                park: aura_config::ParkConfig::default(),
                route: aura_config::DecisionRouteConfig::Webhook {
                    url: aura_config::WebhookUrl::new("http://127.0.0.1:1").unwrap(),
                    timeout_secs: 300,
                    headers: static_headers,
                    headers_from_request: HashMap::new(),
                    tool_headers_from_response: if identity_mapping {
                        crate::approver_headers::tests::mappings(&[(
                            "x-forwarded-user",
                            "x-approver-id",
                        )])
                    } else {
                        aura_config::ToolHeaderMappings::default()
                    },
                    delivery: aura_config::WebhookDelivery::Poll,
                    poll_url: None,
                    poll_interval_secs: 10,
                    poll_request_timeout_secs: 30,
                },
            }
        }

        /// A reconciler built from `config`, pointed at `url`, resolving
        /// through the given registry.
        fn reconciler_over(
            config: &aura_config::HitlConfig,
            store: Arc<dyn ApprovalStore>,
            url: &str,
            registry: &PendingApprovals,
        ) -> PollReconciler {
            let mut config = config.clone();
            if let aura_config::DecisionRouteConfig::Webhook { url: route_url, .. } =
                &mut config.route
            {
                *route_url = aura_config::WebhookUrl::new(url).unwrap();
            }
            PollReconciler::from_config(&config, None, INSTANCE_ID.to_string(), store, registry)
                .expect("a poll config builds a reconciler")
        }

        /// A reconciler over its own registry, the standalone shape.
        fn reconciler_from(
            config: &aura_config::HitlConfig,
            store: Arc<dyn ApprovalStore>,
            url: &str,
        ) -> PollReconciler {
            let registry = PendingApprovals::with_backend(
                store.clone(),
                Arc::new(crate::session_store::InMemoryEventBus::new()),
            );
            reconciler_over(config, store, url, &registry)
        }

        /// Park a durable row carrying `egress` as its resolved authorization
        /// value.
        async fn park_row(store: &Arc<dyn ApprovalStore>, egress: &str) -> DecisionId {
            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                HeaderValue::from_str(egress).expect("sentinels are valid header values"),
            );
            let request = parked_request(DecisionId::generate(), INSTANCE_ID);
            let id = request.decision_id;
            store
                .register(ParkedApproval {
                    request,
                    registered_at: chrono::Utc::now(),
                    expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
                    egress_headers: Some(headers),
                })
                .await
                .expect("pending approval registers");
            id
        }

        /// One scripted receiver response: status line, extra headers, body.
        type ScriptedResponse = (
            &'static str,
            Vec<(&'static str, &'static str)>,
            &'static str,
        );

        /// Sequential-connection receiver whose responses may carry headers:
        /// connection `i` gets `responses[i]`, every captured raw request
        /// lands on the channel in order.
        async fn scripted_receiver_with_headers(
            responses: Vec<ScriptedResponse>,
        ) -> (String, mpsc::Receiver<String>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = mpsc::channel(responses.len());
            tokio::spawn(async move {
                for (status, headers, body) in responses {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let captured = read_full_request(&mut socket).await;
                    let mut response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in &headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    response.push_str(body);
                    socket.write_all(response.as_bytes()).await.ok();
                    socket.shutdown().await.ok();
                    tx.send(captured).await.expect("capture channel open");
                }
            });
            (url, rx)
        }

        /// The `decision_id` a captured notify POST body carries.
        fn body_decision_id(captured: &str) -> String {
            let body = captured
                .split("\r\n\r\n")
                .nth(1)
                .expect("the capture carries a body");
            let json: serde_json::Value = serde_json::from_str(body).expect("wire body is JSON");
            json["decision_id"]
                .as_str()
                .expect("the wire carries the decision id")
                .to_string()
        }

        /// Distinct-credential approvals keep their own headers through
        /// registration, notify retry, and store reopen: each notify POST —
        /// including the retried one — authenticates with ITS row's value,
        /// never the sibling's and never the client's static fallback.
        #[tokio::test]
        async fn distinct_rows_keep_their_own_headers_through_retry_and_reopen() {
            let dir = tempfile::tempdir().unwrap();
            let store_root = dir.path().join("approvals");

            // Registration: both rows land with their own resolved values.
            let writer: Arc<dyn ApprovalStore> =
                Arc::new(FileApprovalStore::open(&store_root).unwrap());
            let alpha = park_row(&writer, EGRESS_ALPHA).await;
            let beta = park_row(&writer, EGRESS_BETA).await;

            // Store reopen: a second handle over the same root — the
            // reconciler's view — still sees each row's own headers.
            let reader: Arc<dyn ApprovalStore> =
                Arc::new(FileApprovalStore::open(&store_root).unwrap());
            let pending = reader.list_pending().await.unwrap();
            let expected: HashMap<String, String> = HashMap::from([
                (alpha.to_string(), EGRESS_ALPHA.to_string()),
                (beta.to_string(), EGRESS_BETA.to_string()),
            ]);
            assert_eq!(pending.len(), 2);
            for parked in &pending {
                let row = parked.egress_headers.as_ref().expect("row headers survive");
                assert_eq!(
                    row.get("authorization")
                        .map(|value| value.to_str().unwrap()),
                    Some(expected[&parked.request.decision_id.to_string()].as_str()),
                );
            }

            // The reconciler's client carries a DIFFERENT static value for
            // the same header name, so a per-row override failure is visible.
            let mut static_headers = HashMap::new();
            static_headers.insert(
                "authorization".to_string(),
                "Bearer client-static".to_string(),
            );
            let config = poll_config_with(static_headers, false);
            let (url, mut rx) = scripted_receiver_with_headers(vec![
                // Tick 1: row A POST 503s (captured) then polls anyway; row B
                // acks and polls. Four connections.
                ("503 Service Unavailable", vec![], ""),
                ("204 No Content", vec![], ""),
                ("200 OK", vec![], ""),
                ("204 No Content", vec![], ""),
                // Tick 2: the failed notify retries (captured); both rows
                // poll. Three connections.
                ("200 OK", vec![], ""),
                ("204 No Content", vec![], ""),
                ("204 No Content", vec![], ""),
                // Tick 3: both rows resolve. Two connections.
                ("200 OK", vec![], r#"{"status":"approved"}"#),
                ("200 OK", vec![], r#"{"status":"approved"}"#),
            ])
            .await;
            let reconciler = reconciler_from(&config, Arc::clone(&reader), &url);
            let mut notified = HashSet::new();

            reconciler.tick(&mut notified).await;
            let first = rx.recv().await.unwrap();
            let _second = rx.recv().await.unwrap();
            let third = rx.recv().await.unwrap();
            let _fourth = rx.recv().await.unwrap();
            reconciler.tick(&mut notified).await;
            let fifth = rx.recv().await.unwrap();
            let _sixth = rx.recv().await.unwrap();
            let _seventh = rx.recv().await.unwrap();
            reconciler.tick(&mut notified).await;
            let _eighth = rx.recv().await.unwrap();
            let _ninth = rx.recv().await.unwrap();

            assert!(first.starts_with("POST ") && third.starts_with("POST "));
            assert!(fifth.starts_with("POST "), "the failed notify retries");

            let posts = [first, third, fifth];
            for captured in &posts {
                let id = body_decision_id(captured);
                let expected_value = expected[&id].as_str();
                let header_line = captured
                    .lines()
                    .find(|line| line.to_lowercase().starts_with("authorization:"))
                    .unwrap_or_else(|| {
                        panic!("every notify POST carries its row header: {captured}")
                    });
                assert_eq!(
                    header_line.split_once(':').expect("header line").1.trim(),
                    expected_value,
                    "each notify POST authenticates with its own row's value: {captured}"
                );
                assert!(
                    !captured.contains("Bearer client-static"),
                    "the client's static value must never leak beside the row's: {captured}"
                );
            }

            // Both rows resolved durably, decisions without identity.
            assert_eq!(
                store_decision(&reader, &alpha).await,
                Some(ResolvedDecision::from(ApprovalDecision::Approved)),
            );
            assert_eq!(
                store_decision(&reader, &beta).await,
                Some(ResolvedDecision::from(ApprovalDecision::Approved)),
            );
        }

        /// The poll-200's configured identity headers are captured and
        /// recorded in the SAME resolve as the decision.
        #[tokio::test]
        async fn poll_200_identity_is_captured_into_the_decision_record() {
            let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
            let id = park_row(&store, EGRESS_ALPHA).await;
            let config = poll_config_with(HashMap::new(), true);
            let (url, mut rx) = scripted_receiver_with_headers(vec![
                ("200 OK", vec![], ""),
                (
                    "200 OK",
                    vec![("x-approver-id", IDENTITY_SENTINEL)],
                    r#"{"status":"approved"}"#,
                ),
            ])
            .await;
            let reconciler = reconciler_from(&config, store.clone(), &url);
            let mut notified = HashSet::new();

            reconciler.tick(&mut notified).await;
            let _ack = rx.recv().await.unwrap();
            let poll = rx.recv().await.unwrap();
            assert!(poll.starts_with("GET "), "the status read: {poll}");
            assert!(
                !poll.contains(EGRESS_ALPHA) && !poll.contains(IDENTITY_SENTINEL),
                "the status GET carries no row egress or identity values: {poll}"
            );

            match store_decision(&store, &id).await.expect("resolved") {
                ResolvedDecision::Approved {
                    identity: Some(captured),
                } => {
                    assert_eq!(
                        captured.captured_names().collect::<Vec<_>>(),
                        ["x-forwarded-user"],
                        "the identity is stored under the outbound name",
                    );
                    assert_eq!(
                        captured.to_pair_map()["x-forwarded-user"],
                        IDENTITY_SENTINEL,
                        "the captured value is the poll-200's, whole",
                    );
                }
                other => panic!("expected Approved with captured identity, got {other:?}"),
            }
        }

        /// A poll-200 missing the mapped identity header records the
        /// decision WITHOUT identity (record-then-block, per the approver
        /// identity ADR): the
        /// resolution is not lost, reify blocks later if identity is
        /// required.
        #[tokio::test]
        async fn identity_capture_failure_records_the_decision_without_identity() {
            let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
            let id = park_row(&store, EGRESS_ALPHA).await;
            let config = poll_config_with(HashMap::new(), true);
            let (url, mut rx) = scripted_receiver_with_headers(vec![
                ("200 OK", vec![], ""),
                ("200 OK", vec![], r#"{"status":"approved"}"#),
            ])
            .await;
            let reconciler = reconciler_from(&config, store.clone(), &url);
            let mut notified = HashSet::new();

            reconciler.tick(&mut notified).await;
            let _ack = rx.recv().await.unwrap();
            let _poll = rx.recv().await.unwrap();

            assert_eq!(
                store_decision(&store, &id).await,
                Some(ResolvedDecision::approved(None)),
                "the decision records without identity",
            );
        }

        /// The sentinel leak guard. Distinct egress and identity sentinels
        /// are proven to reach ONLY the allowed store fields (the undecided
        /// row's `egress_headers`, the decision record's `identity`) and the
        /// intended HTTP headers (the notify POST's authorization), and to be
        /// absent from the decision-bus payload, lifecycle/SSE events,
        /// tracing and error text, and the run's serialized checkpoints (the
        /// parked document, the `.resuming.json`, and the commit's temp
        /// write).
        #[tokio::test]
        async fn sentinels_reach_only_allowed_store_fields_and_intended_headers() {
            const EGRESS: &str = "Bearer egress-leakguard-sentinel";
            const IDENTITY: &str = "identity-leakguard-sentinel";

            let dir = tempfile::tempdir().unwrap();
            let store_root = dir.path().join("approvals");
            let memory_dir = dir.path().join("memory");
            std::fs::create_dir_all(&memory_dir).unwrap();

            let store: Arc<dyn ApprovalStore> =
                Arc::new(FileApprovalStore::open(&store_root).unwrap());
            let bus = Arc::new(crate::session_store::InMemoryEventBus::new());
            let registry = PendingApprovals::with_backend(store.clone(), bus.clone());

            // The parked row: the run-scoped owner a production park arm
            // writes, with the egress sentinel on the allowed field.
            let run_id = "0191e8c0-1eak-7000-8000-000000000001";
            let request = parked_request(DecisionId::generate(), INSTANCE_ID);
            let id = request.decision_id;
            let mut request = request;
            request.request_id = format!("run:{run_id}");
            let mut egress = HeaderMap::new();
            egress.insert("authorization", HeaderValue::from_str(EGRESS).unwrap());
            registry
                .register_durable(ParkedApproval {
                    request,
                    registered_at: chrono::Utc::now(),
                    expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
                    egress_headers: Some(egress),
                })
                .await
                .unwrap();

            // Watch the decision bus and the lifecycle broker before any
            // traffic.
            let mut bus_sub = bus.subscribe(&format!("approval:{id}")).await.unwrap();
            let mut sse = crate::approval_event_broker::subscribe(&format!("run:{run_id}")).await;

            // Tracing capture around the whole reconcile: strict DEBUG, so
            // every warn/error/debug line the flow could emit is checked.
            // The thread-local default reaches the awaited tick on this
            // single-thread test runtime.
            let log_buf = Arc::new(CaptureLog(std::sync::Mutex::new(Vec::<u8>::new())));

            let config = poll_config_with(HashMap::new(), true);
            let (url, mut rx) = scripted_receiver_with_headers(vec![
                ("200 OK", vec![], ""),
                (
                    "200 OK",
                    vec![("x-approver-id", IDENTITY)],
                    r#"{"status":"approved"}"#,
                ),
            ])
            .await;
            let reconciler = reconciler_over(&config, store.clone(), &url, &registry);
            let mut notified = HashSet::new();

            let subscriber = tracing_subscriber::fmt()
                .with_writer(Arc::clone(&log_buf))
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .finish();
            let notify = {
                let _log_guard = tracing::subscriber::set_default(subscriber);
                reconciler.tick(&mut notified).await;
                let captured = rx.recv().await.unwrap();
                let _poll = rx.recv().await.unwrap();
                captured
            };

            // ---- POSITIVE: only the allowed store fields and headers. ----

            // The notify POST carried the egress sentinel on authorization,
            // and nothing else on the wire carried either sentinel.
            assert!(notify.starts_with("POST "));
            let header_line = notify
                .lines()
                .find(|line| line.to_lowercase().starts_with("authorization:"))
                .expect("the notify carries the row header");
            assert_eq!(header_line.split_once(':').unwrap().1.trim(), EGRESS);
            assert!(
                !notify.contains(IDENTITY),
                "identity never rides the notify POST: {notify}"
            );

            // The store decision file holds the captured identity beside the
            // decision and nothing of the egress: resolve moves the approval
            // into `decisions/{id}.json` without its egress headers.
            let decision_file = store_root.join("decisions").join(format!("{id}.json"));
            let stored = std::fs::read_to_string(&decision_file).expect("decision file exists");
            assert!(
                !stored.contains(EGRESS),
                "the egress credential outlived resolve: {stored}"
            );
            assert!(
                stored.contains(IDENTITY),
                "the captured identity persists beside the decision: {stored}"
            );
            // And the carrier reads both back together.
            match store_decision(&store, &id).await.expect("resolved") {
                ResolvedDecision::Approved {
                    identity: Some(got),
                } => {
                    assert_eq!(got.to_pair_map()["x-forwarded-user"], IDENTITY);
                }
                other => panic!("expected the identity to read back, got {other:?}"),
            }

            // ---- NEGATIVE: everywhere else is sentinel-free. ----

            // Decision-bus payload: the credential-free decision alone.
            let payload = tokio::time::timeout(Duration::from_secs(1), bus_sub.next())
                .await
                .expect("the bus wake arrives")
                .expect("stream open");
            let bus_text = String::from_utf8_lossy(&payload).to_string();
            assert!(
                !bus_text.contains(EGRESS) && !bus_text.contains(IDENTITY),
                "the bus payload is credential-free, got: {bus_text}"
            );

            // Lifecycle/SSE events: the reconciler publishes none for the
            // run's owner id, so nothing can carry a sentinel.
            assert!(
                tokio::time::timeout(Duration::from_millis(100), sse.recv())
                    .await
                    .is_err(),
                "no lifecycle event may be published by the reconcile",
            );

            // Tracing and error text across the whole tick.
            let log = String::from_utf8_lossy(&log_buf.0.lock().unwrap()).to_string();
            assert!(
                !log.contains(EGRESS) && !log.contains(IDENTITY),
                "no sentinel may reach tracing, got: {log}"
            );

            // Serialized checkpoints: the same run's parked document, the
            // resuming document, and the commit's temp write.
            let mut plan = crate::orchestration::Plan::new("Deploy");
            plan.add_task(crate::orchestration::Task::new(3, "Gated apply", "r"));
            plan.tasks[0].state = crate::orchestration::TaskState::AwaitingApproval {
                pending: vec![crate::orchestration::PendingCall {
                    decision_id: id,
                    tool_name: "kubectl_apply".to_string(),
                    arguments: serde_json::json!({ "namespace": "prod" }),
                    call_id: "call-1".to_string(),
                }],
            };
            let mut records = std::collections::HashMap::new();
            records.insert(
                3,
                crate::orchestration::ParkedTaskRecord {
                    attempt: 1,
                    snapshot: crate::orchestration::ParkSnapshot {
                        history: vec![rig::completion::Message::user("apply it")],
                        current_prompt: rig::completion::Message::user("tool results"),
                    },
                },
            );
            let inputs = crate::orchestration::ParkCommitInputs {
                state: crate::orchestration::RunStateForPark {
                    run_id,
                    session_id: None,
                    query: "Deploy",
                    chat_history: &[],
                    coordinator_conversation: &[],
                    routing_decision: None,
                    iteration: 1,
                    planning_ms: 0,
                    failure_history: &[],
                },
                plan: &plan,
                records: &records,
                registry: &registry,
                memory_dir: memory_dir.to_str().unwrap(),
                config: &crate::config::AgentRuntimeConfig::default(),
                decision_window: Duration::from_secs(300),
            };
            crate::orchestration::commit_from_run_state(&inputs)
                .await
                .expect("the park commit publishes");

            let parked_doc = memory_dir.join("parked").join(format!("{run_id}.json"));
            let parked_text = std::fs::read_to_string(&parked_doc).expect("parked document");
            assert!(
                !parked_text.contains(EGRESS) && !parked_text.contains(IDENTITY),
                "the parked document must never carry approval credentials, got: {parked_text}"
            );

            let handle = crate::orchestration::ResumingDocumentHandle::open(&parked_doc)
                .await
                .expect("the resuming handle opens");
            handle
                .append_executed_and_publish("call-1")
                .await
                .expect("the resuming append publishes");
            let resuming_text = std::fs::read_to_string(
                memory_dir
                    .join("parked")
                    .join(format!("{run_id}.resuming.json")),
            )
            .expect("resuming document");
            assert!(
                !resuming_text.contains(EGRESS) && !resuming_text.contains(IDENTITY),
                "the resuming document must never carry approval credentials, got: {resuming_text}"
            );
            let tmp_residue = std::fs::read_dir(memory_dir.join("parked"))
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
            assert!(!tmp_residue, "the append's temp write is renamed away");

            crate::approval_event_broker::unsubscribe(&format!("run:{run_id}")).await;
        }
    }
}
