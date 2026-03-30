//! Passthrough tool for client-side tool execution.
//!
//! When clients send tool definitions with their chat requests (OpenAI `tools` field),
//! those tools are registered as "passthrough" tools. The LLM can call them, but instead
//! of executing server-side, they return a marker string. The streaming layer detects
//! this marker and yields the tool call back to the client with `finish_reason: "tool_calls"`.
//! The client executes locally and sends results back in the next request.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker string returned by passthrough tools.
/// The streaming layer checks tool results for this value to detect client-side tool calls.
pub const PASSTHROUGH_MARKER: &str = "__AURA_CLIENT_TOOL_PENDING__";

/// A tool that passes through to the client for execution.
///
/// When the LLM calls this tool, it returns `PASSTHROUGH_MARKER` instead of
/// executing anything. The streaming layer detects this marker, suppresses the
/// tool result, and sets `finish_reason: "tool_calls"` so the client knows
/// to execute the tool locally and send results back.
#[derive(Debug, Clone)]
pub struct PassthroughTool {
    tool_name: String,
    description: String,
    parameters: Value,
}

impl PassthroughTool {
    /// Create a new passthrough tool with the given name, description, and parameter schema.
    pub fn new(tool_name: String, description: String, parameters: Value) -> Self {
        Self {
            tool_name,
            description,
            parameters,
        }
    }
}

/// Arguments type that accepts any JSON value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassthroughArgs(pub Value);

/// Error type for passthrough tools (never actually errors).
#[derive(Debug, thiserror::Error)]
#[error("passthrough tool error: {0}")]
pub struct PassthroughError(String);

impl Tool for PassthroughTool {
    const NAME: &'static str = "passthrough_tool";

    type Error = PassthroughError;
    type Args = PassthroughArgs;
    type Output = String;

    /// Override to return the actual tool name (not the static NAME).
    fn name(&self) -> String {
        self.tool_name.clone()
    }

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: self.tool_name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        Ok(PASSTHROUGH_MARKER.to_string())
    }
}
