//! Session and agent-instance glossary types (ADR 2026-07-21, decisions 16
//! and 17) and the fenced session record.

use serde::{Deserialize, Serialize};

use crate::hitl::Timestamp;
use crate::orchestration::types::{RunId, TaskIdentity};

use super::checkpoint::CheckpointEnvelope;
use super::ids::{AgentInstanceId, ChatSessionId, SessionId};
use super::lease::{CasError, FencingGeneration, Lease};
use super::run_fsm::{ParkReason, RunEvent, RunState};

/// The durable conversation owner: holds identity, artifacts, and the FSM
/// record. Hydratable on any pod; executes nowhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    /// External correlation key, when the client supplied one.
    pub chat_session_id: Option<ChatSessionId>,
    pub created_at: Timestamp,
}

/// A reified execution environment on one pod, born by claiming a session.
/// Distinct from the TOML `[agent]` table, which is the agent *definition*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInstance {
    pub id: AgentInstanceId,
    pub session: SessionId,
    /// The serving request, for attended instances.
    pub request_id: Option<String>,
    /// The task being executed, when the instance serves one.
    pub task: Option<TaskIdentity>,
    pub claimed_at: Timestamp,
}

/// Everything a park commits besides the state transition itself. Bundled
/// so the checkpoint cannot be omitted from the commit.
#[derive(Debug, Serialize, Deserialize)]
pub struct ParkCommit {
    pub checkpoint: CheckpointEnvelope,
    pub reason: ParkReason,
    pub parked_at: Timestamp,
    pub expires_at: Timestamp,
}

/// The session-store record the CAS protocol operates on: run FSM state,
/// checkpoint, lease, and fencing generation for one session. Invariants:
/// at most one non-terminal run per session; `run_id` is `Some` for every
/// state but `Created`; `checkpoint` is `Some` exactly while `Parked`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session: Session,
    pub run_id: Option<RunId>,
    pub state: RunState,
    pub checkpoint: Option<CheckpointEnvelope>,
    pub lease: Option<Lease>,
    pub generation: FencingGeneration,
}

impl SessionRecord {
    /// Apply one run event under fencing, consuming the record.
    ///
    /// `presented` must equal `self.generation` exactly - older or newer is
    /// [`CasError::GenerationMismatch`] - before the FSM is consulted. A
    /// legal event advances the generation, rewrites `lease.generation` to
    /// match when a lease is held, binds `run_id` on `Start`, and clears
    /// `checkpoint` when leaving `Parked`. Backends execute this inside
    /// their atomic primitive - the semantics live here so every backend
    /// enforces the same rules.
    pub fn apply(
        self,
        presented: FencingGeneration,
        event: RunEvent,
    ) -> Result<SessionRecord, CasError> {
        let _ = (presented, event);
        todo!("staged for #271 P-cards: fenced session CAS")
    }

