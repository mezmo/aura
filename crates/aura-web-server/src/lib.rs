//! Aura web server — OpenAI-compatible API for aura agents.

// Counts the cargo-hakari workspace hack as used (cargo::unused_dependencies).
use workspace_hack as _;

pub mod a2a;
pub mod handlers;
pub mod server;
pub mod session_store;
pub mod slack;
pub mod streaming;
pub mod types;
