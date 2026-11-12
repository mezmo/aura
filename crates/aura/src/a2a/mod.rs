//! A2A client side: calling a remote AURA agent as a tool.
//!
//! The web server crate hosts the A2A *server* (the receiving half). This
//! module is the *sending* half: [`A2aClient`] speaks the v1.0 JSON-RPC
//! binding to a remote agent's `/a2a/v1/rpc` endpoint, and
//! [`RemoteAgentTool`] exposes `[a2a.remote.<name>]` entries to the model
//! through one `ask_agent` tool: every entry for a single agent, and for an
//! orchestration worker the ones its `remotes` list names.

mod client;
#[cfg(test)]
pub(crate) mod test_server;
mod tool;

pub use aura_config::ASK_AGENT_TOOL_NAME;
pub use client::{A2aClient, A2aClientError};
pub use tool::{AskAgentOutcome, RemoteAgent, RemoteAgentTool};
