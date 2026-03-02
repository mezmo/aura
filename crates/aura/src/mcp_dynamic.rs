use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rig::tool::{Tool as RigTool, ToolError};
use rmcp::model::Tool as McpTool;
use serde_json::Value;

use crate::mcp_streamable_http::StreamableHttpMcpClient;
use crate::mcp_tool_execution::execute_http_mcp_tool;

/// Dynamic MCP Tool Adaptor for HTTP Streamable clients
#[derive(Clone)]
pub struct HttpMcpToolAdaptor {
    tool: McpTool,
    #[allow(dead_code)]
    server_name: String,
    client: Arc<StreamableHttpMcpClient>,
}

impl HttpMcpToolAdaptor {
    pub fn new(tool: McpTool, server_name: String, client: Arc<StreamableHttpMcpClient>) -> Self {
        Self {
            tool,
            server_name,
            client,
        }
    }
}

impl RigTool for HttpMcpToolAdaptor {
    type Error = ToolError;
    type Args = Value;
    type Output = String;

    const NAME: &'static str = "dynamic_http_mcp_tool";

    fn name(&self) -> String {
        // Tool name is already sanitized at build time
        self.tool.name.to_string()
    }

    #[allow(refining_impl_trait)]
    fn definition(
        &self,
        _prompt: String,
    ) -> Pin<Box<dyn Future<Output = rig::completion::ToolDefinition> + Send + Sync + '_>> {
        // Tool is already sanitized - just extract the cached schema
        let tool_name = self.tool.name.to_string();
        let description = self
            .tool
            .description
            .as_deref()
            .unwrap_or_default()
            .to_string();
        let parameters = self.tool.schema_as_json_value();

        Box::pin(async move {
            rig::completion::ToolDefinition {
                name: tool_name,
                description,
                parameters,
            }
        })
    }

    #[allow(refining_impl_trait)]
    fn call(
        &self,
        args: Self::Args,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + Sync + '_>> {
        let tool_name = self.tool.name.clone();
        let client = self.client.clone();

        Box::pin(async move {
            // Use shared execution function for consistent logging and error handling
            execute_http_mcp_tool(&client, &tool_name, args).await
        })
    }
}
