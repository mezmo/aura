//! A session's journal: the ordered record of everything its runs did and
//! everything that happened to them, and the source every observer reads.
//!
//! A run has one event channel, and whoever holds its receiver is the run's
//! only observer: present from the first byte, and if it stops polling, the
//! bounded channel fills and the run waits on it. That is the shape of a
//! request-bound agent, and it is why attaching to a run already underway was
//! impossible — a channel holds nothing. The journal is the one reader of that
//! receiver. It stamps each [`AgentEvent`] into a [`SessionEvent`] with the
//! session, the run, the next sequence number and the time, and appends it;
//! the run's owner appends its [`LifecycleEvent`]s through the same path, so
//! the session has one sequence. Anyone else subscribes, from a cursor, and
//! reads replay and live as one stream.
//!
//! # One writer, one `append`
//!
//! [`SessionJournal::append`] takes the journal's lock, mints `latest + 1`,
//! hands the event to the [`JournalStore`], publishes it, and releases. In one
//! process that lock plus one live run per session is what makes the writer
//! unique; across instances the session claim will be. The store therefore sees
//! each session as a strictly increasing sequence from one writer at a time,
//! and a store that awaits — a database — changes nothing but how long the lock
//! is held.
//!
//! # Subscribe joins live first, then replays
//!
//! [`SessionJournal::subscribe`] joins the broadcast before anything else and
//! notes the newest sequence number at that moment. Everything up to it is
//! replayed from the store; everything after arrives live, and a live event the
//! replay already covered is dropped by sequence number, so the dedupe is exact
//! and no lock spans the store read. The algorithm is the same whether the read
//! returns at once or awaits a round trip.
//!
//! # A gap has one meaning
//!
//! Sequence numbers are dense, so a subscriber knows what it expects next and
//! yields [`JournalItem::Gap`] whenever the event it is about to deliver is
//! later than that. A subscriber that falls behind the broadcast reads what it
//! missed from the store, so a gap is only ever what the store no longer holds
//! — its cursor was older than the store keeps, the store evicted the session,
//! or the subscriber fell further behind than the store's retention — and the
//! consumer need not tell those apart: it missed events, and `resumed_at` is
//! the first one it sees again. Nothing elides or coalesces an event, so there
//! is no second kind of hole.
//!
//! # Back-pressure
//!
//! A slow store slows `append`, a slow `append` fills the run's channel, and a
//! full channel makes the agent wait on `emit`. That is correct: a run should
//! not outpace what its journal can record. A slow *subscriber* reads from the
//! store what the broadcast moved past, is told only what the store has let go
//! of as well, and holds up neither the run nor anyone else.
//!
//! The journal is for observers. The facts a run's owner keeps about it are
//! held as plain state, not rebuilt from here.
//!
//! [`LifecycleEvent`]: aura_events::run::LifecycleEvent

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aura_events::agent::AgentEvent;
use aura_events::run::{SequenceNumber, SessionEvent, SessionEventPayload, Timestamp};
use aura_events::{RunId, SessionId};
use bytes::Bytes;
use futures::Stream;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::session_store::{JournalEntry, JournalStore, SessionStoreError};

/// Live events buffered for a subscriber before it falls back to the store.
pub const LIVE_CAPACITY: usize = 1024;

/// Entries a replay reads from the store at a time.
const REPLAY_PAGE: u64 = 512;

/// Where in a session's stream a subscriber starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// Everything the store still holds, then live.
    Start,
    /// This sequence number and everything after it, then live. A run's
    /// `Started` is the cursor for "attach to this run".
    From(SequenceNumber),
    /// Everything after this sequence number, then live: what a consumer that
    /// has read up to it asks for.
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

/// The stream [`SessionJournal::subscribe`] returns. It ends when the journal
/// is dropped.
pub type JournalSubscription = Pin<Box<dyn Stream<Item = JournalItem> + Send>>;

/// One session's journal: the sequencer and fan-out in front of its
/// [`JournalStore`] entries.
pub struct SessionJournal {
    session_id: SessionId,
    store: Arc<dyn JournalStore>,
    /// Serializes mint → store → publish.
    append: Mutex<()>,
    /// The newest sequence number the store has confirmed; zero before the
    /// first. Shared with each subscription.
    latest: Arc<AtomicU64>,
    live: broadcast::Sender<Arc<SessionEvent>>,
}

