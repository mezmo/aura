//! Lane tests: which lane a conversation's start lands on, why a lane is
//! busy, starting with an observer attached, and what a run on a lane shows
//! its agent.

use aura_events::run::{Liveness, LivenessPolicy};
use futures::StreamExt;

use super::observe::{harness_gating, observer_attached};
use super::*;
use crate::conversation::{LANES_PER_CONVERSATION, Lane, LaneNumber};
use crate::journal::JournalItem;

fn chat(name: &str) -> SessionId {
    SessionId::new(name).unwrap()
}

fn conversation(chat_id: &str, agent: &str) -> Conversation {
    Conversation::new(chat(chat_id), agent)
}

fn lane(chat_id: &str, agent: &str, number: u8) -> Lane {
    Lane::new(
        conversation(chat_id, agent),
        LaneNumber::new(number).unwrap(),
    )
}

fn on_chat(chat_id: &str, prompt: &str) -> StartRun {
    StartRun {
        target: RunTarget::Conversation {
            chat: chat(chat_id),
        },
        ..StartRun::new(prompt)
    }
}

/// A fake agent that runs until `gate` is notified or the run is stopped.
fn waiting(gate: &Arc<Notify>) -> Prepared {
    Prepared::Fake(vec![Step::Wait(Arc::clone(gate))])
}

impl Harness {
    /// The number of the lane `run` is on.
    fn lane_of(&self, run: RunId) -> u8 {
        let facts = self.runtime.facts(&run).expect("the runtime holds the run");
        facts.lane().expect("the run is on a lane").number().get()
    }

    async fn running(&self, run: RunId) {
        eventually("the run is running", || {
            self.runtime
                .facts(&run)
                .is_some_and(|facts| facts.status == RunStatus::Running)
        })
        .await;
    }
}

/// Checks that the run's lane belongs to the agent that ran it.
fn assert_lane_is_the_agents(facts: &RunFacts) {
    let lane = facts.lane().expect("the run is on a lane");
    assert_eq!(lane.conversation().agent(), facts.agent);
}

/// Checks the stream of a run that a failed claimed start stopped: no
/// observer events, no lost claim, no `Started`, and a `Cancelled` ending
/// that says why.
fn assert_stopped_by_a_failed_start(events: &[Arc<SessionEvent>]) {
    let kinds = kinds(events);
    assert!(!kinds.contains(&"started"), "{kinds:?}");
    assert!(!kinds.contains(&"observer_attached"), "{kinds:?}");
    assert!(!kinds.contains(&"observer_detached"), "{kinds:?}");
    assert!(!kinds.contains(&"claims_exhausted"), "{kinds:?}");
    match last_lifecycle(events) {
        LifecycleEvent::Cancelled {
            reason: RunCancelReason::External,
            message,
            ..
        } => assert_eq!(
            message.as_deref(),
            Some("the start failed before its observer was attached")
        ),
        other => panic!("not cancelled by the failed start: {other:?}"),
    }
}

/// The first start on a conversation lands on its home lane. That lane's
/// session id is built from the chat id, and the chat id itself is not a
/// session.
#[tokio::test]
async fn a_conversations_first_start_lands_on_its_home_lane() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Scripted(vec![ScriptedTurn::text("hello")])],
    ));

    let run = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let facts = h.ended(run).await;

    let home = Lane::home(conversation("c1", "a"));
    assert_eq!(facts.status, RunStatus::Finished);
    assert_eq!(facts.landing, Landing::Lane(home.clone()));
    assert_eq!(facts.session_id().as_str(), "c1:a:0");
    assert_lane_is_the_agents(&facts);
    assert_eq!(kinds(&h.journal(&home.session_id()).await)[0], "started");
    assert_eq!(h.runtime.session_state(&chat("c1")), None);
    assert!(h.journal(&chat("c1")).await.is_empty());
}

