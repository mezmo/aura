//! Introspection tests: reading and listing sessions and runs, and stopping
//! a run, as a caller outside the runtime does.

use super::*;
use crate::conversation::{Lane, LaneNumber};
use crate::journal::Cursor;

fn session(name: &str) -> SessionId {
    SessionId::new(name).unwrap()
}

fn on_chat(chat: &str, prompt: &str) -> StartRun {
    StartRun {
        target: RunTarget::Conversation {
            chat: session(chat),
        },
        ..StartRun::new(prompt)
    }
}

fn seq(n: u64) -> SequenceNumber {
    SequenceNumber::try_from(n).unwrap()
}

fn parked_on(decision: DecisionId) -> AgentEventPayload {
    AgentEventPayload::RunParked {
        run_id: "ignored".to_owned(),
        decision_ids: vec![decision.to_string()],
        expires_at: "2026-10-21T00:00:00Z".to_owned(),
        iteration: 1,
    }
}

/// Everything a session's snapshot shows, read through its accessors, so a
/// test compares all of it at once.
#[derive(Clone, Debug, PartialEq)]
struct Shown {
    id: SessionId,
    run_session: RunSession,
    agent: String,
    state: AgentState,
    current_run: Option<RunId>,
    last_run: Option<RunId>,
    latest_event: Option<SequenceNumber>,
    observers: ObserverCounts,
}

impl Shown {
    fn of(snapshot: &SessionSnapshot) -> Self {
        Self {
            id: snapshot.id(),
            run_session: snapshot.run_session().clone(),
            agent: snapshot.agent().to_owned(),
            state: snapshot.state().clone(),
            current_run: snapshot.current_run(),
            last_run: snapshot.last_run(),
            latest_event: snapshot.latest_event(),
            observers: snapshot.observers(),
        }
    }

    /// The plain session `id`, whose newest run is by the agent `a`, with no
    /// observers.
    fn plain(
        id: &SessionId,
        state: AgentState,
        current_run: Option<RunId>,
        last_run: Option<RunId>,
        latest_event: Option<SequenceNumber>,
    ) -> Self {
        Self {
            id: id.clone(),
            run_session: RunSession::Plain(id.clone()),
            agent: "a".to_owned(),
            state,
            current_run,
            last_run,
            latest_event,
            observers: ObserverCounts::default(),
        }
    }
}

async fn read_session(h: &Harness, id: &SessionId) -> Option<Shown> {
    h.runtime.session(id).await.unwrap().as_ref().map(Shown::of)
}

/// The sessions on the page `query` asks for, and the page's `next`.
async fn listed_sessions(h: &Harness, query: SessionQuery) -> (Vec<Shown>, Option<SessionId>) {
    let page = h.runtime.sessions(&query).await.unwrap();
    (
        page.sessions().iter().map(Shown::of).collect(),
        page.next().cloned(),
    )
}

/// The ids of the sessions on the page `query` asks for, and the page's
/// `next`.
async fn listed_session_ids(
    h: &Harness,
    query: SessionQuery,
) -> (Vec<SessionId>, Option<SessionId>) {
    let page = h.runtime.sessions(&query).await.unwrap();
    (
        page.sessions().iter().map(SessionSnapshot::id).collect(),
        page.next().cloned(),
    )
}

/// The runs on the page `query` asks for, each as its facts and its
/// session's observers, and the page's `next`.
async fn listed_runs(
    h: &Harness,
    query: RunQuery,
) -> (Vec<(RunFacts, ObserverCounts)>, Option<RunId>) {
    let page = h.runtime.runs(&query).await.unwrap();
    (
        page.runs()
            .iter()
            .map(|run| (run.facts().clone(), run.observers()))
            .collect(),
        page.next(),
    )
}

/// The ids of the runs on the page `query` asks for, and the page's `next`.
async fn listed_run_ids(h: &Harness, query: RunQuery) -> (Vec<RunId>, Option<RunId>) {
    let page = h.runtime.runs(&query).await.unwrap();
    (
        page.runs()
            .iter()
            .map(|run| run.facts().run.run_id)
            .collect(),
        page.next(),
    )
}

fn in_state(kind: AgentStateKind) -> SessionQuery {
    SessionQuery {
        state: Some(kind),
        ..SessionQuery::default()
    }
}

/// The newest sequence number the journal store holds for `id`.
async fn latest(h: &Harness, id: &SessionId) -> Option<SequenceNumber> {
    h.journals.latest(id).await.unwrap()
}

async fn until_running(h: &Harness, run: RunId) {
    eventually("the run is running", || {
        h.runtime
            .facts(&run)
            .is_some_and(|facts| facts.run.status == RunStatus::Running)
    })
    .await;
}

/// Waits until the run store holds `run` with facts that pass `check`.
async fn until_stored(h: &Harness, run: RunId, check: impl Fn(&RunFacts) -> bool) {
    tokio::time::timeout(PATIENCE, async {
        loop {
            if h.runs
                .get(&run)
                .await
                .unwrap()
                .is_some_and(|facts| check(&facts))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the run store has the run's facts in time");
}

/// A session is listed for as long as the store keeps one of its runs. Once
/// the runtime has let it go, nothing is attached to it, and its newest
/// number is the journal store's. Here that is past the run's own newest
/// event, because an observer came and went after the run.
#[tokio::test(start_paused = true)]
async fn a_session_whose_run_ended_past_retention_is_still_listed() {
    let retention = Duration::from_secs(60);
    let h = harness_retaining(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]),
        retention,
    );
    let s1 = session("s1");
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;
    stored(&h, run).await;
    h.runtime
        .attach(&s1, Attach::collecting())
        .await
        .unwrap()
        .detach()
        .await;
    let newest = latest(&h, &s1).await;
    assert_eq!(
        newest.map(SequenceNumber::get),
        facts.run.latest_event.map(|n| n.get() + 2),
        "the attach and detach come after the run's ending"
    );

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the runtime lets the session go", || {
        h.runtime.session_state(&s1).is_none()
    })
    .await;

    let expected = Shown::plain(&s1, AgentState::Done, None, Some(run), newest);
    assert_eq!(read_session(&h, &s1).await, Some(expected.clone()));
    assert_eq!(
        listed_sessions(&h, SessionQuery::default()).await,
        (vec![expected], None)
    );
}

