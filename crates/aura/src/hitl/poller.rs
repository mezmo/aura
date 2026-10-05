//! The poll-delivery reconciler: drives a parked webhook-poll approval from
//! notification to durable resolution.
//!
//! Each tick the reconciler lists the undecided, non-expired approvals
//! from the shared [`ApprovalStore`], keeps its own `instance_id`'s rows,
//! and moves up to [`MAX_CONCURRENT_ROW_POLLS`] different rows
//! concurrently, with each row's requests strictly ordered inside its
//! slot — the status read first, then the ack-only notification. A
//! receiver that holds one row's request open costs at most one request
//! timeout for that row while the other admitted rows move on, and the
//! rows queued behind the cap take a slot as it frees. The decision
//! deadline is rechecked immediately before each request a row issues:
//! an expired row gets no new request, and its retained evidence is
//! left for the store's fail-closed expiry — this reconciler never
//! terminalizes an approval. A decided 200 resolves durably through
//! the same [`PendingApprovals::resolve`] path the ingress handler
//! uses, without re-posting the request; the store's atomic resolve
//! remains the final deadline and ownership check, so a decision that
//! lands after the deadline is accepted or rejected there, never here.
//! The notified marker is the durable acknowledgment state on each
//! row, so a row acknowledged at registration (the 207 bridge) is
//! never re-POSTed across restarts. Passes never overlap: the tick
//! loop awaits each pass before the next fires.
//!
//! HA posture: single-writer — parked documents are pod-local, so two
//! instances' reconcilers never see each other's runs. Notify delivery is
//! at-least-once: a crash between a 2xx ack and the durable acknowledgment
//! mark produces one duplicate, idempotent by `decision_id` at the
//! receiver. Poll-claim is exactly-once within an instance; a shared-store,
//! multi-instance deployment has no cross-instance claim guarantee.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::decision::{ApprovalDecision, ResolvedDecision};
use super::outcome::ApprovalAuthority;
use super::registry::{ParkedApproval, PendingApprovals, ResolveError};
use super::route::{PollOutcome, WebhookClient, webhook_client_from_config};
use super::signing::WebhookHmac;
use crate::approver_headers::ApproverHeaders;
use crate::session_store::{AcknowledgeOutcome, ApprovalStore};
use aura_config::{DecisionRouteConfig, HitlConfig, ToolHeaderMappings};
/// The reconciler's wall clock, read immediately before each request a
/// row issues. A field-shaped seam so the decision-deadline checks test
/// deterministically; the production constructor installs `Utc::now`.
/// The file store's `open_with_clock` split is the precedent.
type PollClock = Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>;
/// How many DIFFERENT rows may move concurrently within one reconcile
/// pass. The cap bounds egress and task count against a large shared
/// store — each in-flight row holds at most one request (one request
/// timeout worst case) — not throughput; parked sets are pod-local
/// under the single-writer posture.
const MAX_CONCURRENT_ROW_POLLS: usize = 8;
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
    clock: PollClock,
}

impl PollReconciler {
    /// Build the reconciler for a `[hitl]` config, or `None` for every
    /// configuration the reconciler does not drive: the conversational arm,
    /// and any webhook arm that cannot park. The client is built by the same
    /// construction [`super::route::HitlRuntime::from_config`] uses, so the
    /// reconciler's notify/poll legs carry the exact wire shape of the
    /// per-request routes. Its operator headers are the static set only —
    /// `headers_from_request` values are per-row: each parked approval
    /// carries its own request-scoped resolved values, which the notify
    /// overlays without mutating this client.
    ///
    /// # Panics
    ///
    /// Panics when the webhook route's interval is zero.
    #[must_use]
    pub fn from_config(
        config: &HitlConfig,
        hmac: Option<&WebhookHmac>,
        instance_id: String,
        store: Arc<dyn ApprovalStore>,
        registry: &PendingApprovals,
    ) -> Option<Self> {
        let client = webhook_client_from_config(&config.route, hmac, None, config.park.enabled)?;
        // Cross-comment (see the server boot guard in aura-web-server): this
        // is the "can spawn a reconciler" predicate, keyed on `can_park`. The
        // boot guard's duplicate-id scan sees exactly the configs this arms,
        // so the two must move together.
        if !client.can_park() {
            return None;
        }
        let DecisionRouteConfig::Webhook {
            poll_interval_secs,
            tool_headers_from_response,
            ..
        } = &config.route
        else {
            return None;
        };
        if *poll_interval_secs == 0 {
            panic!(
                "`hitl.route.poll_interval_secs` must be greater than zero: \
                 config validation refuses zero at admission, so this backstop \
                 never clamps or silently disables the reconciler"
            );
        }
        Some(Self {
            client,
            store,
            registry: registry.clone(),
            instance_id,
            interval: Duration::from_secs(*poll_interval_secs),
            tool_header_mappings: tool_headers_from_response.clone(),
            clock: Arc::new(chrono::Utc::now),
        })
    }

    /// Test seam: override the wall clock the deadline checks read.
    /// Mirrors the file store's `open_with_clock` split — the production
    /// constructor always installs `Utc::now`.
    #[cfg(test)]
    fn with_clock(mut self, clock: PollClock) -> Self {
        self.clock = clock;
        self
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
        loop {
            tokio::select! {
                () = token.cancelled() => break,
                _ = tick.tick() => self.tick(&token).await,
            }
        }
    }

    /// One reconcile pass. Up to [`MAX_CONCURRENT_ROW_POLLS`] different
    /// rows move concurrently; passes never overlap (the loop awaits
    /// each pass before the next fires); each row's read-then-notify
    /// chain stays sequential inside its own slot. The notified-tracking
    /// reads the persisted acknowledgment state on each row, never a
    /// process-local set alone.
    async fn tick(&self, token: &CancellationToken) {
        let pending = match self.store.list_pending().await {
            Ok(pending) => pending,
            Err(err) => {
                warn!(error = %err, "approval list_pending failed; retrying next tick");
                return;
            }
        };

        let rows: Vec<ParkedApproval> = pending
            .into_iter()
            // Instance and authority filters are admission predicates:
            // cross-agent polling of a shared store gets no status read
            // and no notify POST.
            .filter(|parked| parked.request.instance_id == self.instance_id)
            .filter(|parked| parked.authority == ApprovalAuthority::WebhookPoll)
            .collect();
        futures::stream::iter(rows)
            .for_each_concurrent(MAX_CONCURRENT_ROW_POLLS, |parked| async move {
                // Cancellation admission: once the shutdown token fires,
                // no new row starts a request. Rows already in flight
                // complete — the never-cut contract — bounding a stop()
                // join by one request chain per admitted row.
                if token.is_cancelled() {
                    return;
                }
                self.process_row(parked).await;
            })
            .await;
    }

    /// The row's decision deadline has passed strictly on this
    /// reconciler's clock. The store keeps its own clock and stays the
    /// final arbiter at resolve; this check only withholds new requests.
    fn row_expired(&self, parked: &ParkedApproval) -> bool {
        (self.clock)() > parked.expires_at
    }

