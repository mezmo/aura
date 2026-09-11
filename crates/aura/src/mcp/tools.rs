//! Legacy static and semi-static `RigTool` wrappers around [`McpClient`].
//!
//! Superseded by [`crate::mcp::McpToolAdaptor`], which the connection paths in
//! `manager.rs` and `builder.rs` use for every discovered tool regardless of
//! transport. Nothing in this crate constructs the types below anymore; they
//! are kept only for external callers that may still reference them and are
//! `#[deprecated]` to steer new code away from them.

use rig::{completion::ToolDefinition, tool::Tool as RigTool};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info};

use crate::mcp::client::McpClient;
use crate::mcp::execution::preview_response;
use crate::mcp::manager::McpManager;

/// Simple error type for tool execution
#[derive(Debug)]
#[deprecated(
    since = "0.2.12",
    note = "only used by the deprecated static/fallback MCP tool wrappers in this module; use rig::tool::ToolError via crate::mcp::McpToolAdaptor instead"
)]
pub struct ToolExecutionError {
    message: String,
}

#[allow(deprecated)]
impl std::fmt::Display for ToolExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tool execution error: {}", self.message)
    }
}

#[allow(deprecated)]
impl std::error::Error for ToolExecutionError {}

#[allow(deprecated)]
impl From<String> for ToolExecutionError {
    fn from(message: String) -> Self {
        Self { message }
    }
}

#[allow(deprecated)]
impl From<&str> for ToolExecutionError {
    fn from(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

/// Macro to create unique MCP tool structs for each tool name
/// This is required because Rig's Tool trait requires each tool to have a unique static NAME
macro_rules! create_mcp_tool_struct {
    ($struct_name:ident, $tool_name:literal) => {
        #[derive(Clone)]
        #[deprecated(
            since = "0.2.12",
            note = "unused static MCP tool wrapper; use crate::mcp::McpToolAdaptor instead"
        )]
        pub struct $struct_name {
            pub tool_name: String,
            pub server_name: String,
            pub client: Arc<McpClient>,
            pub tool_definition: ToolDefinition,
        }

        #[allow(deprecated)]
        impl $struct_name {
            pub fn new(
                tool_name: String,
                server_name: String,
                client: Arc<McpClient>,
                mcp_tool: rmcp::model::Tool,
                sanitize_schemas: bool,
            ) -> Option<Self> {
                // Use centralized conversion method
                let tool_definition =
                    McpManager::convert_tool_to_rig_definition(&mcp_tool, sanitize_schemas)?;
                Some(Self {
                    tool_name,
                    server_name,
                    client,
                    tool_definition,
                })
            }
        }

        #[allow(deprecated)]
        impl RigTool for $struct_name {
            const NAME: &'static str = $tool_name;

            type Error = ToolExecutionError;
            type Args = Value;
            type Output = String;

            async fn definition(&self, _prompt: String) -> ToolDefinition {
                self.tool_definition.clone()
            }

            async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
                debug!(
                    "{}::call - Tool: {}, Server: {}",
                    stringify!($struct_name),
                    self.tool_name,
                    self.server_name
                );
                info!(
                    "Calling tool '{}' on server '{}'",
                    self.tool_name, self.server_name
                );

                let arguments = match args {
                    Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
                    _ => {
                        return Err(ToolExecutionError::from(
                            "Invalid arguments: expected JSON object",
                        ));
                    }
                };

                debug!("  Arguments: {:?}", arguments);

                match self
                    .client
                    .call_tool(&self.tool_name, arguments, None)
                    .await
                {
                    Ok(response) => {
                        debug!("  Response: {}", response);
                        let response_summary = preview_response(&response, 200);
                        info!("Tool '{}' completed: {}", self.tool_name, response_summary);
                        Ok(response)
                    }
                    Err(e) => {
                        let error_msg = format!("MCP tool call failed: {}", e);
                        info!("❌ Tool '{}' failed: {}", self.tool_name, e);
                        Err(ToolExecutionError::from(error_msg))
                    }
                }
            }
        }
    };
}

