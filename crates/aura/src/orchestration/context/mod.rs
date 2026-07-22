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
//! # Implementation status
//!
//! R2 landed this module as a type skeleton; card R3a implemented the
//! continuation-rendering bodies: the parsing constructors in `goal`,
//! `label`, `evidence`, and `failure_history`, and the render methods
//! for completed, failed, and blocked entries and failure records.
//! Card R3b implemented the decision-turn bodies (`turn`); card R3c
//! implemented the worker prior-work frame (`frame`) and the
//! `EvidenceEntry::ArtifactPointerOnly` variant for spilled results with
//! no inline content. The
//! format-bearing `Display` bodies ([`SpilledArtifact`], [`ArtifactRef`])
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
mod named_check;
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
pub use named_check::{CheckIdentity, CheckOutcome, CheckResult, NamedCheck};
pub use rendered::RenderedContext;
pub use turn::{
    ClarificationQuestion, CoordinatorTurn, FinalResponse, PlanShape, RoutingRationale,
};
