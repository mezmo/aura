//! Sessions and runs as callers outside the runtime read them. A snapshot is
//! a copy taken when it is asked for. It is never stored, and it does not
//! change afterwards.

use aura_events::{RunId, SequenceNumber, SessionId};
use serde::Deserialize;

use crate::conversation::RunSession;
use crate::journal::Cursor;
use crate::session_store::{RunDetails, RunFacts, RunStatus};

use super::observe::ObserverCounts;
use super::record::RunRecord;
use super::state::{AgentState, AgentStateKind, BlockReason};

/// How many rows one page of a listing holds: at least 1, and at most
/// [`PageLimit::MAX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "u32")]
pub struct PageLimit(u16);

impl PageLimit {
    pub const DEFAULT: Self = Self(100);
    pub const MAX: Self = Self(1000);

    /// The number of rows.
    pub fn get(self) -> usize {
        usize::from(self.0)
    }
}

impl Default for PageLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl From<u32> for PageLimit {
    /// `rows`, raised to 1 or lowered to [`PageLimit::MAX`] when it is
    /// outside them.
    fn from(rows: u32) -> Self {
        let rows = rows.clamp(1, u32::from(Self::MAX.0));
        Self(u16::try_from(rows).unwrap_or(Self::MAX.0))
    }
}

/// One session as a caller sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSnapshot {
    run_session: RunSession,
    agent: String,
    state: AgentState,
    /// The session's newest run.
    newest_run: RunId,
    /// The newest run that ended among the session's runs older than its
    /// newest.
    previous_ended: Option<RunId>,
    latest_event: Option<SequenceNumber>,
    observers: ObserverCounts,
}

impl SessionSnapshot {
    /// The snapshot of a session whose newest run is `newest`, as the
    /// runtime sees that run now. The run session and the agent are that run's.
    ///
    /// The state is read off the run. A run that is preparing or running
    /// reads `Stopping` when `stopped` says the run was stopped, and
    /// otherwise `Preparing`, `Blocked` when it waits on approvals, or
    /// `Running`. A run that has ended reads `Parked` when it parked and
    /// `Done` otherwise, whatever `stopped` says.
    ///
    /// `previous_ended` is the newest run that ended among the session's
    /// runs older than `newest`. One that names `newest` itself is dropped.
    pub(super) fn of(
        newest: RunFacts,
        stopped: bool,
        previous_ended: Option<RunId>,
        latest_event: Option<SequenceNumber>,
        observers: ObserverCounts,
    ) -> Self {
        let RunFacts {
            session: run_session,
            run:
                RunDetails {
                    run_id,
                    agent,
                    status,
                    pending_approvals,
                    ..
                },
        } = newest;
        let state = match status {
            RunStatus::Preparing | RunStatus::Running if stopped => AgentState::Stopping,
            RunStatus::Preparing => AgentState::Preparing,
            RunStatus::Running if !pending_approvals.is_empty() => AgentState::Blocked {
                on: BlockReason::Approval {
                    decisions: pending_approvals,
                },
            },
            RunStatus::Running => AgentState::Running,
            RunStatus::Parked => AgentState::Parked,
            RunStatus::Finished | RunStatus::Failed | RunStatus::Cancelled => AgentState::Done,
        };
        Self {
            run_session,
            agent,
            state,
            newest_run: run_id,
            previous_ended: previous_ended.filter(|previous| *previous != run_id),
            latest_event,
            observers,
        }
    }

    /// The session's id. For a lane, this is the lane's session id, not the
    /// chat id.
    pub fn id(&self) -> SessionId {
        self.run_session.id()
    }

    /// The `RunSession` of the session's newest run.
    pub fn run_session(&self) -> &RunSession {
        &self.run_session
    }

    /// The agent of the session's newest run, by its `Config::agent_id`.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// The run that is live on the session: its newest run, while the state
    /// is one with a live run.
    pub fn current_run(&self) -> Option<RunId> {
        self.state.has_live_run().then_some(self.newest_run)
    }

