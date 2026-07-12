//! S2 golden-frame harness: typed context fixtures, the request-envelope
//! seam, and snapshot normalization.
//!
//! Test-only (`#[cfg(test)]` at the declaration site in
//! `orchestration/mod.rs`): the module lives INSIDE the crate so the
//! `pub(crate)` production seams (`Orchestrator::build_planning_wrapper`,
//! `Orchestrator::compact_decision_turn`, `Orchestrator::build_task_context`,
//! and the three `#[cfg(test)]` accessors in `orchestrator.rs`) are reachable
//! without widening any production visibility.
//!
//! - [`scenario`] — fixture types composing the existing `context` module
//!   types and production state types; every type maps to one business
//!   rule and names the invalid state it forbids.
//! - [`envelope`] — [`RequestEnvelope`] plus builders that call the real
//!   production assembly functions.
//! - [`normalize`] — the two-pass snapshot normalizer and the byte-identity
//!   assertion entry point.
//!
//! The coverage ledger is `context_fixture/MANIFEST.md`; the type design
//! record is `context_fixture/DESIGN.md`. Snapshot tests land in the S2
//! implementation step, alongside deletion of the
//! `frame_validation_tests.rs` cases they subsume.

mod envelope;
mod normalize;
mod scenario;

#[expect(
    unused_imports,
    reason = "S2 facade: consumed by the snapshot tests that land in the S2 implementation step"
)]
pub(crate) use envelope::{RequestEnvelope, coordinator_envelope, worker_envelope};
#[expect(
    unused_imports,
    reason = "S2 facade: consumed by the snapshot tests that land in the S2 implementation step"
)]
pub(crate) use normalize::{NormalizedSnapshot, assert_envelope_snapshot, normalize};
#[expect(
    unused_imports,
    reason = "S2 facade: consumed by the snapshot tests that land in the S2 implementation step"
)]
pub(crate) use scenario::{
    CompletedResultFixture, ContinuationThread, CoordinatorCall, CoordinatorScenario,
    CoordinatorToolConfig, FailedResultFixture, FixtureError, FrameGraph, HistoryTools,
    IterationFixture, PlanDecision, PlanningBudget, PreambleFixture, ReconTools, ScratchpadWiring,
    SessionHistoryFixture, SpilledStandIn, TaskOutcome, WorkerFrameFixture, WorkerPreambleAppends,
    WorkerPreambleFixture, WorkerRosterFixture, WorkerScenario,
};
