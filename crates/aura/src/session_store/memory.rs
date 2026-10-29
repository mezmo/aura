//! In-memory (single-process) implementations of the session-store
//! capabilities: the default backend, with all state scoped to the process.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use aura_events::run::{SessionEvent, SessionEventPayload};
use aura_events::{RunId, SequenceNumber};
use bytes::Bytes;
use tokio::sync::broadcast;

use crate::config::SessionId;
use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError, Timestamp};

use super::{
    ApprovalStore, EventBus, JournalEntry, JournalStore, MAX_SKILL_RECORDS_PER_LOG, RunFacts,
    RunFilter, RunStore, SessionStoreError, SkillInvocationRecord, SkillInvocationStore,
    SkillLogKey, Subscription,
};

/// Buffered payloads per topic before slow subscribers start lagging.
const TOPIC_CAPACITY: usize = 64;

/// Decision retention margin.
const DECISION_RETENTION_MARGIN_SECS: i64 = 60;

/// Skill logs kept in the skill-invocation store before the least-recently
/// touched one is evicted.
const MAX_SKILL_LOGS: usize = 1024;

/// Events kept per session by [`InMemoryJournalStore::new`] before the oldest
/// is dropped.
pub const DEFAULT_MAX_JOURNAL_ENTRIES_PER_SESSION: usize = 4096;

/// Sessions kept by [`InMemoryJournalStore::new`] before the least-recently
/// touched one is evicted.
pub const DEFAULT_MAX_JOURNAL_SESSIONS: usize = 256;

/// Sessions whose newest sequence number a journal store remembers, per
/// session whose entries it keeps.
const JOURNAL_MARKS_PER_SESSION: usize = 64;

/// Runs kept by [`InMemoryRunStore::new`] before one is let go.
pub const DEFAULT_MAX_RUNS: usize = 4096;

/// A recorded decision and its retention deadline.
struct DecidedEntry {
    decision: ApprovalDecision,
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

    /// Lock decided map, dropping expired entries.
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
        // Lock removal provides at-most-once.
        let parked = {
            let mut entries = self.lock();
            if entries
                .get(id)
                .is_some_and(|parked| chrono::Utc::now() > parked.expires_at)
            {
                return Err(ResolveError::NotFound);
            }
            entries.remove(id)
        };
        let parked = parked.ok_or(ResolveError::NotFound)?;
        self.lock_decided().insert(
            *id,
            DecidedEntry {
                decision,
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

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        self.lock().remove(id);
        Ok(())
    }

    async fn cancel_request(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let cleared: Vec<ParkedApproval> = self
            .lock()
            .extract_if(.., |_, parked| parked.request.request_id == request_id)
            .map(|(_, parked)| parked)
            .collect();
        Ok(cleared)
    }
}

/// The skill-invocation logs as a plain map.
///
/// Unlike approvals, skill records have no natural removal event, so this
/// store bounds growth itself: a per-log record cap and a
/// least-recently-touched log eviction cap.
#[derive(Default)]
pub struct InMemorySkillInvocationStore {
    // `std::sync::Mutex`: every operation is a synchronous map op; nothing
    // awaits while holding the lock.
    inner: Mutex<SkillLogs>,
}

#[derive(Default)]
struct SkillLogs {
    logs: HashMap<SkillLogKey, SkillLogEntry>,
    /// Non-decreasing touch counter backing least-recently-touched eviction.
    clock: u64,
}

#[derive(Default)]
struct SkillLogEntry {
    records: Vec<SkillInvocationRecord>,
    last_touched: u64,
}

impl InMemorySkillInvocationStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SkillLogs> {
        self.inner
            .lock()
            .expect("skill invocation store lock poisoned")
    }
}

#[async_trait]
impl SkillInvocationStore for InMemorySkillInvocationStore {
    async fn record(
        &self,
        log: &SkillLogKey,
        record: SkillInvocationRecord,
    ) -> Result<(), SessionStoreError> {
        let mut inner = self.lock();
        // Saturating rather than wrapping: a wrapped counter would read as
        // the oldest touch and evict the most recently used log. At
        // saturation every touch ties, so eviction picks arbitrarily instead
        // of backwards.
        inner.clock = inner.clock.saturating_add(1);
        let clock = inner.clock;

        let entry = inner.logs.entry(log.clone()).or_default();
        entry.last_touched = clock;

        let key = record.invocation.dedup_key();
        if entry
            .records
            .iter()
            .any(|existing| existing.invocation.dedup_key() == key)
        {
            return Ok(());
        }
        if entry.records.len() >= MAX_SKILL_RECORDS_PER_LOG {
            tracing::warn!(
                session_id = log.session_id.as_str(),
                agent_id = log.agent_id,
                cap = MAX_SKILL_RECORDS_PER_LOG,
                "skill invocation store at per-log capacity; dropping new record"
            );
            return Ok(());
        }
        entry.records.push(record);

        if inner.logs.len() > MAX_SKILL_LOGS
            && let Some(evict) = inner
                .logs
                .iter()
                .min_by_key(|(_, entry)| entry.last_touched)
                .map(|(key, _)| key.clone())
        {
            inner.logs.remove(&evict);
        }
        Ok(())
    }