/// A second start while the first is still running takes lane 1, and both
/// runs finish. Listing by conversation returns both, in start order.
#[tokio::test]
async fn an_overlapping_start_takes_the_next_lane_and_both_run() {
    let (one, two) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let h = harness(TestPreparer::new(
        &["a"],
        vec![waiting(&one), waiting(&two)],
    ));

    let first = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let second = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    assert_eq!(h.lane_of(first), 0);
    assert_eq!(h.lane_of(second), 1);
    h.running(first).await;
    h.running(second).await;

    one.notify_one();
    two.notify_one();
    for run in [first, second] {
        let facts = h.ended(run).await;
        assert_eq!(facts.status, RunStatus::Finished);
        assert_lane_is_the_agents(&facts);
    }
    let listed: Vec<RunId> = h
        .runtime
        .list(&RunFilter {
            conversation: Some(conversation("c1", "a")),
            ..RunFilter::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|facts| facts.run_id)
        .collect();
    assert_eq!(listed, [first, second]);
}

/// A stopped run whose stream has not ended yet still holds its lane. The
/// next start goes to the next lane instead of waiting for it.
#[tokio::test]
async fn a_lane_whose_stopped_run_has_not_ended_is_skipped() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Fake(vec![Step::Stall]),
            Prepared::Scripted(vec![ScriptedTurn::text("hello")]),
        ],
    ));

    let stalled = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    h.running(stalled).await;
    assert!(h.runtime.cancel(&stalled, RunCancelReason::External, None));

    let next = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    assert_eq!(h.lane_of(next), 1);
    assert_eq!(h.ended(next).await.status, RunStatus::Finished);
}

/// A lane whose run has ended is free even while the runtime still keeps
/// that run, so the next start takes the same lane and continues its
/// sequence numbers.
#[tokio::test]
async fn a_lane_holding_only_an_ended_run_is_taken_again() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Scripted(vec![ScriptedTurn::text("one")]),
            Prepared::Scripted(vec![ScriptedTurn::text("two")]),
        ],
    ));

    let first = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let first = h.ended(first).await;
    assert!(
        h.runtime.facts(&first.run_id).is_some(),
        "the runtime still keeps the ended run"
    );
    let second = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let second = h.ended(second).await;

    assert_eq!(second.landing, first.landing);
    assert_eq!(second.first_seq, first.latest_seq.map(SequenceNumber::next));
}

/// A lane with a claimant and no run is busy, so the start goes to the next
/// lane. A collecting observer does not make a lane busy.
#[tokio::test]
async fn a_lane_with_a_claimant_is_skipped() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Scripted(vec![ScriptedTurn::text("one")]),
            Prepared::Scripted(vec![ScriptedTurn::text("two")]),
        ],
    ));
    let home = lane("c1", "a", 0).session_id();

    let claim = h.runtime.attach(&home, Attach::claiming()).await.unwrap();
    let run = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    assert_eq!(h.lane_of(run), 1);
    h.ended(run).await;
    claim.detach().await;

    let collector = h.runtime.attach(&home, Attach::collecting()).await.unwrap();
    let run = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    assert_eq!(h.lane_of(run), 0);
    h.ended(run).await;
    collector.detach().await;
}

/// When every lane is busy the start is refused, with the reason for each
/// lane: a running run, a claimant, or both. A start with an observer is
/// refused the same way. Another chat id is not affected.
#[tokio::test]
async fn a_start_on_a_conversation_with_every_lane_busy_is_refused() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            waiting(&gate),
            waiting(&gate),
            waiting(&gate),
            Prepared::Scripted(vec![ScriptedTurn::text("elsewhere")]),
        ],
    ));

    let held = h
        .runtime
        .start_attached(on_chat("c1", "q"), Attach::claiming())
        .await
        .unwrap();
    let claim = h
        .runtime
        .attach(&lane("c1", "a", 1).session_id(), Attach::claiming())
        .await
        .unwrap();
    let third = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let fourth = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    assert_eq!(h.lane_of(held.run), 0);
    assert_eq!(h.lane_of(third), 2);
    assert_eq!(h.lane_of(fourth), 3);

    let expected = [
        BusyLane::RunningAndClaimed {
            run: held.run,
            by: held.subscription.observer().id.clone(),
        },
        BusyLane::Claimed {
            by: claim.observer().id.clone(),
        },
        BusyLane::Running { run: third },
        BusyLane::Running { run: fourth },
    ];
    match h.runtime.start(on_chat("c1", "q")).await {
        Err(StartError::ConversationBusy {
            conversation: busy,
            lanes,
        }) => {
            assert_eq!(busy, conversation("c1", "a"));
            assert_eq!(*lanes, expected);
            let refusal = StartError::ConversationBusy {
                conversation: busy,
                lanes,
            }
            .to_string();
            assert_eq!(
                refusal,
                format!(
                    "every lane of chat `c1` with agent `a` is busy: \
                     lane 0 running `{}`, claimed by observer `{}`; \
                     lane 1 claimed by observer `{}`; \
                     lane 2 running `{third}`; lane 3 running `{fourth}`",
                    held.run,
                    held.subscription.observer().id,
                    claim.observer().id,
                )
            );
        }
        other => panic!("not refused as busy: {other:?}"),
    }
    match h
        .runtime
        .start_attached(on_chat("c1", "q"), Attach::claiming())
        .await
    {
        Err(StartError::ConversationBusy { lanes, .. }) => assert_eq!(*lanes, expected),
        other => panic!("not refused as busy: {other:?}"),
    }
    assert_eq!(
        h.runtime
            .observer_counts(&lane("c1", "a", 0).session_id())
            .map(|counts| counts.claiming),
        Some(1),
        "the refused start left no observer behind"
    );

    let elsewhere = h.runtime.start(on_chat("c2", "q")).await.unwrap();
    assert_eq!(h.ended(elsewhere).await.status, RunStatus::Finished);
    assert_eq!(LANES_PER_CONVERSATION, expected.len());
    h.runtime.drain(PATIENCE).await;
}

