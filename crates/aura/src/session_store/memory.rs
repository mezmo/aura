//! In-memory (single-process) implementations of the session-store
//! capabilities: the default backend, with all state scoped to the process.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::broadcast;

use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError, Timestamp};
use crate::orchestration::park::{
    AgentInstanceId, CasError, FencingGeneration, Lease, LeaseTtl, ParkCommit, RunEvent, RunState,
    SessionId, SessionRecord, WakeReason,
};

use super::{ApprovalStore, EventBus, RunStore, RunStoreError, SessionStoreError, Subscription};

/// Buffered payloads per topic before slow subscribers start lagging.
const TOPIC_CAPACITY: usize = 64;

/// Decision retention margin.
const DECISION_RETENTION_MARGIN_SECS: i64 = 60;

/// A recorded decision, the wake reason carrying it, and their shared
/// retention deadline.
struct DecidedEntry {
    decision: ApprovalDecision,
    wake: WakeReason,
    keep_until: Timestamp,
}

/// The parked-approval registry as a plain map.
#[derive(Default)]
pub struct InMemoryApprovalStore {
    // Synchronous mutexes.
    entries: Mutex<BTreeMap<DecisionId, ParkedApproval>>,
    decided: Mutex<BTreeMap<DecisionId, DecidedEntry>>,
}

impl InMemoryApprovalStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<DecisionId, ParkedApproval>> {
        self.entries.lock().expect("approval store lock poisoned")
    }

    /// Lock the decided map, dropping entries past their retention window.
    fn lock_decided(&self) -> std::sync::MutexGuard<'_, BTreeMap<DecisionId, DecidedEntry>> {
        let mut decided = self.decided.lock().expect("approval store lock poisoned");
        let now = chrono::Utc::now();
        decided.retain(|_, entry| entry.keep_until > now);
        decided
    }
}