    /// The session's most recent ended run: its newest run once that has
    /// ended, and the one that ended before it while it is live.
    pub fn last_run(&self) -> Option<RunId> {
        if self.state.has_live_run() {
            self.previous_ended
        } else {
            Some(self.newest_run)
        }
    }

    /// The newest sequence number in the session's stream.
    pub fn latest_event(&self) -> Option<SequenceNumber> {
        self.latest_event
    }

    /// The observers attached to the session in this process.
    pub fn observers(&self) -> ObserverCounts {
        self.observers
    }
}

/// Where a snapshot's run was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// The runtime's record of the run.
    Record {
        /// The sequence number of the newest event in the session's stream
        /// when the run was accepted.
        preceding_event: Option<SequenceNumber>,
    },
    /// The run store.
    Store,
}

/// One run as a caller sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSnapshot {
    facts: RunFacts,
    observers: ObserverCounts,
    source: Source,
}

impl RunSnapshot {
    /// The snapshot of a run the runtime holds, with the facts its record
    /// has now.
    pub(super) fn from_record(record: &RunRecord, observers: ObserverCounts) -> Self {
        Self {
            facts: record.facts(),
            observers,
            source: Source::Record {
                preceding_event: record.preceding_event(),
            },
        }
    }

    /// The snapshot of a run read from the run store.
    pub(super) fn from_store(facts: RunFacts, observers: ObserverCounts) -> Self {
        Self {
            facts,
            observers,
            source: Source::Store,
        }
    }

    pub fn facts(&self) -> &RunFacts {
        &self.facts
    }

    /// The observers attached to the run's session in this process.
    pub fn observers(&self) -> ObserverCounts {
        self.observers
    }

    /// The stretch of the session's stream that holds the run's events a
    /// reader has not seen. `last_seen` is the sequence number of the last
    /// event the reader has seen on the session's stream, or `None` for a
    /// reader that has seen nothing. It can be past the run's own newest
    /// event, at an observer's event after it, and is read as it is: a
    /// number past the stream's newest event is the caller's to refuse,
    /// since the run's later events would come at or before it. Returns
    /// `None` when nothing of the run is left to read.
    ///
    /// The stretch never starts before the run, so a `last_seen` from before
    /// the run reads the run from its beginning and nothing of the runs
    /// before it. Where a run begins:
    ///
    /// - A run that started begins at its `Started`.
    /// - A run the runtime holds that has not started begins just after its
    ///   preceding event, or at the start of the stream when the run has
    ///   none. Its `Started`, or its ending if it never starts, comes after
    ///   that.
    /// - A run from the store that never started begins at its newest
    ///   event. A run can end without starting, and its ending is then the
    ///   one event it has.
    ///
    /// Where the stretch ends:
    ///
    /// - A run the runtime holds that has not ended has no last event yet,
    ///   so its stretch has no `to`, and a reader follows it to the run's
    ///   terminal event.
    /// - Any other run's stretch ends at its newest event. That is a run
    ///   that has ended, and also a run from the store that has not: this
    ///   process does not hold it, so it will append nothing more for it.
    ///   Nothing is left of such a run for a reader that has seen that
    ///   event, or when the run has no event on the stream at all.
    pub fn events_after(&self, last_seen: Option<SequenceNumber>) -> Option<EventRange> {
        let run = &self.facts.run;
        let begins = match (run.first_event, self.source) {
            (Some(started), _) => Cursor::From(started),
            (None, Source::Record { preceding_event }) => match preceding_event {
                Some(preceding) => Cursor::After(preceding),
                None => Cursor::Start,
            },
            (None, Source::Store) => Cursor::From(run.latest_event?),
        };
        let to = match self.source {
            Source::Record { .. } if !run.status.is_terminal() => None,
            Source::Record { .. } | Source::Store => Some(run.latest_event?),
        };
        if let (Some(seen), Some(to)) = (last_seen, to)
            && seen >= to
        {
            return None;
        }
        let first = match begins {
            Cursor::From(first) => first,
            Cursor::After(preceding) => preceding.next(),
            Cursor::Start | Cursor::Live => SequenceNumber::FIRST,
        };
        let from = match last_seen {
            Some(seen) if seen >= first => Cursor::After(seen),
            _ => begins,
        };
        Some(EventRange { from, to })
    }
}