/// Whether the event numbered `seq` on the session `id`'s stream is one of
/// `run`'s.
async fn is_event_of_run(h: &Harness, id: &SessionId, seq: SequenceNumber, run: RunId) -> bool {
    matches!(
        h.runtime.journal_entry(id, seq).await.unwrap(),
        Some(JournalEntry::Event(event)) if event.run_id == Some(run)
    )
}

/// An event on a session's stream is read from the journal store while the
/// runtime holds the session and after it lets the session go, with no row
/// for the session in the run store. A number the stream has not reached,
/// and a session the journal store does not know, have none.
#[tokio::test(start_paused = true)]
async fn an_event_of_a_session_needs_no_row_in_the_run_store() {
    let retention = Duration::from_secs(60);
    let runs = Arc::new(InMemoryRunStore::with_bounds(1));
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(Vec::new()), Prepared::Fake(Vec::new())],
        ),
        retention,
        Arc::clone(&runs) as Arc<dyn RunStore>,
        runs,
    );
    let s1 = session("s1");
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let last = h.ended(run).await.run.latest_event.unwrap();
    let other = h.runtime.start(on_session("s2", "q")).await.unwrap();
    h.ended(other).await;
    assert!(
        h.runs.get(&run).await.unwrap().is_none(),
        "the run store dropped the session's row"
    );

    assert!(
        is_event_of_run(&h, &s1, last, run).await,
        "while the runtime holds the session"
    );
    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the runtime lets the session go", || {
        h.runtime.session_state(&s1).is_none()
    })
    .await;
    assert!(
        is_event_of_run(&h, &s1, last, run).await,
        "after the runtime lets the session go"
    );
    assert!(
        h.runtime
            .journal_entry(&s1, last.next())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.runtime
            .journal_entry(&session("s3"), seq(1))
            .await
            .unwrap()
            .is_none()
    );
}

/// Waits until the run store no longer holds `run`.
async fn until_dropped(h: &Harness, run: RunId) {
    tokio::time::timeout(PATIENCE, async {
        while h.runs.get(&run).await.unwrap().is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the run store lets the run go in time");
}

/// Past its bound the run store lets the oldest live run go when none has
/// ended, so it can hold no row for a run the runtime is stopping. The
/// session still reads, and is listed as stopping, by what the runtime
/// holds. Both runs stall and never see a stop, so the first stays
/// stopping for the stop grace.
#[tokio::test]
async fn a_stopping_session_with_no_row_in_the_run_store_still_reads() {
    let runs = Arc::new(InMemoryRunStore::with_bounds(1));
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Fake(vec![Step::Stall]),
                Prepared::Fake(vec![Step::Stall]),
            ],
        ),
        DEFAULT_RETENTION,
        Arc::clone(&runs) as Arc<dyn RunStore>,
        runs,
    );
    let s1 = session("s1");
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    until_running(&h, run).await;
    let outcome = h
        .runtime
        .stop(&run, CancelCause::External, None)
        .await
        .unwrap();
    assert!(matches!(outcome, StopOutcome::Stopping(_)), "{outcome:?}");
    h.runtime.start(on_session("s2", "q")).await.unwrap();
    until_dropped(&h, run).await;

    let snapshot = h
        .runtime
        .session(&s1)
        .await
        .unwrap()
        .expect("the runtime holds the session");
    assert_eq!(snapshot.state().kind(), AgentStateKind::Stopping);
    let (listed, _) = listed_sessions(&h, in_state(AgentStateKind::Stopping)).await;
    assert_eq!(
        listed
            .iter()
            .map(|shown| shown.id.clone())
            .collect::<Vec<_>>(),
        [s1]
    );
}

/// An ended run the runtime keeps for its retention window, whose row the
/// run store let go for a newer run: its session reads, and it and its
/// session are listed, from what the runtime keeps.
#[tokio::test]
async fn a_session_whose_kept_run_has_no_row_in_the_run_store_still_reads() {
    let runs = Arc::new(InMemoryRunStore::with_bounds(1));
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(Vec::new()), Prepared::Fake(Vec::new())],
        ),
        DEFAULT_RETENTION,
        Arc::clone(&runs) as Arc<dyn RunStore>,
        runs,
    );
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.ended(run).await;
    let other = h.runtime.start(on_session("s2", "q")).await.unwrap();
    h.ended(other).await;
    until_dropped(&h, run).await;

    let snapshot = h
        .runtime
        .session(&session("s1"))
        .await
        .unwrap()
        .expect("the runtime keeps the session's run");
    let shown = Shown::of(&snapshot);
    assert_eq!(
        (shown.state, shown.current_run, shown.last_run),
        (AgentState::Done, None, Some(run))
    );
    assert_eq!(
        listed_run_ids(&h, RunQuery::default()).await,
        (vec![run, other], None)
    );
    assert_eq!(
        listed_session_ids(&h, SessionQuery::default()).await,
        (vec![session("s1"), session("s2")], None)
    );
}

/// Past its bound the run store lets the oldest live run go when none has
/// ended. The runtime still holds the run, so it is listed, and so is its
/// session, by what the runtime holds. Both runs stall.
#[tokio::test]
async fn a_live_run_whose_row_the_run_store_dropped_is_still_listed() {
    let runs = Arc::new(InMemoryRunStore::with_bounds(1));
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Fake(vec![Step::Stall]),
                Prepared::Fake(vec![Step::Stall]),
            ],
        ),
        DEFAULT_RETENTION,
        Arc::clone(&runs) as Arc<dyn RunStore>,
        runs,
    );
    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    until_running(&h, first).await;
    let second = h.runtime.start(on_session("s2", "q")).await.unwrap();
    until_running(&h, second).await;
    until_dropped(&h, first).await;

    assert_eq!(
        listed_run_ids(&h, RunQuery::default()).await,
        (vec![first, second], None)
    );
    for query in [SessionQuery::default(), in_state(AgentStateKind::Running)] {
        assert_eq!(
            listed_session_ids(&h, query.clone()).await,
            (vec![session("s1"), session("s2")], None),
            "{query:?}"
        );
    }
}

