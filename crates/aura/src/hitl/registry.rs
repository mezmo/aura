//! The registry that parks conversational approvals (Route B) and the decision
//! a later request resolves them with.
//!
//! This is the first mutable chat-path state that crosses request boundaries:
//! an approval is registered during one request's stream and resolved by a
//! `POST /v1/approvals/{id}` arriving as a different request. It follows the A2A
//! global static.
//!
//! State is split along the serialization boundary:
//!
//! - the [`ParkedApproval`] record and the resolved decision live in an
//!   [`ApprovalStore`] (the durable carrier), and the decision also travels
//!   over an [`EventBus`] topic (the low-latency wake), so with a shared
//!   backend a decision can be resolved by any process and survives a lost
//!   publish; while
//! - the `oneshot` wake handle that resumes the suspended tool call is
//!   inherently process-local and stays in this registry's in-RAM map, fired
//!   by a per-approval task listening on the bus and polling the store.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::warn;

use crate::session_store::{
    ApprovalStore, EventBus, InMemoryApprovalStore, InMemoryEventBus, SessionStoreError,
    Subscription,
};

use super::decision::{ApprovalDecision, AwaitingDecision, DecisionId, Timestamp};
use super::protocol::ApprovalRequest;

/// Bus topic carrying the decision for one parked approval.
fn approval_topic(id: &DecisionId) -> String {
    format!("approval:{id}")
}

/// How often a parked approval's wake task falls back to reading the store
/// for a recorded decision.
const DECISION_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The cross-request registry of parked conversational approvals. A `Clone`
/// newtype over an `Arc`.
#[derive(Clone)]
pub struct PendingApprovals(Arc<PendingApprovalsInner>);

struct PendingApprovalsInner {
    store: Arc<dyn ApprovalStore>,
    bus: Arc<dyn EventBus>,
    // Process-local wake handles. `std::sync::Mutex`: every operation is a
    // synchronous map op; nothing awaits while holding the lock.
    wakes: Mutex<BTreeMap<DecisionId, WakeEntry>>,
}

/// The process-local half of one parked approval.
struct WakeEntry {
    request_id: String,
    wake: oneshot::Sender<ApprovalDecision>,
    wake_task: AbortHandle,
}

impl WakeEntry {
    /// Stop the wake task (bus listener + store poll).
    fn abort_wake_task(&self) {
        self.wake_task.abort();
    }
}

/// The serializable record of a parked approval. Carries everything needed to
/// re-render and re-validate the approval after a restart.
#[derive(Clone)]
pub struct ParkedApproval {
    pub request: ApprovalRequest,
    pub registered_at: Timestamp,
    pub expires_at: Timestamp,
}

/// Why a [`PendingApprovals::resolve`] could not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveError {
    /// No live entry for that id: unknown, already resolved, or expired all
    /// collapse to a missing store entry.
    NotFound,
    /// The backing session store failed; no decision was recorded.
    Store(SessionStoreError),
}

