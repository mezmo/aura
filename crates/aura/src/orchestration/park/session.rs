//! Session and agent-instance glossary types (ADR 2026-07-21, decisions 16
//! and 17) and the fenced session record.

use serde::{Deserialize, Serialize};

use crate::hitl::Timestamp;
use crate::orchestration::types::{RunId, TaskIdentity};

use super::ids::{AgentInstanceId, ChatSessionId, SessionId};
use super::lease::{CasError, FencingGeneration, Lease};
use super::run_fsm::{RunEvent, RunState};

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

/// The session-store record the CAS protocol operates on: run FSM state,
/// lease, and fencing generation for one session. Invariant: at most one
/// non-terminal run per session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session: Session,
    pub run_id: Option<RunId>,
    pub state: RunState,
    pub lease: Option<Lease>,
    pub generation: FencingGeneration,
}

impl SessionRecord {
    /// Apply one run event under fencing, consuming the record.
    ///
    /// Rejects `presented` older than `self.generation` as
    /// [`CasError::StaleGeneration`] before consulting the FSM; a legal
    /// event advances the generation with the new state. Backends execute
    /// this inside their atomic primitive - the semantics live here so every
    /// backend enforces the same rules.
    pub fn apply(
        self,
        presented: FencingGeneration,
        event: RunEvent,
    ) -> Result<SessionRecord, CasError> {
        let _ = (presented, event);
        todo!("staged for #271 P-cards: fenced session CAS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn record() -> SessionRecord {
        SessionRecord {
            session: Session {
                id: SessionId::generate(),
                chat_session_id: Some(ChatSessionId::new("cs_test")),
                created_at: Utc::now(),
            },
            run_id: None,
            state: RunState::Created,
            lease: None,
            generation: FencingGeneration::INITIAL.next().next(),
        }
    }

    #[test]
    fn stale_generation_rejected() {
        let rec = record();
        let current = rec.generation;
        let stale = FencingGeneration::INITIAL;
        assert_eq!(
            rec.apply(stale, RunEvent::Start),
            Err(CasError::StaleGeneration {
                presented: stale,
                current,
            })
        );
    }

    #[test]
    fn cas_success_advances_generation() {
        let rec = record();
        let presented = rec.generation;
        let next = rec.apply(presented, RunEvent::Start).expect("legal");
        assert_eq!(next.state, RunState::Running);
        assert_eq!(next.generation, presented.next());
    }

    #[test]
    fn illegal_event_with_current_generation_is_fsm_error() {
        let rec = record();
        let presented = rec.generation;
        assert!(matches!(
            rec.apply(presented, RunEvent::Complete),
            Err(CasError::Illegal(_))
        ));
    }
}