    /// One row inside a pass: the status read runs before the notify
    /// attempt, so a receiver that holds the POST open delays its own
    /// row's acknowledgment, never that row's status read, and a
    /// decided row never re-posts its request. The deadline is
    /// rechecked immediately before each request the row issues.
    async fn process_row(&self, parked: ParkedApproval) {
        let id = parked.request.decision_id;
        if self.row_expired(&parked) {
            debug!(
                decision_id = %id,
                "row's decision deadline passed; it gets no new request this pass"
            );
            return;
        }
        match self
            .client
            .poll_decision(id, parked.egress_headers.as_ref())
            .await
        {
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
                match self
                    .registry
                    .resolve(&id, ApprovalAuthority::WebhookPoll, resolved)
                    .await
                {
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
                return;
            }
            Err(err) => warn!(
                decision_id = %id,
                error = %err,
                "approval poll failed; retrying next tick"
            ),
        }
        // A row acknowledged at registration (the 207 bridge) is never
        // re-POSTed; the persisted state is the source of truth.
        if parked.acknowledgment.is_requires_notification() {
            if self.row_expired(&parked) {
                debug!(
                    decision_id = %id,
                    "row's decision deadline passed during the read; the notify POST is \
                     withheld"
                );
                return;
            }
            match self
                .client
                .notify(&parked.request, parked.egress_headers.as_ref())
                .await
            {
                Ok(()) => {
                    match self.store.mark_acknowledged(&id).await {
                        Ok(AcknowledgeOutcome::Acknowledged) => {}
                        // The row resolved, was cancelled, or was
                        // removed while the notify was in flight: it no
                        // longer needs notification, so there is
                        // nothing left to mark — the same benign race
                        // the ingress 404 is.
                        Ok(AcknowledgeOutcome::Missing) => debug!(
                            decision_id = %id,
                            "acknowledged row left the store while the notify \
                             was in flight"
                        ),
                        // The marker stays unset, so the next tick
                        // re-POSTs — at-least-once, idempotent by
                        // decision id at the receiver.
                        Err(err) => warn!(
                            decision_id = %id,
                            error = %err,
                            "acknowledgment mark failed; the row re-notifies next tick"
                        ),
                    }
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

/// How a spawned reconciler loop's task ended, as joined by
/// [`PollerHandle::stop`].
#[derive(Debug)]
pub enum PollerExit {
    /// The loop left through its cancel token and joined cleanly.
    Cancelled,
    /// The loop's task died of a panic; the join carried the panic back
    /// to the handle owner.
    Panicked,
}

/// Stop/join handle for a spawned [`PollReconciler`].
pub struct PollerHandle {
    token: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl PollerHandle {
    /// Cancel the loop and await its exit, reporting how the task ended.
    /// The in-flight tick completes first — a notify or poll request
    /// already under way is never cut. A panic join is logged here, then
    /// reported as [`PollerExit::Panicked`]; with no abort path, any other
    /// join error is warned and reported as the clean [`PollerExit::Cancelled`].
    #[must_use]
    pub async fn stop(self) -> PollerExit {
        self.token.cancel();
        match self.task.await {
            Ok(()) => PollerExit::Cancelled,
            Err(join) if join.is_panic() => {
                warn!(panic = %join, "poll reconciler loop died of a panic");
                PollerExit::Panicked
            }
            Err(join) => {
                warn!(error = %join, "poll reconciler loop ended without a panic");
                PollerExit::Cancelled
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use super::super::decision::{AgentScope, ApprovalDecision, ApprovalOrigin, DecisionId};
    use super::super::outcome::ApprovalAuthority;
    use super::super::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};
    use super::super::read_full_request;
    use super::super::registry::{AcknowledgmentState, ParkedApproval};
    use super::*;
    use crate::session_store::{FileApprovalStore, InMemoryApprovalStore, SessionStoreError};

    const INSTANCE_ID: &str = "poll-instance";

    fn poll_config() -> aura_config::HitlConfig {
        aura_config::HitlConfig {
            require_approval: vec![],
            park: aura_config::ParkConfig {
                enabled: true,
                ..Default::default()
            },
            route: aura_config::DecisionRouteConfig::Webhook {
                url: aura_config::WebhookUrl::new("http://127.0.0.1:1").unwrap(),
                timeout_secs: Some(300),
                headers: std::collections::HashMap::new(),
                headers_from_request: std::collections::HashMap::new(),
                tool_headers_from_response: aura_config::ToolHeaderMappings::default(),
                delivery: aura_config::WebhookDelivery::Poll,
                poll_url: None,
                poll_interval_secs: 1,
                poll_request_timeout_secs: 30,
                receiver_wait_timeout_secs: 900,
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
                tool_namespace: None,
                arguments: serde_json::json!({ "namespace": "prod" }),
                tool_call_intent: None,
            }],
        }
    }

    fn reconciler_with(store: Arc<dyn ApprovalStore>, url: &str) -> PollReconciler {
        reconciler_with_timeout(store, url, 30)
    }

    /// [`reconciler_with`] with an explicit poll request timeout, so the
    /// shutdown contract test observes a tight per-request bound.
    fn reconciler_with_timeout(
        store: Arc<dyn ApprovalStore>,
        url: &str,
        poll_request_timeout_secs: u64,
    ) -> PollReconciler {
        let mut config = poll_config();
        let aura_config::DecisionRouteConfig::Webhook {
            url: route_url,
            poll_request_timeout_secs: timeout,
            ..
        } = &mut config.route
        else {
            unreachable!("poll_config builds a webhook route");
        };
        *route_url = aura_config::WebhookUrl::new(url).unwrap();
        *timeout = poll_request_timeout_secs;
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
                authority: ApprovalAuthority::WebhookPoll,
                egress_headers: None,
                acknowledgment: AcknowledgmentState::RequiresNotification,
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

    async fn write_response(socket: &mut tokio::net::TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.ok();
        socket.shutdown().await.ok();
    }

    /// A test clock the test advances manually: reading the clock
    /// returns the knob's current instant (starts at `Utc::now`).
    fn adjustable_clock() -> (
        PollClock,
        Arc<std::sync::Mutex<chrono::DateTime<chrono::Utc>>>,
    ) {
        let knob = Arc::new(std::sync::Mutex::new(chrono::Utc::now()));
        let clock: PollClock = {
            let knob = Arc::clone(&knob);
            Arc::new(move || *knob.lock().unwrap())
        };
        (clock, knob)
    }

    /// Gate receiver: every connection is its own task, so held and
    /// fast rows proceed concurrently. A GET whose captured text
    /// contains a hold marker is held until the gate opens, then served
    /// `held_response`; every other request is served at once (GET 204,
    /// POST 200). Captured request texts land on the channel.
    async fn gate_receiver(
        hold_markers: Vec<String>,
        held_response: (&'static str, String),
    ) -> (
        String,
        mpsc::Receiver<String>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel(64);
        let (gate, gate_open) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                let mut gate_open = gate_open.clone();
                let markers = hold_markers.clone();
                let held = held_response.clone();
                tokio::spawn(async move {
                    let captured = read_full_request(&mut socket).await;
                    let is_get = captured.starts_with("GET ");
                    let is_held = is_get && markers.iter().any(|marker| captured.contains(marker));
                    if tx.send(captured).await.is_err() {
                        return;
                    }
                    if is_held {
                        while !*gate_open.borrow_and_update() {
                            if gate_open.changed().await.is_err() {
                                return;
                            }
                        }
                        write_response(&mut socket, held.0, &held.1).await;
                        return;
                    }
                    if is_get {
                        write_response(&mut socket, "204 No Content", "").await;
                    } else {
                        write_response(&mut socket, "200 OK", "").await;
                    }
                });
            }
        });
        (url, rx, gate)
    }

    /// Counting receiver: each connection task sleeps `hold_get` before
    /// answering a GET (204) and answers a POST (200) at once, tracking
    /// the peak number of simultaneously in-flight connections.
    async fn counting_receiver(
        hold_get: Duration,
    ) -> (
        String,
        mpsc::Receiver<String>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel(64);
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_inflight = Arc::clone(&inflight);
        let task_peak = Arc::clone(&peak);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                let inflight = Arc::clone(&task_inflight);
                let peak = Arc::clone(&task_peak);
                let hold_get = hold_get;
                tokio::spawn(async move {
                    let current = inflight.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    peak.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
                    let captured = read_full_request(&mut socket).await;
                    let is_get = captured.starts_with("GET ");
                    if tx.send(captured).await.is_err() {
                        return;
                    }
                    if is_get {
                        tokio::time::sleep(hold_get).await;
                        write_response(&mut socket, "204 No Content", "").await;
                    } else {
                        write_response(&mut socket, "200 OK", "").await;
                    }
                    inflight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                });
            }
        });
        (url, rx, peak)
    }

    /// Dual-gate receiver for the shutdown contract: every GET is held
    /// until `gate_gets` opens (then 204), every POST until
    /// `gate_posts` opens (then 200). Each connection is its own task,
    /// so reads and notifies proceed concurrently across rows.
    async fn dual_gate_receiver() -> (
        String,
        mpsc::Receiver<String>,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel(64);
        let (gate_gets, gets_open) = tokio::sync::watch::channel(false);
        let (gate_posts, posts_open) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                let mut gets_open = gets_open.clone();
                let mut posts_open = posts_open.clone();
                tokio::spawn(async move {
                    let captured = read_full_request(&mut socket).await;
                    let is_get = captured.starts_with("GET ");
                    if tx.send(captured).await.is_err() {
                        return;
                    }
                    let open = if is_get {
                        &mut gets_open
                    } else {
                        &mut posts_open
                    };
                    while !*open.borrow_and_update() {
                        if open.changed().await.is_err() {
                            return;
                        }
                    }
                    if is_get {
                        write_response(&mut socket, "204 No Content", "").await;
                    } else {
                        write_response(&mut socket, "200 OK", "").await;
                    }
                });
            }
        });
        (url, rx, gate_gets, gate_posts)
    }

