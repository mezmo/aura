//! Reconnaissance tool for listing available tools during planning.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;

/// Lists all available tool names for the coordinator.
#[derive(Clone)]
pub struct ListToolsTool {
    tool_names: Vec<String>,
}

impl ListToolsTool {
    pub fn new(tool_names: Vec<String>) -> Self {
        Self { tool_names }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ListToolsArgs {}

#[derive(Debug, Serialize)]
pub struct ListToolsOutput {
    pub count: usize,
    pub tools: String,
}

impl Tool for ListToolsTool {
    const NAME: &'static str = "list_tools";

    type Error = Infallible;
    type Args = ListToolsArgs;
    type Output = ListToolsOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "List all available MCP tool names. Only use this if tool names \
                 were not already provided in the planning context above."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::info!(
            "Coordinator called list_tools ({} tools available)",
            self.tool_names.len()
        );
        let tools_list = self.tool_names.join("\n");
        Ok(ListToolsOutput {
            count: self.tool_names.len(),
            tools: tools_list,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_tools_empty() {
        let tool = ListToolsTool::new(vec![]);
        let result = tool.call(ListToolsArgs {}).await.unwrap();
        assert_eq!(result.count, 0);
        assert!(result.tools.is_empty());
    }

    #[tokio::test]
    async fn test_list_tools_with_tools() {
        let tool = ListToolsTool::new(vec![
            "mezmo_list_pipelines".to_string(),
            "mezmo_get_topology".to_string(),
            "QueryKnowledgeBases".to_string(),
        ]);
        let result = tool.call(ListToolsArgs {}).await.unwrap();
        assert_eq!(result.count, 3);
        assert!(result.tools.contains("mezmo_list_pipelines"));
        assert!(result.tools.contains("QueryKnowledgeBases"));
    }

    #[tokio::test]
    async fn test_list_tools_definition() {
        let tool = ListToolsTool::new(vec!["test".to_string()]);
        let def = tool.definition("".to_string()).await;
        assert_eq!(def.name, "list_tools");
        assert!(
            def.description
                .contains("List all available MCP tool names")
        );
    }
}