impl SessionJournal {
    /// Opens the session's journal over `store`, continuing the sequence from
    /// the newest event the store holds so a session that outlives a process
    /// does not restart its numbering.
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
            latest: Arc::new(AtomicU64::new(latest.map_or(0, SequenceNumber::get))),
            live,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The newest sequence number in the session's stream.
    pub fn latest(&self) -> Option<SequenceNumber> {
        newest(&self.latest)
    }

    /// The one append path, and the only place a sequence number is minted.
    /// `run_id` is the run the event belongs to, or `None` for an event of the
    /// session itself.
    ///
    /// Holds the lock across the store write, so a second appender waits
    /// rather than interleaving. A store failure mints nothing: the next
    /// append carries the number this one would have.
    pub async fn append(
        &self,
        run_id: Option<RunId>,
        payload: SessionEventPayload,
    ) -> Result<Arc<SessionEvent>, SessionStoreError> {
        let _writer = self.append.lock().await;

        let seq = self
            .latest()
            .map_or(SequenceNumber::FIRST, SequenceNumber::next);
        let event = Arc::new(SessionEvent {
            session_id: self.session_id.clone(),
            run_id,
            seq,
            at: Timestamp::now(),
            payload,
        });
        self.store.append(Arc::clone(&event)).await?;

        // Published after `latest` moves, so a subscriber that joined before
        // this send and then read `latest` either sees this number there and
        // replays the event, or receives it live. Neither path misses it.
        self.latest.store(seq.get(), Ordering::Release);
        // Nobody subscribed is not an error; the store has the event.
        let _ = self.live.send(Arc::clone(&event));
        Ok(event)
    }

    /// Appends every event `run` emits until its channel closes, as the one
    /// reader of the receiver `begin_run` hands out. With the journal reading,
    /// the run never waits on an observer; it waits only on its store, which is
    /// the back-pressure it should feel.
    ///
    /// A store failure ends the drain with that error. What the run emits after
    /// that fills its channel, and whoever drove the drain decides what the run
    /// does about it.
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
    /// live, with no seam visible to the consumer.
    ///
    /// Joins the live broadcast first and reads the store up to the newest
    /// sequence number, in pages, holding no lock across the read. The newest
    /// number is read again after each pass, so a replay that takes long
    /// enough for the broadcast to wrap behind it — a store that awaits, a
    /// busy run — goes on reading the store. A live event the replay covered
    /// is dropped by sequence number, and a subscriber the broadcast moves
    /// past while it is live goes back to the store for what it missed.
    /// Whenever the next event to deliver is later than the one expected —
    /// the store no longer holds the earlier ones, or the read failed — a
    /// [`JournalItem::Gap`] precedes it.
    pub fn subscribe(&self, cursor: Cursor) -> JournalSubscription {
        let mut live = self.live.subscribe();
        // Read after joining the broadcast: an event published before the join
        // is counted here, one published after arrives live, and the number
        // moves before the publish, so nothing falls between.
        let latest = Arc::clone(&self.latest);

        // The first sequence number this subscriber has not yet seen.
        let mut expected = match cursor {
            Cursor::Start => SequenceNumber::FIRST,
            Cursor::From(seq) => seq,
            Cursor::After(seq) => seq.next(),
            Cursor::Live => newest(&latest).map_or(SequenceNumber::FIRST, SequenceNumber::next),
        };
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.clone();

        Box::pin(async_stream::stream! {
            'catch_up: loop {
                // Each pass replays to the newest number at its start; the next
                // pass covers what was appended meanwhile, until nothing was.
                while let Some(newest) = newest(&latest).filter(|newest| expected <= *newest) {
                    let mut from = expected;
                    while from <= newest {
                        let to = page_end(from, newest);
                        let page = match store.read(&session_id, from, to).await {
                            Ok(page) => page,
                            Err(error) => {
                                tracing::warn!(
                                    session_id = %session_id, %from, %to, %error,
                                    "journal replay could not read the store; the subscriber misses these events"
                                );
                                Vec::new()
                            }
                        };
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
                        from = to.next();
                    }
                    // The replay is complete to `newest` whether or not the store
                    // still had it all, so anything it lacked is a gap now rather
                    // than at the next live event, which may never come.
                    if expected <= newest {
                        yield JournalItem::Gap { resumed_at: newest.next() };
                        expected = newest.next();
                    }
                }

                loop {
                    match live.recv().await {
                        Ok(event) => {
                            // Already replayed from the store.
                            if event.seq < expected {
                                continue;
                            }
                            if let Some(gap) = advance(&mut expected, event.seq) {
                                yield gap;
                            }
                            yield JournalItem::Event(event);
                        }
                        // What the broadcast dropped is in the store, from
                        // `expected` on; the replay says what the store lacks too.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue 'catch_up,
                        Err(broadcast::error::RecvError::Closed) => break 'catch_up,
                    }
                }
            }
        })
    }
}