    /// T1 slow-row progress: a held GET on one row does not stop other
    /// rows from being read and acknowledged in the same pass. Red
    /// pre-fill: the serial loop blocks behind the held row (the held
    /// row registers first, matching the store's insertion order).
    #[tokio::test]
    async fn tick_a_held_row_does_not_block_other_rows_in_the_pass() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let held = park_pending(&store, INSTANCE_ID).await;
        let b = park_pending(&store, INSTANCE_ID).await;
        let c = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx, gate) =
            gate_receiver(vec![held.to_string()], ("204 No Content", String::new())).await;
        let reconciler = reconciler_with(store, &url);

        let pass = tokio::spawn(async move { reconciler.tick(&CancellationToken::new()).await });
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            // Full slow-row progress: B and C must complete their notify
            // POSTs too — GETs alone would pass an implementation that
            // overlaps reads but delays every notify behind the held row.
            let mut posted_b = false;
            let mut posted_c = false;
            while !(posted_b && posted_c) {
                let captured = rx.recv().await.expect("capture channel open");
                if !captured.starts_with("POST ") {
                    continue;
                }
                posted_b |= captured.contains(&b.to_string());
                posted_c |= captured.contains(&c.to_string());
            }
        })
        .await;
        assert!(
            observed.is_ok(),
            "other rows must read and notify while one row's read is held"
        );
        gate.send(true).expect("gate channel open");
        tokio::time::timeout(Duration::from_secs(5), pass)
            .await
            .expect("pass completes once the gate opens")
            .expect("tick task joins");
    }

    /// T2 concurrency cap: with more rows than the bound, peak in-flight
    /// requests never exceed `MAX_CONCURRENT_ROW_POLLS`, and the pass
    /// genuinely overlaps rows (peak above one — this assert is the red
    /// signal; the cap alone would hold on serial code too). Red
    /// pre-fill: the serial loop peaks at exactly one.
    #[tokio::test]
    async fn tick_caps_concurrent_rows_and_overlaps_them() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        for _ in 0..(MAX_CONCURRENT_ROW_POLLS + 4) {
            park_pending(&store, INSTANCE_ID).await;
        }
        let (url, _rx, peak) = counting_receiver(Duration::from_millis(100)).await;
        let reconciler = reconciler_with(store, &url);

        tokio::time::timeout(
            Duration::from_secs(10),
            reconciler.tick(&CancellationToken::new()),
        )
        .await
        .expect("pass completes");

        let peak = peak.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            peak <= MAX_CONCURRENT_ROW_POLLS,
            "the concurrency cap holds: {peak}"
        );
        assert!(peak > 1, "rows genuinely overlap within a pass: {peak}");
    }

    /// T3 queued expiry: a row whose decision deadline passes while it
    /// is queued behind the cap is never read or notified when its turn
    /// comes, and its stored evidence is untouched — still pending,
    /// still requiring notification. The store's own clock still sees
    /// the row live (far real expiry); only the poller's clock moved.
    /// Red pre-fill: the loop has no deadline check, so the queued row
    /// is read anyway.
    #[tokio::test]
    async fn tick_skips_a_row_that_expires_while_queued() {
        let (clock, knob) = adjustable_clock();
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        for _ in 0..MAX_CONCURRENT_ROW_POLLS {
            park_pending(&store, INSTANCE_ID).await;
        }
        let queued = DecisionId::generate();
        store
            .register(ParkedApproval {
                request: parked_request(queued, INSTANCE_ID),
                registered_at: chrono::Utc::now(),
                expires_at: chrono::Utc::now() + chrono::Duration::seconds(10),
                authority: ApprovalAuthority::WebhookPoll,
                egress_headers: None,
                acknowledgment: AcknowledgmentState::RequiresNotification,
            })
            .await
            .expect("queued row registers");
        let (url, mut rx, gate) =
            gate_receiver(vec!["GET".to_string()], ("204 No Content", String::new())).await;
        let reconciler = reconciler_with(store.clone(), &url).with_clock(clock);

        let pass = tokio::spawn(async move { reconciler.tick(&CancellationToken::new()).await });
        let mut inflight_gets = 0;
        while inflight_gets < MAX_CONCURRENT_ROW_POLLS {
            let captured = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("rows reach the receiver")
                .expect("capture channel open");
            if captured.starts_with("GET ") {
                inflight_gets += 1;
            }
        }
        // Expire the queued row on the poller's clock only, before its
        // slot frees.
        *knob.lock().unwrap() = chrono::Utc::now() + chrono::Duration::seconds(11);
        gate.send(true).expect("gate channel open");
        tokio::time::timeout(Duration::from_secs(10), pass)
            .await
            .expect("pass completes")
            .expect("tick task joins");

        let still = store.get(&queued).await.unwrap().expect("queued row kept");
        assert!(
            still.acknowledgment.is_requires_notification(),
            "no store write touches the skipped row"
        );
        while let Ok(captured) = rx.try_recv() {
            assert!(
                !captured.contains(&queued.to_string()),
                "an expired queued row gets no read and no notify: {captured}"
            );
        }
    }

    /// T4 expiry between GET and POST: the status read returns NotYet
    /// after the row's decision deadline has passed — the notify POST is
    /// withheld and the row is left exactly as it was. Red pre-fill:
    /// the loop posts immediately after an undecided read.
    #[tokio::test]
    async fn tick_withholds_the_notify_post_when_the_deadline_passes_during_the_read() {
        let (clock, knob) = adjustable_clock();
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx, gate) =
            gate_receiver(vec![id.to_string()], ("204 No Content", String::new())).await;
        let reconciler = reconciler_with(store.clone(), &url).with_clock(clock);

        let pass = tokio::spawn(async move { reconciler.tick(&CancellationToken::new()).await });
        let captured = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the read reaches the receiver")
            .expect("capture channel open");
        assert!(
            captured.starts_with("GET "),
            "the read precedes the notify: {captured}"
        );
        // The deadline passes while the GET is held in flight.
        *knob.lock().unwrap() = chrono::Utc::now() + chrono::Duration::hours(2);
        gate.send(true).expect("gate channel open");
        tokio::time::timeout(Duration::from_secs(5), pass)
            .await
            .expect("pass completes")
            .expect("tick task joins");

        let leaked = rx.try_recv();
        assert!(
            leaked.is_err(),
            "no notify POST after the deadline: {leaked:?}"
        );
        let still = store.get(&id).await.unwrap().expect("row kept");
        assert!(
            still.acknowledgment.is_requires_notification(),
            "the withheld POST leaves the row unacknowledged"
        );
    }

    /// T5 late response: a decided read that lands after the poller's
    /// clock passed the deadline still flows to the store's atomic
    /// arbitration — the store's own deadline and ownership checks
    /// decide, never the poller. The row resolves through the decided
    /// path and never posts its request. Contract pin, green on both
    /// sides of the fill.
    #[tokio::test]
    async fn tick_forwards_a_late_decided_read_to_the_store_arbitration() {
        let (clock, knob) = adjustable_clock();
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx, gate) = gate_receiver(
            vec![id.to_string()],
            ("200 OK", r#"{"approved":true}"#.to_string()),
        )
        .await;
        let reconciler = reconciler_with(store.clone(), &url).with_clock(clock);

        let pass = tokio::spawn(async move { reconciler.tick(&CancellationToken::new()).await });
        let captured = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the read reaches the receiver")
            .expect("capture channel open");
        assert!(captured.starts_with("GET "), "the read is held: {captured}");
        // The deadline passes on the poller's clock while the read is
        // in flight; the decided response lands after it.
        *knob.lock().unwrap() = chrono::Utc::now() + chrono::Duration::hours(2);
        gate.send(true).expect("gate channel open");
        tokio::time::timeout(Duration::from_secs(5), pass)
            .await
            .expect("pass completes")
            .expect("tick task joins");

        let decision = store_decision(&store, &id)
            .await
            .expect("the store arbitrates the late decision");
        assert!(
            matches!(decision, ResolvedDecision::Approved { .. }),
            "the decision recorded is the polled one"
        );
        let leaked = rx.try_recv();
        assert!(
            leaked.is_err(),
            "a decided row never posts its request: {leaked:?}"
        );
    }

    /// T6 shutdown: cancelling mid-pass starts no new rows — the rows
    /// queued behind the cap issue no request — while a stop() join
    /// waits out the admitted rows' full in-flight chains: the held
    /// read plus the notify POST after it, each leg bounded by the
    /// short configured request timeout. Red pre-fill: the serial loop
    /// keeps processing the remaining rows inside the same pass after
    /// cancellation.
    #[tokio::test]
    async fn cancel_mid_pass_starts_no_new_rows_and_joins_promptly() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let mut ids = Vec::new();
        for _ in 0..(MAX_CONCURRENT_ROW_POLLS + 4) {
            ids.push(park_pending(&store, INSTANCE_ID).await);
        }
        let (url, mut rx, gate_gets, gate_posts) = dual_gate_receiver().await;
        let shutdown = CancellationToken::new();
        let reconciler = reconciler_with_timeout(store, &url, 2);
        let handle = reconciler.spawn(&shutdown);

        // Let the cap fill with held reads BEFORE stopping: stop()
        // cancels on call, and the admission checks would bar the rows
        // the cap has not admitted yet.
        let mut captures: Vec<String> = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while captures.iter().filter(|c| c.starts_with("GET ")).count()
                < MAX_CONCURRENT_ROW_POLLS
            {
                captures.push(rx.recv().await.expect("capture channel open"));
            }
        })
        .await
        .expect("the cap fills with held reads");
        // Now stop: the cancel lands mid-pass with the cap full.
        let mut stop_task = tokio::spawn(async { handle.stop().await });
        shutdown.cancel();
        // Release the reads; the admitted rows move to their notify
        // POSTs, which stay held.
        gate_gets.send(true).expect("gets gate open");
        tokio::time::timeout(Duration::from_secs(3), async {
            while captures.iter().filter(|c| c.starts_with("POST ")).count()
                < MAX_CONCURRENT_ROW_POLLS
            {
                captures.push(rx.recv().await.expect("capture channel open"));
            }
        })
        .await
        .expect("the admitted rows reach their notify POSTs");
        // The join waits out the in-flight POST chains: stop has not
        // returned while they are held, well inside the 2s request
        // timeout, so the pending state is the held POST, not a slow
        // leg.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !stop_task.is_finished(),
            "stop() joins only after the in-flight POST chains complete"
        );
        gate_posts.send(true).expect("posts gate open");
        tokio::time::timeout(Duration::from_secs(5), &mut stop_task)
            .await
            .expect("stop must not hang")
            .expect("stop task joins");
        while let Ok(captured) = rx.try_recv() {
            captures.push(captured);
        }

        let read_ids = ids
            .iter()
            .filter(|id| {
                captures
                    .iter()
                    .any(|c| c.starts_with("GET ") && c.contains(&id.to_string()))
            })
            .count();
        let posted = captures.iter().filter(|c| c.starts_with("POST ")).count();
        assert_eq!(
            read_ids, MAX_CONCURRENT_ROW_POLLS,
            "exactly the admitted rows were read; queued rows issue no request after cancel"
        );
        assert_eq!(
            posted, MAX_CONCURRENT_ROW_POLLS,
            "exactly the admitted rows notified; queued rows issue no request after cancel"
        );
    }

    /// T7 pass non-overlap: while a pass is in flight on a row, no
    /// second pass re-reads that row — the tick interval (1s here)
    /// fires during the held pass and must not start another one.
    /// Reliance note: this pin is green by structure on both sides of
    /// the fill — `run` awaits each pass inside `select!` with
    /// `MissedTickBehavior::Delay`, so passes cannot overlap by
    /// construction; the test exists to fail loudly if that loop shape
    /// is ever changed.
    #[tokio::test]
    async fn a_pass_in_flight_blocks_the_next_pass_for_its_row() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx, gate) =
            gate_receiver(vec!["GET".to_string()], ("204 No Content", String::new())).await;
        let shutdown = CancellationToken::new();
        let reconciler = reconciler_with(store, &url);
        let handle = reconciler.spawn(&shutdown);

        // First pass: the row's read is held. Interval ticks at 1s and
        // 2s fire during the hold; none may re-read the row.
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the first pass reads the row")
            .expect("capture channel open");
        assert!(
            first.starts_with("GET "),
            "the pass reads status first: {first}"
        );
        assert!(
            first.contains(&id.to_string()),
            "the held read belongs to the parked row: {first}"
        );
        let second_during_hold =
            tokio::time::timeout(Duration::from_millis(2_500), rx.recv()).await;
        assert!(
            second_during_hold.is_err(),
            "no second read while the pass is in flight"
        );
        gate.send(true).expect("gate channel open");
        tokio::time::timeout(Duration::from_secs(5), handle.stop())
            .await
            .expect("stop must not hang");
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
            poll_decided(r#"{"approved":true}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);

        reconciler.tick(&CancellationToken::new()).await;
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

        reconciler.tick(&CancellationToken::new()).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));
        let retried = rx.recv().await.unwrap();
        assert!(
            retried.starts_with("POST "),
            "an unacked id retries the POST: {retried}"
        );

        reconciler.tick(&CancellationToken::new()).await;
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
    /// the approval still resolves through the GET alone, which also never
    /// re-posts the request once the decision is visible.
    #[tokio::test]
    async fn tick_resolves_via_the_status_get_while_notify_never_acks() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let id = park_pending(&store, INSTANCE_ID).await;
        let (url, mut rx) = scripted_receiver(vec![
            poll_pending(),
            ("503 Service Unavailable", String::new()),
            poll_decided(r#"{"approved":true}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);

        reconciler.tick(&CancellationToken::new()).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));
        assert!(rx.recv().await.unwrap().starts_with("POST "));

        reconciler.tick(&CancellationToken::new()).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));

        assert_eq!(
            store_decision(&store, &id).await,
            Some(ResolvedDecision::from(ApprovalDecision::Approved)),
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

        reconciler.tick(&CancellationToken::new()).await;
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
            poll_decided(r#"{"approved":true}"#), // the approving ack body
            poll_decided(r#"{"approved":true}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);

        reconciler.tick(&CancellationToken::new()).await;
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

        reconciler.tick(&CancellationToken::new()).await;
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
        let (url, mut rx) = scripted_receiver(vec![
            poll_pending(),
            ack_ok(),
            poll_decided(r#"{"approved":true}"#),
        ])
        .await;
        let reconciler = reconciler_with(store.clone(), &url);

        reconciler.tick(&CancellationToken::new()).await;
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

        reconciler.tick(&CancellationToken::new()).await;
        assert!(rx.recv().await.unwrap().starts_with("GET "));

        assert_eq!(
            store_decision(&store, &own).await,
            Some(ResolvedDecision::from(ApprovalDecision::Approved)),
        );
        assert!(
            store.get(&other).await.unwrap().is_some(),
            "the other instance's row stays untouched"
        );
    }

    /// `from_config` gates the production spawn on the admitted parking
    /// route: webhook poll delivery with park mode enabled.
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
    /// and notifies, marking the row acknowledged durably; after a reboot
    /// the fresh reconciler reads the persisted acknowledgment and never
    /// re-POSTs the row, resolving it through the pinned GET alone.
    #[tokio::test]
    async fn restart_does_not_repost_the_acknowledged_row_and_resolves_after_reboot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let (url, mut rx) = scripted_receiver(vec![
            poll_pending(),
            ack_ok(),
            poll_pending(),
            poll_decided(r#"{"approved":true}"#),
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
                    authority: ApprovalAuthority::WebhookPoll,
                    egress_headers: None,
                    acknowledgment: AcknowledgmentState::RequiresNotification,
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
        let _ = handle_a.stop().await;

        // Boot two: a fresh reconciler over the same store root — the
        // acknowledgment boot one's notify persisted is durable, so the
        // reboot never re-POSTs the row and resolves it through the pinned
        // GET alone. Its registry resolves over the same store, as the
        // ingress handler's would.
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
        assert!(
            rx.try_recv().is_err(),
            "the reboot never re-POSTs the acknowledged row"
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

    /// A row born notified (acknowledgment = `Acknowledged`, as the 207
    /// bridge registers it) is never re-POSTed: the reconciler reads the
    /// persisted state and skips the notify, polling only.
    #[tokio::test]
    async fn born_notified_row_is_never_reposted() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let request = parked_request(DecisionId::generate(), INSTANCE_ID);
        store
            .register(ParkedApproval {
                request,
                registered_at: chrono::Utc::now(),
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
                authority: ApprovalAuthority::WebhookPoll,
                egress_headers: None,
                acknowledgment: AcknowledgmentState::Acknowledged,
            })
            .await
            .expect("a born-notified row registers");

        let (url, mut rx) = scripted_receiver(vec![poll_pending()]).await;
        let reconciler = reconciler_with(store.clone(), &url);

        reconciler.tick(&CancellationToken::new()).await;
        let read = rx.recv().await.unwrap();
        assert!(
            read.starts_with("GET "),
            "the born-notified row polls: {read}"
        );
        assert!(
            rx.try_recv().is_err(),
            "no notify POST may follow the read of a born-notified row"
        );
    }

    /// A born-notified row stays un-reposted across a simulated restart: a
    /// fresh reconciler over the same file store reads the persisted
    /// acknowledgment state and still never POSTs.
    #[tokio::test]
    async fn born_notified_row_survives_a_restart_without_a_repost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let store_a: Arc<dyn ApprovalStore> = Arc::new(FileApprovalStore::open(&path).unwrap());
        let request = parked_request(DecisionId::generate(), INSTANCE_ID);
        store_a
            .register(ParkedApproval {
                request,
                registered_at: chrono::Utc::now(),
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
                authority: ApprovalAuthority::WebhookPoll,
                egress_headers: None,
                acknowledgment: AcknowledgmentState::Acknowledged,
            })
            .await
            .expect("a born-notified row registers");

        let (url, mut rx) = scripted_receiver(vec![poll_pending(), poll_pending()]).await;

        let reconciler_a = reconciler_with(store_a.clone(), &url);
        reconciler_a.tick(&CancellationToken::new()).await;
        let read = rx.recv().await.unwrap();
        assert!(read.starts_with("GET "), "boot one polls: {read}");
        assert!(rx.try_recv().is_err(), "no POST on boot one");

        // A fresh reconciler over the same store root (the restart).
        let store_b: Arc<dyn ApprovalStore> = Arc::new(FileApprovalStore::open(&path).unwrap());
        let reconciler_b = reconciler_with(store_b.clone(), &url);
        reconciler_b.tick(&CancellationToken::new()).await;
        let read = rx.recv().await.unwrap();
        assert!(
            read.starts_with("GET "),
            "boot two polls, never POSTs: {read}"
        );
        assert!(rx.try_recv().is_err(), "no POST on boot two");
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
    // Loop lifecycle: panic observation + stop/join surfacing
    // ====================================================================

    /// A store whose pending scan panics: the seam that drives the
    /// spawned loop's task into a panic on its first tick. The interval's
    /// first tick fires immediately and the token is fresh, so the tick
    /// arm is the only ready select branch — the panic lands before any
    /// cancel exists to race it.
    struct PanickingScanStore;

    #[async_trait]
    impl ApprovalStore for PanickingScanStore {
        async fn list_pending(&self) -> Result<Vec<ParkedApproval>, SessionStoreError> {
            panic!("list_pending blew up (test seam)");
        }
        async fn register(&self, _parked: ParkedApproval) -> Result<(), SessionStoreError> {
            unreachable!("the panicking-scan battery never registers");
        }
        async fn mark_acknowledged(
            &self,
            _id: &DecisionId,
        ) -> Result<crate::session_store::AcknowledgeOutcome, SessionStoreError> {
            unreachable!("the panicking-scan battery never acknowledges");
        }
        async fn get(&self, _id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
            unreachable!("the panicking-scan battery never reads a row");
        }
        async fn resolve(
            &self,
            _id: &DecisionId,
            _expected_authority: crate::hitl::ApprovalAuthority,
            _decision: ResolvedDecision,
        ) -> Result<(), ResolveError> {
            unreachable!("the panicking-scan battery never resolves");
        }
        async fn read_or_expire(
            &self,
            _id: &DecisionId,
            _expected_authority: crate::hitl::ApprovalAuthority,
        ) -> Result<crate::hitl::ApprovalRead, SessionStoreError> {
            unreachable!("the panicking-scan battery never reads a row");
        }
        async fn retained_rows(
            &self,
        ) -> Result<Vec<crate::session_store::RetainedApproval>, SessionStoreError> {
            unreachable!("the panicking-scan battery never scans retention");
        }
        async fn decision(
            &self,
            _id: &DecisionId,
        ) -> Result<Option<ResolvedDecision>, SessionStoreError> {
            unreachable!("the panicking-scan battery never reads a decision");
        }
        async fn remove(&self, _id: &DecisionId) -> Result<(), SessionStoreError> {
            unreachable!("the panicking-scan battery never removes");
        }
        async fn cancel_request(
            &self,
            _request_id: &str,
        ) -> Result<Vec<ParkedApproval>, SessionStoreError> {
            unreachable!("the panicking-scan battery never cancels");
        }
    }

    /// A loop whose tick panics must be observable from its handle: the
    /// task is dead before any cancel fires, so the join inside `stop`
    /// carries a panic — the handle must report [`PollerExit::Panicked`],
    /// never a clean [`PollerExit::Cancelled`]. Nothing today records the
    /// join outcome, so this pins red until the fill implements the
    /// contract on `stop`.
    #[tokio::test]
    async fn stop_reports_a_loop_that_died_of_a_panic_as_a_panic() {
        let store: Arc<dyn ApprovalStore> = Arc::new(PanickingScanStore);
        let reconciler = reconciler_with(store, "http://127.0.0.1:1");
        let handle = reconciler.spawn(&CancellationToken::new());

        tokio::time::timeout(Duration::from_secs(5), async {
            while !handle.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the panicking first tick must finish the task");

        let exit = handle.stop().await;
        assert!(
            matches!(exit, PollerExit::Panicked),
            "a loop dead of a panic must surface as Panicked, got {exit:?}"
        );
    }

    /// The other side of the exit contract: a loop that leaves through
    /// its cancel token — never having panicked — reports
    /// [`PollerExit::Cancelled`], so a panicked join can never hide among
    /// clean exits.
    #[tokio::test]
    async fn stop_reports_a_cleanly_cancelled_loop_as_cancelled() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let reconciler = reconciler_with(store, "http://127.0.0.1:1");
        let handle = reconciler.spawn(&CancellationToken::new());

        let exit = handle.stop().await;
        assert!(
            matches!(exit, PollerExit::Cancelled),
            "a cleanly cancelled loop must report Cancelled, got {exit:?}"
        );
    }

    /// `from_config` refuses a zero poll interval loudly, matching the
    /// documented-panic precedent of the webhook-timeout resolver: config
    /// admission already rejects zero at validation, and the reconciler
    /// is the defensive backstop — no silent clamp to some other
    /// interval, and no silent `None` that disables the reconciler. The
    /// panic names the config admission contract it is backing up.
    #[test]
    #[should_panic(expected = "`hitl.route.poll_interval_secs` must be greater than zero")]
    fn from_config_refuses_a_zero_poll_interval_loudly() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let registry = PendingApprovals::new();
        let mut config = poll_config();
        if let aura_config::DecisionRouteConfig::Webhook {
            poll_interval_secs, ..
        } = &mut config.route
        {
            *poll_interval_secs = 0;
        }
        let _ =
            PollReconciler::from_config(&config, None, INSTANCE_ID.to_string(), store, &registry);
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
                park: aura_config::ParkConfig {
                    enabled: true,
                    ..Default::default()
                },
                route: aura_config::DecisionRouteConfig::Webhook {
                    url: aura_config::WebhookUrl::new("http://127.0.0.1:1").unwrap(),
                    timeout_secs: Some(300),
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
                    receiver_wait_timeout_secs: 900,
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
                    authority: ApprovalAuthority::WebhookPoll,
                    egress_headers: Some(headers),
                    acknowledgment: AcknowledgmentState::RequiresNotification,
                })
                .await
                .expect("pending approval registers");
            id
        }

        /// Sibling of [`park_row`] for a BORN-ACKNOWLEDGED durable row —
        /// the truthful post-207 state, `WebhookPoll` authority plus
        /// `AcknowledgmentState::Acknowledged` — carrying `egress` as its
        /// resolved authorization value. Existing `park_row` callers are
        /// untouched.
        async fn park_acknowledged_row(store: &Arc<dyn ApprovalStore>, egress: &str) -> DecisionId {
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
                    authority: ApprovalAuthority::WebhookPoll,
                    egress_headers: Some(headers),
                    acknowledgment: AcknowledgmentState::Acknowledged,
                })
                .await
                .expect("acknowledged approval registers");
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

        /// Content-addressed receiver for the bounded-fan-out era:
        /// responses key on (row, method) state, never connection
        /// position, so a pass may interleave its rows freely. The
        /// `flaky` row's POST fails (503) its first attempt and acks
        /// (200) on every later one; any other row's POST acks at once;
        /// GETs read pending (204) until BOTH rows' POSTs have acked,
        /// then read decided (200, approved).
        async fn row_state_receiver(flaky: DecisionId) -> (String, mpsc::Receiver<String>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = mpsc::channel(16);
            let flaky_acked = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let other_acked = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flaky_posted = Arc::new(std::sync::atomic::AtomicBool::new(false));
            tokio::spawn({
                let flaky = flaky.to_string();
                async move {
                    loop {
                        let Ok((mut socket, _)) = listener.accept().await else {
                            return;
                        };
                        let tx = tx.clone();
                        let flaky = flaky.clone();
                        let flaky_acked = Arc::clone(&flaky_acked);
                        let other_acked = Arc::clone(&other_acked);
                        let flaky_posted = Arc::clone(&flaky_posted);
                        tokio::spawn(async move {
                            let captured = read_full_request(&mut socket).await;
                            let is_get = captured.starts_with("GET ");
                            let mentions_flaky = captured.contains(&flaky);
                            if tx.send(captured).await.is_err() {
                                return;
                            }
                            let (status, body) = if is_get {
                                if flaky_acked.load(std::sync::atomic::Ordering::SeqCst)
                                    && other_acked.load(std::sync::atomic::Ordering::SeqCst)
                                {
                                    ("200 OK", r#"{"approved":true}"#.to_string())
                                } else {
                                    ("204 No Content", String::new())
                                }
                            } else if mentions_flaky {
                                if flaky_posted.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                    flaky_acked.store(true, std::sync::atomic::Ordering::SeqCst);
                                    ("200 OK", String::new())
                                } else {
                                    ("503 Service Unavailable", String::new())
                                }
                            } else {
                                other_acked.store(true, std::sync::atomic::Ordering::SeqCst);
                                ("200 OK", String::new())
                            };
                            write_response(&mut socket, status, &body).await;
                        });
                    }
                }
            });
            (url, rx)
        }

        /// Distinct-credential approvals keep their own headers through
        /// registration, notify retry, and store reopen: each notify POST —
        /// including the retried one — authenticates with ITS row's value,
        /// never the sibling's and never the client's static fallback.
        /// Scripted content-addressed (see [`row_state_receiver`]): the
        /// alpha row's notify fails once and retries; assertions key on
        /// each capture's own row, not its position in the pass.
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
            let (url, mut rx) = row_state_receiver(alpha).await;
            let reconciler = reconciler_from(&config, Arc::clone(&reader), &url);

            let drain = |rx: &mut mpsc::Receiver<String>| {
                let mut captured = Vec::new();
                while let Ok(next) = rx.try_recv() {
                    captured.push(next);
                }
                captured
            };
            let assert_own_value = |captured: &str| {
                let id = body_decision_id(captured);
                let header_line = captured
                    .lines()
                    .find(|line| line.to_lowercase().starts_with("authorization:"))
                    .unwrap_or_else(|| {
                        panic!("every notify POST carries its row header: {captured}")
                    });
                assert_eq!(
                    header_line.split_once(':').expect("header line").1.trim(),
                    expected[&id],
                    "each notify POST authenticates with its own row's value: {captured}"
                );
                assert!(
                    !captured.contains("Bearer client-static"),
                    "the client's static value must never leak beside the row's: {captured}"
                );
            };

            // Pass one: both rows read status and attempt their notify —
            // alpha's fails (503), beta's acks.
            reconciler.tick(&CancellationToken::new()).await;
            let first_pass = drain(&mut rx);
            assert_eq!(
                first_pass.len(),
                4,
                "both rows read and notify: {first_pass:?}"
            );
            assert_eq!(
                first_pass.iter().filter(|c| c.starts_with("GET ")).count(),
                2,
                "each row reads status first: {first_pass:?}"
            );
            let alpha_posts: Vec<&String> = first_pass
                .iter()
                .filter(|c| c.starts_with("POST ") && c.contains(&alpha.to_string()))
                .collect();
            let beta_posts: Vec<&String> = first_pass
                .iter()
                .filter(|c| c.starts_with("POST ") && c.contains(&beta.to_string()))
                .collect();
            assert_eq!(alpha_posts.len(), 1, "alpha notifies once: {first_pass:?}");
            assert_eq!(beta_posts.len(), 1, "beta notifies once: {first_pass:?}");
            for captured in first_pass.iter().filter(|c| c.starts_with("POST ")) {
                assert_own_value(captured);
            }

            // Pass two: alpha's failed notify retries with its own value;
            // beta's marker skips its POST.
            reconciler.tick(&CancellationToken::new()).await;
            let second_pass = drain(&mut rx);
            let alpha_retries: Vec<&String> = second_pass
                .iter()
                .filter(|c| c.starts_with("POST ") && c.contains(&alpha.to_string()))
                .collect();
            assert_eq!(
                alpha_retries.len(),
                1,
                "the failed notify retries: {second_pass:?}"
            );
            assert_own_value(alpha_retries[0]);
            assert!(
                !second_pass
                    .iter()
                    .any(|c| c.starts_with("POST ") && c.contains(&beta.to_string())),
                "beta's marker skips its POST: {second_pass:?}"
            );

            // Pass three: the reads resolve both rows durably.
            reconciler.tick(&CancellationToken::new()).await;
            let third_pass = drain(&mut rx);
            assert!(
                third_pass.iter().all(|c| c.starts_with("GET ")),
                "both rows resolve on the read alone: {third_pass:?}"
            );

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
            let (url, mut rx) = scripted_receiver_with_headers(vec![(
                "200 OK",
                vec![("x-approver-id", IDENTITY_SENTINEL)],
                r#"{"approved":true}"#,
            )])
            .await;
            let reconciler = reconciler_from(&config, store.clone(), &url);

            reconciler.tick(&CancellationToken::new()).await;
            let poll = rx.recv().await.unwrap();
            assert!(poll.starts_with("GET "), "the status read: {poll}");
            // The GET carries the row's own egress value (per-row poll
            // credentials, H2); identity is response-side only and never
            // rides the request.
            assert!(
                poll.contains(EGRESS_ALPHA) && !poll.contains(IDENTITY_SENTINEL),
                "the status GET carries the row's egress value and no identity: {poll}"
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
            let (url, mut rx) =
                scripted_receiver_with_headers(vec![("200 OK", vec![], r#"{"approved":true}"#)])
                    .await;
            let reconciler = reconciler_from(&config, store.clone(), &url);

            reconciler.tick(&CancellationToken::new()).await;
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
                    authority: ApprovalAuthority::WebhookPoll,
                    egress_headers: Some(egress),
                    acknowledgment: AcknowledgmentState::RequiresNotification,
                })
                .await
                .unwrap();

            // Watch the decision bus before any traffic. The reconciler
            // owns no run observer and has no lifecycle-emission path,
            // so no event channel can carry a sentinel.
            let mut bus_sub = bus.subscribe(&format!("approval:{id}")).await.unwrap();

            // Tracing capture around the whole reconcile: strict DEBUG, so
            // every warn/error/debug line the flow could emit is checked.
            // The thread-local default reaches the awaited tick on this
            // single-thread test runtime.
            let log_buf = Arc::new(CaptureLog(std::sync::Mutex::new(Vec::<u8>::new())));

            let config = poll_config_with(HashMap::new(), true);
            let (url, mut rx) = scripted_receiver_with_headers(vec![
                ("204 No Content", vec![], ""),
                ("200 OK", vec![], ""),
                (
                    "200 OK",
                    vec![("x-approver-id", IDENTITY)],
                    r#"{"approved":true}"#,
                ),
            ])
            .await;
            let reconciler = reconciler_over(&config, store.clone(), &url, &registry);
            let subscriber = tracing_subscriber::fmt()
                .with_writer(Arc::clone(&log_buf))
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .finish();
            let notify = {
                let _log_guard = tracing::subscriber::set_default(subscriber);
                reconciler.tick(&CancellationToken::new()).await;
                let _read = rx.recv().await.unwrap();
                let notify = rx.recv().await.unwrap();
                reconciler.tick(&CancellationToken::new()).await;
                let _decided = rx.recv().await.unwrap();
                notify
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

            // Lifecycle/SSE events: the reconciler owns no observer and
            // no emission path, so nothing can carry a sentinel; the bus
            // and tracing guards below carry the leak checks.

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
                park_ttl: aura_config::ParkTtl::default(),
                identity_hash: None,
            };
            crate::orchestration::commit_from_run_state(&inputs, None)
                .await
                .expect("the park commit publishes");

            let parked_doc = memory_dir.join("parked").join(format!("{run_id}.json"));
            let parked_text = std::fs::read_to_string(&parked_doc).expect("parked document");
            assert!(
                !parked_text.contains(EGRESS) && !parked_text.contains(IDENTITY),
                "the parked document must never carry approval credentials, got: {parked_text}"
            );

            let handle = crate::orchestration::ResumingDocumentHandle::open(&parked_doc, None)
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
        }

        /// Every born-acknowledged row's poll GET carries THAT row's
        /// `egress_headers` — the poller passes each row's own values
        /// into the per-row call, replacing the reconciler client's
        /// static fallback for that one request. Each capture maps to
        /// its row through the GET's own `decision_id` query param, so
        /// the assert holds regardless of within-tick row order.
        #[tokio::test]
        async fn polled_get_carries_each_rows_egress_headers() {
            let dir = tempfile::tempdir().unwrap();
            let store_root = dir.path().join("approvals");

            // Registration: two born-acknowledged rows with distinct
            // resolved values on a file store.
            let writer: Arc<dyn ApprovalStore> =
                Arc::new(FileApprovalStore::open(&store_root).unwrap());
            let alpha = park_acknowledged_row(&writer, EGRESS_ALPHA).await;
            let beta = park_acknowledged_row(&writer, EGRESS_BETA).await;
            let expected: HashMap<String, String> = HashMap::from([
                (alpha.to_string(), EGRESS_ALPHA.to_string()),
                (beta.to_string(), EGRESS_BETA.to_string()),
            ]);

            // The reconciler's view: a fresh handle over the same root.
            let reader: Arc<dyn ApprovalStore> =
                Arc::new(FileApprovalStore::open(&store_root).unwrap());

            // The client's static value differs from both rows' values, so
            // a per-row override failure is visible.
            let mut static_headers = HashMap::new();
            static_headers.insert(
                "authorization".to_string(),
                "Bearer client-static".to_string(),
            );
            let config = poll_config_with(static_headers, false);
            let (url, mut rx) = scripted_receiver_with_headers(vec![
                // Both rows poll pending; identical entries, so the
                // within-tick row order cannot change the script's shape.
                ("207 Multi-Status", vec![], ""),
                ("207 Multi-Status", vec![], ""),
            ])
            .await;
            let reconciler = reconciler_from(&config, Arc::clone(&reader), &url);

            reconciler.tick(&CancellationToken::new()).await;
            for _ in 0..2 {
                let captured = rx.recv().await.unwrap();
                assert!(
                    captured.starts_with("GET "),
                    "each born-acknowledged row reads status, never POSTs: {captured}"
                );
                let row_id = captured
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|target| target.split("decision_id=").nth(1))
                    .and_then(|rest| rest.split([' ', '&']).next())
                    .expect("the poll GET carries its row's decision id")
                    .to_string();
                let header_line = captured
                    .lines()
                    .find(|line| line.to_lowercase().starts_with("authorization:"))
                    .unwrap_or_else(|| panic!("every poll GET carries a row header: {captured}"));
                assert_eq!(
                    header_line.split_once(':').expect("header line").1.trim(),
                    expected[&row_id],
                    "each poll GET authenticates with ITS row's value: {captured}"
                );
                assert!(
                    !captured.contains("Bearer client-static"),
                    "the client's static fallback must never leak beside the row's: {captured}"
                );
            }
        }

        /// Restart leg: the persisted `Acknowledged` marker is the source
        /// of truth (the 207 registration IS the acknowledgment), so a
        /// fresh reconciler over the reopened store neither re-POSTs the
        /// row nor drops its credentials: both GETs carry the row's
        /// value, zero POSTs occur, and the row resolves durably on the
        /// decided 200.
        #[tokio::test]
        async fn polled_get_keeps_row_headers_across_a_restart_and_never_reposts() {
            let dir = tempfile::tempdir().unwrap();
            let store_root = dir.path().join("approvals");

            // The 207 bridge registered the row acknowledged; its
            // egress values persist with it.
            let writer: Arc<dyn ApprovalStore> =
                Arc::new(FileApprovalStore::open(&store_root).unwrap());
            let id = park_acknowledged_row(&writer, EGRESS_ALPHA).await;

            // The restart: a fresh handle over the same store root — the
            // same reopen shape as
            // `distinct_rows_keep_their_own_headers_through_retry_and_reopen`.
            let store_b: Arc<dyn ApprovalStore> =
                Arc::new(FileApprovalStore::open(&store_root).unwrap());

            let mut static_headers = HashMap::new();
            static_headers.insert(
                "authorization".to_string(),
                "Bearer client-static".to_string(),
            );
            let config = poll_config_with(static_headers, false);
            let (url, mut rx) = scripted_receiver_with_headers(vec![
                // Tick 1: the restarted row reads status (pending).
                ("207 Multi-Status", vec![], ""),
                // Tick 2: the decided answer resolves the row.
                ("200 OK", vec![], r#"{"approved":true}"#),
            ])
            .await;
            let reconciler = reconciler_from(&config, Arc::clone(&store_b), &url);

            reconciler.tick(&CancellationToken::new()).await;
            let first = rx.recv().await.unwrap();
            reconciler.tick(&CancellationToken::new()).await;
            let second = rx.recv().await.unwrap();

            assert!(first.starts_with("GET "), "boot two polls: {first}");
            assert!(second.starts_with("GET "), "tick two polls: {second}");
            assert!(
                rx.try_recv().is_err(),
                "zero POST captures across both ticks: the acknowledged row is never re-POSTed"
            );
            for captured in [&first, &second] {
                let header_line = captured
                    .lines()
                    .find(|line| line.to_lowercase().starts_with("authorization:"))
                    .unwrap_or_else(|| panic!("every poll GET carries the row header: {captured}"));
                assert_eq!(
                    header_line.split_once(':').expect("header line").1.trim(),
                    EGRESS_ALPHA,
                    "the restarted row's GET keeps its stored egress credential: {captured}"
                );
                assert!(
                    !captured.contains("Bearer client-static"),
                    "the client's static fallback must be replaced: {captured}"
                );
            }

            // The row resolved durably and left the pending scan.
            assert_eq!(
                store_decision(&store_b, &id).await,
                Some(ResolvedDecision::from(ApprovalDecision::Approved)),
                "the restarted row resolves through the pinned GET",
            );
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
        }
    }

    // =============================================================    }
}