/// A run store that counts its `newest_per_session` reads.
struct CountingRunStore {
    inner: Arc<InMemoryRunStore>,
    session_reads: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl RunStore for CountingRunStore {
    async fn put(&self, facts: &RunFacts) -> Result<(), SessionStoreError> {
        self.inner.put(facts).await
    }

    async fn get(&self, run: &RunId) -> Result<Option<RunFacts>, SessionStoreError> {
        self.inner.get(run).await
    }

    async fn list(&self, filter: &RunFilter) -> Result<Vec<RunFacts>, SessionStoreError> {
        self.inner.list(filter).await
    }

    async fn newest_per_session(
        &self,
        filter: &SessionFilter,
    ) -> Result<Vec<SessionRuns>, SessionStoreError> {
        self.session_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.newest_per_session(filter).await
    }

    async fn finish(
        &self,
        terminal: &SessionEvent,
        facts: &RunFacts,
    ) -> Result<(), SessionStoreError> {
        self.inner.finish(terminal, facts).await
    }
}

/// A page reads the earlier runs of a session the runtime holds that the
/// store's page left out only once the page is cut, and only for the
/// sessions left on it. Here the store keeps one row, so two of three live
/// sessions have none there, and a page of one session reads the store
/// twice: once for the page and once for that session.
#[tokio::test]
async fn a_sessions_page_reads_histories_only_for_the_sessions_on_it() {
    let runs = Arc::new(InMemoryRunStore::with_bounds(1));
    let store = Arc::new(CountingRunStore {
        inner: Arc::clone(&runs),
        session_reads: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Fake(vec![Step::Stall]),
                Prepared::Fake(vec![Step::Stall]),
                Prepared::Fake(vec![Step::Stall]),
            ],
        ),
        DEFAULT_RETENTION,
        Arc::clone(&store) as Arc<dyn RunStore>,
        runs,
    );
    let mut started = Vec::new();
    for name in ["s1", "s2", "s3"] {
        let run = h.runtime.start(on_session(name, "q")).await.unwrap();
        until_running(&h, run).await;
        started.push(run);
    }
    until_dropped(&h, started[0]).await;
    until_dropped(&h, started[1]).await;

    let before = store.session_reads.load(Ordering::SeqCst);
    let page = SessionQuery {
        limit: PageLimit::from(1),
        ..SessionQuery::default()
    };
    assert_eq!(
        listed_session_ids(&h, page).await,
        (vec![session("s1")], Some(session("s1")))
    );
    assert_eq!(store.session_reads.load(Ordering::SeqCst) - before, 2);
}

/// A run store that writes `newer` into its rows when one session's rows
/// are first read on their own, as a newer run of that session would if it
/// reached the store between a page's first read and that one.
struct RunStoreWritingNewerRun {
    inner: Arc<InMemoryRunStore>,
    newer: Mutex<Option<RunFacts>>,
}

#[async_trait]
impl RunStore for RunStoreWritingNewerRun {
    async fn put(&self, facts: &RunFacts) -> Result<(), SessionStoreError> {
        self.inner.put(facts).await
    }

    async fn get(&self, run: &RunId) -> Result<Option<RunFacts>, SessionStoreError> {
        self.inner.get(run).await
    }

    async fn list(&self, filter: &RunFilter) -> Result<Vec<RunFacts>, SessionStoreError> {
        self.inner.list(filter).await
    }

    async fn newest_per_session(
        &self,
        filter: &SessionFilter,
    ) -> Result<Vec<SessionRuns>, SessionStoreError> {
        let newer = self
            .newer
            .lock()
            .unwrap()
            .take_if(|newer| filter.session.as_ref() == Some(&newer.session_id()));
        if let Some(newer) = newer {
            self.inner.put(&newer).await?;
        }
        self.inner.newest_per_session(filter).await
    }

    async fn finish(
        &self,
        terminal: &SessionEvent,
        facts: &RunFacts,
    ) -> Result<(), SessionStoreError> {
        self.inner.finish(terminal, facts).await
    }
}

/// A session left on a page whose newest run changes before its rows are
/// read, to one the query does not select, is left off the page. The page
/// comes back short, and its `next` still moves past the session, so the
/// next page goes on with the sessions after it. Here `s1` has no row in
/// the store, and a newer run by another agent reaches the store just
/// before `s1`'s rows are read.
#[tokio::test]
async fn a_session_whose_newest_run_changes_while_the_page_is_read_is_left_off() {
    let runs = Arc::new(InMemoryRunStore::with_bounds(1));
    let store = Arc::new(RunStoreWritingNewerRun {
        inner: Arc::clone(&runs),
        newer: Mutex::new(None),
    });
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Fake(vec![Step::Stall]),
                Prepared::Fake(vec![Step::Stall]),
            ],
        ),
        DEFAULT_RETENTION,
        Arc::clone(&store) as Arc<dyn RunStore>,
        runs,
    );
    let mut started = Vec::new();
    for name in ["s1", "s2"] {
        let run = h.runtime.start(on_session(name, "q")).await.unwrap();
        until_running(&h, run).await;
        started.push(run);
    }
    until_dropped(&h, started[0]).await;
    let newer = preparing(RunId::mint());
    *store.newer.lock().unwrap() = Some(RunFacts {
        run: RunDetails {
            agent: "b".to_owned(),
            status: RunStatus::Running,
            ..newer.run
        },
        ..newer
    });

    let page = SessionQuery {
        agent: Some("a".to_owned()),
        limit: PageLimit::from(1),
        ..SessionQuery::default()
    };
    assert_eq!(
        listed_session_ids(&h, page.clone()).await,
        (vec![], Some(session("s1")))
    );
    assert_eq!(
        listed_session_ids(
            &h,
            SessionQuery {
                after: Some(session("s1")),
                ..page
            }
        )
        .await,
        (vec![session("s2")], None)
    );
}