/// Two agents under one chat id are two conversations, and each gets its
/// own home lane.
#[tokio::test]
async fn two_agents_under_one_chat_id_each_take_their_own_home_lane() {
    let (one, two) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let h = harness(TestPreparer::new(
        &["a", "b"],
        vec![waiting(&one), waiting(&two)],
    ));

    let a = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let b = h
        .runtime
        .start(StartRun {
            agent: Some("b".to_owned()),
            ..on_chat("c1", "q")
        })
        .await
        .unwrap();

    assert_eq!(
        h.runtime.facts(&a).unwrap().landing,
        Landing::Lane(Lane::home(conversation("c1", "a")))
    );
    assert_eq!(
        h.runtime.facts(&b).unwrap().landing,
        Landing::Lane(Lane::home(conversation("c1", "b")))
    );
    one.notify_one();
    two.notify_one();
    for run in [a, b] {
        let facts = h.ended(run).await;
        assert_eq!(facts.status, RunStatus::Finished);
        assert_lane_is_the_agents(&facts);
    }
}

/// All lanes of a conversation use one skill log, keyed by the chat id. A
/// run on lane 1 rehydrates what lane 0's run recorded. The agent is
/// prepared with the chat id, not the lane's session id.
#[tokio::test]
async fn a_run_on_lane_1_rehydrates_skills_recorded_by_lane_0s_run() {
    use crate::session_store::{
        SKILL_INVOCATION_RECORD_VERSION, SkillInvocation, SkillInvocationRecord,
    };

    let skills = tempfile::TempDir::new().unwrap();
    let gate = Arc::new(Notify::new());
    let with_skill = |steps| {
        Prepared::Described(Fake {
            skills: vec![skill(skills.path(), "deploy")],
            ..Fake::single(steps)
        })
    };
    let mut preparer = TestPreparer::new(
        &["a"],
        vec![
            with_skill(vec![Step::Wait(Arc::clone(&gate))]),
            with_skill(Vec::new()),
        ],
    );
    preparer.configs[0]
        .agent
        .skills
        .local
        .push(aura_config::LocalSkillSource {
            source: skills.path().to_path_buf(),
        });
    let h = harness(preparer);

    let first = h.runtime.start(on_chat("c1", "deploy it")).await.unwrap();
    h.running(first).await;
    let (prepared_with, log) = h.preparer.requests.lock().unwrap()[0].clone();
    assert_eq!(prepared_with, chat("c1"));
    let log = log.expect("an agent with skills gets a skill recorder");
    assert_eq!(log, SkillLogKey::new(chat("c1"), "a"));
    // Record a skill load in the log lane 0's run writes to.
    h.runtime
        .shared
        .skill_store
        .record(
            &log,
            SkillInvocationRecord {
                version: SKILL_INVOCATION_RECORD_VERSION,
                invocation: SkillInvocation::LoadSkill {
                    name: "deploy".to_owned(),
                },
                tool_call_id: "call_skill".to_owned(),
                anchor: 1,
                seq: 0,
                invoked_at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();

    let second = h
        .runtime
        .start(StartRun {
            history: vec![Message::user("deploy it"), Message::assistant("deployed")],
            ..on_chat("c1", "again")
        })
        .await
        .unwrap();
    assert_eq!(h.lane_of(second), 1);
    h.ended(second).await;

    assert_eq!(
        h.preparer.requests.lock().unwrap()[1],
        (chat("c1"), Some(log))
    );
    let events = h.journal(&lane("c1", "a", 1).session_id()).await;
    assert_eq!(
        kinds(&events)[..3],
        ["started", "session_info", "skills_rehydrated"]
    );
    assert!(matches!(
        &agent_event(&events[2]).payload,
        AgentEventPayload::SkillsRehydrated { skills } if skills == &["deploy"]
    ));
    gate.notify_one();
    h.ended(first).await;
}

/// After the retention window the runtime lets a lane's session go. The
/// next overlapping start opens the lane again, and its sequence numbers
/// carry on from before.
#[tokio::test(start_paused = true)]
async fn a_lane_let_go_after_retention_is_reopened_with_its_sequence_continued() {
    let retention = Duration::from_secs(60);
    let (one, three) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let h = harness_retaining(
        TestPreparer::new(
            &["a"],
            vec![
                waiting(&one),
                Prepared::Scripted(vec![ScriptedTurn::text("two")]),
                waiting(&three),
                Prepared::Scripted(vec![ScriptedTurn::text("four")]),
            ],
        ),
        retention,
    );
    let lane_1 = lane("c1", "a", 1).session_id();

    let first = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let second = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let second = h.ended(second).await;
    assert_eq!(second.session_id(), lane_1);
    one.notify_one();
    h.ended(first).await;

    tokio::time::sleep(retention + Duration::from_secs(1)).await;
    eventually("both lanes are let go", || {
        h.runtime.session_state(&lane_1).is_none()
            && h.runtime
                .session_state(&lane("c1", "a", 0).session_id())
                .is_none()
    })
    .await;

    let third = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let fourth = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    assert_eq!(h.lane_of(third), 0);
    let fourth = h.ended(fourth).await;
    assert_eq!(fourth.session_id(), lane_1);
    assert_eq!(
        fourth.first_seq,
        second.latest_seq.map(SequenceNumber::next),
        "lane 1 carries on its sequence"
    );
    three.notify_one();
    h.ended(third).await;
}

/// When another writer has fenced the journal of an idle lane, the next
/// start opens that lane's session again and carries on after the other
/// writer's events.
#[tokio::test]
async fn a_fenced_idle_lane_is_reopened() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            Prepared::Scripted(vec![ScriptedTurn::text("one")]),
            Prepared::Scripted(vec![ScriptedTurn::text("two")]),
            Prepared::Scripted(vec![ScriptedTurn::text("three")]),
        ],
    ));
    let home = lane("c1", "a", 0).session_id();
    let first = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    h.ended(first).await;

    let elsewhere = h
        .journals
        .append(
            &home,
            h.journals.latest(&home).await.unwrap(),
            None,
            aura_events::Timestamp::now(),
            SessionEventPayload::Agent(AgentEvent::single_agent(AgentEventPayload::TextDelta {
                content: "from another writer".to_owned(),
            })),
        )
        .await
        .unwrap();

    let second = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let second = h.ended(second).await;
    assert_eq!(
        second.status,
        RunStatus::Failed,
        "the old journal is fenced"
    );
    assert_eq!(second.session_id(), home);

    let third = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let third = h.ended(third).await;
    assert_eq!(third.status, RunStatus::Finished);
    assert_eq!(third.session_id(), home);
    assert_eq!(third.first_seq, Some(elsewhere.seq.next()));
}

