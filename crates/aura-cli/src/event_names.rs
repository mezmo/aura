//! SSE event-name constants used by the CLI's stream consumer.
//!
//! Re-exported from [`aura_events`], the shared source of truth: base
//! `aura.*` names from [`aura_events::event_names`] and orchestrator
//! `aura.orchestrator.*` names from [`aura_events::orchestration::event_names`].
//! The two namespaces have no overlapping const names, so both are flattened
//! here for one-import access at call sites.

pub use aura_events::event_names::*;
pub use aura_events::orchestration::event_names::*;
