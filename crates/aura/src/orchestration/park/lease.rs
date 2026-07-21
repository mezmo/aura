//! Session lease and fencing (ADR 2026-07-21, decision 5).

use serde::{Deserialize, Serialize};

use crate::hitl::Timestamp;

use super::ids::AgentInstanceId;
use super::run_fsm::IllegalTransition;

/// Monotonic fencing generation for a session.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct FencingGeneration(u64);

impl FencingGeneration {
    pub const INITIAL: Self = Self(0);

    /// The generation a successful claim advances to.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Exclusive session ownership held by one [`super::AgentInstance`].
///
/// `generation` is the fencing token issued to the holder; while the lease
/// is held it always equals the record's current generation (fenced
/// mutations rewrite both in the same commit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub holder: AgentInstanceId,
    pub acquired_at: Timestamp,
    pub heartbeat_at: Timestamp,
    pub expires_at: Timestamp,
    pub generation: FencingGeneration,
}

/// A rejected session mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum CasError {
    /// The presented generation is not exactly the record's current one.
    /// Older means a newer owner exists; newer means a token the record
    /// never issued. Both must be rejected.
    GenerationMismatch {
        presented: FencingGeneration,
        current: FencingGeneration,
    },
    /// The generation was current but the run FSM rejected the event.
    Illegal(IllegalTransition),
    /// The record's state does not admit the operation (for example,
    /// parking a session whose run is not `Running`).
    StateMismatch { actual: &'static str },
}
