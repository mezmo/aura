//! Observer tests: attaching and detaching, claims and attendance, and how
//! observers fare through reopens, drains and stalled journal writes.

use aura_events::run::{DetachCause, ObserverKind};
use futures::StreamExt;

use super::*;
use crate::journal::{Cursor, JournalItem};

fn session(name: &str) -> SessionId {
    SessionId::new(name).unwrap()
}

/// The next item on a subscription. Fails the test if none arrives in time.
async fn next_item(sub: &mut Subscription) -> JournalItem {
    tokio::time::timeout(PATIENCE, sub.next())
        .await
        .expect("an item arrives in time")
        .expect("the subscription is open")
}

impl Harness {
    /// The run's context, once the run streams.
    async fn context(&self, run: RunId) -> Arc<RunContext> {
        let record = self
            .runtime
            .shared
            .run(&run)
            .expect("the runtime holds the run");
        eventually("the run binds its context", || record.context().is_some()).await;
        record.context().expect("the run's context is bound")
    }

    /// Waits until the store holds an event of `kind` for `session`.
    async fn journaled(&self, session: &SessionId, kind: &str) {
        tokio::time::timeout(PATIENCE, async {
            while !kinds(&self.journal(session).await).contains(&kind) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for `{kind}` on the journal"));
    }
}

/// Attaching opens the session and records the observer. Detaching records
/// `Released` and lets the now-idle session go. Neither event names a run,
/// since none was live.
#[tokio::test]
async fn an_observer_attaches_and_detaches_before_any_run() {
    let h = harness(TestPreparer::new(&["a"], vec![]));
    let s = session("s1");

    let sub = h
        .runtime
        .attach(&s, Attach::collecting().present())
        .await
        .unwrap();
    let id = sub.observer().id.clone();
    assert_eq!(sub.observer().kind, ObserverKind::Collecting);
    assert!(sub.observer().presence);
    assert_eq!(
        h.runtime.observer_counts(&s),
        Some(ObserverCounts {
            collecting: 1,
            claiming: 0,
            present: 1,
        })
    );

    sub.detach().await;
    assert_eq!(
        h.runtime.observer_counts(&s),
        None,
        "the session went with its last observer"
    );

    let events = h.journal(&s).await;
    assert_eq!(kinds(&events), ["observer_attached", "observer_detached"]);
    assert!(events.iter().all(|event| event.run_id.is_none()));
    match &events[1].payload {
        SessionEventPayload::Lifecycle(LifecycleEvent::ObserverDetached { observer, because }) => {
            assert_eq!(observer.id, id);
            assert_eq!(*because, DetachCause::Released);
        }
        other => panic!("not ObserverDetached: {other:?}"),
    }
}

/// Dropping a subscription detaches the observer the same way.
#[tokio::test]
async fn dropping_a_subscription_detaches_its_observer() {
    let h = harness(TestPreparer::new(&["a"], vec![]));
    let s = session("s1");

    let sub = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    assert_eq!(h.runtime.observer_counts(&s).unwrap().claiming, 1);
    drop(sub);

    assert_eq!(
        h.runtime.observer_counts(&s).unwrap().claiming,
        0,
        "the claim goes with the drop itself"
    );
    h.journaled(&s, "observer_detached").await;
    eventually("the session is let go", || {
        h.runtime.observer_counts(&s).is_none()
    })
    .await;
}

/// A live subscription starts at its own attach. One from the start replays
/// everything the store holds first.
#[tokio::test]
async fn a_subscription_reads_the_sessions_stream_from_its_cursor() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Scripted(vec![ScriptedTurn::text("hello")])],
    ));
    let s = session("s1");

    let mut live = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let facts = h.ended(run).await;
    assert_eq!(
        facts.status,
        RunStatus::Finished,
        "the claim held through the run"
    );

    let mut seen = Vec::new();
    loop {
        match next_item(&mut live).await {
            JournalItem::Event(event) => {
                seen.push(kind(&event));
                if event.ends_run() {
                    break;
                }
            }
            other => panic!("not an event: {other:?}"),
        }
    }
    assert_eq!(seen.first(), Some(&"observer_attached"), "{seen:?}");
    assert_eq!(seen.get(1), Some(&"started"), "{seen:?}");
    assert_eq!(seen.last(), Some(&"finished"), "{seen:?}");

    let mut replay = h
        .runtime
        .attach(&s, Attach::collecting().from(Cursor::Start))
        .await
        .unwrap();
    match next_item(&mut replay).await {
        JournalItem::Event(event) => {
            assert_eq!(event.seq, SequenceNumber::FIRST);
            assert_eq!(kind(&event), "observer_attached");
        }
        other => panic!("not an event: {other:?}"),
    }
}