/// A page holds no run past the last row the store returned for it, since
/// the store may hold rows before that run it has not returned yet. Here
/// the store's first two rows for running runs are runs whose records have
/// ended, which leaves room on the page. The live run past the store's
/// rows waits for the next page, after the run only the store holds.
#[tokio::test]
async fn a_runs_page_holds_no_run_past_the_rows_the_store_returned() {
    let runs = Arc::new(InMemoryRunStore::new());
    let store = Arc::new(GatedRunStore {
        inner: Arc::clone(&runs),
        gate: Arc::new(Notify::new()),
        finishing: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Fake(Vec::new()),
                Prepared::Fake(Vec::new()),
                Prepared::Fake(vec![Step::Stall]),
            ],
        ),
        DEFAULT_RETENTION,
        Arc::clone(&store) as Arc<dyn RunStore>,
        runs,
    );
    for name in ["s1", "s2"] {
        h.runtime.start(on_session(name, "q")).await.unwrap();
    }
    eventually("both runs are writing their final facts", || {
        store.finishing.load(Ordering::SeqCst) == 2
    })
    .await;
    let elsewhere: Vec<RunFacts> = ["e1", "e2"]
        .into_iter()
        .map(|name| {
            let facts = preparing(RunId::mint());
            RunFacts {
                session: RunSession::Plain(session(name)),
                run: RunDetails {
                    status: RunStatus::Running,
                    ..facts.run
                },
            }
        })
        .collect();
    for facts in &elsewhere {
        h.runs.put(facts).await.unwrap();
    }
    let live = h.runtime.start(on_session("s3", "q")).await.unwrap();
    until_running(&h, live).await;
    until_stored(&h, live, |facts| facts.run.status == RunStatus::Running).await;

    let page = RunQuery {
        status: Some(RunStatus::Running),
        limit: PageLimit::from(2),
        ..RunQuery::default()
    };
    let (first, second) = (elsewhere[0].run.run_id, elsewhere[1].run.run_id);
    assert_eq!(
        listed_run_ids(&h, page.clone()).await,
        (vec![first], Some(first))
    );
    assert_eq!(
        listed_run_ids(
            &h,
            RunQuery {
                after: Some(first),
                ..page
            }
        )
        .await,
        (vec![second, live], None)
    );
}

/// A session the runtime holds shows who is attached to it, and so does
/// each of its runs.
#[tokio::test]
async fn a_held_session_reports_its_observers() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))])],
    ));
    let s1 = session("s1");
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    until_running(&h, run).await;
    let _watching = h
        .runtime
        .attach(&s1, Attach::collecting().present())
        .await
        .unwrap();
    let _claiming = h.runtime.attach(&s1, Attach::claiming()).await.unwrap();

    let observers = ObserverCounts {
        collecting: 1,
        claiming: 1,
        present: 1,
    };
    let expected = Shown {
        observers,
        ..Shown::plain(
            &s1,
            AgentState::Running,
            Some(run),
            None,
            latest(&h, &s1).await,
        )
    };
    assert_eq!(read_session(&h, &s1).await, Some(expected.clone()));
    assert_eq!(
        listed_sessions(&h, SessionQuery::default()).await,
        (vec![expected], None)
    );
    assert_eq!(
        listed_runs(&h, RunQuery::default()).await,
        (vec![(h.runtime.facts(&run).unwrap(), observers)], None)
    );

    gate.notify_one();
    h.ended(run).await;
}

/// Six sessions, one in each state. Each reads as that state, and a listing
/// by the state's kind lists that session alone. The last one is never let
/// past preparing.
#[tokio::test(start_paused = true)]
async fn each_state_reads_as_its_kind_and_is_listed_under_it() {
    let prepare = Arc::new(Notify::new());
    let (on_hold, on_approval) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let decision = DecisionId::generate();
    let h = harness(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Fake(Vec::new()),
                Prepared::Fake(vec![Step::Emit(Box::new(
                    parked_on(DecisionId::generate()),
                ))]),
                Prepared::Fake(vec![Step::Wait(Arc::clone(&on_hold))]),
                Prepared::Fake(vec![
                    Step::Emit(Box::new(requested(decision))),
                    Step::Wait(Arc::clone(&on_approval)),
                ]),
                Prepared::Fake(vec![Step::Stall]),
            ],
        )
        .gated(Arc::clone(&prepare)),
    );

    let done = h.runtime.start(on_session("s-done", "q")).await.unwrap();
    prepare.notify_one();
    h.ended(done).await;
    stored(&h, done).await;

    let parked = h.runtime.start(on_session("s-parked", "q")).await.unwrap();
    prepare.notify_one();
    assert_eq!(h.ended(parked).await.run.status, RunStatus::Parked);
    stored(&h, parked).await;

    let running = h.runtime.start(on_session("s-running", "q")).await.unwrap();
    prepare.notify_one();
    until_running(&h, running).await;
    until_stored(&h, running, |facts| facts.run.status == RunStatus::Running).await;

    let blocked = h.runtime.start(on_session("s-blocked", "q")).await.unwrap();
    prepare.notify_one();
    until_stored(&h, blocked, |facts| {
        facts.run.pending_approvals == vec![decision]
    })
    .await;

    let stopping = h
        .runtime
        .start(on_session("s-stopping", "q"))
        .await
        .unwrap();
    prepare.notify_one();
    until_running(&h, stopping).await;
    until_stored(&h, stopping, |facts| facts.run.status == RunStatus::Running).await;
    assert!(h.runtime.cancel(&stopping, CancelCause::External, None));

    let preparing = h
        .runtime
        .start(on_session("s-preparing", "q"))
        .await
        .unwrap();
    until_stored(&h, preparing, |facts| {
        facts.run.status == RunStatus::Preparing
    })
    .await;

    let mut every = Vec::new();
    for (name, state, current_run, last_run) in [
        (
            "s-blocked",
            AgentState::Blocked {
                on: BlockReason::Approval {
                    decisions: vec![decision],
                },
            },
            Some(blocked),
            None,
        ),
        ("s-done", AgentState::Done, None, Some(done)),
        ("s-parked", AgentState::Parked, None, Some(parked)),
        ("s-preparing", AgentState::Preparing, Some(preparing), None),
        ("s-running", AgentState::Running, Some(running), None),
        ("s-stopping", AgentState::Stopping, Some(stopping), None),
    ] {
        let id = session(name);
        let expected = Shown::plain(
            &id,
            state.clone(),
            current_run,
            last_run,
            latest(&h, &id).await,
        );
        assert_eq!(h.runtime.session_state(&id), Some(state.clone()), "{name}");
        assert_eq!(
            read_session(&h, &id).await,
            Some(expected.clone()),
            "{name}"
        );
        assert_eq!(
            listed_sessions(&h, in_state(state.kind())).await,
            (vec![expected.clone()], None),
            "{name}"
        );
        every.push(expected);
    }
    assert_eq!(
        listed_sessions(&h, SessionQuery::default()).await,
        (every, None)
    );
}