impl PendingApprovals {
    /// Create a registry over the in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self::with_backend(
            Arc::new(InMemoryApprovalStore::new()),
            Arc::new(InMemoryEventBus::new()),
        )
    }

    /// Create a registry over an explicit store/bus backend.
    #[must_use]
    pub fn with_backend(store: Arc<dyn ApprovalStore>, bus: Arc<dyn EventBus>) -> Self {
        Self(Arc::new(PendingApprovalsInner {
            store,
            bus,
            wakes: Mutex::new(BTreeMap::new()),
        }))
    }

    /// The approval store this registry parks into.
    #[must_use]
    pub fn store(&self) -> Arc<dyn ApprovalStore> {
        Arc::clone(&self.0.store)
    }

    /// Park an approval, returning the await handle.
    ///
    /// Store or bus faults do not fail registration: the call parks anyway.
    /// An unpersisted park cannot be resolved and fails closed at its
    /// timeout.
    ///
    /// The wake is wired before the store insert: once `store.register`
    /// returns, any process may resolve and publish, and the wake must
    /// already be listening for the decision.
    #[must_use]
    pub async fn register(&self, request: ApprovalRequest, timeout: Duration) -> AwaitingDecision {
        let (parked, id, request_id) = Self::parked_record(request, timeout);
        let handle = self.wire_wake(id, request_id, timeout).await;
        if let Err(err) = self.0.store.register(parked).await {
            warn!(
                decision_id = %id, error = %err,
                "parked approval not persisted; it cannot be resolved and will fail closed",
            );
        }
        handle
    }

    /// Park an approval durably, failing closed on a store fault: the
    /// record is written first, so an `Err` leaves no wake wired and the
    /// caller can refuse the gated call rather than park a run whose
    /// approval does not exist (ADR 2026-07-21, decisions 1 and 10).
    ///
    /// The insert precedes the subscription, so a decision resolved in that
    /// window reaches the waiter through the wake task's store poll rather
    /// than the bus publish.
    pub async fn register_durable(
        &self,
        request: ApprovalRequest,
        timeout: Duration,
    ) -> Result<AwaitingDecision, SessionStoreError> {
        let (parked, id, request_id) = Self::parked_record(request, timeout);
        self.0.store.register(parked).await?;
        Ok(self.wire_wake(id, request_id, timeout).await)
    }

    /// Build the durable record for `request` alongside the two keys the
    /// process-local wake is filed under.
    fn parked_record(
        request: ApprovalRequest,
        timeout: Duration,
    ) -> (ParkedApproval, DecisionId, String) {
        let id = request.decision_id;
        let request_id = request.request_id.clone();
        let now = chrono::Utc::now();
        let parked = ParkedApproval {
            request,
            registered_at: now,
            expires_at: now
                + chrono::Duration::from_std(timeout).expect("approval timeout fits in chrono"),
        };
        (parked, id, request_id)
    }

    /// Subscribe to the approval's decision topic, spawn the task that fires
    /// its wake, and file the process-local entry.
    ///
    /// A failed subscription still parks a wakeable approval — the wake
    /// task's store poll remains as the (slower) carrier.
    async fn wire_wake(
        &self,
        id: DecisionId,
        request_id: String,
        timeout: Duration,
    ) -> AwaitingDecision {
        let (tx, rx) = oneshot::channel();
        let subscription = match self.0.bus.subscribe(&approval_topic(&id)).await {
            Ok(decisions) => Some(decisions),
            Err(err) => {
                warn!(
                    decision_id = %id, error = %err,
                    "approval wake subscription failed; decision delivery falls back to store polling",
                );
                None
            }
        };
        // Instrument with the registering request's span so the wake (and any
        // decode warning) lands in the parked call's trace.
        let task = tracing::Instrument::instrument(
            wake_on_decision(Arc::downgrade(&self.0), id, subscription),
            tracing::Span::current(),
        );
        let wake_task = tokio::spawn(task).abort_handle();
        self.0.wakes.lock().expect("registry lock poisoned").insert(
            id,
            WakeEntry {
                request_id,
                wake: tx,
                wake_task,
            },
        );
        AwaitingDecision::new(id, rx, Instant::now() + timeout)
    }

    /// Resolve a parked approval: durably record the decision in the store
    /// (at most once per `DecisionId`) and publish it on the bus, waking the
    /// parked await wherever it lives.
    pub async fn resolve(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        // Preserve the record for park-restart replay; wake reason discarded
        // here since the bus publish is the wake mechanism.
        let _wake = self.0.store.resolve_durable(id, decision.clone()).await?;
        let payload = serde_json::to_vec(&decision).expect("ApprovalDecision serializes to JSON");
        if let Err(err) = self
            .0
            .bus
            .publish(&approval_topic(id), payload.into())
            .await
        {
            // Only the fast path is lost: the parking side recovers the
            // recorded decision from the store poll.
            warn!(decision_id = %id, error = %err, "approval decision publish failed");
        }
        Ok(())
    }

    /// The durably recorded decision for an already-resolved approval, if
    /// any. A store fault reads as "no recorded decision" (logged), so
    /// callers keep their fail-closed shape.
    pub async fn recorded_decision(&self, id: &DecisionId) -> Option<ApprovalDecision> {
        match self.0.store.decision(id).await {
            Ok(decision) => decision,
            Err(err) => {
                warn!(decision_id = %id, error = %err, "recorded approval decision lookup failed");
                None
            }
        }
    }

    /// Expire a parked approval after timeout/cancellation so later ingress
    /// returns [`ResolveError::NotFound`], reporting whether the store
    /// entry actually went. A caller compensating for a failed park needs
    /// to know: a record it cannot remove is orphaned, not cleaned up.
    pub async fn remove_reporting(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        if let Some(entry) = self
            .0
            .wakes
            .lock()
            .expect("registry lock poisoned")
            .remove(id)
        {
            entry.abort_wake_task();
        }
        self.0.store.remove(id).await
    }

    /// Expire a parked approval after timeout/cancellation so later ingress
    /// returns [`ResolveError::NotFound`].
    pub async fn remove(&self, id: &DecisionId) {
        if let Err(err) = self.remove_reporting(id).await {
            warn!(decision_id = %id, error = %err, "parked approval removal failed");
        }
    }

    /// Poison the wake map's lock so the next local operation on this
    /// registry panics — the stand-in for a panic inside a park commit's
    /// local cleanup, which has no other reachable trigger.
    #[cfg(test)]
    pub fn poison_wakes_for_test(&self) {
        let inner = Arc::clone(&self.0);
        let poisoner = std::thread::spawn(move || {
            let _guard = inner.wakes.lock().expect("registry lock poisoned");
            panic!("poisoning the wake map");
        });
        assert!(poisoner.join().is_err(), "the poisoning thread must panic");
    }

    /// Synchronously drop the wake handles parked under a request id; their
    /// awaits resolve to `Cancelled`. Leaves store entries in place — use
    /// [`Self::cancel_request`] to also clean the store.
    pub fn cancel_request_local(&self, request_id: &str) {
        self.0
            .wakes
            .lock()
            .expect("registry lock poisoned")
            .retain(|_, entry| {
                if entry.request_id == request_id {
                    entry.abort_wake_task();
                    false
                } else {
                    true
                }
            });
    }

    /// Cancel every approval parked under a request id (stream drop /
    /// shutdown); their awaits resolve to `Cancelled`.
    pub async fn cancel_request(&self, request_id: &str) {
        self.cancel_request_local(request_id);
        if let Err(err) = self.0.store.cancel_request(request_id).await {
            warn!(request_id, error = %err, "approval store cancel_request failed");
        }
    }
}

