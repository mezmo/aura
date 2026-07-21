//! The durable run FSM (ADR 2026-07-21, decision 3).

use serde::{Deserialize, Serialize};

use crate::hitl::{DecisionId, Timestamp};

/// Durable run state, persisted in the session store. Terminals stay at
/// three: expiry is a failure cause ([`RunFailureCause::ParkExpired`]), not a
/// fourth terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunState {
    Created,
    Running,
    Parked {
        reason: ParkReason,
        resume_point: ResumePoint,
        parked_at: Timestamp,
        expires_at: Timestamp,
    },
    Completed,
    Failed {
        cause: RunFailureCause,
    },
    Cancelled,
}

/// Why a run is parked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParkReason {
    /// The ready frontier drained with these approvals still outstanding.
    ApprovalsBlocked { decisions: Vec<DecisionId> },
}

/// The boundary a parked run resumes from. A run may only durably exist
/// `Parked` at one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "boundary", rename_all = "snake_case")]
pub enum ResumePoint {
    /// A drained wave inside an iteration.
    WaveBoundary { iteration: u32 },
    /// The replanning edge between iterations.
    IterationBoundary { iteration: u32 },
}

/// A durable reason to wake a parked run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeReason {
    DecisionResolved {
        decision_id: DecisionId,
        resolved_at: Timestamp,
    },
}

/// Why a run terminalized as `Failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum RunFailureCause {
    /// The park outlived `expires_at`.
    ParkExpired,
    ExecutionFailed {
        summary: String,
    },
}

/// Events the run FSM accepts.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    Start,
    Park {
        reason: ParkReason,
        resume_point: ResumePoint,
        parked_at: Timestamp,
        expires_at: Timestamp,
    },
    Reify(WakeReason),
    Complete,
    Fail(RunFailureCause),
    Cancel,
    /// Reaper-issued expiry of a parked run.
    Expire,
}

/// A rejected transition: `event` is not legal from `from`.
#[derive(Debug, Clone, PartialEq)]
pub struct IllegalTransition {
    pub from: RunState,
    pub event: RunEvent,
}

impl RunState {
    /// Apply one event, consuming the current state.
    ///
    /// Legality (ADR decision 3): `Created` only starts; `Running` parks,
    /// completes, fails, or cancels; `Parked` reifies back to `Running` on a
    /// wake reason, and every non-human exit (`Expire`, `Fail`, `Cancel`)
    /// denies - `Parked` never reaches `Completed` directly. Terminals absorb
    /// nothing.
    pub fn apply(self, event: RunEvent) -> Result<RunState, IllegalTransition> {
        let _ = event;
        todo!("staged for #271 P-cards: run FSM transition")
    }

    /// True for `Completed`, `Failed`, and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RunState::Completed | RunState::Failed { .. } | RunState::Cancelled
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn parked_fields() -> (ParkReason, ResumePoint, Timestamp, Timestamp) {
        (
            ParkReason::ApprovalsBlocked {
                decisions: vec![DecisionId::generate()],
            },
            ResumePoint::WaveBoundary { iteration: 1 },
            Utc::now(),
            Utc::now() + chrono::Duration::seconds(300),
        )
    }

    fn parked() -> RunState {
        let (reason, resume_point, parked_at, expires_at) = parked_fields();
        RunState::Parked {
            reason,
            resume_point,
            parked_at,
            expires_at,
        }
    }

    fn park_event() -> RunEvent {
        let (reason, resume_point, parked_at, expires_at) = parked_fields();
        RunEvent::Park {
            reason,
            resume_point,
            parked_at,
            expires_at,
        }
    }

    fn wake() -> WakeReason {
        WakeReason::DecisionResolved {
            decision_id: DecisionId::generate(),
            resolved_at: Utc::now(),
        }
    }

    #[test]
    fn created_starts_running() {
        assert_eq!(
            RunState::Created.apply(RunEvent::Start),
            Ok(RunState::Running)
        );
    }

    #[test]
    fn running_parks_at_boundary() {
        let next = RunState::Running.apply(park_event()).expect("legal");
        assert!(matches!(next, RunState::Parked { .. }));
    }

    #[test]
    fn parked_reifies_to_running() {
        assert_eq!(
            parked().apply(RunEvent::Reify(wake())),
            Ok(RunState::Running)
        );
    }

    #[test]
    fn parked_expiry_fails_closed() {
        assert_eq!(
            parked().apply(RunEvent::Expire),
            Ok(RunState::Failed {
                cause: RunFailureCause::ParkExpired
            })
        );
    }

    #[test]
    fn parked_never_completes_directly() {
        let from = parked();
        assert_eq!(
            from.clone().apply(RunEvent::Complete),
            Err(IllegalTransition {
                from,
                event: RunEvent::Complete
            })
        );
    }

    #[test]
    fn created_cannot_park() {
        assert!(RunState::Created.apply(park_event()).is_err());
    }

    #[test]
    fn terminal_states_are_absorbing() {
        let terminals = [
            RunState::Completed,
            RunState::Failed {
                cause: RunFailureCause::ParkExpired,
            },
            RunState::Cancelled,
        ];
        for terminal in terminals {
            assert!(terminal.clone().apply(RunEvent::Start).is_err());
            assert!(terminal.clone().apply(RunEvent::Reify(wake())).is_err());
            assert!(terminal.clone().apply(RunEvent::Cancel).is_err());
        }
    }
}
