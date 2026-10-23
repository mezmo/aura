//! A session's outward state, read off the runtime's records rather than
//! stored or emitted.

use serde::Serialize;

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
    /// The last run stopped resumable.
    Parked,
    /// No run is executing.
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
}
