//! In-memory (single-process) implementations of the session-store
//! capabilities: the default backend, with all state scoped to the process.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::broadcast;

use crate::config::SessionId;
use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};

use super::{
    ApprovalStore, EventBus, SessionStoreError, SkillInvocationRecord, SkillInvocationStore,
    Subscription,
};

/// Buffered payloads per topic before slow subscribers start lagging.
const TOPIC_CAPACITY: usize = 64;

/// Skill-invocation records kept per session before new writes are dropped.
const MAX_SKILL_RECORDS_PER_SESSION: usize = 64;

/// Sessions kept in the skill-invocation store before the least-recently
/// touched one is evicted.
const MAX_SKILL_SESSIONS: usize = 1024;

/// The parked-approval registry as a plain map.
#[derive(Default)]
pub struct InMemoryApprovalStore {
    // `std::sync::Mutex`: every operation is a synchronous map op; nothing
    // awaits while holding the lock.
    entries: Mutex<BTreeMap<DecisionId, ParkedApproval>>,
}

impl InMemoryApprovalStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<DecisionId, ParkedApproval>> {
        self.entries.lock().expect("approval store lock poisoned")
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
        _decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        // Removal under the lock is the at-most-once guarantee.
        match self.lock().remove(id) {
            Some(_) => Ok(()),
            None => Err(ResolveError::NotFound),
        }
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        self.lock().remove(id);
        Ok(())
    }

    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError> {
        self.lock()
            .retain(|_, parked| parked.request.request_id != request_id);
        Ok(())
    }
}

/// The per-session skill-invocation log as a plain map.
///
/// Unlike approvals, skill records have no natural removal event, so this
/// store bounds growth itself: a per-session record cap and a
/// least-recently-touched session eviction cap.
#[derive(Default)]
pub struct InMemorySkillInvocationStore {
    // `std::sync::Mutex`: every operation is a synchronous map op; nothing
    // awaits while holding the lock.
    inner: Mutex<SkillSessions>,
}

#[derive(Default)]
struct SkillSessions {
    sessions: HashMap<String, SkillSessionEntry>,
    /// Monotonic touch counter backing least-recently-touched eviction.
    clock: u64,
}

#[derive(Default)]
struct SkillSessionEntry {
    records: Vec<SkillInvocationRecord>,
    last_touched: u64,
}

impl InMemorySkillInvocationStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SkillSessions> {
        self.inner
            .lock()
            .expect("skill invocation store lock poisoned")
    }
}

#[async_trait]
impl SkillInvocationStore for InMemorySkillInvocationStore {
    async fn record(
        &self,
        session_id: &SessionId,
        record: SkillInvocationRecord,
    ) -> Result<(), SessionStoreError> {
        let mut inner = self.lock();
        inner.clock += 1;
        let clock = inner.clock;

        let entry = inner
            .sessions
            .entry(session_id.as_str().to_string())
            .or_default();
        entry.last_touched = clock;

        let key = record.invocation.dedup_key();
        if entry
            .records
            .iter()
            .any(|existing| existing.invocation.dedup_key() == key)
        {
            return Ok(());
        }
        if entry.records.len() >= MAX_SKILL_RECORDS_PER_SESSION {
            tracing::warn!(
                session_id = session_id.as_str(),
                cap = MAX_SKILL_RECORDS_PER_SESSION,
                "skill invocation store at per-session capacity; dropping new record"
            );
            return Ok(());
        }
        entry.records.push(record);

        if inner.sessions.len() > MAX_SKILL_SESSIONS
            && let Some(evict) = inner
                .sessions
                .iter()
                .min_by_key(|(_, entry)| entry.last_touched)
                .map(|(id, _)| id.clone())
        {
            inner.sessions.remove(&evict);
        }
        Ok(())
    }

    async fn list(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SkillInvocationRecord>, SessionStoreError> {
        let mut inner = self.lock();
        inner.clock += 1;
        let clock = inner.clock;
        let Some(entry) = inner.sessions.get_mut(session_id.as_str()) else {
            return Ok(Vec::new());
        };
        entry.last_touched = clock;
        let mut records = entry.records.clone();
        records.sort_by_key(|r| (r.anchor, r.seq));
        Ok(records)
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

    fn skill_record(name: &str, anchor: u32, seq: u32) -> SkillInvocationRecord {
        SkillInvocationRecord {
            version: crate::session_store::SKILL_INVOCATION_RECORD_VERSION,
            invocation: crate::session_store::SkillInvocation::LoadSkill {
                name: name.to_string(),
            },
            tool_call_id: format!("call_{name}_{anchor}_{seq}"),
            anchor,
            seq,
            invoked_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn skill_store_lists_records_ordered_by_anchor_then_seq() {
        let store = InMemorySkillInvocationStore::new();
        let session = SessionId::new("sess-1");

        store
            .record(&session, skill_record("late", 5, 0))
            .await
            .unwrap();
        store
            .record(&session, skill_record("second", 1, 1))
            .await
            .unwrap();
        store
            .record(&session, skill_record("first", 1, 0))
            .await
            .unwrap();

        let names: Vec<String> = store
            .list(&session)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.invocation.label())
            .collect();
        assert_eq!(names, vec!["first", "second", "late"]);
    }

    #[tokio::test]
    async fn skill_store_record_is_idempotent_per_invocation() {
        let store = InMemorySkillInvocationStore::new();
        let session = SessionId::new("sess-1");

        store
            .record(&session, skill_record("dup", 1, 0))
            .await
            .unwrap();
        // Same invocation re-recorded later must keep the first position.
        store
            .record(&session, skill_record("dup", 7, 3))
            .await
            .unwrap();

        let records = store.list(&session).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].anchor, 1);
    }

    #[tokio::test]
    async fn skill_store_sessions_are_independent() {
        let store = InMemorySkillInvocationStore::new();
        store
            .record(&SessionId::new("sess-a"), skill_record("a", 1, 0))
            .await
            .unwrap();

        assert!(
            store
                .list(&SessionId::new("sess-b"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.list(&SessionId::new("sess-a")).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn skill_store_caps_records_per_session() {
        let store = InMemorySkillInvocationStore::new();
        let session = SessionId::new("sess-cap");
        for i in 0..(MAX_SKILL_RECORDS_PER_SESSION + 5) {
            store
                .record(&session, skill_record(&format!("s{i}"), i as u32, 0))
                .await
                .unwrap();
        }
        assert_eq!(
            store.list(&session).await.unwrap().len(),
            MAX_SKILL_RECORDS_PER_SESSION
        );
    }

    #[tokio::test]
    async fn skill_store_evicts_least_recently_touched_session() {
        let store = InMemorySkillInvocationStore::new();
        for i in 0..MAX_SKILL_SESSIONS {
            store
                .record(
                    &SessionId::new(format!("sess-{i}")),
                    skill_record("s", 1, 0),
                )
                .await
                .unwrap();
        }
        // Touch the oldest session so it is no longer the eviction candidate.
        let oldest = SessionId::new("sess-0");
        assert_eq!(store.list(&oldest).await.unwrap().len(), 1);

        // One past the cap evicts the least-recently-touched session (sess-1).
        store
            .record(&SessionId::new("sess-overflow"), skill_record("s", 1, 0))
            .await
            .unwrap();

        assert_eq!(store.list(&oldest).await.unwrap().len(), 1);
        assert!(
            store
                .list(&SessionId::new("sess-1"))
                .await
                .unwrap()
                .is_empty()
        );
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
