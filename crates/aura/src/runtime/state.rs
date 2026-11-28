//! A session's state as callers see it. It is worked out from the runtime's
//! records when asked for, and is never stored or emitted.

use serde::{Deserialize, Serialize};

use crate::hitl::DecisionId;

/// Where a session stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentState {
    /// The session's agent is being prepared for a run.
    Preparing,
    /// A run is executing.
    Running,
    /// A run is waiting on something outside the model.
    Blocked { on: BlockReason },
    /// A run is ending after a stop.
    Stopping,
    /// The last run parked, and can be resumed.
    Parked,
    /// No run is executing.
    Done,
}

impl AgentState {
    /// Whether a run is live in this state.
    pub fn has_live_run(&self) -> bool {
        match self {
            Self::Preparing | Self::Running | Self::Blocked { .. } | Self::Stopping => true,
            Self::Parked | Self::Done => false,
        }
    }

    pub fn kind(&self) -> AgentStateKind {
        match self {
            Self::Preparing => AgentStateKind::Preparing,
            Self::Running => AgentStateKind::Running,
            Self::Blocked { .. } => AgentStateKind::Blocked,
            Self::Stopping => AgentStateKind::Stopping,
            Self::Parked => AgentStateKind::Parked,
            Self::Done => AgentStateKind::Done,
        }
    }
}

/// An [`AgentState`] without its details.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStateKind {
    Preparing,
    Running,
    Blocked,
    Stopping,
    Parked,
    Done,
}

/// What a blocked run is waiting on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockReason {
    /// Human decisions on gated tool calls.
    Approval { decisions: Vec<DecisionId> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_serialize_with_their_tags() {
        let id = DecisionId::generate();
        let blocked = AgentState::Blocked {
            on: BlockReason::Approval {
                decisions: vec![id],
            },
        };

        assert_eq!(
            serde_json::to_value(&blocked).unwrap(),
            serde_json::json!({
                "state": "blocked",
                "on": { "kind": "approval", "decisions": [id.to_string()] },
            })
        );
        assert_eq!(
            serde_json::to_value(AgentState::Done).unwrap(),
            serde_json::json!({ "state": "done" })
        );
    }

    #[test]
    fn stopping_serializes_as_its_tag_alone() {
        assert_eq!(
            serde_json::to_value(AgentState::Stopping).unwrap(),
            serde_json::json!({ "state": "stopping" })
        );
    }

    #[test]
    fn each_kind_parses_from_its_name() {
        for (name, kind) in [
            ("preparing", AgentStateKind::Preparing),
            ("running", AgentStateKind::Running),
            ("blocked", AgentStateKind::Blocked),
            ("stopping", AgentStateKind::Stopping),
            ("parked", AgentStateKind::Parked),
            ("done", AgentStateKind::Done),
        ] {
            assert_eq!(
                serde_json::from_value::<AgentStateKind>(serde_json::json!(name)).unwrap(),
                kind,
                "{name}"
            );
        }
    }

    #[test]
    fn a_name_that_is_no_kind_is_refused() {
        for name in ["runing", "Running", "STOPPING", ""] {
            assert!(
                serde_json::from_value::<AgentStateKind>(serde_json::json!(name)).is_err(),
                "{name:?}"
            );
        }
    }

    /// A state's tag on the wire is the name its kind parses from, so a
    /// client can filter by the `state` it reads.
    #[test]
    fn each_states_tag_parses_as_its_kind() {
        let states = [
            AgentState::Preparing,
            AgentState::Running,
            AgentState::Blocked {
                on: BlockReason::Approval {
                    decisions: vec![DecisionId::generate()],
                },
            },
            AgentState::Stopping,
            AgentState::Parked,
            AgentState::Done,
        ];
        for state in states {
            let tag = serde_json::to_value(&state).unwrap()["state"].clone();
            assert_eq!(
                serde_json::from_value::<AgentStateKind>(tag).unwrap(),
                state.kind(),
                "{state:?}"
            );
        }
    }
}