    /// Park the running run, committing the checkpoint and the
    /// `Running -> Parked` transition as one record write (ADR decision 7).
    ///
    /// Parking is deliberately not a [`RunEvent`]: this is the only path to
    /// `Parked`, so a parked record without its checkpoint is
    /// unconstructable through the FSM surface. Fencing rules match
    /// [`Self::apply`]; a non-`Running` state is [`CasError::StateMismatch`].
    pub fn park(
        self,
        presented: FencingGeneration,
        commit: ParkCommit,
    ) -> Result<SessionRecord, CasError> {
        let _ = (presented, commit);
        todo!("staged for #271 P-cards: atomic park commit")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hitl::DecisionId;
    use crate::orchestration::park::checkpoint::RunCheckpoint;
    use crate::orchestration::park::non_empty::NonEmpty;
    use chrono::Utc;

    fn record_in(state: RunState, run_id: Option<RunId>) -> SessionRecord {
        SessionRecord {
            session: Session {
                id: SessionId::generate(),
                chat_session_id: Some(ChatSessionId::new("cs_test")),
                created_at: Utc::now(),
            },
            run_id,
            state,
            checkpoint: None,
            lease: Some(Lease {
                holder: AgentInstanceId::generate(),
                acquired_at: Utc::now(),
                heartbeat_at: Utc::now(),
                expires_at: Utc::now() + chrono::Duration::seconds(60),
                generation: FencingGeneration::INITIAL.next().next(),
            }),
            generation: FencingGeneration::INITIAL.next().next(),
        }
    }

    fn run_id() -> RunId {
        "018f9d2e-7c3a-7000-8000-000000000271".parse().unwrap()
    }

    fn park_commit() -> ParkCommit {
        ParkCommit {
            checkpoint: CheckpointEnvelope::new(RunCheckpoint::test_minimal()),
            reason: ParkReason::ApprovalsBlocked {
                decisions: NonEmpty::new(vec![DecisionId::generate()]).unwrap(),
            },
            parked_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::seconds(300),
        }
    }

    #[test]
    fn stale_generation_rejected() {
        let rec = record_in(RunState::Created, None);
        let current = rec.generation;
        let stale = FencingGeneration::INITIAL;
        match rec.apply(stale, RunEvent::Start { run_id: run_id() }) {
            Err(CasError::GenerationMismatch {
                presented,
                current: c,
            }) => {
                assert_eq!(presented, stale);
                assert_eq!(c, current);
            }
            other => panic!("expected GenerationMismatch, got {other:?}"),
        }
    }

    #[test]
    fn unissued_future_generation_rejected() {
        let rec = record_in(RunState::Created, None);
        let future = rec.generation.next();
        assert!(matches!(
            rec.apply(future, RunEvent::Start { run_id: run_id() }),
            Err(CasError::GenerationMismatch { .. })
        ));
    }

    #[test]
    fn cas_success_advances_generation_and_lease_coherently() {
        let rec = record_in(RunState::Created, None);
        let presented = rec.generation;
        let next = rec
            .apply(presented, RunEvent::Start { run_id: run_id() })
            .expect("legal");
        assert_eq!(next.state, RunState::Running);
        assert_eq!(next.generation, presented.next());
        assert_eq!(
            next.lease.expect("lease retained").generation,
            next.generation,
            "a held lease's fencing token tracks the record generation"
        );
    }

    #[test]
    fn start_binds_run_identity() {
        let rec = record_in(RunState::Created, None);
        let presented = rec.generation;
        let next = rec
            .apply(presented, RunEvent::Start { run_id: run_id() })
            .expect("legal");
        assert_eq!(next.run_id, Some(run_id()));
    }

    #[test]
    fn illegal_event_with_current_generation_is_fsm_error() {
        let rec = record_in(RunState::Created, None);
        let presented = rec.generation;
        assert!(matches!(
            rec.apply(presented, RunEvent::Complete),
            Err(CasError::Illegal(_))
        ));
    }

    #[test]
    fn park_commits_checkpoint_with_the_transition() {
        let rec = record_in(RunState::Running, Some(run_id()));
        let presented = rec.generation;
        let next = rec.park(presented, park_commit()).expect("legal");
        assert!(matches!(next.state, RunState::Parked { .. }));
        assert!(
            next.checkpoint.is_some(),
            "a parked record always carries its checkpoint"
        );
        assert_eq!(next.generation, presented.next());
    }

    #[test]
    fn park_outside_running_rejected() {
        let rec = record_in(RunState::Created, None);
        let presented = rec.generation;
        assert!(matches!(
            rec.park(presented, park_commit()),
            Err(CasError::StateMismatch { .. })
        ));
    }

    #[test]
    fn park_with_stale_generation_rejected() {
        let rec = record_in(RunState::Running, Some(run_id()));
        assert!(matches!(
            rec.park(FencingGeneration::INITIAL, park_commit()),
            Err(CasError::GenerationMismatch { .. })
        ));
    }
}