/// The newest sequence number a journal's shared counter holds.
fn newest(latest: &AtomicU64) -> Option<SequenceNumber> {
    SequenceNumber::try_from(latest.load(Ordering::Acquire)).ok()
}

/// Moves `expected` past an item at `seq` that is about to be delivered, and
/// the gap to yield first when `seq` is later than what was expected.
fn advance(expected: &mut SequenceNumber, seq: SequenceNumber) -> Option<JournalItem> {
    let gap = (seq > *expected).then_some(JournalItem::Gap { resumed_at: seq });
    *expected = seq.next();
    gap
}

/// The last sequence number of the replay page starting at `from`, never past
/// `newest`.
fn page_end(from: SequenceNumber, newest: SequenceNumber) -> SequenceNumber {
    let end = from.get().saturating_add(REPLAY_PAGE - 1).min(newest.get());
    SequenceNumber::try_from(end).expect("a page ends at or after a non-zero start")
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
        SessionJournal::open(SessionId::new(name), store)
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
        async fn append(&self, event: Arc<SessionEvent>) -> Result<(), SessionStoreError> {
            if self.fail_appends.load(Ordering::SeqCst) {
                return Err(SessionStoreError::Request {
                    reason: "append refused".to_string(),
                });
            }
            self.inner.append(event).await
        }

        async fn read(
            &self,
            session: &SessionId,
            from: SequenceNumber,
            to: SequenceNumber,
        ) -> Result<Vec<JournalEntry>, SessionStoreError> {
            if self.fail_reads.load(Ordering::SeqCst) {
                return Err(SessionStoreError::Request {
                    reason: "read refused".to_string(),
                });
            }
            let undecodable = self.undecodable.lock().unwrap().clone();
            Ok(self
                .inner
                .read(session, from, to)
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

    /// Sequence numbers are confirmed by the store, not promised ahead of it:
    /// a number the store never took is the next append's.
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
            aura_events::AgentContext::single_agent(),
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

    /// A subscriber that falls further behind than the store keeps is told
    /// what it lost, once, and handed what remains in order.
    #[tokio::test]
    async fn a_subscriber_slower_than_the_store_keeps_is_told_what_it_lost() {
        let total = LIVE_CAPACITY as u64 + 8;
        let kept = 8u64;
        let journal = open(
            "sess_1",
            Arc::new(InMemoryJournalStore::with_bounds(kept as usize, 8)),
        )
        .await;
        let run = RunId::mint();

        let mut sub = journal.subscribe(Cursor::Live);
        assert!(quiet(&mut sub).await);
        say_times(&journal, run, total).await;

        let resumed_at = total - kept + 1;
        let mut expected = vec![format!("gap→{resumed_at}")];
        expected.extend(events(resumed_at..=total));
        assert_eq!(take(&mut sub, kept as usize + 1).await, expected);
        assert!(quiet(&mut sub).await);
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
        async fn append(&self, event: Arc<SessionEvent>) -> Result<(), SessionStoreError> {
            self.inner.append(event).await
        }

        async fn read(
            &self,
            session: &SessionId,
            from: SequenceNumber,
            to: SequenceNumber,
        ) -> Result<Vec<JournalEntry>, SessionStoreError> {
            if self.hold_first_read.swap(false, Ordering::SeqCst) {
                self.release.notified().await;
            }
            self.inner.read(session, from, to).await
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
        let total = REPLAY_PAGE * 2 + 37;
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
}
