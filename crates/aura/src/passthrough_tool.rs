//! Passthrough tool for client-side tool execution.
//!
//! When clients send tool definitions with their chat requests (OpenAI `tools` field),
//! those tools are registered as "passthrough" tools on the agent. The LLM can call them,
//! but instead of executing server-side, they return a marker string. The streaming layer
//! detects this marker, suppresses the tool result, and emits `finish_reason: "tool_calls"`
//! so the client can execute the tool locally and send results back.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker string returned by passthrough tools.
///
/// The streaming layer checks tool results for this value to detect that a
/// client-side tool was called. Chosen to be unlikely to collide with real
/// tool output.
pub const PASSTHROUGH_MARKER: &str = "__AURA_CLIENT_TOOL_PENDING__";

/// A tool that defers execution to the client.
///
/// The LLM sees this tool as a normal callable tool and may invoke it. When it does,
/// `call()` returns `PASSTHROUGH_MARKER` instead of actually executing anything. The
/// streaming layer recognizes the marker, suppresses the result, and ends the stream
/// with `finish_reason: "tool_calls"` so the caller (the client) can execute the tool
/// locally and submit the result back in the next request.
#[derive(Debug, Clone)]
pub struct PassthroughTool {
    tool_name: String,
    description: String,
    parameters: Value,
}

impl PassthroughTool {
    /// Create a passthrough tool from a client-supplied tool definition.
    pub fn new(tool_name: String, description: String, parameters: Value) -> Self {
        Self {
            tool_name,
            description,
            parameters,
        }
    }
}

/// Argument shape that accepts any JSON value.
///
/// Passthrough tools never inspect arguments — execution happens on the client —
/// so we accept anything the LLM emits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassthroughArgs(pub Value);

/// Error type for passthrough tools. Never actually raised today — `call()` is
/// infallible — but rig's `Tool` trait requires an associated error type.
#[derive(Debug, thiserror::Error)]
#[error("passthrough tool error: {0}")]
pub struct PassthroughError(String);

impl Tool for PassthroughTool {
    const NAME: &'static str = "passthrough_tool";

    type Error = PassthroughError;
    type Args = PassthroughArgs;
    type Output = String;

    /// Override the static `NAME` so each registered tool reports its real name.
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
