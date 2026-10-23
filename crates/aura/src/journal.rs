//! A session's journal: the ordered record of everything its runs did and
//! everything that happened to them, and the source every observer reads.
//!
//! A run has one event channel, and whoever holds its receiver is the run's
//! only observer: present from the first byte, and if it stops polling, the
//! bounded channel fills and the run waits on it. That is the shape of a
//! request-bound agent, and it is why attaching to a run already underway was
//! impossible — a channel holds nothing. [`SessionJournal::drain`] makes the
//! journal the one reader of that receiver; the run's owner appends its
//! [`LifecycleEvent`]s through the same [`SessionJournal::append`], so a
//! session has one sequence; and anyone else reads it from a cursor through
//! [`SessionJournal::subscribe`], replay and live as one stream.
//!
//! The journal is for observers. The facts a run's owner keeps about it are
//! held as plain state, not rebuilt from here.
//!
//! [`LifecycleEvent`]: aura_events::run::LifecycleEvent

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use aura_events::agent::AgentEvent;
use aura_events::run::{SessionEvent, SessionEventPayload};
use aura_events::{RunId, SequenceNumber, SessionId, Timestamp};
use bytes::Bytes;
use futures::Stream;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::session_store::{JournalEntry, JournalStore, SessionStoreError};

/// Live events buffered for a subscriber before it falls back to the store.
pub const LIVE_CAPACITY: usize = 1024;

/// Entries a replay reads from the store at a time.
const REPLAY_PAGE: usize = 512;

/// Where in a session's stream a subscriber starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// Everything the store still holds, then live.
    Start,
    /// This sequence number and everything after it, then live.
    From(SequenceNumber),
    /// Everything after this sequence number, then live.
    After(SequenceNumber),
    /// Only what is appended from now on.
    Live,
}

/// What a subscriber reads.
#[derive(Debug, Clone)]
pub enum JournalItem {
    Event(Arc<SessionEvent>),
    /// The subscriber missed events; `resumed_at` is the first one it sees
    /// again.
    Gap {
        resumed_at: SequenceNumber,
    },
    /// A stored entry this binary cannot decode — a newer variant after a
    /// rollback. `raw` is kept for a reader that can.
    Unknown {
        seq: SequenceNumber,
        raw: Bytes,
    },
}

impl JournalItem {
    /// The position this item speaks for: an event's or unknown entry's own,
    /// or the one a gap resumes at.
    pub fn seq(&self) -> SequenceNumber {
        match self {
            Self::Event(event) => event.seq,
            Self::Gap { resumed_at } => *resumed_at,
            Self::Unknown { seq, .. } => *seq,
        }
    }
}

/// The stream [`SessionJournal::subscribe`] returns.
pub type JournalSubscription = Pin<Box<dyn Stream<Item = JournalItem> + Send>>;

/// One session's journal: the sequencer and fan-out in front of its
/// [`JournalStore`] entries.
pub struct SessionJournal {
    session_id: SessionId,
    store: Arc<dyn JournalStore>,
    /// Serializes mint → store → publish.
    append: Mutex<()>,
    /// The newest sequence number the store has minted, zero for none.
    latest: AtomicU64,
    /// The conflict that showed another writer on the session.
    fenced: OnceLock<SessionStoreError>,
    live: broadcast::Sender<Arc<SessionEvent>>,
}

