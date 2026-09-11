//! The poll-delivery reconciler: drives a parked webhook-poll approval from
//! notification to durable resolution.
//!
//! Each tick the reconciler lists the undecided, non-expired approvals
//! from the shared [`ApprovalStore`], keeps its own `instance_id`'s rows,
//! and per id — one row at a time — reads the status endpoint before
//! attempting the ack-only notification POST: a receiver that holds the
//! POST open costs at most one request timeout per tick before the row
//! and the rows queued behind it move on, and a decided 200 resolves
//! durably through the same [`PendingApprovals::resolve`] path the
//! ingress handler uses without re-posting the request. The reconciler
//! never terminalizes an approval — expiry stays fail-closed at resolve
//! time — and its notified markers are in-memory only, self-pruning when
//! an id leaves `list_pending`.
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

use aura_config::{DecisionRouteConfig, HitlConfig, WebhookDelivery};

use super::decision::DecisionId;
use super::registry::{PendingApprovals, ResolveError};
use super::route::{PollOutcome, WebhookClient, webhook_client_from_config};
use super::signing::WebhookHmac;
use crate::session_store::ApprovalStore;

/// The poll-delivery reconciler for one process: a private webhook client,
/// the shared approval store it scans, and the ingress registry it resolves
/// through. Built once at startup from the `[hitl.route]` config; [`Self::spawn`]
/// runs the tick loop until its shutdown token cancels.
pub struct PollReconciler {
    client: WebhookClient,
    store: Arc<dyn ApprovalStore>,
    registry: PendingApprovals,
    instance_id: String,
    interval: Duration,
}

impl PollReconciler {
    /// Build the reconciler for a `[hitl]` config, or `None` for every
    /// configuration the reconciler does not drive: the conversational arm,
    /// and the webhook arm under sync delivery. The client is built by the
    /// same construction [`super::route::HitlRuntime::from_config`] uses, so
    /// the reconciler's notify/poll legs carry the exact wire shape of the
    /// per-request routes (static operator headers only — poll delivery
    /// refuses `headers_from_request` at config validation).
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
            // The status read runs before the notify attempt, so a
            // receiver that holds the POST open delays its own row's
            // acknowledgment, never that row's status read, and a decided
            // row never re-posts its request.
            match self.client.poll_decision(id).await {
                Ok(PollOutcome::NotYet) => {}
                Ok(PollOutcome::Decided {
                    decision,
                    response_headers,
                }) => {
                    // The response headers are not consumed here; the
                    // reconciler resolves on the decision alone.
                    drop(response_headers);
                    match self.registry.resolve(&id, decision).await {
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
                    continue;
                }
                Err(err) => warn!(
                    decision_id = %id,
                    error = %err,
                    "approval poll failed; retrying next tick"
                ),
            }
            if !notified.contains(&id) {
                match self.client.notify(&parked.request).await {
                    Ok(()) => {
                        notified.insert(id);
                    }
                    Err(err) => {
                        warn!(
                            decision_id = %id,
                            error = %err,
                            "approval notify failed; retrying next tick"
                        );
                    }
                }
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
                poll_interval_secs: 1,
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
    ) -> Option<ApprovalDecision> {
        store.decision(id).await.unwrap()
    }

    /// The tick loop against a scripted receiver: the status read runs
    /// before the notify attempt, a failed notify retries the POST next
    /// tick, an acked id skips the POST (the marker short-circuits it)
    /// and keeps polling, and the decided 200 resolves durably through
    /// the ingress registry.
    #[tokio::test]
    async fn tick_polls_while_unacked_and_the_marker_skips_the_post() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx) = scripted_receiver(vec![
            poll_pending(),
            ("503 Service Unavailable", String::new()),
            poll_pending(),
            ack_ok(),
            poll_decided(r#"{"status":"approved"}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        let first = rx.recv().await.unwrap();
        assert!(
            first.starts_with("GET "),
            "first tick reads status: {first}"
        );
        assert!(
            first.contains(&format!("decision_id={id}")),
            "the poll query carries the decision id: {first}"
        );
        let same_tick = rx.recv().await.unwrap();
        assert!(
            same_tick.starts_with("POST "),
            "the notify attempt follows the read: {same_tick}"
        );

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));
        let retried = rx.recv().await.unwrap();
        assert!(
            retried.starts_with("POST "),
            "an unacked id retries the POST: {retried}"
        );

        reconciler.tick(&mut notified).await;
        let marked = rx.recv().await.unwrap();
        assert!(
            marked.starts_with("GET "),
            "the notified marker must skip the POST: {marked}"
        );

        assert_eq!(
            store_decision(&store, &id).await,
            Some(ApprovalDecision::Approved),
            "the decided 200 resolves durably"
        );
        assert!(
            store.get(&id).await.unwrap().is_none(),
            "resolve removed the pending ticket"
        );
    }

