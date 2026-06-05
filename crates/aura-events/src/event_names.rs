//! SSE event-name constants for the base `aura.*` namespace.
//!
//! Orchestration events are kept separate in
//! [`crate::orchestration::event_names`] under the `aura.orchestrator.*`
//! namespace.

pub const SESSION_INFO: &str = "aura.session_info";
pub const TOOL_REQUESTED: &str = "aura.tool_requested";
pub const TOOL_START: &str = "aura.tool_start";
pub const TOOL_COMPLETE: &str = "aura.tool_complete";
pub const REASONING: &str = "aura.reasoning";
pub const PROGRESS: &str = "aura.progress";
pub const WORKER_PHASE: &str = "aura.worker_phase";
pub const TOOL_USAGE: &str = "aura.tool_usage";
pub const USAGE: &str = "aura.usage";
pub const SCRATCHPAD_USAGE: &str = "aura.scratchpad_usage";