/// A stopped run reads `stopping` from the stop until its ending, both in
/// the session's state and in its snapshot. It is listed under `stopping`
/// and not under `running`, although the store still holds it as running.
/// Once it ends it reads `done`.
#[tokio::test(start_paused = true)]
async fn a_stopped_run_reads_stopping_until_it_ends() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Stall])],
    ));
    let s1 = session("s1");
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    until_running(&h, run).await;
    until_stored(&h, run, |facts| facts.run.status == RunStatus::Running).await;

    assert!(h.runtime.cancel(&run, CancelCause::External, None));

    let stopping = Shown::plain(
        &s1,
        AgentState::Stopping,
        Some(run),
        None,
        latest(&h, &s1).await,
    );
    assert_eq!(h.runtime.session_state(&s1), Some(AgentState::Stopping));
    assert_eq!(read_session(&h, &s1).await, Some(stopping.clone()));
    assert_eq!(
        listed_sessions(&h, in_state(AgentStateKind::Stopping)).await,
        (vec![stopping], None)
    );
    assert_eq!(
        listed_sessions(&h, in_state(AgentStateKind::Running)).await,
        (vec![], None)
    );

    assert_eq!(h.ended(run).await.run.status, RunStatus::Cancelled);
    stored(&h, run).await;

    let done = Shown::plain(
        &s1,
        AgentState::Done,
        None,
        Some(run),
        latest(&h, &s1).await,
    );
    assert_eq!(h.runtime.session_state(&s1), Some(AgentState::Done));
    assert_eq!(read_session(&h, &s1).await, Some(done.clone()));
    assert_eq!(
        listed_sessions(&h, in_state(AgentStateKind::Stopping)).await,
        (vec![], None)
    );
    assert_eq!(
        listed_sessions(&h, in_state(AgentStateKind::Done)).await,
        (vec![done], None)
    );
}

/// A stop comes before a pending approval: a stopped run that waits on one
/// reads `stopping`, not `blocked`.
#[tokio::test(start_paused = true)]
async fn a_stopped_run_waiting_on_an_approval_reads_stopping() {
    let decision = DecisionId::generate();
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![
            Step::Emit(Box::new(requested(decision))),
            Step::Stall,
        ])],
    ));
    let s1 = session("s1");
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    until_stored(&h, run, |facts| {
        facts.run.pending_approvals == vec![decision]
    })
    .await;

    assert!(h.runtime.cancel(&run, CancelCause::External, None));

    let stopping = Shown::plain(
        &s1,
        AgentState::Stopping,
        Some(run),
        None,
        latest(&h, &s1).await,
    );
    assert_eq!(h.runtime.session_state(&s1), Some(AgentState::Stopping));
    assert_eq!(read_session(&h, &s1).await, Some(stopping.clone()));
    assert_eq!(
        listed_sessions(&h, in_state(AgentStateKind::Stopping)).await,
        (vec![stopping], None)
    );
    assert_eq!(
        listed_sessions(&h, in_state(AgentStateKind::Blocked)).await,
        (vec![], None)
    );
}

/// The store returns `s1`, `s2` and `s3` as running, which is one more than
/// the page holds, so the page is `s1` and `s2` and the listing goes on.
/// `s2` is stopping and is left off, and `next` is still `s2`, so the next
/// page starts after it.
#[tokio::test(start_paused = true)]
async fn a_page_cut_short_by_a_stopping_session_still_has_next() {
    let (one, three) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fake(vec![Step::Wait(Arc::clone(&one))]),
            Prepared::Fake(vec![Step::Stall]),
            Prepared::Fake(vec![Step::Wait(Arc::clone(&three))]),
        ],
    ));
    let mut runs = Vec::new();
    for name in ["s1", "s2", "s3"] {
        let run = h.runtime.start(on_session(name, "q")).await.unwrap();
        until_running(&h, run).await;
        until_stored(&h, run, |facts| facts.run.status == RunStatus::Running).await;
        runs.push(run);
    }
    assert!(h.runtime.cancel(&runs[1], CancelCause::External, None));

    let page = SessionQuery {
        state: Some(AgentStateKind::Running),
        limit: PageLimit::from(2),
        ..SessionQuery::default()
    };
    assert_eq!(
        listed_session_ids(&h, page.clone()).await,
        (vec![session("s1")], Some(session("s2")))
    );
    assert_eq!(
        listed_session_ids(
            &h,
            SessionQuery {
                after: Some(session("s2")),
                ..page
            }
        )
        .await,
        (vec![session("s3")], None)
    );
}

/// A `stopping` listing pages through the sessions the runtime holds in
/// session-id order, as a listing from the store does. `s2a` is running and
/// not stopped, so it is never listed.
#[tokio::test(start_paused = true)]
async fn a_stopping_listing_pages_in_session_id_order() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fake(vec![Step::Stall]),
            Prepared::Fake(vec![Step::Stall]),
            Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))]),
            Prepared::Fake(vec![Step::Stall]),
        ],
    ));
    let mut runs = Vec::new();
    for name in ["s3", "s1", "s2a", "s2"] {
        let run = h.runtime.start(on_session(name, "q")).await.unwrap();
        until_running(&h, run).await;
        until_stored(&h, run, |facts| facts.run.status == RunStatus::Running).await;
        runs.push(run);
    }
    for run in [runs[0], runs[1], runs[3]] {
        assert!(h.runtime.cancel(&run, CancelCause::External, None));
    }

    let page = SessionQuery {
        state: Some(AgentStateKind::Stopping),
        limit: PageLimit::from(2),
        ..SessionQuery::default()
    };
    assert_eq!(
        listed_session_ids(&h, page.clone()).await,
        (vec![session("s1"), session("s2")], Some(session("s2")))
    );
    assert_eq!(
        listed_session_ids(
            &h,
            SessionQuery {
                after: Some(session("s2")),
                ..page
            }
        )
        .await,
        (vec![session("s3")], None)
    );
}