/// A drain stops the runs on every lane, refuses later starts, and lets
/// `idle` resolve.
#[tokio::test]
async fn drain_stops_lane_runs_and_idle_resolves() {
    let never = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![waiting(&never), waiting(&never)],
    ));
    let first = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    let second = h.runtime.start(on_chat("c1", "q")).await.unwrap();
    h.running(first).await;
    h.running(second).await;

    h.runtime.drain(PATIENCE).await;
    for run in [first, second] {
        let facts = stored(&h, run).await;
        assert_eq!(facts.status, RunStatus::Cancelled);
    }
    tokio::time::timeout(PATIENCE, h.runtime.idle())
        .await
        .expect("idle resolves");
    assert!(matches!(
        h.runtime.start(on_chat("c1", "q")).await,
        Err(StartError::ShuttingDown)
    ));
}

// ---------------------------------------------------------------------------
// Starting with an observer attached
// ---------------------------------------------------------------------------

/// The next event on a subscription. Fails the test if none arrives in time.
async fn next_event(sub: &mut Subscription) -> Arc<SessionEvent> {
    match tokio::time::timeout(PATIENCE, sub.next())
        .await
        .expect("an item arrives in time")
        .expect("the subscription is open")
    {
        JournalItem::Event(event) => event,
        other => panic!("not an event: {other:?}"),
    }
}

