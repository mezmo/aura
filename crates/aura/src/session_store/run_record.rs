//! A run's facts as the run store keeps them: which run it is, where it
//! stands, and what it has cost.
//!
//! Observers read a run's events from the session's journal. The facts answer
//! questions about a run without replaying those events. The runtime writes
//! them as the run progresses, and they are never rebuilt from the journal.

use aura_events::run::{CheckpointRef, Liveness};
use aura_events::{RunId, SequenceNumber, SessionId, Timestamp, TokenUsage};
use serde::{Deserialize, Serialize};

use crate::conversation::{Conversation, Lane, RunSession};
use crate::hitl::DecisionId;

/// The most runs a default [`RunFilter`] lists.
pub const DEFAULT_RUN_LIST_LIMIT: usize = 100;

/// What is known about one run: the session it is on, and the run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFacts {
    /// The session the run is on.
    pub session: RunSession,
    pub run: RunDetails,
}

/// A run, apart from the session it is on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDetails {
    pub run_id: RunId,
    /// The agent that runs, by its `Config::agent_id`.
    pub agent: String,
    pub status: RunStatus,
    pub liveness: Liveness,
    /// When the runtime accepted the run.
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    /// Cumulative provider-billed tokens across every turn of the run.
    pub usage: TokenUsage,
    /// The sequence number of the run's `Started` event.
    pub first_event: Option<SequenceNumber>,
    /// The sequence number of the run's newest event.
    pub latest_event: Option<SequenceNumber>,
    /// Approvals the run is waiting on.
    pub pending_approvals: Vec<DecisionId>,
    /// Where a parked run's checkpoint is.
    pub checkpoint: Option<CheckpointRef>,
    /// The run this one continues.
    pub continues: Option<RunId>,
}

impl RunFacts {
    /// The id of the session the run is on. For a run on a lane, this is
    /// the lane's session id, not the chat id.
    pub fn session_id(&self) -> SessionId {
        self.session.id()
    }

    /// The conversation's lane the run is on, if it is on one.
    pub fn lane(&self) -> Option<&Lane> {
        self.session.lane()
    }
}

/// Where a run stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Accepted; its agent is being prepared.
    Preparing,
    Running,
    Finished,
    Failed,
    Cancelled,
    /// Stopped, and can be resumed from its checkpoint.
    Parked,
}

impl RunStatus {
    /// Whether the run has ended: finished, failed, cancelled, or parked.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Preparing | Self::Running)
    }
}

/// Which runs a listing returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFilter {
    pub session: Option<SessionId>,
    /// The conversation whose lanes the run was on.
    pub conversation: Option<Conversation>,
    /// The chat id of the conversation the run's session is a lane of.
    pub chat: Option<SessionId>,
    pub status: Option<RunStatus>,
    /// Whether the run is waiting on an approval.
    pub pending_approval: Option<bool>,
    /// The most runs to return.
    pub limit: usize,
    /// The last run id of the previous page.
    pub after: Option<RunId>,
}

impl Default for RunFilter {
    fn default() -> Self {
        Self {
            session: None,
            conversation: None,
            chat: None,
            status: None,
            pending_approval: None,
            limit: DEFAULT_RUN_LIST_LIMIT,
            after: None,
        }
    }
}

impl RunFilter {
    /// Whether `facts` is a run this filter selects, ignoring `limit` and
    /// `after`, which select a page rather than a run.
    pub fn matches(&self, facts: &RunFacts) -> bool {
        let waiting = !facts.run.pending_approvals.is_empty();
        self.session
            .as_ref()
            .is_none_or(|session| match &facts.session {
                RunSession::Plain(id) => id == session,
                RunSession::Lane(lane) => lane.session_id() == *session,
            })
            && self.conversation.as_ref().is_none_or(|conversation| {
                facts
                    .lane()
                    .is_some_and(|lane| lane.conversation() == conversation)
            })
            && self.chat.as_ref().is_none_or(|chat| {
                facts
                    .lane()
                    .is_some_and(|lane| lane.conversation().chat() == chat)
            })
            && self.status.is_none_or(|status| status == facts.run.status)
            && self
                .pending_approval
                .is_none_or(|pending| pending == waiting)
    }
}

/// The most sessions a default [`SessionFilter`] lists.
pub const DEFAULT_SESSION_LIST_LIMIT: usize = 100;

