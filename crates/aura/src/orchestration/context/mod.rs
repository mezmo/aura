//! Typed context frames for the coordinator context redesign.
//!
//! This module is the type home for the redesigned coordinator and worker
//! context shapes: the pinned goal line, evidence-framed per-task entries,
//! compact coordinator decision turns, failure-history handles, and the
//! worker dependency-context frame. The shapes are specified by the
//! coordinator context redesign architecture document
//! (`docs/redesign/ARCHITECTURE.md` in the program repo), and every public
//! type here maps to one business rule in `docs/redesign/TYPE_PLAN.md`.
//!
//! # Skeleton status
//!
//! This is the R2 type skeleton: signatures are the contract, and every body
//! that carries parsing or rendering logic is `todo!()` until the
//! implementation cards land (R3a continuation rendering, R3b decision
//! turns, R3c worker frame). Plain data definitions are complete. The two
//! implemented `Display` bodies ([`SpilledArtifact`], [`ArtifactRef`])
//! reproduce today's artifact footer and inventory line formats, which the
//! architecture pins as unchanged.
//!
//! # Design rules
//!
//! - Parse, don't validate: fallible constructors return [`ContextError`];
//!   downstream code only handles already-valid values.
//! - No bare `String` or `usize` domain value crosses this module's public
//!   boundary. Raw text and numbers enter only through parsing constructors;
//!   validated values leave through read-only accessors and `Display`.
//! - Coordinator-authored task descriptions have no field anywhere in this
//!   module except the truncated [`FailureHandle`]. Replaying imperative
//!   task text next to worker evidence is unrepresentable by construction.

mod error;
mod evidence;
mod failure_history;
mod frame;
mod goal;
mod label;
mod rendered;
mod turn;

pub use error::ContextError;
pub use evidence::{
    ArtifactRef, ArtifactStandIn, BlockedEntry, CompletedEntry, ErrorPreview, EvidenceEntry,
    EvidenceText, FailedEntry, FailureReport, ResultPreview, SpilledArtifact,
};
pub use failure_history::{FailureHandle, FailureRecord};
pub use frame::{
    AncestorDistance, DependencyRelation, PriorWorkEntry, PriorWorkFrame, TokenBudget,
};
pub use goal::PinnedGoal;
pub use label::{CorrelationLabel, IterationNumber, TaskId, WorkerClaim, WorkerRole};
pub use rendered::RenderedContext;
pub use turn::{
    ClarificationQuestion, CoordinatorTurn, FinalResponse, PlanShape, RoutingRationale,
};
