//! Durable park and reify domain types for orchestration runs.
//!
//! Decision record: `docs/adr/2026-07-21-hitl-park-reify.md`. A run that hits
//! an approval gate drains to a quiescent wave boundary, commits one versioned
//! [`RunCheckpoint`] by compare-and-swap, and is later claimed under a fenced
//! [`Lease`] and reified - in the same process or a fresh one.
//!
//! This module holds type definitions and their unit tests only. Wiring these
//! types through the orchestrator, the HITL registry, and the session store is
//! staged behind them (#271); the transition and codec functions carry
//! `todo!()` bodies until that work fills them, and the staged tests here are
//! red against those holes on purpose.

mod checkpoint;
mod dispatch;
mod headers;
mod ids;
mod lease;
mod non_empty;
mod outcomes;
mod run_fsm;
mod session;

pub use checkpoint::{
    ApprovalOriginSnapshot, BlockedTaskBinding, CHECKPOINT_SCHEMA_VERSION, CheckpointCodecError,
    CheckpointEnvelope, ParkedApprovalSnapshot, PodLocalRef, RunCheckpoint,
};
pub use dispatch::{ArgsDigest, DispatchError, DispatchEvent, DispatchState};
pub use headers::{CredentialSource, HeaderClass, IdentityHeader, UnparkableCredential};
pub use ids::{AgentInstanceId, ChatSessionId, ConfigFingerprint, SessionId};
pub use lease::{CasError, FencingGeneration, Lease};
pub use non_empty::{EmptyNonEmpty, NonEmpty};
pub use outcomes::{ApprovalRef, TaskExecutionOutcome, ToolAttemptOutcome, WaveOutcome};
pub use run_fsm::{
    IllegalTransition, ParkReason, ResumePoint, RunEvent, RunFailureCause, RunState, WakeReason,
};
pub use session::{AgentInstance, ParkCommit, Session, SessionRecord};