// Create unique tool structs for each known Mezmo tool
create_mcp_tool_struct!(ExportLogsRelativeTimeTool, "export_logs_relative_time");
create_mcp_tool_struct!(ExportLogsTimeRangeTool, "export_logs_time_range");
create_mcp_tool_struct!(
    AnalyzeLogsRelativeTimeTool,
    "analyze_logs_for_root_cause_relative_time"
);
create_mcp_tool_struct!(
    AnalyzeLogsTimeRangeTool,
    "analyze_logs_for_root_cause_time_range"
);
create_mcp_tool_struct!(GetCurrentTimeTool, "get_current_time");
create_mcp_tool_struct!(GetPipelineTool, "get_pipeline");
create_mcp_tool_struct!(ListPipelinesTool, "list_pipelines");

/// Generic fallback for unknown tools - A Rig-compatible tool wrapper for MCP client tools
#[derive(Clone)]
#[deprecated(
    since = "0.2.12",
    note = "unused; use crate::mcp::McpToolAdaptor instead"
)]
pub struct StreamableHttpMcpTool {
    pub tool_name: String,
    pub server_name: String,
    pub client: Arc<McpClient>,
    pub tool_definition: ToolDefinition,
}

#[allow(deprecated)]
impl StreamableHttpMcpTool {
    pub fn new(
        tool_name: String,
        server_name: String,
        client: Arc<McpClient>,
        mcp_tool: rmcp::model::Tool,
        sanitize_schemas: bool,
    ) -> Option<Self> {
        // Use centralized conversion method from McpManager
        let tool_definition =
            McpManager::convert_tool_to_rig_definition(&mcp_tool, sanitize_schemas)?;

        Some(Self {
            tool_name,
            server_name,
            client,
            tool_definition,
        })
    }
}

#[allow(deprecated)]
impl RigTool for StreamableHttpMcpTool {
    const NAME: &'static str = "streamable_http_mcp_tool";

    type Error = ToolExecutionError;
    type Args = Value;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        self.tool_definition.clone()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        debug!(
            "StreamableHttpMcpTool::call - Tool: {}, Server: {}",
            self.tool_name, self.server_name
        );
        info!(
            "Calling HTTP streamable tool '{}' on server '{}'",
            self.tool_name, self.server_name
        );

        // Convert Value to HashMap<String, Value> for the streamable client
        let arguments = match args {
            Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
            _ => {
                return Err(ToolExecutionError::from(
                    "Invalid arguments: expected JSON object",
                ));
            }
        };

        debug!("  Arguments: {:?}", arguments);

        // Call the streamable HTTP client
        match self
            .client
            .call_tool(&self.tool_name, arguments, None)
            .await
        {
            Ok(result) => {
                debug!("  Tool execution successful");
                let response_summary = preview_response(&result, 200);
                info!(
                    "HTTP streamable tool '{}' completed: {}",
                    self.tool_name, response_summary
                );
                Ok(result)
            }
            Err(err) => {
                info!(
                    "❌ HTTP streamable tool '{}' failed: {}",
                    self.tool_name, err
                );
                Err(ToolExecutionError::from(format!(
                    "Tool execution failed: {err}"
                )))
            }
        }
    }
}

#[allow(deprecated)]
impl McpManager {
    /// Get Rig-compatible tools for streamable HTTP MCP clients
    #[deprecated(
        since = "0.2.12",
        note = "unused; connection paths build crate::mcp::McpToolAdaptor for every discovered tool instead"
    )]
    pub fn get_streamable_http_tools(&self) -> Vec<StreamableHttpMcpTool> {
        let mut tools = Vec::new();

        for (server_name, client) in &self.streamable_clients {
            if let Some(server_tools) = self.streamable_tools.get(server_name) {
                debug!(
                    "Processing {} streamable HTTP tools for server: {}",
                    server_tools.len(),
                    server_name
                );

                for mcp_tool in server_tools {
                    if let Some(rig_tool) = StreamableHttpMcpTool::new(
                        mcp_tool.name.to_string(),
                        server_name.clone(),
                        Arc::new(client.clone()),
                        mcp_tool.clone(),
                        self.sanitize_schemas,
                    ) {
                        tools.push(rig_tool);
                    }
                    // If None, tool was rejected due to invalid schema - already logged as warning
                }
            } else {
                debug!("No tools found for streamable server: {}", server_name);
            }
        }

        debug!(
            "Created {} Rig-compatible tools from streamable HTTP clients",
            tools.len()
        );
        tools
    }
}