impl Default for PendingApprovals {
    fn default() -> Self {
        Self::new()
    }
}

/// Wait for one approval's decision and fire its local wake handle. The
/// decision arrives over the approval's bus topic (fast path) or, every
/// [`DECISION_POLL_INTERVAL`], from the store's recorded decision — the
/// durable fallback that recovers a lost publish. One short-lived task per
/// parked approval; ends after the first decision or when aborted (entry
/// removed without a decision).
async fn wake_on_decision(
    inner: Weak<PendingApprovalsInner>,
    id: DecisionId,
    mut decisions: Option<Subscription>,
) {
    let mut poll = tokio::time::interval_at(
        Instant::now() + DECISION_POLL_INTERVAL,
        DECISION_POLL_INTERVAL,
    );
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let decision = tokio::select! {
            payload = subscription_next(&mut decisions) => match payload {
                Some(payload) => match serde_json::from_slice::<ApprovalDecision>(&payload) {
                    Ok(decision) => decision,
                    Err(err) => {
                        warn!(
                            decision_id = %id, error = %err,
                            "undecodable approval decision payload ignored",
                        );
                        continue;
                    }
                },
                // The bus closed the topic; the store poll carries on alone.
                None => {
                    decisions = None;
                    continue;
                }
            },
            _ = poll.tick() => {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                match inner.store.decision(&id).await {
                    Ok(Some(decision)) => decision,
                    Ok(None) => continue,
                    Err(err) => {
                        warn!(
                            decision_id = %id, error = %err,
                            "approval decision store poll failed",
                        );
                        continue;
                    }
                }
            }
        };
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if let Some(entry) = inner
            .wakes
            .lock()
            .expect("registry lock poisoned")
            .remove(&id)
        {
            let _ = entry.wake.send(decision);
        }
        return;
    }
}