/// A second claim is refused with the claimant's id, and succeeds once the
/// claimant detaches. Collecting observers do not count against the claim.
#[tokio::test]
async fn a_second_claim_is_refused_until_the_first_lets_go() {
    let h = harness(TestPreparer::new(&["a"], vec![]));
    let s = session("s1");

    let first = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    let _collector = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    let _another = h
        .runtime
        .attach(&s, Attach::collecting().present())
        .await
        .unwrap();
    match h.runtime.attach(&s, Attach::claiming()).await {
        Err(AttachError::AlreadyClaimed { by }) => assert_eq!(by, first.observer().id),
        other => panic!("not refused: {other:?}"),
    }
    assert_eq!(
        h.runtime.observer_counts(&s),
        Some(ObserverCounts {
            collecting: 2,
            claiming: 1,
            present: 1,
        })
    );

    first.detach().await;
    let second = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    assert_eq!(h.runtime.observer_counts(&s).unwrap().claiming, 1);
    second.detach().await;
}

/// A run started on a session with a claim and presence sees both from the
/// start. The agent receives them when it begins streaming, the run's
/// context reports them, and a child context, as a worker uses, reads the
/// same values.
#[tokio::test]
async fn a_run_started_under_a_claim_sees_it_from_the_start() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))])],
    ));
    let s = session("s1");

    let claim = h
        .runtime
        .attach(&s, Attach::claiming().present())
        .await
        .unwrap();
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let context = h.context(run).await;
    let attended = Attendance {
        present: true,
        claimed: true,
    };
    assert_eq!(context.attendance(), attended);
    assert_eq!(
        h.preparer.attended.lock().unwrap().as_slice(),
        [attended],
        "the agent saw the session's attendance as it began streaming"
    );
    let worker = RunContext::child(&context, None, None, None);
    assert_eq!(worker.attendance(), attended);

    gate.notify_one();
    assert_eq!(h.ended(run).await.status, RunStatus::Finished);
    claim.detach().await;
}

/// When the claimant leaves a live run, `ClaimsExhausted` follows the
/// detach and, under the Cancel policy, the run ends `Cancelled` as
/// `Unclaimed`. A collecting observer leaving changes nothing. Events
/// recorded during the run carry its id.
#[tokio::test]
async fn a_live_run_whose_last_claimant_leaves_is_cancelled_as_unclaimed() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Wait(gate)])],
    ));
    let s = session("s1");

    let claim = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    let collector = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let context = h.context(run).await;
    h.journaled(&s, "session_info").await;

    collector.detach().await;
    assert!(context.attendance().claimed);
    assert_eq!(h.runtime.facts(&run).unwrap().status, RunStatus::Running);

    claim.detach().await;
    assert!(!context.attendance().claimed);
    let facts = h.ended(run).await;
    assert_eq!(facts.status, RunStatus::Cancelled);

    let events = h.journal(&s).await;
    assert_eq!(
        kinds(&events),
        [
            "observer_attached",
            "observer_attached",
            "started",
            "session_info",
            "observer_detached",
            "observer_detached",
            "claims_exhausted",
            "cancelled",
        ]
    );
    assert!(
        events[4..].iter().all(|event| event.run_id == Some(run)),
        "events while the run was live name it"
    );
    match last_lifecycle(&events) {
        LifecycleEvent::Cancelled {
            reason: RunCancelReason::Unclaimed,
            ..
        } => {}
        other => panic!("not cancelled as unclaimed: {other:?}"),
    }
}