#[async_trait]
impl ApprovalStore for InMemoryApprovalStore {
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError> {
        self.lock().insert(parked.request.decision_id, parked);
        Ok(())
    }

    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
        Ok(self.lock().get(id).cloned())
    }

    async fn resolve(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        // Removal under the lock is the at-most-once guarantee.
        let parked = self.lock().remove(id).ok_or(ResolveError::NotFound)?;
        self.lock_decided().insert(
            *id,
            DecidedEntry {
                decision,
                wake: WakeReason::DecisionResolved {
                    decision_id: *id,
                    resolved_at: chrono::Utc::now(),
                },
                keep_until: parked.expires_at
                    + chrono::Duration::seconds(DECISION_RETENTION_MARGIN_SECS),
            },
        );
        Ok(())
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ApprovalDecision>, SessionStoreError> {
        Ok(self.lock_decided().get(id).map(|e| e.decision.clone()))
    }

    async fn resolve_durable(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<WakeReason, ResolveError> {
        // The parked record stays for park-restart replay, and the decision
        // lands in the decided map that `decision()` reads back.
        let expires_at = match self.lock().get(id) {
            Some(parked) => parked.expires_at,
            None => return Err(ResolveError::NotFound),
        };
        // Insert-if-absent under the lock is the first-write-wins guarantee:
        // a repeat or a contradiction reads back the stored resolution.
        let wake = self
            .lock_decided()
            .entry(*id)
            .or_insert_with(|| DecidedEntry {
                decision,
                wake: WakeReason::DecisionResolved {
                    decision_id: *id,
                    resolved_at: chrono::Utc::now(),
                },
                keep_until: expires_at + chrono::Duration::seconds(DECISION_RETENTION_MARGIN_SECS),
            })
            .wake
            .clone();
        Ok(wake)
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        self.lock().remove(id);
        self.lock_decided().remove(id);
        Ok(())
    }

    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError> {
        // A decided entry carries no request id, so the ids to discard are
        // read off the parked records before those records go.
        let cancelled: Vec<DecisionId> = {
            let mut entries = self.lock();
            let ids = entries
                .iter()
                .filter(|(_, parked)| parked.request.request_id == request_id)
                .map(|(id, _)| *id)
                .collect();
            entries.retain(|_, parked| parked.request.request_id != request_id);
            ids
        };
        let mut decided = self.lock_decided();
        for id in cancelled {
            decided.remove(&id);
        }
        Ok(())
    }
}

/// The in-memory run store: fenced session records in a plain map, with the
/// process-wide lock standing in for the backend atomic primitive.
/// Single-process like every capability of the memory backend — it proves
/// the protocol, not cross-instance claims.
#[derive(Default)]
pub struct InMemoryRunStore {
    // `std::sync::Mutex`: every operation is a synchronous map op; nothing
    // awaits while holding the lock.
    records: Mutex<BTreeMap<SessionId, SessionRecord>>,
}

impl InMemoryRunStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Verify the record has a live lease whose generation matches the
/// presented fencing token. Every mutation except `acquire_lease` and
/// `release_lease` must pass this guard: once a lease expires or is
/// released, the old token cannot mutate the record.
fn require_live_lease(
    record: &SessionRecord,
    presented: FencingGeneration,
) -> Result<&Lease, RunStoreError> {
    if presented != record.generation {
        return Err(RunStoreError::Cas(CasError::GenerationMismatch {
            presented,
            current: record.generation,
        }));
    }
    let Some(lease) = record.lease.as_ref() else {
        return Err(RunStoreError::Cas(CasError::StateMismatch {
            actual: "unleased",
        }));
    };
    if lease.generation != presented {
        return Err(RunStoreError::Cas(CasError::GenerationMismatch {
            presented,
            current: lease.generation,
        }));
    }
    if lease.expires_at <= chrono::Utc::now() {
        return Err(RunStoreError::Cas(CasError::StateMismatch {
            actual: "expired",
        }));
    }
    Ok(lease)
}

#[async_trait]
impl RunStore for InMemoryRunStore {
    async fn create(&self, record: SessionRecord) -> Result<(), RunStoreError> {
        let mut records = self.records.lock().expect("run store lock poisoned");
        if records.contains_key(&record.session.id) {
            return Err(RunStoreError::SessionExists {
                session: record.session.id,
            });
        }
        if !matches!(record.state, RunState::Created) {
            return Err(RunStoreError::Cas(CasError::StateMismatch {
                actual: "non-Created",
            }));
        }
        records.insert(record.session.id, record);
        Ok(())
    }

    async fn load(&self, session: SessionId) -> Result<Option<SessionRecord>, RunStoreError> {
        let records = self.records.lock().expect("run store lock poisoned");
        Ok(records.get(&session).cloned())
    }

    async fn acquire_lease(
        &self,
        session: SessionId,
        holder: AgentInstanceId,
        ttl: LeaseTtl,
    ) -> Result<Lease, RunStoreError> {
        let mut records = self.records.lock().expect("run store lock poisoned");
        let record = records
            .get_mut(&session)
            .ok_or(RunStoreError::UnknownSession { session })?;

        let now = chrono::Utc::now();
        if let Some(ref lease) = record.lease
            && lease.expires_at > now
        {
            return Err(RunStoreError::LeaseHeld {
                holder: lease.holder,
                expires_at: lease.expires_at,
            });
        }

        let next_generation = record.generation.next();
        let expires_at = now
            + chrono::Duration::from_std(ttl.get()).expect("positive ttl fits in chrono duration");
        let lease = Lease {
            holder,
            acquired_at: now,
            heartbeat_at: now,
            expires_at,
            generation: next_generation,
        };
        record.lease = Some(lease.clone());
        record.generation = next_generation;
        Ok(lease)
    }

    async fn heartbeat_lease(
        &self,
        session: SessionId,
        generation: FencingGeneration,
        ttl: LeaseTtl,
    ) -> Result<Lease, RunStoreError> {
        let mut records = self.records.lock().expect("run store lock poisoned");
        let record = records
            .get_mut(&session)
            .ok_or(RunStoreError::UnknownSession { session })?;

        let (holder, acquired_at) = {
            let lease = require_live_lease(record, generation)?;
            (lease.holder, lease.acquired_at)
        };

        let now = chrono::Utc::now();
        let expires_at = now
            + chrono::Duration::from_std(ttl.get()).expect("positive ttl fits in chrono duration");
        let renewed = Lease {
            holder,
            acquired_at,
            heartbeat_at: now,
            expires_at,
            generation,
        };
        record.lease = Some(renewed.clone());
        Ok(renewed)
    }

    async fn release_lease(
        &self,
        session: SessionId,
        generation: FencingGeneration,
    ) -> Result<(), RunStoreError> {
        let mut records = self.records.lock().expect("run store lock poisoned");
        let record = records
            .get_mut(&session)
            .ok_or(RunStoreError::UnknownSession { session })?;

        if generation != record.generation {
            return Err(RunStoreError::Cas(CasError::GenerationMismatch {
                presented: generation,
                current: record.generation,
            }));
        }

        record.lease = None;
        Ok(())
    }

    async fn apply(
        &self,
        session: SessionId,
        presented: FencingGeneration,
        event: RunEvent,
    ) -> Result<SessionRecord, RunStoreError> {
        let mut records = self.records.lock().expect("run store lock poisoned");
        let record = records
            .get(&session)
            .ok_or(RunStoreError::UnknownSession { session })?;

        require_live_lease(record, presented)?;
        let next = record
            .clone()
            .apply(presented, event)
            .map_err(RunStoreError::Cas)?;
        records.insert(session, next.clone());
        Ok(next)
    }

    async fn park(
        &self,
        session: SessionId,
        presented: FencingGeneration,
        commit: ParkCommit,
    ) -> Result<SessionRecord, RunStoreError> {
        let mut records = self.records.lock().expect("run store lock poisoned");
        let record = records
            .get(&session)
            .ok_or(RunStoreError::UnknownSession { session })?;

        require_live_lease(record, presented)?;
        let next = record
            .clone()
            .park(presented, commit)
            .map_err(RunStoreError::Cas)?;
        records.insert(session, next.clone());
        Ok(next)
    }
}

/// A local `tokio::broadcast` registry keyed by topic. Single-instance pub/sub:
/// publish and subscribe never leave the process.
#[derive(Default)]
pub struct InMemoryEventBus {
    // Shared with each subscription's `SubscriptionGuard`.
    topics: Arc<Mutex<HashMap<String, broadcast::Sender<Bytes>>>>,
}

impl InMemoryEventBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Owns a topic receiver and removes the topic entry when the last
/// subscriber drops, so abandoned topics do not accumulate.
struct SubscriptionGuard {
    rx: broadcast::Receiver<Bytes>,
    topics: Arc<Mutex<HashMap<String, broadcast::Sender<Bytes>>>>,
    topic: String,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        let mut topics = self.topics.lock().expect("event bus lock poisoned");
        // `self.rx` is still alive here, so a count of 1 means we are the
        // last subscriber. Subscribe/publish also lock the map, so the check
        // and removal are atomic with respect to them.
        if let Some(sender) = topics.get(&self.topic)
            && sender.receiver_count() <= 1
        {
            topics.remove(&self.topic);
        }
    }
}

