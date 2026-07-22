//! Typed outcomes that carry a park signal from the tool layer to the run
//! loop (ADR 2026-07-21, decision 11).

use crate::hitl::DecisionId;
use crate::orchestration::types::{FailureCategory, TaskIdentity};

use super::non_empty::NonEmpty;

/// The parked approval a blocked task is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRef {
    pub decision_id: DecisionId,
    pub task: TaskIdentity,
}

/// Outcome of one gated tool attempt inside a worker.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolAttemptOutcome {
    Completed {
        output: String,
    },
    /// The gate parked the call; the attempt ends without executing.
    Blocked(ApprovalRef),
    Failed {
        error: String,
    },
}

/// Outcome of one task's worker attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskExecutionOutcome {
    Complete {
        result: String,
    },
    /// The attempt hit an approval gate; the task enters `Blocked` and the
    /// attempt re-runs on reify (no mid-worker resume in V1).
    Blocked(ApprovalRef),
    Failed {
        error: String,
        category: FailureCategory,
    },
}

/// Outcome of one drained wave.
#[derive(Debug, Clone, PartialEq)]
pub enum WaveOutcome {
    /// Tasks remain ready or pending: keep executing.
    Continue,
    /// The plan is finished.
    Finished,
    /// The ready frontier is empty, nothing is running, and the run is
    /// blocked on these approvals: the quiescence park point. Non-empty by
    /// construction - a drained wave with no blocked task is not one.
    Blocked { on: NonEmpty<ApprovalRef> },
}
