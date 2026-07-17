//! Artifact owner module: storage, spill trigger, pointer render, and read tool.
//!
//! This module consolidates the four artifact concerns that were previously split
//! across `persistence.rs`, `orchestrator.rs`, `context/evidence.rs`, and
//! `tools/read_artifact.rs`. The public facade is re-exported from the parent
//! `persistence` module so existing import paths remain unchanged.

pub mod read_tool;
mod spill;
mod storage;

pub use read_tool::ReadArtifactTool;
#[cfg(test)]
pub use read_tool::{ReadArtifactArgs, ReadArtifactError, ReadArtifactOutput};
pub use spill::{ArtifactRef, SpilledArtifact, artifact_kind_from_filename, maybe_spill_result};
pub use storage::{ExecutionPersistence, lock_persistence, sanitize_filename_component};