#[async_trait]
impl EventBus for InMemoryEventBus {
    async fn publish(&self, topic: &str, payload: Bytes) -> Result<(), SessionStoreError> {
        let mut topics = self.topics.lock().expect("event bus lock poisoned");
        if let Some(sender) = topics.get(topic)
            && sender.send(payload).is_err()
        {
            // No live subscribers: fire-and-forget semantics, and the dead
            // topic entry can go.
            topics.remove(topic);
        }
        Ok(())
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription, SessionStoreError> {
        let rx = {
            let mut topics = self.topics.lock().expect("event bus lock poisoned");
            topics
                .entry(topic.to_string())
                .or_insert_with(|| broadcast::channel(TOPIC_CAPACITY).0)
                .subscribe()
        };
        let mut guard = SubscriptionGuard {
            rx,
            topics: Arc::clone(&self.topics),
            topic: topic.to_string(),
        };
        Ok(Box::pin(async_stream::stream! {
            loop {
                match guard.rx.recv().await {
                    Ok(payload) => yield payload,
                    // A lagged subscriber skips missed payloads but stays
                    // subscribed.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;
    use crate::hitl::{
        AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, PROTOCOL_VERSION,
    };
    use crate::session_store::conformance;

    #[tokio::test]
    async fn conforms_to_the_approval_store_contract() {
        conformance::assert_approval_store_conformance(Arc::new(InMemoryApprovalStore::new()))
            .await;
    }

    #[tokio::test]
    async fn conforms_to_the_event_bus_contract() {
        conformance::assert_event_bus_conformance(Arc::new(InMemoryEventBus::new())).await;
    }

    #[tokio::test]
    async fn conforms_to_the_run_store_contract() {
        conformance::assert_run_store_conformance(Arc::new(InMemoryRunStore::new())).await;
    }

    fn parked(request_id: &str) -> ParkedApproval {
        let now = chrono::Utc::now();
        ParkedApproval {
            request: ApprovalRequest {
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
                    arguments: serde_json::json!({}),
                    tool_call_intent: None,
                }],
            },
            registered_at: now,
            expires_at: now + chrono::Duration::seconds(60),
        }
    }

    #[tokio::test]
    async fn approval_store_register_get_resolve() {
        let store = InMemoryApprovalStore::new();
        let entry = parked("req-1");
        let id = entry.request.decision_id;

        store.register(entry).await.unwrap();
        assert!(store.get(&id).await.unwrap().is_some());

        store
            .resolve(&id, ApprovalDecision::Approved)
            .await
            .unwrap();
        assert!(store.get(&id).await.unwrap().is_none());
        assert_eq!(
            store.resolve(&id, ApprovalDecision::Approved).await,
            Err(ResolveError::NotFound),
        );
    }

    #[tokio::test]
    async fn approval_store_resolve_records_readable_decision() {
        let store = InMemoryApprovalStore::new();
        let entry = parked("req-durable");
        let id = entry.request.decision_id;
        store.register(entry).await.unwrap();

        let denied = ApprovalDecision::Denied {
            reason: Some("not safe".into()),
        };
        store.resolve(&id, denied.clone()).await.unwrap();

        assert_eq!(store.decision(&id).await.unwrap(), Some(denied.clone()));
        // Recorded decision survives rejected second resolve.
        assert_eq!(
            store.resolve(&id, ApprovalDecision::Approved).await,
            Err(ResolveError::NotFound)
        );
        assert_eq!(store.decision(&id).await.unwrap(), Some(denied));
        assert_eq!(store.decision(&DecisionId::generate()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn recorded_decision_is_pruned_after_retention_window() {
        let store = InMemoryApprovalStore::new();
        let mut entry = parked("req-prune");
        // Retention margin is already past.
        entry.expires_at =
            chrono::Utc::now() - chrono::Duration::seconds(2 * DECISION_RETENTION_MARGIN_SECS);
        let id = entry.request.decision_id;
        store.register(entry).await.unwrap();
        store
            .resolve(&id, ApprovalDecision::Approved)
            .await
            .unwrap();

        assert_eq!(store.decision(&id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn approval_store_cancel_request_removes_only_matching() {
        let store = InMemoryApprovalStore::new();
        let cancel = parked("req-cancel");
        let keep = parked("req-keep");
        let keep_id = keep.request.decision_id;
        store.register(cancel).await.unwrap();
        store.register(keep).await.unwrap();

        store.cancel_request("req-cancel").await.unwrap();

        assert!(store.get(&keep_id).await.unwrap().is_some());
        assert_eq!(store.lock().len(), 1);
    }

    #[tokio::test]
    async fn event_bus_delivers_to_subscriber() {
        let bus = InMemoryEventBus::new();
        let mut sub = bus.subscribe("topic-a").await.unwrap();

        bus.publish("topic-a", Bytes::from_static(b"hello"))
            .await
            .unwrap();

        assert_eq!(sub.next().await.unwrap(), Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn event_bus_publish_without_subscribers_is_ok() {
        let bus = InMemoryEventBus::new();
        bus.publish("nobody-home", Bytes::from_static(b"x"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn event_bus_topic_cleaned_up_when_last_subscriber_drops() {
        let bus = InMemoryEventBus::new();
        let sub_a = bus.subscribe("topic-b").await.unwrap();
        let sub_b = bus.subscribe("topic-b").await.unwrap();
        assert_eq!(bus.topics.lock().unwrap().len(), 1);

        drop(sub_a);
        assert_eq!(bus.topics.lock().unwrap().len(), 1);
        drop(sub_b);
        assert!(bus.topics.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn event_bus_fans_out_to_all_subscribers() {
        let bus = InMemoryEventBus::new();
        let mut sub_a = bus.subscribe("topic-fan").await.unwrap();
        let mut sub_b = bus.subscribe("topic-fan").await.unwrap();

        bus.publish("topic-fan", Bytes::from_static(b"payload"))
            .await
            .unwrap();

        assert_eq!(sub_a.next().await.unwrap(), Bytes::from_static(b"payload"));
        assert_eq!(sub_b.next().await.unwrap(), Bytes::from_static(b"payload"));
    }

    #[tokio::test]
    async fn event_bus_lagged_subscriber_skips_but_stays_subscribed() {
        let bus = InMemoryEventBus::new();
        let mut sub = bus.subscribe("topic-lag").await.unwrap();

        // Overflow the topic buffer without polling the subscriber, then
        // publish a sentinel: the lagged stream must skip forward and keep
        // yielding rather than end.
        for i in 0..(TOPIC_CAPACITY * 2) {
            bus.publish("topic-lag", Bytes::from(format!("m{i}")))
                .await
                .unwrap();
        }
        bus.publish("topic-lag", Bytes::from_static(b"sentinel"))
            .await
            .unwrap();

        let mut saw_sentinel = false;
        for _ in 0..=TOPIC_CAPACITY {
            if sub.next().await.expect("stream stays open") == Bytes::from_static(b"sentinel") {
                saw_sentinel = true;
                break;
            }
        }
        assert!(saw_sentinel, "subscription must survive lagging");
    }

    #[tokio::test]
    async fn event_bus_topics_are_independent() {
        let bus = InMemoryEventBus::new();
        let mut sub_a = bus.subscribe("topic-a").await.unwrap();
        let mut sub_b = bus.subscribe("topic-b").await.unwrap();

        bus.publish("topic-a", Bytes::from_static(b"for-a"))
            .await
            .unwrap();
        bus.publish("topic-b", Bytes::from_static(b"for-b"))
            .await
            .unwrap();

        assert_eq!(sub_a.next().await.unwrap(), Bytes::from_static(b"for-a"));
        assert_eq!(sub_b.next().await.unwrap(), Bytes::from_static(b"for-b"));
    }
}
