//! A run's facts as the run store keeps them: which run it is, where it
//! stands, and what it has cost.
//!
//! Observers read a run's events from the session's journal. The facts answer
//! questions about a run without replaying those events. The runtime writes
//! them as the run progresses, and they are never rebuilt from the journal.

use aura_events::run::{CheckpointRef, Liveness};
use aura_events::{RunId, SequenceNumber, SessionId, Timestamp, TokenUsage};
use serde::{Deserialize, Serialize};

use crate::hitl::DecisionId;

/// The most runs a default [`RunFilter`] lists.
pub const DEFAULT_RUN_LIST_LIMIT: usize = 100;

/// What is known about one run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFacts {
    pub run_id: RunId,
    pub session_id: SessionId,
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
    pub first_seq: Option<SequenceNumber>,
    /// The sequence number of the run's newest event.
    pub latest_seq: Option<SequenceNumber>,
    /// Approvals the run is waiting on.
    pub pending_approvals: Vec<DecisionId>,
    /// Where a parked run's checkpoint is.
    pub checkpoint: Option<CheckpointRef>,
    /// The run this one continues.
    pub continues: Option<RunId>,
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
        let waiting = !facts.pending_approvals.is_empty();
        self.session
            .as_ref()
            .is_none_or(|session| *session == facts.session_id)
            && self.status.is_none_or(|status| status == facts.status)
            && self
                .pending_approval
                .is_none_or(|pending| pending == waiting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            run_id: RunId::mint(),
            session_id: SessionId::new("sess-1").unwrap(),
            agent: "math".to_owned(),
            status: RunStatus::Parked,
            liveness: Liveness::default(),
            started_at: Timestamp::from_unix_millis(10),
            ended_at: Some(Timestamp::from_unix_millis(20)),
            usage: TokenUsage::default(),
            first_seq: Some(SequenceNumber::FIRST),
            latest_seq: Some(SequenceNumber::FIRST.next()),
            pending_approvals: vec![DecisionId::generate()],
            checkpoint: Some(CheckpointRef::new("run:abc").unwrap()),
            continues: Some(RunId::mint()),
        };

        let json = serde_json::to_string(&facts).unwrap();
        assert_eq!(serde_json::from_str::<RunFacts>(&json).unwrap(), facts);
        assert!(json.contains(r#""status":"parked""#), "{json}");
    }
}