/// Next bus payload, pending forever without a subscription (the poll arm is
/// then the only wake source).
async fn subscription_next(subscription: &mut Option<Subscription>) -> Option<Bytes> {
    match subscription {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::request_cancellation::RequestCancelToken;
    use serde_json::json;

    use super::*;
    use crate::hitl::decision::{
        AgentScope, ApprovalOrigin, ApprovalOutcome, CancelReason, DecisionId,
    };
    use crate::hitl::protocol::{ApprovalItem, ApprovalRequest, PROTOCOL_VERSION};

    fn test_request(request_id: &str) -> ApprovalRequest {
        ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: request_id.to_string(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "test_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "test_tool".to_string(),
                arguments: json!({}),
                tool_call_intent: None,
            }],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn register_and_resolve_approved() {
        let registry = PendingApprovals::new();
        let req = test_request("req-1");
        let id = req.decision_id;
        let handle = registry.register(req, Duration::from_secs(60)).await;
        let cancel = RequestCancelToken::unbound();

        registry
            .resolve(&id, ApprovalDecision::Approved)
            .await
            .expect("resolve succeeds");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn register_and_resolve_denied() {
        let registry = PendingApprovals::new();
        let req = test_request("req-2");
        let id = req.decision_id;
        let handle = registry.register(req, Duration::from_secs(60)).await;
        let cancel = RequestCancelToken::unbound();

        registry
            .resolve(
                &id,
                ApprovalDecision::Denied {
                    reason: Some("not safe".into()),
                },
            )
            .await
            .expect("resolve succeeds");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: Some("not safe".into())
            })
        );
    }

    #[tokio::test]
    async fn resolve_unknown_id_returns_not_found() {
        let registry = PendingApprovals::new();
        let unknown = DecisionId::generate();
        assert_eq!(
            registry.resolve(&unknown, ApprovalDecision::Approved).await,
            Err(ResolveError::NotFound)
        );
    }

    #[tokio::test]
    async fn resolve_is_idempotent_with_durable_store() {
        // resolve_durable preserves the record, so a second resolve on the same
        // id also succeeds rather than returning NotFound.
        let registry = PendingApprovals::new();
        let req = test_request("req-3");
        let id = req.decision_id;
        let _handle = registry.register(req, Duration::from_secs(60)).await;

        registry
            .resolve(&id, ApprovalDecision::Approved)
            .await
            .expect("first resolve succeeds");
        registry
            .resolve(&id, ApprovalDecision::Approved)
            .await
            .expect("second resolve is idempotent: record persists for park-restart replay");
    }

    #[tokio::test]
    async fn remove_makes_resolve_return_not_found() {
        let registry = PendingApprovals::new();
        let req = test_request("req-remove");
        let id = req.decision_id;
        let _handle = registry.register(req, Duration::from_secs(60)).await;

        registry.remove(&id).await;

        assert_eq!(
            registry.resolve(&id, ApprovalDecision::Approved).await,
            Err(ResolveError::NotFound)
        );
    }

    /// Resolve records the decision in the store even when the parked await
    /// is already gone (the waiter dropped between park and decision).
    #[tokio::test]
    async fn resolve_succeeds_after_awaiting_handle_dropped() {
        let registry = PendingApprovals::new();
        let req = test_request("req-dropped");
        let id = req.decision_id;
        let handle = registry.register(req, Duration::from_secs(60)).await;
        drop(handle);

        assert_eq!(
            registry.resolve(&id, ApprovalDecision::Approved).await,
            Ok(())
        );
    }

    /// An undecodable payload on the approval topic is ignored; a valid
    /// decision published afterwards still wakes the parked call.
    #[tokio::test(start_paused = true)]
    async fn undecodable_decision_payload_is_ignored() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        let registry = PendingApprovals::with_backend(store, bus.clone());
        let cancel = RequestCancelToken::unbound();

        let req = test_request("req-garbage");
        let id = req.decision_id;
        let handle = registry.register(req, Duration::from_secs(60)).await;

        bus.publish(&approval_topic(&id), bytes::Bytes::from_static(b"not json"))
            .await
            .expect("publish succeeds");
        registry
            .resolve(&id, ApprovalDecision::Approved)
            .await
            .expect("resolve succeeds");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
    }

    /// The synchronous half of cancellation: wake handles drop (awaits
    /// cancel) while store entries remain until the async half runs.
    #[tokio::test(start_paused = true)]
    async fn cancel_request_local_cancels_await_but_keeps_store_entry() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let registry =
            PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
        let req = test_request("req-local");
        let id = req.decision_id;
        let handle = registry.register(req, Duration::from_secs(60)).await;
        let cancel = RequestCancelToken::unbound();

        registry.cancel_request_local("req-local");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Cancelled(CancelReason::SenderDropped)
        );
        assert!(
            store.get(&id).await.unwrap().is_some(),
            "store entry remains for the async half"
        );

        registry.cancel_request("req-local").await;
        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_request_drops_matching_entries() {
        let registry = PendingApprovals::new();
        let req_a = test_request("req-cancel");
        let req_b = test_request("req-keep");
        let id_a = req_a.decision_id;
        let id_b = req_b.decision_id;
        let handle_a = registry.register(req_a, Duration::from_secs(60)).await;
        let handle_b = registry.register(req_b, Duration::from_secs(60)).await;
        let cancel = RequestCancelToken::unbound();

        registry.cancel_request("req-cancel").await;

        assert_eq!(
            handle_a.outcome(&cancel).await,
            ApprovalOutcome::Cancelled(CancelReason::SenderDropped)
        );
        assert_eq!(
            registry.resolve(&id_a, ApprovalDecision::Approved).await,
            Err(ResolveError::NotFound),
            "cancelled approval must be gone from the store too",
        );

        registry
            .resolve(&id_b, ApprovalDecision::Approved)
            .await
            .expect("unrelated entry survives");
        assert_eq!(
            handle_b.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
    }

    #[tokio::test]
    async fn register_sets_expires_at_from_timeout() {
        let store = Arc::new(InMemoryApprovalStore::new());
        let registry =
            PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
        let req = test_request("req-ts");
        let id = req.decision_id;
        let timeout = Duration::from_secs(300);
        let before = chrono::Utc::now();
        let _handle = registry.register(req, timeout).await;
        let after = chrono::Utc::now();

        let parked = store.get(&id).await.unwrap().expect("entry exists");
        let delta = parked.expires_at - parked.registered_at;
        assert_eq!(delta, chrono::Duration::from_std(timeout).unwrap());
        assert!(parked.registered_at >= before);
        assert!(parked.registered_at <= after);
    }

    /// Bus double whose `publish` drops every payload: the pub/sub loss
    /// window, or a resolver crash between the store write and the publish.
    struct LossyBus(InMemoryEventBus);

    #[async_trait::async_trait]
    impl EventBus for LossyBus {
        async fn publish(&self, _topic: &str, _payload: Bytes) -> Result<(), SessionStoreError> {
            Ok(())
        }

        async fn subscribe(&self, topic: &str) -> Result<Subscription, SessionStoreError> {
            self.0.subscribe(topic).await
        }
    }

    /// Bus double that cannot subscribe at all.
    struct DeafBus;

    #[async_trait::async_trait]
    impl EventBus for DeafBus {
        async fn publish(&self, _topic: &str, _payload: Bytes) -> Result<(), SessionStoreError> {
            Ok(())
        }

        async fn subscribe(&self, _topic: &str) -> Result<Subscription, SessionStoreError> {
            Err(SessionStoreError::Request {
                reason: "subscriptions unavailable".to_string(),
            })
        }
    }

    /// The #474 shape: the decision is durably recorded but its wake publish
    /// is lost. The parked await recovers the decision from the store within
    /// the poll interval instead of failing closed at its timeout.
    #[tokio::test(start_paused = true)]
    async fn lost_wake_publish_is_recovered_from_the_store() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let bus: Arc<dyn EventBus> = Arc::new(LossyBus(InMemoryEventBus::new()));
        let parker = PendingApprovals::with_backend(store.clone(), bus.clone());
        let resolver = PendingApprovals::with_backend(store, bus);
        let cancel = RequestCancelToken::unbound();

        let req = test_request("req-lost-wake");
        let id = req.decision_id;
        let started = Instant::now();
        let handle = parker.register(req, Duration::from_secs(60)).await;

        resolver
            .resolve(&id, ApprovalDecision::Approved)
            .await
            .expect("resolve succeeds");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
        assert!(
            started.elapsed() <= DECISION_POLL_INTERVAL,
            "the decision must arrive via the store poll, not the timeout; took {:?}",
            started.elapsed(),
        );
    }

    /// A failed wake subscription still parks a wakeable approval: the store
    /// poll is the only carrier, and the decision arrives through it.
    #[tokio::test(start_paused = true)]
    async fn failed_subscription_still_wakes_via_store_poll() {
        let registry = PendingApprovals::with_backend(
            Arc::new(InMemoryApprovalStore::new()),
            Arc::new(DeafBus),
        );
        let cancel = RequestCancelToken::unbound();

        let req = test_request("req-deaf");
        let id = req.decision_id;
        let handle = registry.register(req, Duration::from_secs(60)).await;

        registry
            .resolve(
                &id,
                ApprovalDecision::Denied {
                    reason: Some("nope".into()),
                },
            )
            .await
            .expect("resolve succeeds");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: Some("nope".into())
            })
        );
    }

    /// The cross-instance seam: two registries (as two instances) sharing one store and
    /// bus. An approval parked on one is resolved through the other, and the
    /// parking side's await wakes.
    #[tokio::test(start_paused = true)]
    async fn resolve_on_shared_backend_wakes_other_registry() {
        let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        let instance_a = PendingApprovals::with_backend(store.clone(), bus.clone());
        let instance_b = PendingApprovals::with_backend(store, bus);
        let cancel = RequestCancelToken::unbound();

        let req = test_request("req-cross");
        let id = req.decision_id;
        let handle = instance_a.register(req, Duration::from_secs(60)).await;

        instance_b
            .resolve(&id, ApprovalDecision::Approved)
            .await
            .expect("resolve on the other instance succeeds");

        assert_eq!(
            handle.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
    }
}
