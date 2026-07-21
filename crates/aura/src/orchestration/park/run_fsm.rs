//! The durable run FSM (ADR 2026-07-21, decision 3).

use serde::{Deserialize, Serialize};

use crate::hitl::{DecisionId, Timestamp};
use crate::orchestration::types::RunId;

use super::non_empty::NonEmpty;

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
    ApprovalsBlocked { decisions: NonEmpty<DecisionId> },
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
    ParkExpired {
        summary: String,
    },
    ExecutionFailed {
        summary: String,
    },
}

/// Events the run FSM accepts. Parking is not an event: it is the
/// checkpoint-carrying [`super::SessionRecord::park`] operation, so a
/// `Running -> Parked` transition cannot exist without its checkpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    Start {
        run_id: RunId,
    },
    Reify(WakeReason),
    Complete,
    Fail(RunFailureCause),
    Cancel,
    /// Reaper-issued expiry of a parked run; the summary lands in
    /// [`RunFailureCause::ParkExpired`].
    Expire {
        summary: String,
    },
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
    /// Legality (ADR decision 3): `Created` only starts; `Running`
    /// completes, fails, or cancels (it parks via
    /// [`super::SessionRecord::park`]); `Parked` reifies back to `Running`
    /// only on a wake reason whose decision id is one of the parked
    /// reason's decisions - an unknown decision id is rejected, and every
    /// non-human exit (`Expire`, `Fail`, `Cancel`) denies. `Parked` never
    /// reaches `Completed` directly. Terminals accept no event.
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

    fn run_id() -> RunId {
        "018f9d2e-7c3a-7000-8000-000000000271".parse().unwrap()
    }

    fn parked_on(decision_id: DecisionId) -> RunState {
        RunState::Parked {
            reason: ParkReason::ApprovalsBlocked {
                decisions: NonEmpty::new(vec![decision_id]).unwrap(),
            },
            resume_point: ResumePoint::WaveBoundary { iteration: 1 },
            parked_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::seconds(300),
        }
    }

    fn wake(decision_id: DecisionId) -> WakeReason {
        WakeReason::DecisionResolved {
            decision_id,
            resolved_at: Utc::now(),
        }
    }

    fn all_events() -> Vec<RunEvent> {
        vec![
            RunEvent::Start { run_id: run_id() },
            RunEvent::Reify(wake(DecisionId::generate())),
            RunEvent::Complete,
            RunEvent::Fail(RunFailureCause::ExecutionFailed {
                summary: "boom".to_string(),
            }),
            RunEvent::Cancel,
            RunEvent::Expire {
                summary: "expired".to_string(),
            },
        ]
    }

    #[test]
    fn created_starts_running() {
        assert_eq!(
            RunState::Created.apply(RunEvent::Start { run_id: run_id() }),
            Ok(RunState::Running)
        );
    }

    #[test]
    fn parked_reifies_on_a_parked_decision() {
        let decision = DecisionId::generate();
        assert_eq!(
            parked_on(decision).apply(RunEvent::Reify(wake(decision))),
            Ok(RunState::Running)
        );
    }

    #[test]
    fn reify_with_unknown_decision_rejected() {
        let parked = parked_on(DecisionId::generate());
        let stranger = RunEvent::Reify(wake(DecisionId::generate()));
        assert_eq!(
            parked.clone().apply(stranger.clone()),
            Err(IllegalTransition {
                from: parked,
                event: stranger
            })
        );
    }

    #[test]
    fn parked_expiry_fails_closed() {
        assert_eq!(
            parked_on(DecisionId::generate()).apply(RunEvent::Expire {
                summary: "2 approvals denied by expiry".to_string()
            }),
            Ok(RunState::Failed {
                cause: RunFailureCause::ParkExpired {
                    summary: "2 approvals denied by expiry".to_string()
                }
            })
        );
    }

    #[test]
    fn parked_never_completes_directly() {
        let from = parked_on(DecisionId::generate());
        assert_eq!(
            from.clone().apply(RunEvent::Complete),
            Err(IllegalTransition {
                from,
                event: RunEvent::Complete
            })
        );
    }

    #[test]
    fn terminal_states_absorb_no_event() {
        let terminals = [
            RunState::Completed,
            RunState::Failed {
                cause: RunFailureCause::ParkExpired {
                    summary: "expired".to_string(),
                },
            },
            RunState::Cancelled,
        ];
        for terminal in terminals {
            for event in all_events() {
                assert!(
                    terminal.clone().apply(event.clone()).is_err(),
                    "{terminal:?} must reject {event:?}"
                );
            }
        }
    }

    #[test]
    fn running_accepts_no_start_or_reify() {
        for event in [
            RunEvent::Start { run_id: run_id() },
            RunEvent::Reify(wake(DecisionId::generate())),
        ] {
            assert!(RunState::Running.apply(event).is_err());
        }
    }
}