/// A stretch of a session's stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventRange {
    /// Where the stretch starts.
    pub from: Cursor,
    /// The sequence number of the stretch's last event, which is part of
    /// the stretch.
    pub to: Option<SequenceNumber>,
}

/// Which sessions a listing returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionQuery {
    pub state: Option<AgentStateKind>,
    /// The agent of the session's newest run, by its `Config::agent_id`.
    pub agent: Option<String>,
    /// The chat id of the conversation the session is a lane of.
    pub chat: Option<SessionId>,
    pub limit: PageLimit,
    /// The last session id of the previous page.
    pub after: Option<SessionId>,
}

/// Which runs a listing returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunQuery {
    pub session: Option<SessionId>,
    /// The chat id of the conversation the run's session is a lane of.
    pub chat: Option<SessionId>,
    pub status: Option<RunStatus>,
    /// Whether the run is waiting on an approval.
    pub pending_approval: Option<bool>,
    pub limit: PageLimit,
    /// The last run id of the previous page.
    pub after: Option<RunId>,
}

/// One page of a session listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPage {
    sessions: Vec<SessionSnapshot>,
    next: Option<SessionId>,
}

impl SessionPage {
    pub(super) fn new(sessions: Vec<SessionSnapshot>, next: Option<SessionId>) -> Self {
        Self { sessions, next }
    }

    pub fn sessions(&self) -> &[SessionSnapshot] {
        &self.sessions
    }

    /// The session id the next page starts after.
    pub fn next(&self) -> Option<&SessionId> {
        self.next.as_ref()
    }
}

/// One page of a run listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPage {
    runs: Vec<RunSnapshot>,
    next: Option<RunId>,
}

impl RunPage {
    pub(super) fn new(runs: Vec<RunSnapshot>, next: Option<RunId>) -> Self {
        Self { runs, next }
    }

    pub fn runs(&self) -> &[RunSnapshot] {
        &self.runs
    }

    /// The run id the next page starts after.
    pub fn next(&self) -> Option<RunId> {
        self.next
    }
}

/// What a request to stop a run came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The run is stopping, with its snapshot.
    Stopping(RunSnapshot),
    /// The run had ended before the stop, with its snapshot.
    AlreadyEnded(RunSnapshot),
    /// No run with the id is here.
    NotHere,
}

#[cfg(test)]
mod tests {
    use aura_events::run::Liveness;
    use aura_events::{Timestamp, TokenUsage};

    use super::*;
    use crate::conversation::{Conversation, Lane, LaneNumber};
    use crate::hitl::DecisionId;
    use crate::runtime::state::BlockReason;
    use crate::session_store::RunDetails;

    fn seq(n: u64) -> SequenceNumber {
        SequenceNumber::try_from(n).unwrap()
    }

    fn run_id(n: u128) -> RunId {
        RunId::try_from(uuid::Uuid::from_u128(n)).expect("a non-zero UUID is a run id")
    }

    /// Run `n` of the plain session `s1`, in `status`, with no events.
    fn run(n: u128, status: RunStatus) -> RunFacts {
        RunFacts {
            session: RunSession::Plain(SessionId::new("s1").unwrap()),
            run: RunDetails {
                run_id: run_id(n),
                agent: "a".to_owned(),
                status,
                liveness: Liveness::default(),
                started_at: Timestamp::from_unix_millis(1),
                ended_at: None,
                usage: TokenUsage::default(),
                first_event: None,
                latest_event: None,
                pending_approvals: Vec::new(),
                checkpoint: None,
                continues: None,
            },
        }
    }

    /// Run 1, in `status`, whose `Started` is `first` and whose newest
    /// event is `latest`.
    fn with_events(status: RunStatus, first: Option<u64>, latest: Option<u64>) -> RunFacts {
        let mut facts = run(1, status);
        facts.run.first_event = first.map(seq);
        facts.run.latest_event = latest.map(seq);
        facts
    }

