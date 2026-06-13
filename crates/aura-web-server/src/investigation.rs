//! Webhook-triggered investigation pipeline.
//!
//! - `handler` exposes `POST /v1/launch_investigation`. Validates auth headers,
//!   creates an investigation record in ai-history-service, returns 202, and
//!   spawns the runner.
//! - `runner` drives the Aura streaming agent (single-agent mode) with the
//!   `finalize_investigation` tool attached, PATCHing ai-history-service as
//!   state advances.
//! - `client` is a thin reqwest wrapper for the ai-history-service `internal/investigation` API.

pub mod client;
pub mod finalize;
pub mod handler;
pub mod runner;

pub use client::{
    AiHistoryClient, AiHistoryError, CreateInvestigationRequest, InvestigationRecord,
    UpdateInvestigationRequest,
};
pub use finalize::{FinalizeArguments, parse_finalize_tail};
pub use handler::launch_investigation;
pub use runner::run_investigation;

/// Five auth headers the webhook requires; all forwarded as-is to ai-history-service.
pub const REQUIRED_AUTH_HEADERS: &[&str] = &[
    "x-auth-subject-id",
    "x-auth-subject-email",
    "x-auth-account-id",
    "x-auth-account-short-id",
    "x-auth-account-plan",
];