/// A start with an observer returns where the run landed and a
/// subscription that starts at the observer's own attach event. The attach
/// event has no run id, because the observer was added before the run
/// existed, and the run's `Started` comes after it.
#[tokio::test]
async fn a_claimed_start_attaches_to_its_lane_before_the_run_starts() {
    let h = harness(TestPreparer::new(
        &["a"],
        vec![Prepared::Scripted(vec![ScriptedTurn::text("hello")])],
    ));

    let Admitted {
        run,
        landing,
        mut subscription,
    } = h
        .runtime
        .start_attached(on_chat("c1", "q"), Attach::claiming().present())
        .await
        .unwrap();

    assert_eq!(landing, Landing::Lane(Lane::home(conversation("c1", "a"))));
    assert_eq!(landing, h.runtime.facts(&run).unwrap().landing);
    let mut seen = Vec::new();
    loop {
        let event = next_event(&mut subscription).await;
        let ended = event.ends_run();
        seen.push(event);
        if ended {
            break;
        }
    }
    assert_eq!(kinds(&seen)[..2], ["observer_attached", "started"]);
    assert_eq!(seen[0].run_id, None);
    assert_eq!(seen[1].run_id, Some(run));
    assert_eq!(kinds(&seen).last(), Some(&"finished"));
    assert_eq!(
        h.runtime.observer_counts(&landing.session_id()),
        Some(ObserverCounts {
            collecting: 0,
            claiming: 1,
            present: 1,
        })
    );
    subscription.detach().await;
}

/// A start with a claiming observer on a named session lands on that
/// session. It is refused while the session has a claimant or a running
/// run, and a refused start leaves no observer behind and starts nothing.
#[tokio::test]
async fn a_claimed_start_on_a_busy_session_is_refused() {
    let gate = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(
        &["a"],
        vec![
            waiting(&gate),
            Prepared::Scripted(vec![ScriptedTurn::text("hello")]),
        ],
    ));
    let s = chat("s1");

    let claim = h.runtime.attach(&s, Attach::claiming()).await.unwrap();
    match h
        .runtime
        .start_attached(on_session("s1", "q"), Attach::claiming())
        .await
    {
        Err(StartError::AlreadyClaimed { by }) => assert_eq!(by, claim.observer().id),
        other => panic!("not refused as claimed: {other:?}"),
    }
    assert_eq!(h.runtime.session_state(&s), Some(AgentState::Done));
    claim.detach().await;

    let run = h.runtime.start(on_session("s1", "q")).await.unwrap();
    match h
        .runtime
        .start_attached(on_session("s1", "q"), Attach::claiming())
        .await
    {
        Err(StartError::RunInProgress(live)) => assert_eq!(live, run),
        other => panic!("not refused as running: {other:?}"),
    }
    assert_eq!(h.runtime.observer_counts(&s).unwrap().claiming, 0);
    gate.notify_one();
    h.ended(run).await;

    let admitted = h
        .runtime
        .start_attached(on_session("s1", "q"), Attach::claiming())
        .await
        .unwrap();
    assert_eq!(admitted.landing, Landing::Session(s.clone()));
    assert_eq!(h.ended(admitted.run).await.status, RunStatus::Finished);
    assert_eq!(*h.preparer.prepared.lock().unwrap(), ["a", "a"]);
    admitted.subscription.detach().await;
}

/// Dropping the subscription from a claimed start detaches the observer.
/// Under the Cancel policy the run then stops as `Unclaimed`, the same as
/// on a plain session.
#[tokio::test]
async fn dropping_a_claimed_starts_subscription_stops_its_run() {
    let never = Arc::new(Notify::new());
    let h = harness(TestPreparer::new(&["a"], vec![waiting(&never)]));

    let admitted = h
        .runtime
        .start_attached(on_chat("c1", "q"), Attach::claiming())
        .await
        .unwrap();
    let run = admitted.run;
    h.running(run).await;
    drop(admitted.subscription);

    let facts = h.ended(run).await;
    assert_eq!(facts.status, RunStatus::Cancelled);
    let events = h.journal(&facts.session_id()).await;
    assert!(kinds(&events).contains(&"claims_exhausted"));
    match last_lifecycle(&events) {
        LifecycleEvent::Cancelled {
            reason: RunCancelReason::Unclaimed,
            ..
        } => {}
        other => panic!("not cancelled as unclaimed: {other:?}"),
    }
}