/// Under the Continue policy the run goes on unclaimed.
#[tokio::test]
async fn a_run_under_continue_outlives_its_claimant() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Wait(Arc::clone(&gate))])],
    ));
    let s = session("s1");

    let claim = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    let run = h
        .runtime
        .start(StartRun {
            liveness: Liveness {
                policy: LivenessPolicy::Continue,
                grace: Duration::ZERO,
            },
            ..on_session("s1", "q")
        })
        .await
        .unwrap();
    h.context(run).await;

    claim.detach().await;
    h.journaled(&s, "claims_exhausted").await;
    assert_eq!(h.runtime.facts(&run).unwrap().status, RunStatus::Running);

    gate.notify_one();
    assert_eq!(h.ended(run).await.status, RunStatus::Finished);
}

/// Attaching and detaching during a run update the run's context at once.
/// A run with no claimant keeps going, and one claimed mid-run is cancelled
/// when that claimant leaves.
#[tokio::test]
async fn attaching_mid_run_reaches_the_runs_context() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Fake(vec![Step::Wait(gate)])],
    ));
    let s = session("s1");

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let context = h.context(run).await;
    assert!(!context.attendance().present);
    assert!(!context.attendance().claimed);

    let present = h
        .runtime
        .attach(&s, Attach::collecting().present())
        .await
        .unwrap();
    assert!(context.attendance().present);
    assert!(!context.attendance().claimed);
    let claim = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    assert!(context.attendance().claimed);

    drop(present);
    assert!(!context.attendance().present);
    assert!(context.attendance().claimed);

    claim.detach().await;
    assert_eq!(h.ended(run).await.status, RunStatus::Cancelled);
    let events = h.journal(&s).await;
    let attaches: Vec<_> = events
        .iter()
        .filter(|event| kind(event) == "observer_attached")
        .collect();
    assert_eq!(attaches.len(), 2);
    assert!(attaches.iter().all(|event| event.run_id == Some(run)));
}

/// A session with an observer is kept after its runs are let go, and is let
/// go when the observer detaches.
#[tokio::test]
async fn a_session_with_an_observer_outlives_its_runs_retention() {
    let h = harness_retaining(
        TestPreparer::new(
            &["a"],
            vec![Prepared::Scripted(vec![ScriptedTurn::text("hello")])],
        ),
        Duration::from_millis(10),
    );
    let s = session("s1");

    let sub = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.ended(run).await;
    eventually("the run is let go", || h.runtime.facts(&run).is_none()).await;
    assert!(
        h.runtime.observer_counts(&s).is_some(),
        "the session stays for its observer"
    );

    sub.detach().await;
    assert!(h.runtime.observer_counts(&s).is_none());
}

/// Draining ends existing subscriptions, since each session's journal goes
/// with it, and refuses new attaches.
#[tokio::test]
async fn drain_ends_subscriptions_and_refuses_new_ones() {
    let h = harness(TestPreparer::new(&["a"], vec![]));
    let s = session("s1");

    let mut sub = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    match next_item(&mut sub).await {
        JournalItem::Event(event) => assert_eq!(kind(&event), "observer_attached"),
        other => panic!("not an event: {other:?}"),
    }
    h.runtime.drain(Duration::from_millis(10)).await;

    let ended = tokio::time::timeout(PATIENCE, sub.next())
        .await
        .expect("the stream ends in time");
    assert!(ended.is_none(), "{ended:?}");
    assert!(matches!(
        h.runtime.attach(&s, Attach::collecting()).await,
        Err(AttachError::ShuttingDown)
    ));
}

