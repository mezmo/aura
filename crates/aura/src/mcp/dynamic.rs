use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rig::tool::{Tool as RigTool, ToolError};
use rmcp::model::Tool as McpTool;
use serde_json::Value;

use crate::approver_headers::{
    McpTransportKind, current_approver_overrides, ensure_transport_delivers_overrides,
};
use crate::mcp::client::McpClient;
use crate::mcp::execution::execute_mcp_tool;

/// Dynamic MCP Tool Adaptor for MCP clients (transport-agnostic)
#[derive(Clone)]
pub struct McpToolAdaptor {
    tool: McpTool,
    #[allow(dead_code)]
    server_name: String,
    client: Arc<McpClient>,
    /// Which transport this adaptor fronts, tagged at construction.
    transport_kind: McpTransportKind,
}

impl McpToolAdaptor {
    /// `transport_kind` must name the transport `client` actually fronts;
    /// the type cannot enforce it because one `McpClient` serves all three
    /// transports. Construction sites are the three `add_all_tools`
    /// branches in `builder.rs`; a mistag bypasses the stdio fail-closed
    /// check.
    pub fn new(
        tool: McpTool,
        server_name: String,
        client: Arc<McpClient>,
        transport_kind: McpTransportKind,
    ) -> Self {
        Self {
            tool,
            server_name,
            client,
            transport_kind,
        }
    }
}

impl RigTool for McpToolAdaptor {
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
        let transport_kind = self.transport_kind;

        Box::pin(async move {
            // Approver header overrides for this call, if the HITL gate
            // captured any. Unscoped reads yield `None` (wrapper-less
            // agents).
            let approver_overrides = current_approver_overrides();
            if approver_overrides.is_some() {
                // Fail closed before dispatch when the transport cannot
                // deliver per-request headers.
                ensure_transport_delivers_overrides(transport_kind)
                    .map_err(|e| ToolError::ToolCallError(Box::new(e)))?;
            }

            // Use shared execution function for consistent logging and error handling
            execute_mcp_tool(&client, &tool_name, args, approver_overrides).await
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mcp::client::tests::client_and_server;

    /// An adaptor for `tool_name`, fronting `client`, tagged as `kind`. The client is always streamable-HTTP: the tag, not the wire, is what the override path consults.
    async fn adaptor_for(
        client: McpClient,
        tool_name: &str,
        kind: McpTransportKind,
    ) -> McpToolAdaptor {
        let tool = rmcp::model::Tool::new(
            tool_name.to_owned(),
            "test tool".to_owned(),
            std::sync::Arc::new(serde_json::Map::new()),
        );
        McpToolAdaptor::new(tool, "test-server".to_owned(), Arc::new(client), kind)
    }

    #[tokio::test]
    async fn stdio_adaptor_runs_an_ungated_call() {
        let (server, client) = client_and_server(&std::collections::HashMap::new()).await;
        let adaptor = adaptor_for(client, "ungated", McpTransportKind::Stdio).await;

        adaptor
            .call(json!({}))
            .await
            .expect("an ungated stdio call proceeds");

        assert_eq!(server.tool_calls().len(), 1);
    }
}