/// A session the runtime let go and then reopened for a new run holds no
/// last run of its own. Its last run is the earlier run, which the store
/// names as the session's previous ended run.
#[tokio::test(start_paused = true)]
async fn last_run_on_a_session_reopened_with_a_live_run_is_the_earlier_run() {
    let retention = Duration::from_secs(60);
    let gate = Arc::new(Notify::new());
    let h = harness_retaining(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Fake(Vec::new()),
                Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))]),
            ],
        ),
        retention,
    );
    let s1 = session("s1");
    let first = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.ended(first).await;
    stored(&h, first).await;
    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the runtime lets the session go", || {
        h.runtime.session_state(&s1).is_none()
    })
    .await;

    let second = h.runtime.start(on_session("s1", "q")).await.unwrap();
    until_running(&h, second).await;
    until_stored(&h, second, |facts| facts.run.status == RunStatus::Running).await;

    let expected = Shown::plain(
        &s1,
        AgentState::Running,
        Some(second),
        Some(first),
        latest(&h, &s1).await,
    );
    assert_eq!(read_session(&h, &s1).await, Some(expected.clone()));
    assert_eq!(
        listed_sessions(&h, SessionQuery::default()).await,
        (vec![expected], None)
    );

    gate.notify_one();
    h.ended(second).await;
}

/// A session with no run in the store is `None`, even while an observer
/// holds it open, and is not listed.
#[tokio::test]
async fn a_session_that_never_ran_is_none() {
    let h = harness(TestPreparer::new(&["a"], Vec::new()));

    assert_eq!(read_session(&h, &session("nowhere")).await, None);

    let s1 = session("s1");
    let _watching = h.runtime.attach(&s1, Attach::collecting()).await.unwrap();
    assert!(
        h.runtime.observer_counts(&s1).is_some(),
        "the runtime holds s1"
    );
    assert_eq!(read_session(&h, &s1).await, None);
    assert_eq!(
        listed_sessions(&h, SessionQuery::default()).await,
        (vec![], None)
    );
}

/// The store has a live run's facts as of its `Started`. A listing shows
/// the runtime's record instead, so the run's usage and newest number are
/// current, and so does reading the run alone.
#[tokio::test]
async fn runs_shows_a_live_runs_usage_and_newest_number() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![
            Step::Bill {
                prompt: 1_000,
                completion: 50,
            },
            Step::Emit(Box::new(AgentEventPayload::TextDelta {
                content: "partial".to_owned(),
            })),
            Step::Wait(Arc::clone(&gate)),
        ])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    // `Started`, the session info, then the text.
    eventually("the run has journaled its text", || {
        h.runtime
            .facts(&run)
            .is_some_and(|facts| facts.run.latest_event == Some(seq(3)))
    })
    .await;

    let live = h.runtime.facts(&run).unwrap();
    assert_eq!(live.run.usage, tokens(1_000, 50));
    let in_store = h.runs.get(&run).await.unwrap().unwrap();
    assert_eq!(
        (in_store.run.usage, in_store.run.latest_event),
        (TokenUsage::default(), Some(seq(1))),
        "the store is behind the record"
    );

    assert_eq!(
        listed_runs(&h, RunQuery::default()).await,
        (vec![(live.clone(), ObserverCounts::default())], None)
    );
    let read = h.runtime.run(&run).await.unwrap().expect("the run is held");
    assert_eq!(
        (read.facts().clone(), read.observers()),
        (live, ObserverCounts::default())
    );

    gate.notify_one();
    h.ended(run).await;
}

/// A run the store holds as running whose record has ended is matched by
/// its record: a page asking for running runs leaves it off, with the next
/// running run in its place, and a page asking for finished runs has it.
#[tokio::test]
async fn runs_matches_a_run_the_runtime_holds_by_its_record() {
    let runs = Arc::new(InMemoryRunStore::new());
    let store = Arc::new(GatedRunStore {
        inner: Arc::clone(&runs),
        gate: Arc::new(Notify::new()),
        finishing: std::sync::atomic::AtomicUsize::new(0),
    });
    let gate = Arc::new(Notify::new());
    let h = harness_over(
        TestPreparer::new(
            &["a"],
            vec![
                Prepared::Fake(Vec::new()),
                Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))]),
            ],
        ),
        DEFAULT_RETENTION,
        Arc::clone(&store) as Arc<dyn RunStore>,
        runs,
    );
    let ended = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the first run is writing its final facts", || {
        store.finishing.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(
        h.runtime.facts(&ended).map(|facts| facts.run.status),
        Some(RunStatus::Finished)
    );
    assert_eq!(
        h.runs
            .get(&ended)
            .await
            .unwrap()
            .map(|facts| facts.run.status),
        Some(RunStatus::Running),
        "the store has not got the ending yet"
    );
    let live = h.runtime.start(on_session("s2", "q")).await.unwrap();
    until_running(&h, live).await;
    until_stored(&h, live, |facts| facts.run.status == RunStatus::Running).await;

    let page = RunQuery {
        status: Some(RunStatus::Running),
        limit: PageLimit::from(1),
        ..RunQuery::default()
    };
    assert_eq!(
        listed_run_ids(&h, page.clone()).await,
        (vec![live], Some(live))
    );
    assert_eq!(
        listed_run_ids(
            &h,
            RunQuery {
                after: Some(live),
                ..page
            }
        )
        .await,
        (vec![], None)
    );
    assert_eq!(
        listed_run_ids(
            &h,
            RunQuery {
                status: Some(RunStatus::Finished),
                ..RunQuery::default()
            }
        )
        .await,
        (vec![ended], None)
    );
}