/// A session reopened after another writer fenced it keeps its observers.
/// The claim still holds, a run started afterwards sees it, the claimant's
/// stream moves to the new journal with the other writer's events in it,
/// and the observer's detach is recorded on the new journal.
#[tokio::test]
async fn a_reopened_fenced_session_keeps_its_observers() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Scripted(vec![ScriptedTurn::text("one")]),
            Prepared::Fake(vec![Step::Wait(gate)]),
        ],
    ));
    let s = session("s1");

    let mut claim = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    h.journals
        .append(
            &s,
            h.journals.latest(&s).await.unwrap(),
            None,
            aura_events::Timestamp::now(),
            SessionEventPayload::Agent(AgentEvent::single_agent(AgentEventPayload::TextDelta {
                content: "from another writer".to_owned(),
            })),
        )
        .await
        .unwrap();
    let fenced = h.runtime.start(on_session("s1", "q")).await.unwrap();
    assert_eq!(h.ended(fenced).await.status, RunStatus::Failed);

    match h.runtime.attach(&s, Attach::claiming()).await {
        Err(AttachError::AlreadyClaimed { by }) => assert_eq!(by, claim.observer().id),
        other => panic!("the claim did not survive the reopen: {other:?}"),
    }
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    let context = h.context(run).await;
    assert!(context.attendance().claimed);

    let mut seen = Vec::new();
    while seen.last() != Some(&"started") {
        match next_item(&mut claim).await {
            JournalItem::Event(event) => seen.push(kind(&event)),
            other => panic!("not an event: {other:?}"),
        }
    }
    assert_eq!(seen, ["observer_attached", "text_delta", "started"]);

    claim.detach().await;
    assert_eq!(h.ended(run).await.status, RunStatus::Cancelled);
    let events = h.journal(&s).await;
    let tail: Vec<_> = kinds(&events).into_iter().rev().take(3).collect();
    assert_eq!(tail, ["cancelled", "claims_exhausted", "observer_detached"]);
}

