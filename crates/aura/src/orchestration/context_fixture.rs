//! S2 golden-frame harness: typed context fixtures, the request-envelope
//! seam, and snapshot normalization.
//!
//! Test-only (`#[cfg(test)]` at the declaration site in
//! `orchestration/mod.rs`): the module lives INSIDE the crate so the
//! `pub(crate)` production seams (`Orchestrator::build_planning_wrapper`,
//! `Orchestrator::compact_decision_turn`, `Orchestrator::build_task_context`,
//! and the four `#[cfg(test)]` accessors in `orchestrator.rs`) are reachable
//! without widening any production visibility.
//!
//! - [`scenario`] — fixture types composing the existing `context` module
//!   types and production state types; every type maps to one business
//!   rule and names the invalid state it forbids.
//! - [`envelope`] — `RequestEnvelope` plus builders that call the real
//!   production assembly functions.
//! - [`normalize`] — the two-pass snapshot normalizer and the byte-identity
//!   assertion entry point.
//!
//! The coverage ledger is `context_fixture/MANIFEST.md`; the type design
//! record is `context_fixture/DESIGN.md`. The snapshot corpus and the
//! REQUIRED R3/R5 comparison gates live in [`corpus`]; the
//! `frame_validation_tests.rs` cases the corpus subsumes were deleted in
//! the S2 implementation step.

mod corpus;
mod envelope;
mod normalize;
mod scenario;

pub(crate) use envelope::{coordinator_envelope, worker_envelope};
pub(crate) use normalize::{NormalizedSnapshot, assert_envelope_snapshot, normalize};
pub(crate) use scenario::{
    CompletedResultFixture, ContinuationThread, CoordinatorCall, CoordinatorScenario,
    CoordinatorToolConfig, FailedResultFixture, FixtureError, FrameGraph, HistoryTools,
    IterationFixture, PlanDecision, PlanningBudget, PreambleFixture, ReconTools, ScratchpadWiring,
    SessionHistoryFixture, SpilledStandIn, TaskOutcome, WorkerFrameFixture, WorkerPreambleAppends,
    WorkerPreambleFixture, WorkerRosterFixture, WorkerScenario,
};