/// A run the runtime holds is read from its record, and once the runtime
/// lets it go, from the store. A run that failed before `Started` shows
/// which: its record knows the event before it, and the store does not.
/// Here events 1 and 2 are an observer's attach and detach, and 3 is the
/// run's `Failed`.
#[tokio::test(start_paused = true)]
async fn run_reads_a_held_run_from_its_record_and_a_run_let_go_from_the_store() {
    let retention = Duration::from_secs(60);
    let h = harness_retaining(
        TestPreparer::new(&["a"], vec![Prepared::Fail("no provider".to_owned())]),
        retention,
    );
    let s1 = session("s1");
    h.runtime
        .attach(&s1, Attach::collecting())
        .await
        .unwrap()
        .detach()
        .await;
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;
    stored(&h, run).await;
    assert_eq!(
        (facts.run.first_event, facts.run.latest_event),
        (None, Some(seq(3)))
    );

    let snapshot = h.runtime.run(&run).await.unwrap().expect("the run is held");
    assert_eq!(
        (snapshot.facts().clone(), snapshot.observers()),
        (facts.clone(), ObserverCounts::default())
    );
    assert_eq!(
        snapshot.events_after(None),
        Some(EventRange {
            from: Cursor::After(seq(2)),
            to: Some(seq(3)),
        })
    );

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the runtime lets the run go", || {
        h.runtime.facts(&run).is_none()
    })
    .await;

    let from_store = h
        .runtime
        .run(&run)
        .await
        .unwrap()
        .expect("the store keeps the run");
    assert_eq!(
        (from_store.facts().clone(), from_store.observers()),
        (facts, ObserverCounts::default())
    );
    assert_eq!(
        from_store.events_after(None),
        Some(EventRange {
            from: Cursor::From(seq(3)),
            to: Some(seq(3)),
        })
    );
    assert_eq!(h.runtime.run(&RunId::mint()).await.unwrap(), None);
}

/// A stop on a live run is `Stopping`, with the run as it was before the
/// stop. A second stop is `Stopping` again, and the run ends with the first
/// stop's cause and reason.
#[tokio::test(start_paused = true)]
async fn stopping_a_live_run_twice_keeps_the_first_cause_and_reason() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Stall])],
    ));
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    until_running(&h, run).await;
    let before = h.runtime.facts(&run).unwrap();

    for (cause, reason) in [
        (CancelCause::External, "the user pressed stop"),
        (CancelCause::Unclaimed, "a second stop"),
    ] {
        match h
            .runtime
            .stop(&run, cause, Some(reason.to_owned()))
            .await
            .unwrap()
        {
            StopOutcome::Stopping(snapshot) => assert_eq!(
                (snapshot.facts().clone(), snapshot.observers()),
                (before.clone(), ObserverCounts::default()),
                "{reason}"
            ),
            other => panic!("not Stopping: {other:?}"),
        }
    }

    assert_eq!(h.ended(run).await.run.status, RunStatus::Cancelled);
    match last_lifecycle(&h.journal(&session("s1")).await) {
        LifecycleEvent::Cancelled { cause, reason, .. } => assert_eq!(
            (*cause, reason.as_deref()),
            (CancelCause::External, Some("the user pressed stop"))
        ),
        other => panic!("not Cancelled: {other:?}"),
    }
}

/// A stop on a run that has ended is `AlreadyEnded` with the run's final
/// facts, while the runtime holds the run and after it lets the run go. The
/// run keeps the one ending it had.
#[tokio::test(start_paused = true)]
async fn stopping_an_ended_run_is_already_ended_whether_held_or_not() {
    let retention = Duration::from_secs(60);
    let h = harness_retaining(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]),
        retention,
    );
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;
    stored(&h, run).await;

    match h
        .runtime
        .stop(&run, CancelCause::External, None)
        .await
        .unwrap()
    {
        StopOutcome::AlreadyEnded(snapshot) => assert_eq!(
            (snapshot.facts().clone(), snapshot.observers()),
            (facts.clone(), ObserverCounts::default())
        ),
        other => panic!("not AlreadyEnded: {other:?}"),
    }

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("the runtime lets the run go", || {
        h.runtime.facts(&run).is_none()
    })
    .await;

    match h
        .runtime
        .stop(&run, CancelCause::External, None)
        .await
        .unwrap()
    {
        StopOutcome::AlreadyEnded(snapshot) => assert_eq!(
            (snapshot.facts().clone(), snapshot.observers()),
            (facts, ObserverCounts::default())
        ),
        other => panic!("not AlreadyEnded: {other:?}"),
    }
    let events = h.journal(&session("s1")).await;
    assert_eq!(
        events.iter().filter(|event| event.ends_run()).count(),
        1,
        "the run has one ending"
    );
    assert_eq!(kinds(&events).last(), Some(&"finished"));
}

fn finished(payload: &SessionEventPayload) -> bool {
    matches!(
        payload,
        SessionEventPayload::Lifecycle(LifecycleEvent::Finished { .. })
    )
}