/// A claimant that leaves while the run is writing its ending is recorded
/// without naming the run, and no `ClaimsExhausted` follows the run's
/// ending.
#[tokio::test]
async fn a_claimant_leaving_as_the_run_ends_does_not_name_it() {
    let gate = Arc::new(Notify::new());
    let runs = Arc::new(InMemoryRunStore::new());
    let store = Arc::new(GatedRunStore {
        inner: Arc::clone(&runs),
        gate: Arc::clone(&gate),
        finishing: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_over(
        TestPreparer::new(&["a"], vec![Prepared::Fake(Vec::new())]),
        DEFAULT_RETENTION,
        Arc::clone(&store) as Arc<dyn RunStore>,
        runs,
    );
    let s = session("s1");

    let claim = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    eventually("the run is writing its ending", || {
        store.finishing.load(Ordering::SeqCst) == 1
    })
    .await;
    claim.detach().await;
    gate.notify_one();

    assert_eq!(h.ended(run).await.status, RunStatus::Finished);
    let events = h.journal(&s).await;
    assert_eq!(
        kinds(&events),
        [
            "observer_attached",
            "started",
            "session_info",
            "finished",
            "observer_detached",
        ]
    );
    assert_eq!(events.last().unwrap().run_id, None);
}

/// A detach is recorded, and the session let go, even if the caller stops
/// waiting part-way.
#[tokio::test]
async fn a_detach_dropped_part_way_is_still_recorded() {
    let h = harness(TestPreparer::new(&["a"], vec![]));
    let s = session("s1");

    let sub = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    let _ = tokio::time::timeout(Duration::ZERO, sub.detach()).await;

    h.journaled(&s, "observer_detached").await;
    eventually("the session is let go", || {
        h.runtime.observer_counts(&s).is_none()
    })
    .await;
}

/// A journal store with a gate on `append`.
struct GatedJournalStore {
    inner: Arc<InMemoryJournalStore>,
    gate: Arc<Notify>,
    /// Which appends wait for the gate.
    gates: fn(&SessionEventPayload) -> bool,
}

fn every_event(_: &SessionEventPayload) -> bool {
    true
}

fn observer_attached(payload: &SessionEventPayload) -> bool {
    matches!(
        payload,
        SessionEventPayload::Lifecycle(LifecycleEvent::ObserverAttached { .. })
    )
}

fn observer_detached(payload: &SessionEventPayload) -> bool {
    matches!(
        payload,
        SessionEventPayload::Lifecycle(LifecycleEvent::ObserverDetached { .. })
    )
}

#[async_trait]
impl JournalStore for GatedJournalStore {
    /// Waits for the gate before an append `gates` picks, so a test can
    /// pause a caller mid-append.
    async fn append(
        &self,
        session: &SessionId,
        expected: Option<SequenceNumber>,
        run_id: Option<RunId>,
        at: aura_events::Timestamp,
        payload: SessionEventPayload,
    ) -> Result<Arc<SessionEvent>, SessionStoreError> {
        if (self.gates)(&payload) {
            self.gate.notified().await;
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
        self.inner.read(session, from, to, limit).await
    }

    async fn latest(
        &self,
        session: &SessionId,
    ) -> Result<Option<SequenceNumber>, SessionStoreError> {
        self.inner.latest(session).await
    }
}

/// An attach dropped while its event is being appended leaves no observer,
/// no claim, and no session behind.
#[tokio::test]
async fn an_attach_dropped_while_recording_leaves_nothing_behind() {
    let gate = Arc::new(Notify::new());
    let h = harness_gating(
        TestPreparer::new(&["a"], vec![]),
        DEFAULT_RETENTION,
        &gate,
        every_event,
    );
    let s = session("s1");

    let dropped = tokio::time::timeout(
        Duration::from_millis(20),
        h.runtime.attach(&s, Attach::claiming().present()),
    )
    .await;
    assert!(dropped.is_err(), "the attach was still writing its record");
    assert_eq!(
        h.runtime
            .observer_counts(&s)
            .map_or(0, |counts| counts.claiming),
        0,
        "the claim goes with the dropped attach"
    );
    eventually("the session is let go", || {
        h.runtime.observer_counts(&s).is_none()
    })
    .await;

    gate.notify_one();
    let claim = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    assert_eq!(h.runtime.observer_counts(&s).unwrap().claiming, 1);
    assert_eq!(kinds(&h.journal(&s).await), ["observer_attached"]);
    gate.notify_one();
    claim.detach().await;
}

/// A harness whose runtime writes events through a journal store that holds
/// the appends `gates` picks until `gate` is notified.
fn harness_gating(
    preparer: TestPreparer,
    retention: Duration,
    gate: &Arc<Notify>,
    gates: fn(&SessionEventPayload) -> bool,
) -> Harness {
    let runs = Arc::new(InMemoryRunStore::new());
    let journals = Arc::new(InMemoryJournalStore::new());
    harness_build(
        preparer,
        retention,
        Arc::clone(&runs) as Arc<dyn RunStore>,
        runs,
        Arc::new(GatedJournalStore {
            inner: Arc::clone(&journals),
            gate: Arc::clone(gate),
            gates,
        }),
        journals,
    )
}

/// An attach waiting on a stalled journal write does not hold up a start,
/// and the run it started still appends `Started` after the attach.
#[tokio::test]
async fn a_stalled_attach_does_not_hold_up_a_start() {
    let gate = Arc::new(Notify::new());
    let h = harness_gating(
        TestPreparer::new(
            &["a"],
            vec![Prepared::Scripted(vec![ScriptedTurn::text("hello")])],
        ),
        DEFAULT_RETENTION,
        &gate,
        observer_attached,
    );
    let s = session("s1");

    let attach = h.runtime.attach(&s, Attach::claiming());
    tokio::pin!(attach);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), attach.as_mut())
            .await
            .is_err(),
        "the attach is waiting on its write"
    );
    let run = tokio::time::timeout(PATIENCE, h.runtime.start(on_session("s1", "q")))
        .await
        .expect("the start is not held up by the stalled attach")
        .unwrap();

    gate.notify_one();
    let claim = attach.await.unwrap();
    assert_eq!(h.ended(run).await.status, RunStatus::Finished);
    let events = h.journal(&s).await;
    assert_eq!(kinds(&events)[..2], ["observer_attached", "started"]);
    assert_eq!(events[0].run_id, None, "no run had started at the attach");
    drop(claim);
}