    /// The snapshot of a run the runtime holds, accepted when the session's
    /// newest event was `preceding`.
    fn snapshot_from_record(facts: RunFacts, preceding: Option<u64>) -> RunSnapshot {
        RunSnapshot {
            facts,
            observers: ObserverCounts::default(),
            source: Source::Record {
                preceding_event: preceding.map(seq),
            },
        }
    }

    fn snapshot_from_store(facts: RunFacts) -> RunSnapshot {
        RunSnapshot {
            facts,
            observers: ObserverCounts::default(),
            source: Source::Store,
        }
    }

    fn from(from: Cursor, to: Option<u64>) -> Option<EventRange> {
        Some(EventRange {
            from,
            to: to.map(seq),
        })
    }

    /// What a reader that has seen up to each number in `last_seen` is left
    /// to read of `snapshot`.
    fn read(snapshot: &RunSnapshot, last_seen: &[Option<u64>]) -> Vec<Option<EventRange>> {
        last_seen
            .iter()
            .map(|last_seen| snapshot.events_after(last_seen.map(seq)))
            .collect()
    }

    /// A started run the runtime holds, accepted after event 5, with its
    /// `Started` at 6 and its newest event at 9. It has not ended, so its
    /// stretch has no end. A reader past its newest event, at an observer's
    /// event after it, is read from there.
    #[test]
    fn a_held_run_that_has_not_ended_is_read_until_its_ending() {
        let snapshot =
            snapshot_from_record(with_events(RunStatus::Running, Some(6), Some(9)), Some(5));

        assert_eq!(
            read(&snapshot, &[None, Some(2), Some(7), Some(9), Some(12)]),
            [
                from(Cursor::From(seq(6)), None),
                from(Cursor::From(seq(6)), None),
                from(Cursor::After(seq(7)), None),
                from(Cursor::After(seq(9)), None),
                from(Cursor::After(seq(12)), None),
            ]
        );
    }

    /// A started run that has ended, whether the runtime holds it or it is
    /// read from the store, is read through its ending and no further.
    #[test]
    fn a_started_run_that_has_ended_is_read_through_its_ending() {
        let facts = with_events(RunStatus::Finished, Some(6), Some(9));
        for snapshot in [
            snapshot_from_record(facts.clone(), Some(5)),
            snapshot_from_store(facts),
        ] {
            assert_eq!(
                read(&snapshot, &[None, Some(2), Some(7), Some(9), Some(12)]),
                [
                    from(Cursor::From(seq(6)), Some(9)),
                    from(Cursor::From(seq(6)), Some(9)),
                    from(Cursor::After(seq(7)), Some(9)),
                    None,
                    None,
                ],
                "{:?}",
                snapshot.source
            );
        }
    }

    /// A run the runtime holds that is still being prepared begins just
    /// after the event before it, or at the start of a stream that had
    /// none.
    #[test]
    fn a_held_run_still_preparing_is_read_from_just_after_its_preceding_event() {
        let after_five =
            snapshot_from_record(with_events(RunStatus::Preparing, None, None), Some(5));
        assert_eq!(
            read(&after_five, &[None, Some(2)]),
            [
                from(Cursor::After(seq(5)), None),
                from(Cursor::After(seq(5)), None),
            ]
        );

        let first_in_session =
            snapshot_from_record(with_events(RunStatus::Preparing, None, None), None);
        assert_eq!(
            read(&first_in_session, &[None, Some(3)]),
            [from(Cursor::Start, None), from(Cursor::After(seq(3)), None)],
            "a reader that saw observers' events 1 to 3 reads on after them"
        );
    }

    /// A held run with no event yet is read from a `last_seen` past its
    /// preceding event, an observer's event after it, so the reader gets
    /// nothing a second time. The run's `Started` comes after every event
    /// the reader can have seen.
    #[test]
    fn a_held_run_with_no_event_yet_is_read_after_a_last_seen_past_its_preceding_event() {
        let snapshot = snapshot_from_record(with_events(RunStatus::Preparing, None, None), Some(5));

        assert_eq!(
            read(&snapshot, &[Some(6), Some(8)]),
            [
                from(Cursor::After(seq(6)), None),
                from(Cursor::After(seq(8)), None),
            ]
        );
    }

