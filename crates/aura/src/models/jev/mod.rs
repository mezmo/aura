//! TypeSafe's Jev (System One) decision model.
//!
//! Jev answers typed questions about a JSON state — choice (pick an option),
//! score (rate against a rubric), noul (P(yes) for a statement) — with
//! probability distributions, not text. The client, wire format and answer
//! validation come from the [`jev_driver`] crate; this module holds what is
//! aura's own: `decision` lowers config and tool questions into a
//! jev-driver decision, `answer` is the serializable view of its answers,
//! `worker` pairs a configured worker with its client, and `tools` exposes
//! Jev to the coordinator.

mod answer;
mod decision;
pub mod tools;
mod worker;

#[cfg(test)]
pub(crate) mod fake;

pub use answer::{Answer, AnsweredQuestion, Answers};
pub use decision::Decision;
pub use jev_driver::JevError;
pub use jev_driver::client::{DEFAULT_MODEL, JevClient, RetryConfig};
pub use tools::{DecisionRequest, DecisionRequestRef, DecisionRequestStore, DecisionToolset};
pub use worker::{JevWorker, JevWorkers};