/// An attach waiting on a stalled journal write does not hold up a drain,
/// and gives up once the drain is done, so the session's other
/// subscriptions still end.
#[tokio::test]
async fn a_stalled_attach_neither_holds_up_nor_outlives_a_drain() {
    let gate = Arc::new(Notify::new());
    let h = harness_gating(
        TestPreparer::new(&["a"], vec![]),
        DEFAULT_RETENTION,
        &gate,
        observer_attached,
    );

    let s = session("s1");
    gate.notify_one();
    let mut staying = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    let attach = h.runtime.attach(&s, Attach::claiming());
    tokio::pin!(attach);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), attach.as_mut())
            .await
            .is_err(),
        "the attach is waiting on its write"
    );
    tokio::time::timeout(PATIENCE, h.runtime.drain(Duration::from_millis(10)))
        .await
        .expect("the drain is not held up by the stalled attach");

    let refused = tokio::time::timeout(PATIENCE, attach)
        .await
        .expect("the attach gives up once the drain is done");
    assert!(
        matches!(refused, Err(AttachError::ShuttingDown)),
        "{refused:?}"
    );
    let ended =
        tokio::time::timeout(PATIENCE, async { while staying.next().await.is_some() {} }).await;
    assert!(ended.is_ok(), "the subscription ends after the drain");
}

/// A session is not let go while an observer's detach is still being
/// written, even once its last run's retention has passed, so a later start
/// cannot open a second journal beside the one the detach is writing to.
#[tokio::test]
async fn a_session_stays_while_a_detach_is_written() {
    let gate = Arc::new(Notify::new());
    let h = harness_gating(
        TestPreparer::new(
            &["a"],
            vec![Prepared::Scripted(vec![ScriptedTurn::text("hello")])],
        ),
        Duration::from_millis(10),
        &gate,
        observer_detached,
    );
    let s = session("s1");

    let sub = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.ended(run).await;
    drop(sub);
    eventually("the run is let go", || h.runtime.facts(&run).is_none()).await;
    assert!(
        h.runtime.observer_counts(&s).is_some(),
        "the session stays while its detach is written"
    );

    gate.notify_one();
    h.journaled(&s, "observer_detached").await;
    eventually("the session is let go", || {
        h.runtime.observer_counts(&s).is_none()
    })
    .await;
}

/// Losing the last claim stops the run at once, without waiting for the
/// detach to be written. The run's ending waits for the detach instead, so
/// the stream still shows the claim going first.
#[tokio::test]
async fn losing_the_claim_stops_the_run_before_the_detach_is_written() {
    let journal_gate = Arc::new(Notify::new());
    let h = harness_gating(
        TestPreparer::new(
            &["a"],
            vec![Prepared::Fake(vec![Step::Wait(Arc::new(Notify::new()))])],
        ),
        DEFAULT_RETENTION,
        &journal_gate,
        observer_detached,
    );
    let s = session("s1");

    let claim = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    h.context(run).await;
    h.journaled(&s, "session_info").await;
    let record = h.runtime.shared.run(&run).unwrap();

    drop(claim);
    assert_eq!(
        record.stopped(),
        Some((RunCancelReason::Unclaimed, None)),
        "the run is stopped as the claim goes"
    );
    eventually("the run's stream ends", || !record.stoppable()).await;
    assert!(
        !kinds(&h.journal(&s).await).contains(&"cancelled"),
        "the run's ending waits for the detach"
    );

    journal_gate.notify_one();
    assert_eq!(h.ended(run).await.status, RunStatus::Cancelled);
    let events = h.journal(&s).await;
    let tail: Vec<_> = kinds(&events).into_iter().rev().take(3).collect();
    assert_eq!(tail, ["cancelled", "claims_exhausted", "observer_detached"]);
}

/// A detach stalled on its write does not keep the session's journal alive
/// past a drain, so the session's other subscriptions still end.
#[tokio::test]
async fn a_stalled_detach_does_not_outlive_a_drain() {
    let gate = Arc::new(Notify::new());
    let h = harness_gating(
        TestPreparer::new(&["a"], vec![]),
        DEFAULT_RETENTION,
        &gate,
        observer_detached,
    );
    let s = session("s1");

    let leaving = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    let mut staying = h.runtime.attach(&s, Attach::collecting()).await.unwrap();
    drop(leaving);
    h.runtime.drain(Duration::from_millis(10)).await;

    let ended =
        tokio::time::timeout(PATIENCE, async { while staying.next().await.is_some() {} }).await;
    assert!(ended.is_ok(), "the subscription ends after the drain");
}