    /// A run the runtime holds that failed before `Started` still begins
    /// just after its preceding event, and its stretch ends at its ending.
    /// Here events 6 and 7 are an observer's attach and detach, and 8 is
    /// the run's `Failed`.
    #[test]
    fn a_held_run_that_failed_before_started_is_read_from_its_preceding_event_through_its_ending() {
        let snapshot = snapshot_from_record(with_events(RunStatus::Failed, None, Some(8)), Some(5));
        assert_eq!(
            read(&snapshot, &[None, Some(2), Some(6), Some(8), Some(12)]),
            [
                from(Cursor::After(seq(5)), Some(8)),
                from(Cursor::After(seq(5)), Some(8)),
                from(Cursor::After(seq(6)), Some(8)),
                None,
                None,
            ]
        );

        let first_in_session =
            snapshot_from_record(with_events(RunStatus::Failed, None, Some(1)), None);
        assert_eq!(
            read(&first_in_session, &[None, Some(1)]),
            [from(Cursor::Start, Some(1)), None]
        );
    }

    /// A run read from the store that failed before `Started` has one event
    /// on the stream, its ending.
    #[test]
    fn a_stored_run_that_failed_before_started_is_its_ending_alone() {
        let snapshot = snapshot_from_store(with_events(RunStatus::Failed, None, Some(8)));

        assert_eq!(
            read(&snapshot, &[None, Some(2), Some(8), Some(12)]),
            [
                from(Cursor::From(seq(8)), Some(8)),
                from(Cursor::From(seq(8)), Some(8)),
                None,
                None,
            ]
        );
    }

    /// A run the store holds as not ended that this process does not hold
    /// will get nothing more from this process, so it is read through its
    /// newest event, and is nothing at all when it has no event.
    #[test]
    fn a_stored_run_that_has_not_ended_is_read_through_its_newest_event() {
        let snapshot = snapshot_from_store(with_events(RunStatus::Running, Some(6), Some(9)));
        assert_eq!(
            read(&snapshot, &[None, Some(2), Some(7), Some(9), Some(12)]),
            [
                from(Cursor::From(seq(6)), Some(9)),
                from(Cursor::From(seq(6)), Some(9)),
                from(Cursor::After(seq(7)), Some(9)),
                None,
                None,
            ]
        );

        let no_event = snapshot_from_store(with_events(RunStatus::Preparing, None, None));
        assert_eq!(read(&no_event, &[None, Some(2)]), [None, None]);
    }

    /// The snapshot `of` should build: its run session, agent and newest run
    /// read off `newest`, with the state, previous ended run, newest number
    /// and observers given.
    fn expected(
        newest: &RunFacts,
        state: AgentState,
        previous_ended: Option<RunId>,
        latest_event: Option<SequenceNumber>,
        observers: ObserverCounts,
    ) -> SessionSnapshot {
        SessionSnapshot {
            run_session: newest.session.clone(),
            agent: newest.run.agent.clone(),
            state,
            newest_run: newest.run.run_id,
            previous_ended,
            latest_event,
            observers,
        }
    }

    fn waiting_on(decision: DecisionId) -> RunFacts {
        let mut facts = run(2, RunStatus::Running);
        facts.run.pending_approvals = vec![decision];
        facts
    }

    #[test]
    fn a_live_run_reads_preparing_blocked_or_running() {
        let decision = DecisionId::generate();
        let observers = ObserverCounts {
            collecting: 2,
            claiming: 1,
            present: 1,
        };
        let on_lane = RunFacts {
            session: RunSession::Lane(Lane::new(
                Conversation::new(SessionId::new("c1").unwrap(), "a"),
                LaneNumber::new(1).unwrap(),
            )),
            ..run(2, RunStatus::Running)
        };
        for (newest, state) in [
            (run(2, RunStatus::Preparing), AgentState::Preparing),
            (
                waiting_on(decision),
                AgentState::Blocked {
                    on: BlockReason::Approval {
                        decisions: vec![decision],
                    },
                },
            ),
            (run(2, RunStatus::Running), AgentState::Running),
            (on_lane, AgentState::Running),
        ] {
            let snapshot = SessionSnapshot::of(
                newest.clone(),
                false,
                Some(run_id(1)),
                Some(seq(7)),
                observers,
            );
            assert_eq!(
                snapshot,
                expected(&newest, state, Some(run_id(1)), Some(seq(7)), observers)
            );
            assert_eq!(snapshot.current_run(), Some(run_id(2)));
            assert_eq!(snapshot.last_run(), Some(run_id(1)));
        }
    }

