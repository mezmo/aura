//! Typed outcomes that carry a park signal from the tool layer to the run
//! loop (ADR 2026-07-21, decision 11).

use crate::hitl::{ApprovalRef, Timestamp};
use crate::orchestration::types::{FailureCategory, StructuredTaskOutput};

use super::ids::SessionId;
use super::non_empty::NonEmpty;

/// A blocked tool attempt. The field is private and the only constructor is
/// [`ToolAttemptOutcome::from_blocked_pre_call`], so every
/// `ToolAttemptOutcome::Blocked` in existence was projected from the gate's
/// pre-call outcome — an independently built one is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedAttempt {
    approval: ApprovalRef,
}

impl BlockedAttempt {
    #[must_use]
    pub fn approval(&self) -> &ApprovalRef {
        &self.approval
    }

    #[must_use]
    pub fn into_approval(self) -> ApprovalRef {
        self.approval
    }
}

/// Outcome of one gated tool attempt inside a worker.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolAttemptOutcome {
    Completed {
        output: String,
    },
    /// The gate parked the call; the attempt ends without executing.
    Blocked(BlockedAttempt),
    Failed {
        error: String,
    },
}

impl ToolAttemptOutcome {
    /// The single construction point for [`ToolAttemptOutcome::Blocked`]:
    /// the lossless projection of a blocked pre-call gate outcome.
    /// `Blocked(r)` projects to `Blocked` carrying the same [`ApprovalRef`];
    /// the other pre-call outcomes describe a call that continues or
    /// completes, so they have no attempt outcome yet and project to `None`.
    pub fn from_blocked_pre_call(outcome: crate::tool_wrapper::PreCallOutcome) -> Option<Self> {
        use crate::tool_wrapper::PreCallOutcome;
        match outcome {
            PreCallOutcome::Blocked(approval) => {
                Some(ToolAttemptOutcome::Blocked(BlockedAttempt { approval }))
            }
            PreCallOutcome::Proceed { overrides: _ } | PreCallOutcome::ShortCircuit { .. } => None,
        }
    }
}

/// Outcome of one task's worker attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskExecutionOutcome {
    Complete {
        result: String,
        /// Structured metadata from `submit_result`; its absence marks a
        /// soft failure (the worker never called the tool).
        structured_output: Option<StructuredTaskOutput>,
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

/// A committed park, as the run loop reports it outward. Constructed only
/// after the `Running -> Parked` CAS succeeds (ADR 2026-07-21, decision 15),
/// so a client that sees it can always retrieve the run it names.
#[derive(Debug, Clone, PartialEq)]
pub struct ParkedRun {
    session: SessionId,
    approvals: NonEmpty<ApprovalRef>,
    parked_at: Timestamp,
    expires_at: Timestamp,
}

/// A park window whose expiry does not come after its park time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a park's expiry must come after its park time")]
pub struct ExpiryNotAfterPark;

impl ParkedRun {
    pub fn new(
        session: SessionId,
        approvals: NonEmpty<ApprovalRef>,
        parked_at: Timestamp,
        expires_at: Timestamp,
    ) -> Result<Self, ExpiryNotAfterPark> {
        if expires_at <= parked_at {
            return Err(ExpiryNotAfterPark);
        }
        Ok(Self {
            session,
            approvals,
            parked_at,
            expires_at,
        })
    }

    /// The reify handle.
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub fn approvals(&self) -> &NonEmpty<ApprovalRef> {
        &self.approvals
    }

    #[must_use]
    pub fn parked_at(&self) -> Timestamp {
        self.parked_at
    }

    #[must_use]
    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
}