    async fn list(
        &self,
        log: &SkillLogKey,
    ) -> Result<Vec<SkillInvocationRecord>, SessionStoreError> {
        let mut inner = self.lock();
        inner.clock = inner.clock.saturating_add(1);
        let clock = inner.clock;
        let Some(entry) = inner.logs.get_mut(log) else {
            return Ok(Vec::new());
        };
        entry.last_touched = clock;
        let mut records = entry.records.clone();
        records.sort_by_key(|r| (r.anchor, r.seq));
        Ok(records)
    }
}

/// The session journals as a bounded ring per session.
pub struct InMemoryJournalStore {
    // `std::sync::Mutex`: every operation is a synchronous ring op; nothing
    // awaits while holding the lock.
    inner: Mutex<Journals>,
    max_entries_per_session: usize,
    max_sessions: usize,
    max_marks: usize,
}

#[derive(Default)]
struct Journals {
    sessions: HashMap<SessionId, JournalRing>,
    /// Each session's newest sequence number.
    marks: HashMap<SessionId, Mark>,
    /// Non-decreasing touch counter backing least-recently-touched eviction.
    clock: u64,
}

struct Mark {
    latest: SequenceNumber,
    last_touched: u64,
}

#[derive(Default)]
struct JournalRing {
    /// Contiguous, in sequence order.
    entries: VecDeque<Arc<SessionEvent>>,
    last_touched: u64,
}