    #[test]
    fn a_stopped_live_run_reads_stopping() {
        for newest in [
            run(2, RunStatus::Preparing),
            run(2, RunStatus::Running),
            waiting_on(DecisionId::generate()),
        ] {
            let snapshot = SessionSnapshot::of(
                newest.clone(),
                true,
                None,
                Some(seq(7)),
                ObserverCounts::default(),
            );
            assert_eq!(
                snapshot,
                expected(
                    &newest,
                    AgentState::Stopping,
                    None,
                    Some(seq(7)),
                    ObserverCounts::default()
                )
            );
            assert_eq!(snapshot.current_run(), Some(run_id(2)));
            assert_eq!(snapshot.last_run(), None);
        }
    }

    #[test]
    fn an_ended_run_reads_parked_or_done_whatever_stopped_says() {
        for stopped in [false, true] {
            for (status, state) in [
                (RunStatus::Parked, AgentState::Parked),
                (RunStatus::Finished, AgentState::Done),
                (RunStatus::Failed, AgentState::Done),
                (RunStatus::Cancelled, AgentState::Done),
            ] {
                let newest = run(2, status);
                let snapshot = SessionSnapshot::of(
                    newest.clone(),
                    stopped,
                    Some(run_id(1)),
                    Some(seq(7)),
                    ObserverCounts::default(),
                );
                assert_eq!(
                    snapshot,
                    expected(
                        &newest,
                        state,
                        Some(run_id(1)),
                        Some(seq(7)),
                        ObserverCounts::default()
                    ),
                    "{status:?}, stopped: {stopped}"
                );
                assert_eq!(snapshot.current_run(), None);
                assert_eq!(snapshot.last_run(), Some(run_id(2)));
            }
        }
    }

    #[test]
    fn a_previous_ended_run_naming_the_newest_run_is_dropped() {
        for (newest, state, last_run) in [
            (
                run(2, RunStatus::Finished),
                AgentState::Done,
                Some(run_id(2)),
            ),
            (run(2, RunStatus::Running), AgentState::Running, None),
        ] {
            let snapshot = SessionSnapshot::of(
                newest.clone(),
                false,
                Some(run_id(2)),
                None,
                ObserverCounts::default(),
            );
            assert_eq!(
                snapshot,
                expected(&newest, state, None, None, ObserverCounts::default())
            );
            assert_eq!(snapshot.last_run(), last_run);
        }
    }

    #[test]
    fn a_page_limit_is_raised_to_one_and_lowered_to_the_most() {
        assert_eq!(PageLimit::default(), PageLimit(100));
        assert_eq!(PageLimit::DEFAULT.get(), 100);
        assert_eq!(PageLimit::MAX.get(), 1000);
        for (rows, limit) in [
            (0, 1),
            (1, 1),
            (250, 250),
            (1000, 1000),
            (1001, 1000),
            (u32::MAX, 1000),
        ] {
            assert_eq!(PageLimit::from(rows), PageLimit(limit), "{rows}");
        }
    }

    #[test]
    fn a_page_limit_deserializes_as_it_converts() {
        for (json, limit) in [("0", 1), ("50", 50), ("5000", 1000)] {
            assert_eq!(
                serde_json::from_str::<PageLimit>(json).unwrap(),
                PageLimit(limit),
                "{json}"
            );
        }
        for json in ["-1", "\"50\"", "4294967296", "1.5"] {
            assert!(serde_json::from_str::<PageLimit>(json).is_err(), "{json}");
        }
    }
}
