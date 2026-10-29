//! Model clients aura calls directly, outside a Rig agent.
//!
//! Rig agents cover the chat-completion providers; a model here has its own
//! API shape and is driven by aura code, with any agent-facing tools
//! packaged beside its client.

pub mod jev;
