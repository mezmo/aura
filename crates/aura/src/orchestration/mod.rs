//! Orchestration module for multi-agent workflows.
//!
//! This module provides configuration and types for orchestrated agent execution,
//! where a coordinator decomposes tasks into subtasks and manages worker agents.
//!
//! # Enabling Orchestration
//!
//! Set `orchestration.enabled = true` in your config to use orchestrated mode:
//!
//! ```toml
//! [orchestration]
//! enabled = true
//! max_planning_cycles = 3
//! quality_threshold = 0.8
//! ```
//!
//! When disabled (default), the standard single-agent streaming is used.
//!
//! # Architecture
//!
//! The orchestrator implements `StreamingAgent`, allowing it to be used as a
//! drop-in replacement for the standard `Agent`. It coordinates:
//!
//! 1. **Coordinator** - decomposes queries into plans
//! 2. **Workers** - execute individual tasks
//! 3. **Synthesizer** - combines results into final response
//!
//! # Example Usage
//!
//! ```ignore
//! use aura::{AgentConfig, Orchestrator, StreamingAgent};
//!
//! let config = AgentConfig::from_file("config.toml")?;
//! let agent: Box<dyn StreamingAgent> = if config.orchestration_enabled() {
//!     Box::new(Orchestrator::new(config).await?)
//! } else {
//!     Box::new(Agent::new(&config).await?)
//! };
//!
//! let stream = agent.stream(query, history, cancel_token).await?;
//! ```

mod config;
mod duplicate_call_guard;
mod events;
mod memory_fs;
mod memory_writer;
mod observer_wrapper;
mod orchestrator;
mod persistence;
mod persistence_wrapper;
mod prompt_constants;
mod prompt_journal;
mod stream_events;
mod templates;
pub mod tools;
mod types;

pub use config::{
    ArtifactsConfig, MemoryConfig, OrchestrationConfig, TimeoutsConfig, ToolVisibility,
    WorkerConfig,
};
pub use events::{OrchestratorEvent, RoutingMode};
pub use observer_wrapper::ObserverWrapper;
pub use orchestrator::Orchestrator;
pub use persistence::{
    ExecutionPersistence, RunManifest, RunStatus, TaskExecutionRecord, TaskSummary, ToolCallRecord,
    build_session_context, load_session_manifests,
};
pub use persistence_wrapper::PersistenceWrapper;
pub use stream_events::EventContext;
pub use stream_events::OrchestrationStreamEvent;
pub use stream_events::event_names;
pub use tools::GetConversationContextTool;
pub use tools::ReadArtifactTool;

pub use prompt_constants::{context, fields, sections};
pub use types::{
    Phase, PhaseContinuation, PhaseJson, Plan, PlanAttemptFailure, PlanningResponse, StepInput,
    Task, TaskJson, TaskStatus,
};