/// Create a fallback tool with a unique name to avoid Claude API "Tool names must be unique" errors
///
/// Returns None if the tool has an invalid schema and should be rejected.
#[deprecated(
    since = "0.2.12",
    note = "unused; use crate::mcp::McpToolAdaptor instead"
)]
#[allow(deprecated)]
pub fn create_fallback_http_tool(
    unique_tool_name: String,
    original_tool_name: String,
    server_name: String,
    client: Arc<McpClient>,
    mcp_tool: rmcp::model::Tool,
    sanitize_schemas: bool,
) -> Option<FallbackHttpMcpTool> {
    FallbackHttpMcpTool::new(
        unique_tool_name,
        original_tool_name,
        server_name,
        client,
        mcp_tool,
        sanitize_schemas,
    )
}

/// Fallback tool with dynamic unique naming to avoid Claude API conflicts
#[derive(Clone)]
#[deprecated(
    since = "0.2.12",
    note = "unused; use crate::mcp::McpToolAdaptor instead"
)]
pub struct FallbackHttpMcpTool {
    pub unique_name: String,
    pub original_tool_name: String,
    pub server_name: String,
    pub client: Arc<McpClient>,
    pub tool_definition: ToolDefinition,
}

#[allow(deprecated)]
impl FallbackHttpMcpTool {
    pub fn new(
        unique_name: String,
        original_tool_name: String,
        server_name: String,
        client: Arc<McpClient>,
        mcp_tool: rmcp::model::Tool,
        sanitize_schemas: bool,
    ) -> Option<Self> {
        // Use centralized conversion method from McpManager
        let mut tool_definition =
            McpManager::convert_tool_to_rig_definition(&mcp_tool, sanitize_schemas)?;

        // Override the name with our unique name to avoid conflicts
        tool_definition.name = unique_name.clone();

        Some(Self {
            unique_name,
            original_tool_name,
            server_name,
            client,
            tool_definition,
        })
    }
}

#[allow(deprecated)]
impl RigTool for FallbackHttpMcpTool {
    const NAME: &'static str = "fallback_http_mcp_tool";

    type Error = ToolExecutionError;
    type Args = Value;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        self.tool_definition.clone()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        debug!(
            "FallbackHttpMcpTool::call - Original Tool: {}, Unique Name: {}, Server: {}",
            self.original_tool_name, self.unique_name, self.server_name
        );
        info!(
            "Calling fallback HTTP tool '{}' (original: '{}') on server '{}'",
            self.unique_name, self.original_tool_name, self.server_name
        );

        // Convert Value to HashMap<String, Value> for the streamable client
        let arguments = match args {
            Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
            _ => {
                return Err(ToolExecutionError::from(
                    "Invalid arguments: expected JSON object",
                ));
            }
        };

        debug!("  Arguments: {:?}", arguments);

        // Call the streamable HTTP client using the original tool name
        match self
            .client
            .call_tool(&self.original_tool_name, arguments, None)
            .await
        {
            Ok(result) => {
                debug!("  Fallback tool execution successful");
                let response_summary = preview_response(&result, 200);
                info!(
                    "Fallback HTTP tool '{}' completed: {}",
                    self.unique_name, response_summary
                );
                Ok(result)
            }
            Err(err) => {
                info!(
                    "❌ Fallback HTTP tool '{}' failed: {}",
                    self.unique_name, err
                );
                Err(ToolExecutionError::from(format!(
                    "Fallback tool execution failed: {err}"
                )))
            }
        }
    }
}