/// One or more run statuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStatuses(Vec<RunStatus>);

impl RunStatuses {
    /// Only `status`.
    pub fn just(status: RunStatus) -> Self {
        Self(vec![status])
    }

    /// These statuses and `status`.
    #[must_use]
    pub fn and(mut self, status: RunStatus) -> Self {
        self.0.push(status);
        self
    }

    pub fn contains(&self, status: RunStatus) -> bool {
        self.0.contains(&status)
    }
}

impl From<RunStatus> for RunStatuses {
    fn from(status: RunStatus) -> Self {
        Self::just(status)
    }
}

/// Which sessions a listing returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFilter {
    pub session: Option<SessionId>,
    /// The chat id of the conversation the session is a lane of.
    pub chat: Option<SessionId>,
    /// The agent of the session's newest run.
    pub agent: Option<String>,
    /// The statuses the session's newest run may have.
    pub statuses: Option<RunStatuses>,
    /// Whether the session's newest run is waiting on an approval.
    pub pending_approval: Option<bool>,
    /// The most sessions to return.
    pub limit: usize,
    /// The last session id of the previous page.
    pub after: Option<SessionId>,
}

impl Default for SessionFilter {
    fn default() -> Self {
        Self {
            session: None,
            chat: None,
            agent: None,
            statuses: None,
            pending_approval: None,
            limit: DEFAULT_SESSION_LIST_LIMIT,
            after: None,
        }
    }
}

impl SessionFilter {
    /// Whether `session` is one this filter selects, ignoring `limit` and
    /// `after`, which select a page rather than a session. Only the
    /// session's newest run is read.
    pub fn matches(&self, session: &SessionRuns) -> bool {
        self.matches_newest_run(session.newest())
    }

    /// Whether a session whose newest run is `newest` is one this filter
    /// selects, as [`matches`](Self::matches) decides.
    pub fn matches_newest_run(&self, newest: &RunFacts) -> bool {
        let waiting = !newest.run.pending_approvals.is_empty();
        self.session
            .as_ref()
            .is_none_or(|session| newest.session_id() == *session)
            && self.chat.as_ref().is_none_or(|chat| {
                newest
                    .lane()
                    .is_some_and(|lane| lane.conversation().chat() == chat)
            })
            && self
                .agent
                .as_ref()
                .is_none_or(|agent| *agent == newest.run.agent)
            && self
                .statuses
                .as_ref()
                .is_none_or(|statuses| statuses.contains(newest.run.status))
            && self
                .pending_approval
                .is_none_or(|pending| pending == waiting)
    }
}

/// One session, as its runs in the store describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRuns {
    newest: RunFacts,
    previous_ended: Option<RunId>,
}

impl SessionRuns {
    /// A session whose only run so far is `first`.
    pub fn new(first: RunFacts) -> Self {
        Self {
            newest: first,
            previous_ended: None,
        }
    }

    /// Records `next` as the session's newest run. The run that was newest
    /// becomes the previous ended run if it has ended.
    ///
    /// A `next` with the newest run's id is the same run again. Its facts
    /// replace the newest run's, and the previous ended run stays as it is.
    ///
    /// A `next` with a different id has to be a later run of the same
    /// session. A store meets this by calling `push` with a session's runs in
    /// the order they started.
    pub fn push(&mut self, next: RunFacts) {
        if next.run.run_id == self.newest.run.run_id {
            self.newest = next;
            return;
        }
        let replaced = std::mem::replace(&mut self.newest, next);
        if replaced.run.status.is_terminal() {
            self.previous_ended = Some(replaced.run.run_id);
        }
    }

    /// The session's newest run.
    pub fn newest(&self) -> &RunFacts {
        &self.newest
    }

    /// The newest run that ended among the session's runs older than its
    /// newest.
    pub fn previous_ended(&self) -> Option<RunId> {
        self.previous_ended
    }