/// A journal store that refuses every `ObserverAttached` append.
struct RefusingAttaches {
    inner: Arc<InMemoryJournalStore>,
}

#[async_trait]
impl JournalStore for RefusingAttaches {
    async fn append(
        &self,
        session: &SessionId,
        expected: Option<SequenceNumber>,
        run_id: Option<RunId>,
        at: aura_events::Timestamp,
        payload: SessionEventPayload,
    ) -> Result<Arc<SessionEvent>, SessionStoreError> {
        if observer_attached(&payload) {
            return Err(SessionStoreError::Request {
                reason: "refused".to_owned(),
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
        self.inner.read(session, from, to, limit).await
    }

    async fn latest(
        &self,
        session: &SessionId,
    ) -> Result<Option<SequenceNumber>, SessionStoreError> {
        self.inner.latest(session).await
    }
}

/// The run a claimed start made, which is the only run the harness holds.
async fn only_run(h: &Harness) -> RunId {
    let runs = tokio::time::timeout(PATIENCE, async {
        loop {
            let runs = h.runs.list(&RunFilter::default()).await.unwrap();
            if !runs.is_empty() {
                return runs;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the store lists the run");
    assert_eq!(runs.len(), 1);
    runs[0].run_id
}

/// When the journal refuses the attach event, the start fails with the
/// store's error and the run it started is stopped, under the Continue
/// policy as well as Cancel: the caller never got the run's id, so nobody
/// would read it. The observer is removed.
#[tokio::test]
async fn a_claimed_start_whose_attach_is_refused_stops_its_run() {
    for policy in [LivenessPolicy::Cancel, LivenessPolicy::Continue] {
        let never = Arc::new(Notify::new());
        let runs = Arc::new(InMemoryRunStore::new());
        let journals = Arc::new(InMemoryJournalStore::new());
        let h = harness_with(
            TestPreparer::new(&["a"], vec![waiting(&never)]),
            DEFAULT_RETENTION,
            Arc::clone(&runs) as Arc<dyn RunStore>,
            runs,
            Arc::new(RefusingAttaches {
                inner: Arc::clone(&journals),
            }),
            journals,
        );

        let refused = h
            .runtime
            .start_attached(
                StartRun {
                    liveness: Liveness {
                        policy,
                        grace: Duration::ZERO,
                    },
                    ..on_chat("c1", "q")
                },
                Attach::claiming(),
            )
            .await;
        assert!(
            matches!(
                refused,
                Err(StartError::Store(SessionStoreError::Request { .. }))
            ),
            "{policy:?}: {refused:?}"
        );

        let run = only_run(&h).await;
        let facts = stored(&h, run).await;
        assert_eq!(facts.status, RunStatus::Cancelled, "{policy:?}");
        assert_stopped_by_a_failed_start(&h.journal(&facts.session_id()).await);
        assert_eq!(
            h.runtime
                .observer_counts(&facts.session_id())
                .map_or(0, |counts| counts.claiming),
            0,
            "{policy:?}"
        );
    }
}

/// A caller that drops a claimed start while its attach event is being
/// written leaves no claim behind, and the run started for it is stopped.
#[tokio::test]
async fn a_claimed_start_dropped_while_attaching_stops_its_run() {
    let gate = Arc::new(Notify::new());
    let never = Arc::new(Notify::new());
    let h = harness_gating(
        TestPreparer::new(&["a"], vec![waiting(&never)]),
        DEFAULT_RETENTION,
        &gate,
        observer_attached,
    );

    let dropped = tokio::time::timeout(
        Duration::from_millis(20),
        h.runtime
            .start_attached(on_chat("c1", "q"), Attach::claiming()),
    )
    .await;
    assert!(dropped.is_err(), "the attach was still being written");

    let run = only_run(&h).await;
    let facts = stored(&h, run).await;
    assert_eq!(facts.status, RunStatus::Cancelled);
    assert_stopped_by_a_failed_start(&h.journal(&facts.session_id()).await);
}