impl Default for InMemoryJournalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryJournalStore {
    /// A store with the default bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::with_bounds(
            DEFAULT_MAX_JOURNAL_ENTRIES_PER_SESSION,
            DEFAULT_MAX_JOURNAL_SESSIONS,
        )
    }

    /// A store keeping at most `max_entries_per_session` events of a session
    /// and the events of at most `max_sessions` sessions, and remembering the
    /// newest sequence number of many more sessions than that. A bound of zero
    /// keeps one: a store that holds nothing would make every append a silent
    /// drop.
    #[must_use]
    pub fn with_bounds(max_entries_per_session: usize, max_sessions: usize) -> Self {
        let max_sessions = max_sessions.max(1);
        Self {
            inner: Mutex::new(Journals::default()),
            max_entries_per_session: max_entries_per_session.max(1),
            max_sessions,
            max_marks: max_sessions.saturating_mul(JOURNAL_MARKS_PER_SESSION),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Journals> {
        self.inner.lock().expect("journal store lock poisoned")
    }
}

impl Journals {
    /// The touch counter's next value.
    fn tick(&mut self) -> u64 {
        // Saturating rather than wrapping, for the reason the skill store's
        // counter is.
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    /// Lets the least recently touched session's entries go past
    /// `max_sessions`, and the least recently touched mark of a session whose
    /// entries are already gone past `max_marks`. A session with entries
    /// keeps its mark, so its numbering never outlives what backs it.
    fn evict_past(&mut self, max_sessions: usize, max_marks: usize) {
        if self.sessions.len() > max_sessions
            && let Some(evict) = self
                .sessions
                .iter()
                .min_by_key(|(_, ring)| ring.last_touched)
                .map(|(key, _)| key.clone())
        {
            self.sessions.remove(&evict);
        }
        if self.marks.len() > max_marks
            && let Some(evict) = self
                .marks
                .iter()
                .filter(|(key, _)| !self.sessions.contains_key(*key))
                .min_by_key(|(_, mark)| mark.last_touched)
                .map(|(key, _)| key.clone())
        {
            self.marks.remove(&evict);
        }
    }
}

#[async_trait]
impl JournalStore for InMemoryJournalStore {
    /// Journal entries have no natural removal event, so this store bounds
    /// itself here: a session at its entry cap lets its oldest entry go, past
    /// the session cap the least recently touched session's entries go, and
    /// its newest sequence number is remembered on after them.
    ///
    /// Checks `expected` against the session's mark, which outlives its
    /// entries, so a session whose entries were let go goes on numbering past
    /// them. A session whose mark is gone too is one this store forgot by its
    /// own bounding rather than one another writer moved, so `expected` is
    /// taken as where it stands, and a journal still open over it goes on
    /// numbering where it was.
    async fn append(
        &self,
        session: &SessionId,
        expected: Option<SequenceNumber>,
        run_id: Option<RunId>,
        at: aura_events::Timestamp,
        payload: SessionEventPayload,
    ) -> Result<Arc<SessionEvent>, SessionStoreError> {
        let mut inner = self.lock();
        let clock = inner.tick();
        let actual = inner.marks.get(session).map(|mark| mark.latest);
        if actual.is_some() && actual != expected {
            return Err(SessionStoreError::Conflict { expected, actual });
        }
        let seq = expected.map_or(SequenceNumber::FIRST, SequenceNumber::next);
        let event = Arc::new(SessionEvent {
            session_id: session.clone(),
            run_id,
            seq,
            at,
            payload,
        });

        inner.marks.insert(
            session.clone(),
            Mark {
                latest: seq,
                last_touched: clock,
            },
        );
        let ring = inner.sessions.entry(session.clone()).or_default();
        ring.last_touched = clock;
        if ring.entries.len() >= self.max_entries_per_session {
            ring.entries.pop_front();
        }
        ring.entries.push_back(Arc::clone(&event));

        inner.evict_past(self.max_sessions, self.max_marks);
        Ok(event)
    }

    async fn read(
        &self,
        session: &SessionId,
        from: SequenceNumber,
        to: SequenceNumber,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        let mut inner = self.lock();
        let clock = inner.tick();
        if let Some(mark) = inner.marks.get_mut(session) {
            mark.last_touched = clock;
        }
        let Some(ring) = inner.sessions.get_mut(session) else {
            return Ok(Vec::new());
        };
        ring.last_touched = clock;

        let start = ring.entries.partition_point(|event| event.seq < from);
        Ok(ring
            .entries
            .range(start..)
            .take_while(|event| event.seq <= to)
            .take(limit)
            .map(|event| JournalEntry::Event(Arc::clone(event)))
            .collect())
    }

    async fn latest(
        &self,
        session: &SessionId,
    ) -> Result<Option<SequenceNumber>, SessionStoreError> {
        Ok(self.lock().marks.get(session).map(|mark| mark.latest))
    }
}

/// A [`RunStore`] in this process: each run's facts in a map ordered by run
/// id.
pub struct InMemoryRunStore {
    // `std::sync::Mutex`: every operation is a synchronous map op; nothing
    // awaits while holding the lock.
    runs: Mutex<BTreeMap<RunId, RunFacts>>,
    max_runs: usize,
}

impl Default for InMemoryRunStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryRunStore {
    /// A store with the default bound.
    #[must_use]
    pub fn new() -> Self {
        Self::with_bounds(DEFAULT_MAX_RUNS)
    }

    /// A store keeping the facts of at most `max_runs` runs. A bound of zero
    /// keeps one, as the journal store's does.
    #[must_use]
    pub fn with_bounds(max_runs: usize) -> Self {
        Self {
            runs: Mutex::new(BTreeMap::new()),
            max_runs: max_runs.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<RunId, RunFacts>> {
        self.runs.lock().expect("run store lock poisoned")
    }
}

#[async_trait]
impl RunStore for InMemoryRunStore {
    /// Past the bound, drops the oldest ended run, or the oldest run when
    /// none has ended. Minted run ids sort by when they were minted, so the
    /// map's first entries are the oldest.
    async fn put(&self, facts: &RunFacts) -> Result<(), SessionStoreError> {
        let mut runs = self.lock();
        runs.insert(facts.run_id, facts.clone());
        if runs.len() > self.max_runs {
            let evict = runs
                .iter()
                .find(|(_, facts)| facts.status.is_terminal())
                .or_else(|| runs.iter().next())
                .map(|(id, _)| *id);
            if let Some(evict) = evict {
                runs.remove(&evict);
            }
        }
        Ok(())
    }

    async fn get(&self, run: &RunId) -> Result<Option<RunFacts>, SessionStoreError> {
        Ok(self.lock().get(run).cloned())
    }

    async fn list(&self, filter: &RunFilter) -> Result<Vec<RunFacts>, SessionStoreError> {
        use std::ops::Bound;

        let start = filter.after.map_or(Bound::Unbounded, Bound::Excluded);
        Ok(self
            .lock()
            .range((start, Bound::Unbounded))
            .map(|(_, facts)| facts)
            .filter(|facts| filter.matches(facts))
            .take(filter.limit)
            .cloned()
            .collect())
    }

    async fn finish(
        &self,
        _terminal: &SessionEvent,
        facts: &RunFacts,
    ) -> Result<(), SessionStoreError> {
        self.put(facts).await
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
    use crate::config::SessionId;
    use crate::hitl::{
        AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, PROTOCOL_VERSION,
    };
    use crate::session_store::RunStatus;

    fn parked(request_id: &str) -> ParkedApproval {
        let now = chrono::Utc::now();
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "test-instance".to_string(),
                decision_id: DecisionId::generate(),
                request_id: request_id.to_string(),
                scope: AgentScope::Single { session_id: None },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "test_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "test_tool".to_string(),
                    tool_namespace: None,
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

    /// Retention pruning drops entries past window.
    #[tokio::test]
    async fn recorded_decision_is_pruned_after_retention_window() {
        let store = InMemoryApprovalStore::new();
        let id = parked("req-prune").request.decision_id;
        store.lock_decided().insert(
            id,
            DecidedEntry {
                decision: ApprovalDecision::Approved,
                keep_until: chrono::Utc::now() - chrono::Duration::seconds(1),
            },
        );

        assert_eq!(store.decision(&id).await.unwrap(), None);
    }

    /// `resolve` refuses expired tickets.
    #[tokio::test]
    async fn expired_ticket_refuses_resolve() {
        let store = InMemoryApprovalStore::new();
        let mut entry = parked("req-expired");
        entry.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        let id = entry.request.decision_id;
        store.register(entry).await.unwrap();

        assert_eq!(
            store.resolve(&id, ApprovalDecision::Approved).await,
            Err(ResolveError::NotFound)
        );
        assert_eq!(store.decision(&id).await.unwrap(), None);
        assert!(store.get(&id).await.unwrap().is_some());
    }

    /// `get` returns expired tickets.
    #[tokio::test]
    async fn expired_ticket_is_returned_by_get_until_remove() {
        let store = InMemoryApprovalStore::new();
        let mut entry = parked("req-expired-get");
        entry.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        let id = entry.request.decision_id;
        store.register(entry).await.unwrap();

        assert!(store.get(&id).await.unwrap().is_some());
        store.remove(&id).await.unwrap();
        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn approval_store_cancel_request_removes_only_matching() {
        let store = InMemoryApprovalStore::new();
        let cancel = parked("req-cancel");
        let cancel_id = cancel.request.decision_id;
        let keep = parked("req-keep");
        let keep_id = keep.request.decision_id;
        store.register(cancel).await.unwrap();
        store.register(keep).await.unwrap();

        let cleared = store.cancel_request("req-cancel").await.unwrap();

        assert_eq!(cleared.len(), 1, "only the matching ticket is cleared");
        assert_eq!(cleared[0].request.decision_id, cancel_id);
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

    fn skill_log(session_id: impl Into<String>, agent_id: &str) -> SkillLogKey {
        SkillLogKey::new(SessionId::new(session_id).unwrap(), agent_id)
    }

    #[tokio::test]
    async fn skill_store_lists_records_ordered_by_anchor_then_seq() {
        let store = InMemorySkillInvocationStore::new();
        let log = skill_log("sess-1", "agent");

        store
            .record(&log, skill_record("late", 5, 0))
            .await
            .unwrap();
        store
            .record(&log, skill_record("second", 1, 1))
            .await
            .unwrap();
        store
            .record(&log, skill_record("first", 1, 0))
            .await
            .unwrap();

        let names: Vec<String> = store
            .list(&log)
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
        let log = skill_log("sess-1", "agent");

        store.record(&log, skill_record("dup", 1, 0)).await.unwrap();
        // Same invocation re-recorded later must keep the first position.
        store.record(&log, skill_record("dup", 7, 3)).await.unwrap();

        let records = store.list(&log).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].anchor, 1);
    }

    #[tokio::test]
    async fn skill_store_sessions_are_independent() {
        let store = InMemorySkillInvocationStore::new();
        store
            .record(&skill_log("sess-a", "agent"), skill_record("a", 1, 0))
            .await
            .unwrap();

        assert!(
            store
                .list(&skill_log("sess-b", "agent"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .list(&skill_log("sess-a", "agent"))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// One session id under two agents is two logs: neither lists the
    /// other's records, and the same invocation records independently in
    /// each rather than being deduped across them.
    #[tokio::test]
    async fn skill_store_agents_sharing_a_session_are_independent() {
        let store = InMemorySkillInvocationStore::new();
        let first = skill_log("sess-1", "agent-a");
        let second = skill_log("sess-1", "agent-b");

        store
            .record(&first, skill_record("shared", 1, 0))
            .await
            .unwrap();
        assert!(store.list(&second).await.unwrap().is_empty());

        store
            .record(&second, skill_record("shared", 5, 0))
            .await
            .unwrap();
        assert_eq!(store.list(&first).await.unwrap()[0].anchor, 1);
        assert_eq!(store.list(&second).await.unwrap()[0].anchor, 5);
    }

    #[tokio::test]
    async fn skill_store_caps_records_per_log() {
        let store = InMemorySkillInvocationStore::new();
        let log = skill_log("sess-cap", "agent");
        for i in 0..(MAX_SKILL_RECORDS_PER_LOG + 5) {
            store
                .record(&log, skill_record(&format!("s{i}"), i as u32, 0))
                .await
                .unwrap();
        }
        assert_eq!(
            store.list(&log).await.unwrap().len(),
            MAX_SKILL_RECORDS_PER_LOG
        );

        // The cap is per log: another agent in the same session still records.
        let other = skill_log("sess-cap", "other-agent");
        store
            .record(&other, skill_record("s0", 1, 0))
            .await
            .unwrap();
        assert_eq!(store.list(&other).await.unwrap().len(), 1);
    }

    /// A saturated touch counter keeps the store working: writes and reads
    /// still succeed, and eviction still bounds the log map — it just stops
    /// being ordered by recency.
    #[tokio::test]
    async fn skill_store_survives_a_saturated_clock() {
        let store = InMemorySkillInvocationStore::new();
        store.lock().clock = u64::MAX;

        for i in 0..=MAX_SKILL_LOGS {
            store
                .record(
                    &skill_log(format!("sess-{i}"), "agent"),
                    skill_record("s", 1, 0),
                )
                .await
                .unwrap();
        }
        // Reads bump the counter too, so they must survive saturation as well.
        store.list(&skill_log("sess-0", "agent")).await.unwrap();

        let inner = store.lock();
        assert_eq!(
            inner.clock,
            u64::MAX,
            "the counter stops instead of wrapping"
        );
        assert!(
            inner.logs.len() <= MAX_SKILL_LOGS,
            "eviction still bounds the map at {} logs, got {}",
            MAX_SKILL_LOGS,
            inner.logs.len()
        );
    }

    #[tokio::test]
    async fn skill_store_evicts_least_recently_touched_log() {
        let store = InMemorySkillInvocationStore::new();
        for i in 0..MAX_SKILL_LOGS {
            store
                .record(
                    &skill_log(format!("sess-{i}"), "agent"),
                    skill_record("s", 1, 0),
                )
                .await
                .unwrap();
        }
        // Touch the oldest log so it is no longer the eviction candidate.
        let oldest = skill_log("sess-0", "agent");
        assert_eq!(store.list(&oldest).await.unwrap().len(), 1);

        // One past the cap evicts the least-recently-touched log (sess-1).
        store
            .record(
                &skill_log("sess-overflow", "agent"),
                skill_record("s", 1, 0),
            )
            .await
            .unwrap();

        assert_eq!(store.list(&oldest).await.unwrap().len(), 1);
        assert!(
            store
                .list(&skill_log("sess-1", "agent"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Appends one event to `session` at the newest number the store holds
    /// and returns the number it minted.
    async fn append_one(store: &InMemoryJournalStore, session: &str) -> u64 {
        let session = SessionId::new(session).unwrap();
        let expected = store.latest(&session).await.unwrap();
        append_expecting(store, &session, expected)
            .await
            .unwrap()
            .seq
            .get()
    }

    async fn append_expecting(
        store: &InMemoryJournalStore,
        session: &SessionId,
        expected: Option<SequenceNumber>,
    ) -> Result<Arc<SessionEvent>, SessionStoreError> {
        store
            .append(
                session,
                expected,
                None,
                aura_events::Timestamp::from_unix_millis(1),
                SessionEventPayload::Lifecycle(aura_events::run::LifecycleEvent::ClaimsExhausted),
            )
            .await
    }

    /// Appends to `session` until it has minted every number in `seqs`,
    /// asserting each is the one minted.
    async fn fill(
        store: &InMemoryJournalStore,
        session: &str,
        seqs: std::ops::RangeInclusive<u64>,
    ) {
        for seq in seqs {
            assert_eq!(append_one(store, session).await, seq);
        }
    }

    async fn read_seqs(
        store: &InMemoryJournalStore,
        session: &str,
        from: u64,
        to: u64,
    ) -> Vec<u64> {
        store
            .read(
                &SessionId::new(session).unwrap(),
                SequenceNumber::try_from(from).unwrap(),
                SequenceNumber::try_from(to).unwrap(),
                usize::MAX,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.seq().get())
            .collect()
    }

    #[tokio::test]
    async fn journal_store_reads_a_range_inclusive_in_order() {
        let store = InMemoryJournalStore::new();
        fill(&store, "sess-1", 1..=6).await;

        assert_eq!(read_seqs(&store, "sess-1", 2, 4).await, vec![2, 3, 4]);
        assert_eq!(
            read_seqs(&store, "sess-1", 1, 6).await,
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            read_seqs(&store, "sess-1", 5, 40).await,
            vec![5, 6],
            "a range past the end returns what exists"
        );
        assert!(read_seqs(&store, "sess-1", 7, 9).await.is_empty());
        assert!(
            read_seqs(&store, "sess-unknown", 1, 9).await.is_empty(),
            "a session the store never saw reads as empty"
        );
    }

    /// A limit counts the entries returned, not the numbers the range spans,
    /// so one read starting before what the store retains reaches the
    /// retained entries rather than coming back empty.
    #[tokio::test]
    async fn journal_store_read_stops_at_its_limit() {
        let store = InMemoryJournalStore::with_bounds(4, 8);
        fill(&store, "sess-1", 1..=100).await;
        let session = SessionId::new("sess-1").unwrap();
        let read = |from: u64, limit: usize| {
            let (store, session) = (&store, &session);
            async move {
                store
                    .read(
                        session,
                        SequenceNumber::try_from(from).unwrap(),
                        SequenceNumber::try_from(100).unwrap(),
                        limit,
                    )
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|entry| entry.seq().get())
                    .collect::<Vec<_>>()
            }
        };

        assert_eq!(read(1, 2).await, vec![97, 98]);
        assert_eq!(read(99, 2).await, vec![99, 100]);
        assert_eq!(read(1, 10).await, vec![97, 98, 99, 100]);
        assert!(read(1, 0).await.is_empty());
    }

    #[tokio::test]
    async fn journal_store_latest_is_the_newest_or_none() {
        let store = InMemoryJournalStore::new();
        let session = SessionId::new("sess-1").unwrap();
        assert_eq!(store.latest(&session).await.unwrap(), None);

        fill(&store, "sess-1", 1..=3).await;
        assert_eq!(
            store.latest(&session).await.unwrap(),
            Some(SequenceNumber::try_from(3).unwrap())
        );
    }

    /// Dropping the oldest first is what keeps what remains a suffix, which is
    /// what lets a reader name what is missing from the numbers alone.
    #[tokio::test]
    async fn journal_store_drops_a_sessions_oldest_entries_past_its_cap() {
        let store = InMemoryJournalStore::with_bounds(3, 8);
        fill(&store, "sess-1", 1..=5).await;

        assert_eq!(read_seqs(&store, "sess-1", 1, 5).await, vec![3, 4, 5]);
        assert_eq!(
            store
                .latest(&SessionId::new("sess-1").unwrap())
                .await
                .unwrap(),
            Some(SequenceNumber::try_from(5).unwrap())
        );
    }

    #[tokio::test]
    async fn journal_store_evicts_the_least_recently_touched_session() {
        let store = InMemoryJournalStore::with_bounds(8, 2);
        fill(&store, "sess-a", 1..=1).await;
        fill(&store, "sess-b", 1..=1).await;
        // Reading touches: `sess-a` is now the more recent of the two.
        assert_eq!(read_seqs(&store, "sess-a", 1, 1).await, vec![1]);

        fill(&store, "sess-c", 1..=1).await;

        assert_eq!(read_seqs(&store, "sess-a", 1, 1).await, vec![1]);
        assert!(read_seqs(&store, "sess-b", 1, 1).await.is_empty());
        assert_eq!(read_seqs(&store, "sess-c", 1, 1).await, vec![1]);
        assert_eq!(store.lock().sessions.len(), 2);
    }

    /// The store mints each session's numbers from the first, one at a time,
    /// whatever other sessions are doing, and stores the event under the
    /// number it returns.
    #[tokio::test]
    async fn journal_store_mints_each_sessions_numbers_densely() {
        let store = InMemoryJournalStore::new();
        assert_eq!(append_one(&store, "sess-a").await, 1);
        assert_eq!(append_one(&store, "sess-b").await, 1);
        assert_eq!(append_one(&store, "sess-a").await, 2);
        assert_eq!(append_one(&store, "sess-a").await, 3);
        assert_eq!(append_one(&store, "sess-b").await, 2);

        assert_eq!(read_seqs(&store, "sess-a", 1, 9).await, vec![1, 2, 3]);
        assert_eq!(read_seqs(&store, "sess-b", 1, 9).await, vec![1, 2]);
    }

    /// An append expecting any number but the one the session is at is
    /// refused, whether behind it, ahead of it, or expecting an empty
    /// session, and leaves the session as it was.
    #[tokio::test]
    async fn journal_store_refuses_an_append_expecting_another_number() {
        let store = InMemoryJournalStore::new();
        fill(&store, "sess-1", 1..=3).await;
        let session = SessionId::new("sess-1").unwrap();
        let at = |n: u64| Some(SequenceNumber::try_from(n).unwrap());

        for expected in [at(2), at(5), None] {
            let err = append_expecting(&store, &session, expected)
                .await
                .unwrap_err();
            assert_eq!(
                err,
                SessionStoreError::Conflict {
                    expected,
                    actual: at(3)
                }
            );
        }
        assert_eq!(read_seqs(&store, "sess-1", 1, 9).await, vec![1, 2, 3]);
        assert_eq!(append_one(&store, "sess-1").await, 4);
    }

    /// A session the store forgot entirely, entries and newest number both,
    /// takes the writer's expected number as where it stands, so a writer
    /// still holding its stream goes on numbering where it was.
    #[tokio::test]
    async fn journal_store_takes_the_writers_number_for_a_session_it_forgot() {
        let store = InMemoryJournalStore::new();
        let session = SessionId::new("sess-forgotten").unwrap();
        let event = append_expecting(&store, &session, Some(SequenceNumber::try_from(7).unwrap()))
            .await
            .unwrap();
        assert_eq!(event.seq.get(), 8);
        assert_eq!(
            store.latest(&session).await.unwrap(),
            Some(SequenceNumber::try_from(8).unwrap())
        );
    }

    /// A session whose entries are let go is still remembered by its newest
    /// number, and goes on numbering past it rather than handing one out a
    /// second time.
    #[tokio::test]
    async fn journal_store_remembers_a_sessions_newest_number_past_its_entries() {
        let store = InMemoryJournalStore::with_bounds(8, 1);
        fill(&store, "sess-a", 1..=3).await;
        fill(&store, "sess-b", 1..=1).await;

        assert!(read_seqs(&store, "sess-a", 1, 9).await.is_empty());
        assert_eq!(
            store
                .latest(&SessionId::new("sess-a").unwrap())
                .await
                .unwrap(),
            Some(SequenceNumber::try_from(3).unwrap())
        );
        assert_eq!(append_one(&store, "sess-a").await, 4);
        assert_eq!(read_seqs(&store, "sess-a", 1, 9).await, vec![4]);
    }

    /// Marks are bounded too, far past the sessions whose entries are kept,
    /// and the one forgotten is never a session whose entries are still held.
    #[tokio::test]
    async fn journal_store_bounds_the_marks_it_remembers() {
        let store = InMemoryJournalStore::with_bounds(8, 1);
        let cap = JOURNAL_MARKS_PER_SESSION;
        for i in 0..=cap {
            fill(&store, &format!("sess-{i}"), 1..=1).await;
        }

        let latest = |name: String| {
            let store = &store;
            async move { store.latest(&SessionId::new(name).unwrap()).await.unwrap() }
        };
        assert_eq!(
            latest("sess-0".into()).await,
            None,
            "the oldest mark is forgotten"
        );
        assert!(latest("sess-1".into()).await.is_some());
        assert!(
            latest(format!("sess-{cap}")).await.is_some(),
            "the session whose entries are held keeps its mark"
        );
        assert_eq!(store.lock().marks.len(), cap);
    }

    #[tokio::test]
    async fn journal_store_sessions_are_independent() {
        let store = InMemoryJournalStore::new();
        fill(&store, "sess-a", 1..=2).await;
        fill(&store, "sess-b", 1..=1).await;

        assert_eq!(read_seqs(&store, "sess-a", 1, 9).await, vec![1, 2]);
        assert_eq!(read_seqs(&store, "sess-b", 1, 9).await, vec![1]);
    }

    /// A store that held nothing would turn every append into a silent drop,
    /// so the smallest bound is one.
    #[tokio::test]
    async fn journal_store_bounds_of_zero_keep_one() {
        let store = InMemoryJournalStore::with_bounds(0, 0);
        fill(&store, "sess-a", 1..=2).await;
        assert_eq!(read_seqs(&store, "sess-a", 1, 9).await, vec![2]);

        fill(&store, "sess-b", 1..=1).await;
        assert!(read_seqs(&store, "sess-a", 1, 9).await.is_empty());
        assert_eq!(read_seqs(&store, "sess-b", 1, 9).await, vec![1]);
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

    /// A run id that orders by `n`, as minted ids order by when they were
    /// minted.
    fn run_id(n: u128) -> RunId {
        RunId::try_from(uuid::Uuid::from_u128(n)).expect("a non-zero UUID is a run id")
    }

    fn facts(n: u128, session: &str, status: RunStatus) -> RunFacts {
        RunFacts {
            run_id: run_id(n),
            session_id: SessionId::new(session).unwrap(),
            agent: "agent".to_owned(),
            status,
            liveness: aura_events::run::Liveness::default(),
            started_at: aura_events::Timestamp::from_unix_millis(n as u64),
            ended_at: None,
            usage: aura_events::TokenUsage::default(),
            first_seq: None,
            latest_seq: None,
            pending_approvals: Vec::new(),
            checkpoint: None,
            continues: None,
        }
    }

    async fn listed(store: &InMemoryRunStore, filter: RunFilter) -> Vec<u128> {
        store
            .list(&filter)
            .await
            .unwrap()
            .into_iter()
            .map(|facts| facts.run_id.as_uuid().as_u128())
            .collect()
    }

    #[tokio::test]
    async fn run_store_put_replaces_and_get_reads_back() {
        use RunStatus::*;
        let store = InMemoryRunStore::new();
        assert_eq!(store.get(&run_id(1)).await.unwrap(), None);

        store.put(&facts(1, "sess-1", Preparing)).await.unwrap();
        store.put(&facts(1, "sess-1", Running)).await.unwrap();

        assert_eq!(
            store.get(&run_id(1)).await.unwrap(),
            Some(facts(1, "sess-1", Running))
        );
        assert_eq!(store.lock().len(), 1);
    }

    #[tokio::test]
    async fn run_store_finish_keeps_the_terminal_facts() {
        use RunStatus::*;
        let store = InMemoryRunStore::new();
        store.put(&facts(1, "sess-1", Running)).await.unwrap();
        let terminal = SessionEvent {
            session_id: SessionId::new("sess-1").unwrap(),
            run_id: Some(run_id(1)),
            seq: SequenceNumber::FIRST,
            at: aura_events::Timestamp::from_unix_millis(2),
            payload: SessionEventPayload::Lifecycle(aura_events::run::LifecycleEvent::Finished {
                usage: aura_events::TokenUsage::default(),
            }),
        };

        store
            .finish(&terminal, &facts(1, "sess-1", Finished))
            .await
            .unwrap();

        assert_eq!(
            store.get(&run_id(1)).await.unwrap().unwrap().status,
            Finished
        );
    }

    #[tokio::test]
    async fn run_store_lists_in_run_id_order_by_session_and_status() {
        use RunStatus::*;
        let store = InMemoryRunStore::new();
        for run in [
            facts(3, "sess-a", Finished),
            facts(1, "sess-a", Running),
            facts(2, "sess-b", Finished),
            facts(4, "sess-b", Cancelled),
        ] {
            store.put(&run).await.unwrap();
        }

        assert_eq!(listed(&store, RunFilter::default()).await, vec![1, 2, 3, 4]);
        let in_a = RunFilter {
            session: Some(SessionId::new("sess-a").unwrap()),
            ..RunFilter::default()
        };
        assert_eq!(listed(&store, in_a.clone()).await, vec![1, 3]);
        let finished = RunFilter {
            status: Some(Finished),
            ..RunFilter::default()
        };
        assert_eq!(listed(&store, finished).await, vec![2, 3]);
        let finished_in_a = RunFilter {
            status: Some(Finished),
            ..in_a
        };
        assert_eq!(listed(&store, finished_in_a).await, vec![3]);
    }

    #[tokio::test]
    async fn run_store_lists_by_whether_a_run_waits_on_an_approval() {
        use RunStatus::*;
        let store = InMemoryRunStore::new();
        let mut waiting = facts(2, "sess-a", Running);
        waiting.pending_approvals = vec![DecisionId::generate()];
        store.put(&facts(1, "sess-a", Running)).await.unwrap();
        store.put(&waiting).await.unwrap();

        let pending = |pending| RunFilter {
            pending_approval: Some(pending),
            ..RunFilter::default()
        };
        assert_eq!(listed(&store, pending(true)).await, vec![2]);
        assert_eq!(listed(&store, pending(false)).await, vec![1]);
    }

    /// The last id of a page continues the listing, and the limit counts the
    /// runs the filter selects, not the ones it skips.
    #[tokio::test]
    async fn run_store_pages_through_a_listing() {
        use RunStatus::*;
        let store = InMemoryRunStore::new();
        for n in 1..=7 {
            let status = if n % 2 == 0 { Running } else { Finished };
            store.put(&facts(n, "sess-a", status)).await.unwrap();
        }
        let page = |after: Option<u128>| RunFilter {
            status: Some(Finished),
            limit: 2,
            after: after.map(run_id),
            ..RunFilter::default()
        };

        assert_eq!(listed(&store, page(None)).await, vec![1, 3]);
        assert_eq!(listed(&store, page(Some(3))).await, vec![5, 7]);
        assert!(listed(&store, page(Some(7))).await.is_empty());
        let none = RunFilter {
            limit: 0,
            ..RunFilter::default()
        };
        assert!(listed(&store, none).await.is_empty());
    }

    #[tokio::test]
    async fn run_store_lets_the_oldest_ended_run_go_past_its_bound() {
        use RunStatus::*;
        let store = InMemoryRunStore::with_bounds(3);
        store.put(&facts(1, "sess-a", Running)).await.unwrap();
        store.put(&facts(2, "sess-a", Finished)).await.unwrap();
        store.put(&facts(3, "sess-a", Failed)).await.unwrap();

        store.put(&facts(4, "sess-a", Running)).await.unwrap();

        assert_eq!(listed(&store, RunFilter::default()).await, vec![1, 3, 4]);
    }

    #[tokio::test]
    async fn run_store_lets_the_oldest_run_go_when_none_has_ended() {
        use RunStatus::*;
        let store = InMemoryRunStore::with_bounds(2);
        for n in 1..=3 {
            store.put(&facts(n, "sess-a", Running)).await.unwrap();
        }

        assert_eq!(listed(&store, RunFilter::default()).await, vec![2, 3]);
    }

    #[tokio::test]
    async fn run_store_bound_of_zero_keeps_one() {
        use RunStatus::*;
        let store = InMemoryRunStore::with_bounds(0);
        store.put(&facts(1, "sess-a", Finished)).await.unwrap();
        store.put(&facts(2, "sess-a", Finished)).await.unwrap();

        assert_eq!(listed(&store, RunFilter::default()).await, vec![2]);
    }
}