    /// A receiver whose POSTs never succeed cannot starve the status read:
    /// the approval still resolves through the GET alone, which also never
    /// re-posts the request once the decision is visible.
    #[tokio::test]
    async fn tick_resolves_via_the_status_get_while_notify_never_acks() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx) = scripted_receiver(vec![
            poll_pending(),
            ("503 Service Unavailable", String::new()),
            poll_decided(r#"{"status":"approved"}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));
        assert!(rx.recv().await.unwrap().starts_with("POST "));

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));

        assert_eq!(
            store_decision(&store, &id).await,
            Some(ApprovalDecision::Approved),
            "the status GET alone must carry the resolution"
        );
    }

    /// A 200 whose body is outside the status envelope keeps the approval
    /// pending: nothing is recorded and the row survives for the next tick.
    #[tokio::test]
    async fn tick_unparsable_poll_200_records_no_decision() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx) = scripted_receiver(vec![
            poll_decided("<html>upstream error page</html>"),
            ack_ok(),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        let out_of_envelope = rx.recv().await.unwrap();
        assert!(out_of_envelope.starts_with("GET "));
        assert!(rx.recv().await.unwrap().starts_with("POST "));

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
            poll_pending(),
            poll_decided(r#"{"status":"approved"}"#), // the approving ack body
            poll_decided(r#"{"status":"approved"}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        let first_read = rx.recv().await.unwrap();
        assert!(
            first_read.starts_with("GET "),
            "the status read runs before the notify: {first_read}"
        );
        assert!(rx.recv().await.unwrap().starts_with("POST "));

        assert!(
            store_decision(&store, &id).await.is_none(),
            "an approving ack body must never mint a decision"
        );

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));
        assert_eq!(
            store_decision(&store, &id).await,
            Some(ApprovalDecision::Approved),
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
        let (url, mut rx) = scripted_receiver(vec![
            poll_pending(),
            ack_ok(),
            poll_decided(r#"{"status":"approved"}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);
        let mut notified = HashSet::new();

        reconciler.tick(&mut notified).await;
        let first = rx.recv().await.unwrap();
        assert!(first.starts_with("GET "));
        assert!(
            first.contains(&format!("decision_id={own}")) && !first.contains(&other.to_string()),
            "the status read targets the own-instance row only"
        );
        let second = rx.recv().await.unwrap();
        assert!(second.starts_with("POST "));
        assert!(
            second.contains(&own.to_string()) && !second.contains(&other.to_string()),
            "the notify belongs to the own-instance row only"
        );

        reconciler.tick(&mut notified).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));

        assert_eq!(
            store_decision(&store, &own).await,
            Some(ApprovalDecision::Approved),
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
    /// spawn over a persistent file-backed store): boot one reads status
    /// and notifies; stop the runtime; the reboot's fresh markers
    /// re-notify the still-undecided row (idempotent by decision id), and
    /// the next tick's decided poll resolves durably.
    #[tokio::test]
    async fn restart_renotifies_the_undecided_row_and_resolves_after_reboot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let (url, mut rx) = scripted_receiver(vec![
            poll_pending(),
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
                })
                .await
                .expect("pending approval parks durably");
            id
        };
        let handle_a = reconciler_with(store_a.clone(), &url).spawn(&shutdown);
        let read = rx.recv().await.unwrap();
        assert!(read.starts_with("GET "), "boot one reads status: {read}");
        let notify = rx.recv().await.unwrap();
        assert!(notify.starts_with("POST "), "boot one notifies: {notify}");
        handle_a.stop().await;

        // Boot two: a fresh reconciler over the same store root — notified
        // markers are gone, so the reboot re-notifies the undecided row and
        // the next tick's poll picks up the decision. Its registry resolves
        // over the same store, as the ingress handler's would.
        let store_b: Arc<dyn ApprovalStore> = Arc::new(FileApprovalStore::open(&path).unwrap());
        let _handle_b = reconciler_with(store_b.clone(), &url).spawn(&shutdown);
        let reread = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("boot two reads status")
            .unwrap();
        assert!(
            reread.starts_with("GET "),
            "the reboot reads status: {reread}"
        );
        let renotify = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("boot two re-notifies")
            .unwrap();
        assert!(
            renotify.starts_with("POST "),
            "the reboot re-notifies: {renotify}"
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
        .expect("the reboot's decided tick resolves");
        assert_eq!(recorded, ApprovalDecision::Approved);
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
}