/// A stop after the run's stream ended, while its ending is still being
/// written, is refused, and it waits for the ending, so its snapshot shows
/// how the run ended. Here the journal store holds the run's `Finished`.
#[tokio::test]
async fn a_stop_while_the_ending_is_written_waits_for_it() {
    let gate = Arc::new(Notify::new());
    let h = super::observe::harness_gating(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]),
        DEFAULT_RETENTION,
        &gate,
        finished,
    );
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run's stream has ended", || {
        h.runtime
            .shared
            .run(&run)
            .is_some_and(|record| !record.stoppable())
    })
    .await;

    let stop = tokio::spawn({
        let runtime = Arc::clone(&h.runtime);
        async move { runtime.stop(&run, CancelCause::External, None).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!stop.is_finished(), "the stop waits for the run's ending");
    gate.notify_one();
    match stop.await.unwrap().unwrap() {
        StopOutcome::AlreadyEnded(snapshot) => {
            assert_eq!(snapshot.facts().run.status, RunStatus::Finished);
        }
        other => panic!("not AlreadyEnded: {other:?}"),
    }
}

/// A read of a run whose stream has ended, while its ending is still being
/// written, waits for the ending, so the snapshot never shows the run live
/// once its terminal event can be on the session's stream. Here the
/// journal store holds the run's `Finished`.
#[tokio::test]
async fn a_read_while_the_ending_is_written_waits_for_it() {
    let gate = Arc::new(Notify::new());
    let h = super::observe::harness_gating(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]),
        DEFAULT_RETENTION,
        &gate,
        finished,
    );
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run's stream has ended", || {
        h.runtime
            .shared
            .run(&run)
            .is_some_and(|record| !record.stoppable())
    })
    .await;

    let read = tokio::spawn({
        let runtime = Arc::clone(&h.runtime);
        async move { runtime.run(&run).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!read.is_finished(), "the read waits for the run's ending");
    gate.notify_one();
    let snapshot = read
        .await
        .unwrap()
        .unwrap()
        .expect("the runtime holds the run");
    assert_eq!(snapshot.facts().run.status, RunStatus::Finished);
}

/// A stop on a run this process does not hold is `NotHere`, both for an id
/// nothing knows and for a run the store holds as not ended, such as one
/// another process is running.
#[tokio::test]
async fn stopping_a_run_that_is_not_held_and_not_ended_is_not_here() {
    let h = harness(TestPreparer::new(&["a"], Vec::new()));

    assert_eq!(
        h.runtime
            .stop(&RunId::mint(), CancelCause::External, None)
            .await
            .unwrap(),
        StopOutcome::NotHere
    );

    let elsewhere = preparing(RunId::mint());
    let elsewhere = RunFacts {
        run: RunDetails {
            status: RunStatus::Running,
            ..elsewhere.run
        },
        ..elsewhere
    };
    h.runs.put(&elsewhere).await.unwrap();
    assert_eq!(
        h.runtime
            .stop(&elsewhere.run.run_id, CancelCause::External, None)
            .await
            .unwrap(),
        StopOutcome::NotHere
    );
}

/// A `chat` query lists the lanes of that chat and nothing else: not a lane
/// of another chat, and not a plain session named for the chat. A query for
/// `stopping` with a chat narrows the same way.
#[tokio::test(start_paused = true)]
async fn a_chat_query_lists_only_that_chats_lanes() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fake(vec![Step::Stall]),
            Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))]),
            Prepared::Fake(vec![Step::Stall]),
            Prepared::Fake(vec![Step::Stall]),
        ],
    ));
    let mut started = Vec::new();
    for start in [
        on_chat("c1", "q"),
        on_chat("c1", "q"),
        on_chat("c2", "q"),
        on_session("c1", "q"),
    ] {
        let run = h.runtime.start(start).await.unwrap();
        until_running(&h, run).await;
        until_stored(&h, run, |facts| facts.run.status == RunStatus::Running).await;
        started.push(run);
    }
    let [home, second, other_chat, plain] = started[..] else {
        panic!("four runs started");
    };
    for run in [home, other_chat, plain] {
        assert!(h.runtime.cancel(&run, CancelCause::External, None));
    }

    let in_c1 = SessionQuery {
        chat: Some(session("c1")),
        ..SessionQuery::default()
    };
    assert_eq!(
        listed_session_ids(&h, in_c1.clone()).await,
        (vec![session("c1:a:0"), session("c1:a:1")], None)
    );
    assert_eq!(
        listed_session_ids(
            &h,
            SessionQuery {
                state: Some(AgentStateKind::Running),
                ..in_c1.clone()
            }
        )
        .await,
        (vec![session("c1:a:1")], None)
    );
    let home_lane = session("c1:a:0");
    assert_eq!(
        listed_sessions(
            &h,
            SessionQuery {
                state: Some(AgentStateKind::Stopping),
                ..in_c1
            }
        )
        .await,
        (
            vec![Shown {
                id: home_lane.clone(),
                run_session: RunSession::Lane(Lane::new(
                    Conversation::new(session("c1"), "a"),
                    LaneNumber::HOME,
                )),
                agent: "a".to_owned(),
                state: AgentState::Stopping,
                current_run: Some(home),
                last_run: None,
                latest_event: latest(&h, &home_lane).await,
                observers: ObserverCounts::default(),
            }],
            None
        )
    );
    assert_eq!(
        listed_session_ids(&h, in_state(AgentStateKind::Stopping)).await,
        (
            vec![session("c1"), session("c1:a:0"), session("c2:a:0")],
            None
        ),
        "without a chat, every stopping session is listed"
    );
    assert_eq!(
        listed_run_ids(
            &h,
            RunQuery {
                chat: Some(session("c1")),
                ..RunQuery::default()
            }
        )
        .await,
        (vec![home, second], None)
    );
}

/// A run stopped while its agent is still being prepared reads `stopping`
/// and is listed under it until it ends. It ends `Cancelled` without
/// `Started`, and the session then reads `done`.
#[tokio::test(start_paused = true)]
async fn a_run_stopped_while_preparing_reads_stopping_until_it_ends() {
    let prepare = Arc::new(Notify::new());
    let h = harness(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]).gated(Arc::clone(&prepare)),
    );
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    until_stored(&h, run, |facts| facts.run.status == RunStatus::Preparing).await;

    let outcome = h
        .runtime
        .stop(&run, CancelCause::External, None)
        .await
        .unwrap();
    assert!(matches!(outcome, StopOutcome::Stopping(_)), "{outcome:?}");
    assert_eq!(
        h.runtime.session_state(&session("s1")),
        Some(AgentState::Stopping)
    );
    let (listed, _) = listed_sessions(&h, in_state(AgentStateKind::Stopping)).await;
    assert_eq!(listed.len(), 1, "{listed:?}");

    prepare.notify_one();
    let facts = h.ended(run).await;
    assert_eq!(facts.run.status, RunStatus::Cancelled);
    assert_eq!(facts.run.first_event, None, "it ended without `Started`");
    assert_eq!(
        h.runtime.session_state(&session("s1")),
        Some(AgentState::Done)
    );
    let (listed, _) = listed_sessions(&h, in_state(AgentStateKind::Done)).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
}
