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
    /// The presented generation is older than the record's current one: a
    /// newer owner exists and the caller must not mutate.
    StaleGeneration {
        presented: FencingGeneration,
        current: FencingGeneration,
    },
    /// The generation was current but the run FSM rejected the event.
    Illegal(IllegalTransition),
}