    /// The newest run and the previous ended run.
    pub fn into_parts(self) -> (RunFacts, Option<RunId>) {
        (self.newest, self.previous_ended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::LaneNumber;

    #[test]
    fn only_preparing_and_running_are_live() {
        use RunStatus::*;
        for status in [Preparing, Running] {
            assert!(!status.is_terminal(), "{status:?}");
        }
        for status in [Finished, Failed, Cancelled, Parked] {
            assert!(status.is_terminal(), "{status:?}");
        }
    }

    #[test]
    fn facts_round_trip_through_json() {
        let facts = RunFacts {
            session: RunSession::Lane(Lane::new(
                Conversation::new(SessionId::new("chat-1").unwrap(), "math"),
                LaneNumber::new(2).unwrap(),
            )),
            run: RunDetails {
                run_id: RunId::mint(),
                agent: "math".to_owned(),
                status: RunStatus::Parked,
                liveness: Liveness::default(),
                started_at: Timestamp::from_unix_millis(10),
                ended_at: Some(Timestamp::from_unix_millis(20)),
                usage: TokenUsage::default(),
                first_event: Some(SequenceNumber::FIRST),
                latest_event: Some(SequenceNumber::FIRST.next()),
                pending_approvals: vec![DecisionId::generate()],
                checkpoint: Some(CheckpointRef::new("run:abc").unwrap()),
                continues: Some(RunId::mint()),
            },
        };

        let json = serde_json::to_string(&facts).unwrap();
        assert_eq!(serde_json::from_str::<RunFacts>(&json).unwrap(), facts);
        assert!(json.contains(r#""status":"parked""#), "{json}");

        let on_session = RunFacts {
            session: RunSession::Plain(SessionId::new("sess-1").unwrap()),
            ..facts
        };
        let json = serde_json::to_string(&on_session).unwrap();
        assert_eq!(serde_json::from_str::<RunFacts>(&json).unwrap(), on_session);
        assert!(json.contains(r#""session":{"plain":"sess-1"}"#), "{json}");
    }

    /// A run id that orders by `n`, as minted ids order by when they were
    /// minted.
    fn run_id(n: u128) -> RunId {
        RunId::try_from(uuid::Uuid::from_u128(n)).expect("a non-zero UUID is a run id")
    }

    fn id(name: &str) -> SessionId {
        SessionId::new(name).unwrap()
    }

    fn lane(chat: &str, agent: &str, number: u8) -> RunSession {
        RunSession::Lane(Lane::new(
            Conversation::new(id(chat), agent),
            LaneNumber::new(number).unwrap(),
        ))
    }

    /// Run `n` of `session`, by the agent `agent`, in `status`.
    fn on(session: RunSession, n: u128, status: RunStatus) -> RunFacts {
        RunFacts {
            session,
            run: RunDetails {
                run_id: run_id(n),
                agent: "agent".to_owned(),
                status,
                liveness: Liveness::default(),
                started_at: Timestamp::from_unix_millis(n as u64),
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

    /// Run `n` of the plain session `s1`.
    fn run(n: u128, status: RunStatus) -> RunFacts {
        on(RunSession::Plain(id("s1")), n, status)
    }

    #[test]
    fn a_later_run_keeps_the_run_it_replaced_as_previous_ended_once_that_run_has_ended() {
        use RunStatus::*;
        for status in [Finished, Failed, Cancelled, Parked] {
            let mut session = SessionRuns::new(run(1, status));
            session.push(run(2, Running));
            assert_eq!(
                session.into_parts(),
                (run(2, Running), Some(run_id(1))),
                "{status:?}"
            );
        }
        for status in [Preparing, Running] {
            let mut session = SessionRuns::new(run(1, status));
            session.push(run(2, Running));
            assert_eq!(session.into_parts(), (run(2, Running), None), "{status:?}");
        }
    }

    #[test]
    fn a_replaced_run_that_had_not_ended_leaves_the_previous_ended_run_as_it_was() {
        use RunStatus::*;
        let mut session = SessionRuns::new(run(1, Finished));
        session.push(run(2, Running));
        session.push(run(3, Preparing));

        assert_eq!(session.into_parts(), (run(3, Preparing), Some(run_id(1))));
    }

    #[test]
    fn pushing_the_newest_run_again_replaces_its_facts_and_nothing_else() {
        use RunStatus::*;
        let mut session = SessionRuns::new(run(1, Finished));
        session.push(run(2, Running));
        session.push(run(2, Finished));
        assert_eq!(session.into_parts(), (run(2, Finished), Some(run_id(1))));

        let mut session = SessionRuns::new(run(1, Running));
        session.push(run(1, Finished));
        assert_eq!(
            session.into_parts(),
            (run(1, Finished), None),
            "a run is never its own previous ended run"
        );
    }

    #[test]
    fn a_status_set_contains_the_statuses_it_was_built_with_and_no_others() {
        use RunStatus::*;
        let all = [Preparing, Running, Finished, Failed, Cancelled, Parked];
        let contained = |statuses: &RunStatuses| -> Vec<RunStatus> {
            all.into_iter()
                .filter(|status| statuses.contains(*status))
                .collect()
        };

        assert_eq!(contained(&RunStatuses::just(Running)), [Running]);
        assert_eq!(contained(&RunStatuses::from(Parked)), [Parked]);
        assert_eq!(
            contained(&RunStatuses::just(Finished).and(Failed).and(Cancelled)),
            [Finished, Failed, Cancelled]
        );
    }

    #[test]
    fn a_session_filter_reads_the_newest_runs_session_agent_status_and_approvals() {
        use RunStatus::*;
        let mut newest = run(2, Running);
        newest.run.pending_approvals = vec![DecisionId::generate()];
        let session = SessionRuns::new(newest);
        let selects = |filter: SessionFilter| filter.matches(&session);

        assert!(selects(SessionFilter::default()));
        assert!(selects(SessionFilter {
            session: Some(id("s1")),
            ..SessionFilter::default()
        }));
        assert!(!selects(SessionFilter {
            session: Some(id("s2")),
            ..SessionFilter::default()
        }));
        assert!(selects(SessionFilter {
            agent: Some("agent".to_owned()),
            ..SessionFilter::default()
        }));
        assert!(!selects(SessionFilter {
            agent: Some("other".to_owned()),
            ..SessionFilter::default()
        }));
        assert!(selects(SessionFilter {
            statuses: Some(RunStatuses::just(Preparing).and(Running)),
            ..SessionFilter::default()
        }));
        assert!(!selects(SessionFilter {
            statuses: Some(RunStatuses::just(Finished)),
            ..SessionFilter::default()
        }));
        assert!(selects(SessionFilter {
            pending_approval: Some(true),
            ..SessionFilter::default()
        }));
        assert!(!selects(SessionFilter {
            pending_approval: Some(false),
            ..SessionFilter::default()
        }));
        assert!(
            !selects(SessionFilter {
                agent: Some("agent".to_owned()),
                statuses: Some(RunStatuses::just(Finished)),
                ..SessionFilter::default()
            }),
            "every clause has to hold"
        );
    }

    #[test]
    fn a_run_filter_on_a_chat_matches_only_lanes_of_that_chat() {
        use RunStatus::*;
        let in_c1 = RunFilter {
            chat: Some(id("c1")),
            ..RunFilter::default()
        };

        assert!(in_c1.matches(&on(lane("c1", "a", 0), 1, Running)));
        assert!(in_c1.matches(&on(lane("c1", "b", 2), 2, Finished)));
        assert!(!in_c1.matches(&on(lane("c2", "a", 0), 3, Running)));
        assert!(
            !in_c1.matches(&on(RunSession::Plain(id("c1")), 4, Running)),
            "a plain session named for the chat is not a lane of it"
        );
        assert!(
            !in_c1.matches(&on(RunSession::Plain(id("c1:a:0")), 5, Running)),
            "a plain session named for a lane is not a lane"
        );
    }

    #[test]
    fn a_session_filter_on_a_chat_matches_only_lanes_of_that_chat() {
        use RunStatus::*;
        let in_c1 = SessionFilter {
            chat: Some(id("c1")),
            ..SessionFilter::default()
        };
        let selects = |facts: RunFacts| in_c1.matches(&SessionRuns::new(facts));

        assert!(selects(on(lane("c1", "a", 0), 1, Running)));
        assert!(selects(on(lane("c1", "b", 2), 2, Finished)));
        assert!(!selects(on(lane("c2", "a", 0), 3, Running)));
        assert!(
            !selects(on(RunSession::Plain(id("c1")), 4, Running)),
            "a plain session named for the chat is not a lane of it"
        );
        assert!(
            !selects(on(RunSession::Plain(id("c1:a:0")), 5, Running)),
            "a plain session named for a lane is not a lane"
        );
    }
}