impl SessionJournal {
    /// Opens the session's journal over `store`, reading the newest number the
    /// store has minted for the session as where the stream stands. The store
    /// mints, so a journal opened after another, here or on another instance,
    /// continues the same sequence. A session the store remembers nothing of
    /// starts at the first number, and a cursor taken from an earlier stream of
    /// the same id names positions in this one.
    pub async fn open(
        session_id: SessionId,
        store: Arc<dyn JournalStore>,
    ) -> Result<Self, SessionStoreError> {
        let latest = store.latest(&session_id).await?;
        let (live, _) = broadcast::channel(LIVE_CAPACITY);
        Ok(Self {
            session_id,
            store,
            append: Mutex::new(()),
            latest: AtomicU64::new(latest.map_or(0, SequenceNumber::get)),
            fenced: OnceLock::new(),
            live,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The newest sequence number in the session's stream.
    pub fn latest(&self) -> Option<SequenceNumber> {
        SequenceNumber::try_from(self.latest.load(Ordering::Acquire)).ok()
    }

    /// Whether a conflict has fenced the journal, so that every append is
    /// refused.
    pub fn is_fenced(&self) -> bool {
        self.fenced.get().is_some()
    }

    /// The one append path for the session's events, whose numbers the store
    /// mints. `run_id` is the run the event belongs to, or `None` for an event
    /// of the session itself.
    ///
    /// Holds the lock across the store's append, so a second appender waits
    /// rather than interleaving and events reach subscribers in the order
    /// their numbers were minted. Every event is stored, none elided or
    /// coalesced.
    ///
    /// Each append tells the store the newest number this journal has seen,
    /// so a store that holds another refuses it: some other writer appended
    /// to the session since, here or on another instance. That conflict
    /// fences the journal, and every later append returns it without reaching
    /// the store. Its events have already happened and cannot be decided
    /// again against what the other writer did, so retrying would only
    /// interleave the two.
    pub async fn append(
        &self,
        run_id: Option<RunId>,
        payload: SessionEventPayload,
    ) -> Result<Arc<SessionEvent>, SessionStoreError> {
        let _writer = self.append.lock().await;
        if let Some(conflict) = self.fenced.get() {
            return Err(conflict.clone());
        }

        let event = match self
            .store
            .append(
                &self.session_id,
                self.latest(),
                run_id,
                Timestamp::now(),
                payload,
            )
            .await
        {
            Ok(event) => event,
            Err(conflict @ SessionStoreError::Conflict { .. }) => {
                tracing::warn!(
                    session_id = %self.session_id, %conflict,
                    "another writer appended to this session; the journal stops appending"
                );
                let _ = self.fenced.set(conflict.clone());
                return Err(conflict);
            }
            Err(error) => return Err(error),
        };

        // Published after `latest` moves, so a subscriber that joined before
        // this send and then read `latest` either sees this number there and
        // replays the event, or receives it live. Neither path misses it.
        self.latest.store(event.seq.get(), Ordering::Release);
        // Nobody subscribed is not an error; the store has the event.
        let _ = self.live.send(Arc::clone(&event));
        Ok(event)
    }

    /// Appends every event `run` emits until its channel closes, as the one
    /// reader of the receiver `begin_run` hands out. With the journal reading,
    /// the run never waits on an observer; it waits only on its store, which is
    /// the back-pressure it should feel.
    ///
    /// A store failure ends the drain with that error and drops the receiver,
    /// so what the run emits after that is refused at once rather than
    /// waiting: `RunContext::emit` returns `false`. Stopping the run is for
    /// whoever drove the drain.
    pub async fn drain(
        &self,
        run: RunId,
        mut events: mpsc::Receiver<AgentEvent>,
    ) -> Result<(), SessionStoreError> {
        while let Some(event) = events.recv().await {
            self.append(Some(run), SessionEventPayload::Agent(event))
                .await?;
        }
        Ok(())
    }

    /// Reads the session's stream from `cursor`: replay from the store, then
    /// live, with no seam visible to the consumer, until the journal is
    /// dropped. Attaching to a run is `Cursor::From` its `Started`.
    ///
    /// Joins the live broadcast first, so everything published from then on is
    /// in the subscriber's own buffer, and goes to the store only for what that
    /// buffer cannot hold: what was published before the join, and a stretch
    /// the broadcast drops when the subscriber falls behind it, which ends
    /// where the oldest event the buffer still holds begins. A live event the
    /// replay covered is dropped by sequence number. No lock is held across a
    /// store read, so a store that awaits a round trip runs the same way, and a
    /// subscriber that falls behind holds up neither the run nor any other
    /// subscriber. Whenever the next event to deliver is later than the one
    /// expected — neither the buffer nor the store holds the ones between, or
    /// the read failed — a [`JournalItem::Gap`] precedes it.
    pub fn subscribe(&self, cursor: Cursor) -> JournalSubscription {
        let mut live = self.live.subscribe();
        // Read after joining the broadcast: an event published before the join
        // is counted here, one published after arrives live, and the number
        // moves before the publish, so nothing falls between.
        let joined_at = self.latest();

        // The first sequence number this subscriber has not yet seen.
        let mut expected = match cursor {
            Cursor::Start => SequenceNumber::FIRST,
            Cursor::From(seq) => seq,
            Cursor::After(seq) => seq.next(),
            Cursor::Live => joined_at.map_or(SequenceNumber::FIRST, SequenceNumber::next),
        };
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.clone();

        Box::pin(async_stream::stream! {
            // The last number to read from the store before going back to the
            // broadcast: first what came before the join.
            let mut fill_to = joined_at;
            // A live event that arrived past a stretch the broadcast dropped,
            // delivered once the stretch has been read from the store.
            let mut held: Option<Arc<SessionEvent>> = None;

            loop {
                if let Some(to) = fill_to.take() {
                    let mut from = expected;
                    while from <= to {
                        let page = match store.read(&session_id, from, to, REPLAY_PAGE).await {
                            Ok(page) => page,
                            Err(error) => {
                                tracing::warn!(
                                    session_id = %session_id, %from, %to, %error,
                                    "journal replay could not read the store; the subscriber misses these events"
                                );
                                break;
                            }
                        };
                        // A short page is the last one in range.
                        let last_page = page.len() < REPLAY_PAGE;
                        for entry in page {
                            let item = match entry {
                                JournalEntry::Event(event) => JournalItem::Event(event),
                                JournalEntry::Undecodable { seq, raw } => {
                                    JournalItem::Unknown { seq, raw }
                                }
                            };
                            if let Some(gap) = advance(&mut expected, item.seq()) {
                                yield gap;
                            }
                            yield item;
                        }
                        if last_page {
                            break;
                        }
                        from = expected;
                    }
                    // What the store lacked up to `to` is missed. A held event
                    // says so as it is delivered; without one, say it now
                    // rather than at the next live event, which may never come.
                    if held.is_none() && expected <= to {
                        yield JournalItem::Gap { resumed_at: to.next() };
                        expected = to.next();
                    }
                }
                if let Some(event) = held.take() {
                    if let Some(gap) = advance(&mut expected, event.seq) {
                        yield gap;
                    }
                    yield JournalItem::Event(event);
                }

                match live.recv().await {
                    // Already replayed from the store.
                    Ok(event) if event.seq < expected => {}
                    // The broadcast dropped the events between; read them from
                    // the store before this one.
                    Ok(event) if event.seq > expected => {
                        fill_to = Some(
                            SequenceNumber::try_from(event.seq.get() - 1)
                                .expect("a number past the expected one is past the first"),
                        );
                        held = Some(event);
                    }
                    Ok(event) => {
                        expected = event.seq.next();
                        yield JournalItem::Event(event);
                    }
                    // The next event received shows where the drop ends; a
                    // receiver that lagged still holds a full buffer after it.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    // Only once everything buffered has been received.
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

/// Moves `expected` past an item at `seq` that is about to be delivered, and
/// the gap to yield first when `seq` is later than what was expected.
fn advance(expected: &mut SequenceNumber, seq: SequenceNumber) -> Option<JournalItem> {
    let gap = (seq > *expected).then_some(JournalItem::Gap { resumed_at: seq });
    *expected = seq.next();
    gap
}

impl std::fmt::Debug for SessionJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionJournal")
            .field("session_id", &self.session_id)
            .field("latest", &self.latest())
            .field("subscribers", &self.live.receiver_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use aura_events::agent::AgentEventPayload;
    use aura_events::run::{LifecycleEvent, Liveness};
    use aura_events::{TokenCount, TokenUsage};
    use futures::StreamExt;

    use super::*;
    use crate::session_store::InMemoryJournalStore;

    fn seq(n: u64) -> SequenceNumber {
        SequenceNumber::try_from(n).expect("test sequence numbers start at 1")
    }

    fn said(text: &str) -> SessionEventPayload {
        SessionEventPayload::Agent(AgentEvent::single_agent(AgentEventPayload::TextDelta {
            content: text.to_string(),
        }))
    }

    fn started() -> SessionEventPayload {
        SessionEventPayload::Lifecycle(LifecycleEvent::Started {
            agent: "sre".to_string(),
            prompt: "hi".to_string(),
            timeout: None,
            liveness: Liveness::default(),
            continues: None,
        })
    }

    fn finished() -> SessionEventPayload {
        SessionEventPayload::Lifecycle(LifecycleEvent::Finished {
            usage: TokenUsage {
                prompt_tokens: TokenCount::new(10),
                completion_tokens: TokenCount::new(5),
                total_tokens: TokenCount::new(15),
            },
        })
    }

    async fn open(name: &str, store: Arc<dyn JournalStore>) -> SessionJournal {
        SessionJournal::open(SessionId::new(name).unwrap(), store)
            .await
            .expect("the in-memory store opens")
    }

    async fn journal() -> SessionJournal {
        open("sess_1", Arc::new(InMemoryJournalStore::new())).await
    }

    async fn say_times(journal: &SessionJournal, run: RunId, n: u64) {
        for i in 0..n {
            journal
                .append(Some(run), said(&format!("w{i}")))
                .await
                .expect("append succeeds");
        }
    }

    async fn next(sub: &mut JournalSubscription) -> JournalItem {
        tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("an item within 5s")
            .expect("the stream is open")
    }

    /// Whether nothing more is pending on the subscription right now.
    async fn quiet(sub: &mut JournalSubscription) -> bool {
        tokio::time::timeout(Duration::from_millis(50), sub.next())
            .await
            .is_err()
    }

    /// `event 3`, `gap→4`, `unknown 2`: the shape assertions compare.
    fn describe(item: &JournalItem) -> String {
        match item {
            JournalItem::Event(event) => format!("event {}", event.seq),
            JournalItem::Gap { resumed_at } => format!("gap→{resumed_at}"),
            JournalItem::Unknown { seq, .. } => format!("unknown {seq}"),
        }
    }

    async fn take(sub: &mut JournalSubscription, n: usize) -> Vec<String> {
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            items.push(describe(&next(sub).await));
        }
        items
    }

    fn events(range: std::ops::RangeInclusive<u64>) -> Vec<String> {
        range.map(|n| format!("event {n}")).collect()
    }

    /// An in-memory store with faults a test switches on: appends that fail,
    /// reads that fail, and entries it hands back undecoded.
    #[derive(Default)]
    struct FaultyStore {
        inner: InMemoryJournalStore,
        fail_appends: AtomicBool,
        fail_reads: AtomicBool,
        undecodable: std::sync::Mutex<HashSet<u64>>,
    }

    impl FaultyStore {
        fn set(flag: &AtomicBool, on: bool) {
            flag.store(on, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl JournalStore for FaultyStore {
        async fn append(
            &self,
            session: &SessionId,
            expected: Option<SequenceNumber>,
            run_id: Option<RunId>,
            at: Timestamp,
            payload: SessionEventPayload,
        ) -> Result<Arc<SessionEvent>, SessionStoreError> {
            if self.fail_appends.load(Ordering::SeqCst) {
                return Err(SessionStoreError::Request {
                    reason: "append refused".to_string(),
                });
            }
            self.inner
                .append(session, expected, run_id, at, payload)
                .await
        }

        async fn read(
            &self,
            session: &SessionId,
            from: SequenceNumber,
            to: SequenceNumber,
            limit: usize,
        ) -> Result<Vec<JournalEntry>, SessionStoreError> {
            if self.fail_reads.load(Ordering::SeqCst) {
                return Err(SessionStoreError::Request {
                    reason: "read refused".to_string(),
                });
            }
            let undecodable = self.undecodable.lock().unwrap().clone();
            Ok(self
                .inner
                .read(session, from, to, limit)
                .await?
                .into_iter()
                .map(|entry| match entry {
                    JournalEntry::Event(event) if undecodable.contains(&event.seq.get()) => {
                        JournalEntry::Undecodable {
                            seq: event.seq,
                            raw: Bytes::from_static(b"{\"from\":\"the future\"}"),
                        }
                    }
                    other => other,
                })
                .collect())
        }

        async fn latest(
            &self,
            session: &SessionId,
        ) -> Result<Option<SequenceNumber>, SessionStoreError> {
            self.inner.latest(session).await
        }
    }

    #[tokio::test]
    async fn appends_mint_dense_numbers_from_the_first() {
        let store = Arc::new(InMemoryJournalStore::new());
        let journal = open("sess_1", store.clone()).await;
        let run = RunId::mint();
        assert_eq!(journal.latest(), None);

        let first = journal.append(Some(run), said("a")).await.unwrap();
        let second = journal.append(Some(run), said("b")).await.unwrap();
        let third = journal.append(None, started()).await.unwrap();

        assert_eq!(first.seq, SequenceNumber::FIRST);
        assert!(second.seq.follows(first.seq));
        assert!(third.seq.follows(second.seq));
        assert_eq!(journal.latest(), Some(third.seq));
        assert_eq!(
            store.latest(journal.session_id()).await.unwrap(),
            Some(third.seq),
            "the store confirmed every number the journal minted"
        );

        assert_eq!(first.session_id, "sess_1");
        assert_eq!(first.run_id, Some(run));
        assert_eq!(third.run_id, None, "an event of the session names no run");
        assert!(first.at.unix_millis() > 0);
    }

    /// A session that outlives a process keeps counting from where the store
    /// left it, so a consumer holding an old cursor still has a position.
    #[tokio::test]
    async fn opening_a_journal_continues_the_sequence_the_store_holds() {
        let store: Arc<dyn JournalStore> = Arc::new(InMemoryJournalStore::new());
        let earlier = open("sess_1", store.clone()).await;
        say_times(&earlier, RunId::mint(), 3).await;
        drop(earlier);

        let reopened = open("sess_1", store).await;
        assert_eq!(reopened.latest(), Some(seq(3)));

        let event = reopened
            .append(Some(RunId::mint()), said("d"))
            .await
            .unwrap();
        assert_eq!(event.seq, seq(4));
    }

    /// A session whose entries the store let go still goes on numbering past
    /// what it handed out, so a reader holding an old number is never handed a
    /// different event under it, and one reading from the start is told what
    /// is gone.
    #[tokio::test]
    async fn a_reopened_session_whose_entries_were_evicted_keeps_numbering() {
        let store: Arc<dyn JournalStore> = Arc::new(InMemoryJournalStore::with_bounds(8, 1));
        let earlier = open("sess_a", store.clone()).await;
        say_times(&earlier, RunId::mint(), 3).await;
        drop(earlier);
        let other = open("sess_b", store.clone()).await;
        say_times(&other, RunId::mint(), 1).await;

        let reopened = open("sess_a", store).await;
        assert_eq!(reopened.latest(), Some(seq(3)));
        let next = reopened
            .append(Some(RunId::mint()), said("again"))
            .await
            .unwrap();
        assert_eq!(next.seq, seq(4));

        let mut sub = reopened.subscribe(Cursor::Start);
        assert_eq!(take(&mut sub, 2).await, ["gap→4", "event 4"]);
        let mut resumed = reopened.subscribe(Cursor::After(seq(3)));
        assert_eq!(take(&mut resumed, 1).await, events(4..=4));
    }

    /// Two journals open on one session are two writers. The first to append
    /// wins; the second is refused instead of interleaving its events, and it
    /// stays refused, its drain included, while the first goes on.
    #[tokio::test]
    async fn a_second_writer_on_a_session_is_refused_and_stays_refused() {
        let store: Arc<dyn JournalStore> = Arc::new(InMemoryJournalStore::new());
        let first = open("sess_1", store.clone()).await;
        let second = open("sess_1", store.clone()).await;
        let run = RunId::mint();

        first.append(Some(run), said("one")).await.unwrap();
        let conflict = second.append(Some(run), said("two")).await.unwrap_err();
        assert_eq!(
            conflict,
            SessionStoreError::Conflict {
                expected: None,
                actual: Some(seq(1)),
            }
        );

        // Fenced: refused without reaching the store, even once it would
        // have matched.
        let (context, events) = crate::run_context::RunContext::channel(run);
        assert!(
            context
                .emit(AgentEvent::single_agent(AgentEventPayload::TextDelta {
                    content: "late".to_string(),
                }))
                .await
        );
        drop(context);
        assert_eq!(second.drain(run, events).await.unwrap_err(), conflict);
        assert_eq!(
            second.append(Some(run), said("three")).await.unwrap_err(),
            conflict
        );

        let next = first.append(Some(run), said("four")).await.unwrap();
        assert_eq!(next.seq, seq(2));
        assert_eq!(
            store.latest(first.session_id()).await.unwrap(),
            Some(seq(2))
        );
    }

    /// A store failure that is not a conflict leaves the journal able to
    /// append: the next attempt reaches the store.
    #[tokio::test]
    async fn a_store_failure_does_not_fence_the_journal() {
        let store = Arc::new(FaultyStore::default());
        let journal = open("sess_1", store.clone()).await;
        FaultyStore::set(&store.fail_appends, true);
        journal.append(None, said("lost")).await.unwrap_err();
        FaultyStore::set(&store.fail_appends, false);
        assert_eq!(
            journal.append(None, said("kept")).await.unwrap().seq,
            seq(1)
        );
    }

    /// A store that forgets a session a journal still has open, entries and
    /// newest number both, does not restart the journal's numbering: the
    /// journal's expected number is where the session stands.
    #[tokio::test]
    async fn an_open_journal_keeps_numbering_when_the_store_forgets_its_session() {
        let store = Arc::new(InMemoryJournalStore::with_bounds(8, 1));
        let journal = open("sess_kept", store.clone()).await;
        say_times(&journal, RunId::mint(), 3).await;
        // Enough other sessions to push this one's entries and its number out.
        for i in 0..100 {
            let other = open(&format!("sess_{i}"), store.clone()).await;
            say_times(&other, RunId::mint(), 1).await;
        }
        assert_eq!(store.latest(journal.session_id()).await.unwrap(), None);

        let next = journal.append(None, said("on")).await.unwrap();
        assert_eq!(next.seq, seq(4));
    }

    /// The store mints a number only for an event it keeps: after a failed
    /// append the next one gets the number the failed one would have.
    #[tokio::test]
    async fn a_store_failure_mints_nothing() {
        let store = Arc::new(FaultyStore::default());
        let journal = open("sess_1", store.clone()).await;
        let run = RunId::mint();

        FaultyStore::set(&store.fail_appends, true);
        let err = journal.append(Some(run), said("lost")).await.unwrap_err();
        assert!(err.to_string().contains("append refused"), "{err}");
        assert_eq!(journal.latest(), None);

        FaultyStore::set(&store.fail_appends, false);
        let event = journal.append(Some(run), said("kept")).await.unwrap();
        assert_eq!(event.seq, SequenceNumber::FIRST);
    }

    /// The run's task appends the lifecycle around what it drains, and what
    /// comes out is one sequence with no second numbering for either half.
    #[tokio::test]
    async fn lifecycle_and_agent_events_share_one_sequence() {
        let journal = journal().await;
        let run = RunId::mint();
        let (context, events) = crate::run_context::RunContext::channel(run);

        journal.append(Some(run), started()).await.unwrap();
        for text in ["one", "two"] {
            assert!(
                context
                    .emit(AgentEvent::single_agent(AgentEventPayload::TextDelta {
                        content: text.to_string(),
                    }))
                    .await
            );
        }
        drop(context);
        journal.drain(run, events).await.unwrap();
        journal.append(Some(run), finished()).await.unwrap();

        let mut sub = journal.subscribe(Cursor::Start);
        let mut kinds = Vec::new();
        for _ in 0..4 {
            let JournalItem::Event(event) = next(&mut sub).await else {
                panic!("every item is an event");
            };
            assert_eq!(event.run_id, Some(run));
            kinds.push(match &event.payload {
                SessionEventPayload::Lifecycle(LifecycleEvent::Started { .. }) => "started",
                SessionEventPayload::Lifecycle(LifecycleEvent::Finished { .. }) => "finished",
                SessionEventPayload::Agent(_) => "agent",
                other => panic!("unexpected payload {other:?}"),
            });
        }
        assert_eq!(kinds, ["started", "agent", "agent", "finished"]);
        assert_eq!(journal.latest(), Some(seq(4)));
    }

    /// The drain stops at a store failure and lets go of the run's channel, so
    /// the run's next emit is refused at once rather than left waiting on a
    /// reader that is gone.
    #[tokio::test]
    async fn a_failed_drain_refuses_what_the_run_emits_next() {
        let store = Arc::new(FaultyStore::default());
        let journal = open("sess_1", store.clone()).await;
        let run = RunId::mint();
        let (context, events) = crate::run_context::RunContext::channel(run);
        FaultyStore::set(&store.fail_appends, true);

        let say = |text: &str| {
            AgentEvent::single_agent(AgentEventPayload::TextDelta {
                content: text.to_string(),
            })
        };
        assert!(context.emit(say("first")).await);
        let err = journal.drain(run, events).await.unwrap_err();
        assert!(err.to_string().contains("append refused"), "{err}");

        let next = tokio::time::timeout(Duration::from_secs(5), context.emit(say("next")))
            .await
            .expect("the emit returns rather than waiting");
        assert!(!next, "nothing reads the run's channel any more");
    }

    /// `tee_content` copies what the agent says onto the run's channel for a
    /// consumer built on the run's events. The drain is that consumer, so a
    /// late observer reads the words and not only the tool calls.
    #[tokio::test]
    async fn the_drain_records_what_tee_content_copies() {
        use crate::provider_agent::{FinalResponseInfo, StreamItem, StreamedAssistantContent};

        let journal = journal().await;
        let run = RunId::mint();
        let (context, events) = crate::run_context::RunContext::channel(run);

        let items: Vec<Result<StreamItem, crate::provider_agent::StreamError>> = vec![
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text("Hel".to_string()),
            )),
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text("lo".to_string()),
            )),
            Ok(StreamItem::Final(FinalResponseInfo {
                content: "Hello".to_string(),
                usage: rig::completion::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                },
                cache_usage: None,
            })),
        ];
        // Consuming the teed stream drops the run with it, which closes the
        // channel the drain reads.
        let passed = crate::streaming::tee_content(
            context,
            aura_events::EmittingAgent::single_agent(),
            futures::stream::iter(items),
        )
        .collect::<Vec<_>>()
        .await
        .len();
        assert_eq!(passed, 3, "the content path still reaches its own consumer");

        journal.drain(run, events).await.unwrap();

        let mut sub = journal.subscribe(Cursor::Start);
        let mut words = Vec::new();
        for _ in 0..3 {
            let JournalItem::Event(event) = next(&mut sub).await else {
                panic!("every item is an event");
            };
            let SessionEventPayload::Agent(agent) = &event.payload else {
                panic!("every event is the agent's");
            };
            words.push(match &agent.payload {
                AgentEventPayload::TextDelta { content } => format!("delta:{content}"),
                AgentEventPayload::Completed { content, usage } => {
                    format!("completed:{content}:{}", usage.total_tokens.get())
                }
                other => panic!("unexpected payload {other:?}"),
            });
        }
        assert_eq!(words, ["delta:Hel", "delta:lo", "completed:Hello:15"]);
        assert!(quiet(&mut sub).await);
    }

    #[tokio::test]
    async fn a_subscriber_from_the_start_reads_replay_and_live_as_one_stream() {
        let journal = journal().await;
        let run = RunId::mint();
        say_times(&journal, run, 3).await;

        let mut sub = journal.subscribe(Cursor::Start);
        say_times(&journal, run, 2).await;

        assert_eq!(take(&mut sub, 5).await, events(1..=5));
        assert!(quiet(&mut sub).await, "nothing is delivered twice");
    }

    /// Attaching to a run is a cursor at its `Started`, inclusive, so the first
    /// thing the observer reads is the run beginning — and a consumer resuming
    /// from the last number it saw asks for what follows it.
    #[tokio::test]
    async fn from_and_after_cursors_place_a_subscriber_in_the_stream() {
        let journal = journal().await;
        let earlier = RunId::mint();
        say_times(&journal, earlier, 2).await;
        let run = RunId::mint();
        let started = journal.append(Some(run), started()).await.unwrap();
        say_times(&journal, run, 2).await;

        let mut attached = journal.subscribe(Cursor::From(started.seq));
        assert_eq!(take(&mut attached, 3).await, events(3..=5));
        assert!(quiet(&mut attached).await);

        let mut resumed = journal.subscribe(Cursor::After(seq(4)));
        assert_eq!(take(&mut resumed, 1).await, events(5..=5));
        assert!(quiet(&mut resumed).await);

        let mut from_first = journal.subscribe(Cursor::From(SequenceNumber::FIRST));
        assert_eq!(take(&mut from_first, 5).await, events(1..=5));
    }

    #[tokio::test]
    async fn a_live_subscriber_sees_only_what_follows_its_joining() {
        let journal = journal().await;
        let run = RunId::mint();
        say_times(&journal, run, 3).await;

        let mut sub = journal.subscribe(Cursor::Live);
        assert!(quiet(&mut sub).await, "nothing is replayed");

        say_times(&journal, run, 2).await;
        assert_eq!(take(&mut sub, 2).await, events(4..=5));
    }

    /// A cursor past the end of the stream is a live subscription that also
    /// skips whatever is appended up to it.
    #[tokio::test]
    async fn a_cursor_beyond_the_newest_event_waits_for_it() {
        let journal = journal().await;
        let run = RunId::mint();
        say_times(&journal, run, 2).await;

        let mut sub = journal.subscribe(Cursor::After(seq(4)));
        say_times(&journal, run, 3).await;

        assert_eq!(take(&mut sub, 1).await, events(5..=5));
        assert!(quiet(&mut sub).await);
    }

    /// The appender never waits on a reader: every append completes while the
    /// subscriber polls nothing. What the broadcast moved past is still in the
    /// store, so when the subscriber finally reads, it reads everything.
    #[tokio::test]
    async fn a_subscriber_slower_than_the_broadcast_catches_up_from_the_store() {
        let total = LIVE_CAPACITY as u64 + 8;
        let journal = open(
            "sess_1",
            Arc::new(InMemoryJournalStore::with_bounds(total as usize, 8)),
        )
        .await;
        let run = RunId::mint();

        let mut sub = journal.subscribe(Cursor::Live);
        assert!(
            quiet(&mut sub).await,
            "the subscriber is waiting on the broadcast"
        );
        tokio::time::timeout(Duration::from_secs(5), say_times(&journal, run, total))
            .await
            .expect("appends complete without the subscriber reading");

        let mut expected = SequenceNumber::FIRST;
        for _ in 0..total {
            let item = next(&mut sub).await;
            let JournalItem::Event(event) = &item else {
                panic!("expected event {expected}, got {}", describe(&item));
            };
            assert_eq!(event.seq, expected);
            expected = expected.next();
        }
        assert!(quiet(&mut sub).await);
    }

    /// A subscriber that falls further behind than either the broadcast or the
    /// store keeps is told what neither holds, once, and handed the rest in
    /// order: what its buffer still holds as well as what the store does.
    #[tokio::test]
    async fn a_subscriber_slower_than_the_store_keeps_is_told_only_what_it_lost() {
        let total = LIVE_CAPACITY as u64 + 8;
        let journal = open("sess_1", Arc::new(InMemoryJournalStore::with_bounds(8, 8))).await;
        let run = RunId::mint();

        let mut sub = journal.subscribe(Cursor::Live);
        assert!(quiet(&mut sub).await);
        say_times(&journal, run, total).await;

        // The buffer kept the newest `LIVE_CAPACITY`; the store, a subset of them.
        let resumed_at = total - LIVE_CAPACITY as u64 + 1;
        let mut expected = vec![format!("gap→{resumed_at}")];
        expected.extend(events(resumed_at..=total));
        assert_eq!(take(&mut sub, expected.len()).await, expected);
        assert!(quiet(&mut sub).await);
    }

    /// Events published after a subscriber joined are in its own buffer, so a
    /// store that keeps fewer of them than the buffer does loses none of them.
    #[tokio::test]
    async fn a_store_keeping_fewer_than_the_buffer_loses_no_buffered_event() {
        let journal = open("sess_1", Arc::new(InMemoryJournalStore::with_bounds(1, 8))).await;
        let run = RunId::mint();

        let mut live = journal.subscribe(Cursor::Live);
        let mut from_start = journal.subscribe(Cursor::Start);
        say_times(&journal, run, 3).await;

        assert_eq!(take(&mut live, 3).await, events(1..=3));
        assert!(quiet(&mut live).await);
        assert_eq!(take(&mut from_start, 3).await, events(1..=3));
        assert!(quiet(&mut from_start).await);
    }

    /// A subscriber that lagged and is only read after the journal has gone
    /// still gets every event before its stream ends.
    #[tokio::test]
    async fn a_lagged_subscriber_reads_everything_after_the_journal_closes() {
        let total = LIVE_CAPACITY as u64 + 8;
        let journal = open(
            "sess_1",
            Arc::new(InMemoryJournalStore::with_bounds(total as usize, 8)),
        )
        .await;
        let mut sub = journal.subscribe(Cursor::Live);
        assert!(quiet(&mut sub).await);
        say_times(&journal, RunId::mint(), total).await;
        drop(journal);

        let mut seen = Vec::new();
        while let Some(item) = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("the stream ends within 5s")
        {
            seen.push(describe(&item));
        }
        assert_eq!(seen, events(1..=total));
    }

    /// The store keeps a suffix of the session. A cursor older than that gets
    /// the suffix, told first what it will not get.
    #[tokio::test]
    async fn a_cursor_older_than_the_store_holds_replays_from_the_bound_and_says_so() {
        let journal = open("sess_1", Arc::new(InMemoryJournalStore::with_bounds(5, 8))).await;
        say_times(&journal, RunId::mint(), 8).await;

        let mut sub = journal.subscribe(Cursor::Start);
        let mut expected = vec!["gap→4".to_string()];
        expected.extend(events(4..=8));
        assert_eq!(take(&mut sub, 6).await, expected);
        assert!(quiet(&mut sub).await);

        let mut inside = journal.subscribe(Cursor::From(seq(6)));
        assert_eq!(take(&mut inside, 3).await, events(6..=8));
    }

    /// A store that evicted the whole session cannot say what it held, and the
    /// journal does not need it to: the sequence it minted says what is gone.
    #[tokio::test]
    async fn a_session_the_store_evicted_gaps_to_the_next_event_at_once() {
        let store = Arc::new(InMemoryJournalStore::with_bounds(100, 1));
        let evicted = open("sess_a", store.clone()).await;
        let run = RunId::mint();
        say_times(&evicted, run, 3).await;
        let other = open("sess_b", store).await;
        say_times(&other, RunId::mint(), 1).await;

        let mut sub = evicted.subscribe(Cursor::Start);
        assert_eq!(
            take(&mut sub, 1).await,
            ["gap→4"],
            "the gap is reported now, not held until something live arrives"
        );
        assert!(quiet(&mut sub).await);

        say_times(&evicted, run, 1).await;
        assert_eq!(take(&mut sub, 1).await, events(4..=4));
    }

    /// An entry a newer binary wrote is a position in the stream, not a hole:
    /// replay carries it as unknown and continues.
    #[tokio::test]
    async fn an_undecodable_entry_is_replayed_in_its_place() {
        let store = Arc::new(FaultyStore::default());
        let journal = open("sess_1", store.clone()).await;
        say_times(&journal, RunId::mint(), 3).await;
        store.undecodable.lock().unwrap().insert(2);

        let mut sub = journal.subscribe(Cursor::Start);
        let first = next(&mut sub).await;
        let second = next(&mut sub).await;
        let third = next(&mut sub).await;
        assert_eq!(describe(&first), "event 1");
        assert_eq!(describe(&third), "event 3");
        let JournalItem::Unknown { seq: at, raw } = second else {
            panic!("the undecodable entry is carried as unknown, got {second:?}");
        };
        assert_eq!(at, seq(2));
        assert_eq!(raw, Bytes::from_static(b"{\"from\":\"the future\"}"));
        assert!(quiet(&mut sub).await);
    }

    /// A store that cannot be read is a store whose entries the subscriber
    /// missed. Silence would read as a quiet session.
    #[tokio::test]
    async fn a_failed_read_is_reported_as_a_gap_not_silence() {
        let store = Arc::new(FaultyStore::default());
        let journal = open("sess_1", store.clone()).await;
        let run = RunId::mint();
        say_times(&journal, run, 3).await;
        FaultyStore::set(&store.fail_reads, true);

        let mut sub = journal.subscribe(Cursor::Start);
        assert_eq!(take(&mut sub, 1).await, ["gap→4"]);
        assert!(quiet(&mut sub).await);

        say_times(&journal, run, 1).await;
        assert_eq!(take(&mut sub, 1).await, events(4..=4));
    }

    /// A store whose first read holds until released, standing in for one
    /// that awaits a round trip while the run keeps going.
    struct SlowStore {
        inner: InMemoryJournalStore,
        hold_first_read: AtomicBool,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl JournalStore for SlowStore {
        async fn append(
            &self,
            session: &SessionId,
            expected: Option<SequenceNumber>,
            run_id: Option<RunId>,
            at: Timestamp,
            payload: SessionEventPayload,
        ) -> Result<Arc<SessionEvent>, SessionStoreError> {
            self.inner
                .append(session, expected, run_id, at, payload)
                .await
        }

        async fn read(
            &self,
            session: &SessionId,
            from: SequenceNumber,
            to: SequenceNumber,
            limit: usize,
        ) -> Result<Vec<JournalEntry>, SessionStoreError> {
            if self.hold_first_read.swap(false, Ordering::SeqCst) {
                self.release.notified().await;
            }
            self.inner.read(session, from, to, limit).await
        }

        async fn latest(
            &self,
            session: &SessionId,
        ) -> Result<Option<SequenceNumber>, SessionStoreError> {
            self.inner.latest(session).await
        }
    }

    /// The broadcast wraps behind a replay that takes long enough. What it
    /// dropped is still in the store, so the subscriber reads it from there
    /// and never learns the broadcast moved on without it.
    #[tokio::test]
    async fn a_replay_the_broadcast_wraps_behind_catches_up_from_the_store() {
        let total = LIVE_CAPACITY as u64 + 50;
        let store = Arc::new(SlowStore {
            inner: InMemoryJournalStore::with_bounds(total as usize, 8),
            hold_first_read: AtomicBool::new(true),
            release: tokio::sync::Notify::new(),
        });
        let journal = open("sess_1", store.clone()).await;
        let run = RunId::mint();
        say_times(&journal, run, 3).await;

        let mut sub = journal.subscribe(Cursor::Start);
        // Poll once so the replay is parked inside the held read, then let the
        // run outpace the broadcast's buffer while it waits.
        assert!(quiet(&mut sub).await, "the first read is held");
        say_times(&journal, run, total - 3).await;
        store.release.notify_one();

        let mut expected = SequenceNumber::FIRST;
        for _ in 0..total {
            let item = next(&mut sub).await;
            let JournalItem::Event(event) = &item else {
                panic!("expected event {expected}, got {}", describe(&item));
            };
            assert_eq!(event.seq, expected);
            expected = expected.next();
        }
        assert!(quiet(&mut sub).await);
    }

    /// Replay crosses page boundaries without a seam: no gap, no repeat.
    #[tokio::test]
    async fn a_long_replay_is_dense_across_its_pages() {
        let total = REPLAY_PAGE as u64 * 2 + 37;
        let journal = open(
            "sess_1",
            Arc::new(InMemoryJournalStore::with_bounds(total as usize, 8)),
        )
        .await;
        say_times(&journal, RunId::mint(), total).await;

        let mut sub = journal.subscribe(Cursor::Start);
        let mut expected = SequenceNumber::FIRST;
        for _ in 0..total {
            let JournalItem::Event(event) = next(&mut sub).await else {
                panic!("replay is events only");
            };
            assert_eq!(event.seq, expected);
            expected = expected.next();
        }
        assert!(quiet(&mut sub).await);
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_journal_is_dropped() {
        let journal = journal().await;
        say_times(&journal, RunId::mint(), 1).await;
        let mut sub = journal.subscribe(Cursor::Start);
        assert_eq!(take(&mut sub, 1).await, events(1..=1));

        drop(journal);
        let end = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("the stream ends within 5s");
        assert!(end.is_none());
    }

    /// Subscribers join while the writer is mid-stream, on other threads, and
    /// each still reads every event exactly once: the replay-then-live join is
    /// exact by sequence number, whatever the timing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_subscriber_sees_each_event_exactly_once_however_it_joins() {
        const TOTAL: u64 = 400;
        const SUBSCRIBERS: u64 = 12;

        let journal = Arc::new(
            open(
                "sess_1",
                Arc::new(InMemoryJournalStore::with_bounds(TOTAL as usize, 8)),
            )
            .await,
        );
        let run = RunId::mint();

        let writer = {
            let journal = Arc::clone(&journal);
            tokio::spawn(async move {
                for i in 0..TOTAL {
                    journal
                        .append(Some(run), said(&i.to_string()))
                        .await
                        .unwrap();
                    if i % 7 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
        };

        let mut readers = Vec::new();
        for i in 0..SUBSCRIBERS {
            let journal = Arc::clone(&journal);
            readers.push(tokio::spawn(async move {
                // Stagger the joins across the writer's progress.
                tokio::time::sleep(Duration::from_micros(i * 150)).await;
                let mut sub = journal.subscribe(Cursor::Start);
                let mut seen = Vec::new();
                loop {
                    match next(&mut sub).await {
                        JournalItem::Event(event) => {
                            seen.push(event.seq.get());
                            if event.seq.get() == TOTAL {
                                break seen;
                            }
                        }
                        other => panic!("subscriber {i} saw {}", describe(&other)),
                    }
                }
            }));
        }

        writer.await.unwrap();
        let complete: Vec<u64> = (1..=TOTAL).collect();
        for (i, reader) in readers.into_iter().enumerate() {
            let seen = reader.await.unwrap();
            assert_eq!(
                seen, complete,
                "subscriber {i} read every event once, in order"
            );
        }
    }

    /// A store that counts its reads.
    #[derive(Default)]
    struct CountingStore {
        inner: InMemoryJournalStore,
        reads: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl JournalStore for CountingStore {
        async fn append(
            &self,
            session: &SessionId,
            expected: Option<SequenceNumber>,
            run_id: Option<RunId>,
            at: Timestamp,
            payload: SessionEventPayload,
        ) -> Result<Arc<SessionEvent>, SessionStoreError> {
            self.inner
                .append(session, expected, run_id, at, payload)
                .await
        }
        async fn read(
            &self,
            session: &SessionId,
            from: SequenceNumber,
            to: SequenceNumber,
            limit: usize,
        ) -> Result<Vec<JournalEntry>, SessionStoreError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.read(session, from, to, limit).await
        }
        async fn latest(
            &self,
            session: &SessionId,
        ) -> Result<Option<SequenceNumber>, SessionStoreError> {
            self.inner.latest(session).await
        }
    }

    /// Replaying the short suffix a store retains of a long stream costs reads
    /// in proportion to what is retained, not to every number ever minted.
    #[tokio::test]
    async fn a_replay_reads_only_what_the_store_retains() {
        let store = Arc::new(CountingStore {
            inner: InMemoryJournalStore::with_bounds(8, 8),
            reads: Default::default(),
        });
        let journal = open("sess_1", store.clone()).await;
        say_times(&journal, RunId::mint(), 100_000).await;

        let mut sub = journal.subscribe(Cursor::Start);
        let mut items = take(&mut sub, 9).await;
        assert_eq!(items.remove(0), "gap→99993");
        assert_eq!(items, events(99_993..=100_000));
        let reads = store.reads.load(Ordering::SeqCst);
        assert!(
            reads <= 2,
            "replaying 8 retained events took {reads} store reads"
        );
    }
}
