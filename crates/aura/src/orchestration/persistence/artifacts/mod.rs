//! Artifact storage, spill trigger, pointer render, and the read tool for the
//! orchestration coordinator.
//!
//! The public facade is re-exported from the parent `persistence` module so
//! existing import paths remain unchanged.

pub mod read_tool;
mod spill;
mod storage;

pub use read_tool::ReadArtifactTool;
#[cfg(test)]
pub use read_tool::{ReadArtifactArgs, ReadArtifactError, ReadArtifactOutput};
pub use spill::{ArtifactRef, SpilledArtifact, artifact_kind_from_filename, maybe_spill_result};
pub use storage::{ExecutionPersistence, lock_persistence, sanitize_filename_component};
