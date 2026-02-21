//! Reconnaissance tool for inspecting tool parameter schemas during planning.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;

/// Inspects a specific tool's parameter schema for the coordinator.
///
/// Returns pretty-printed JSON schema, or helpful suggestions if tool not found.
#[derive(Clone)]
pub struct InspectToolParamsTool {
    tool_schemas: HashMap<String, serde_json::Value>,
}

impl InspectToolParamsTool {
    pub fn new(tool_schemas: HashMap<String, serde_json::Value>) -> Self {
        Self { tool_schemas }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct InspectToolParamsArgs {
    pub tool_name: String,
}

#[derive(Debug, Serialize)]
pub struct InspectToolParamsOutput {
    pub found: bool,
    pub tool_name: String,
    /// JSON schema if found, or error message with suggestions if not.
    pub schema: String,
}

impl Tool for InspectToolParamsTool {
    const NAME: &'static str = "inspect_tool_params";

    type Error = Infallible;
    type Args = InspectToolParamsArgs;
    type Output = InspectToolParamsOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Get the parameter schema for a specific tool. Only use when you need \
                 exact parameter details and the tool name alone is insufficient for planning."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "The name of the tool to inspect"
                    }
                },
                "required": ["tool_name"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::info!(
            "Coordinator called inspect_tool_params(\"{}\")",
            args.tool_name
        );
        if let Some(schema) = self.tool_schemas.get(&args.tool_name) {
            let pretty_schema =
                serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
            Ok(InspectToolParamsOutput {
                found: true,
                tool_name: args.tool_name,
                schema: pretty_schema,
            })
        } else {
            // Build helpful error message with suggestions
            let available: Vec<&str> = self.tool_schemas.keys().map(|s| s.as_str()).collect();
            let suggestions = if available.is_empty() {
                "No tools are available.".to_string()
            } else {
                // Find similar tool names (simple substring match)
                let similar: Vec<&str> = available
                    .iter()
                    .filter(|name| {
                        name.to_lowercase().contains(&args.tool_name.to_lowercase())
                            || args.tool_name.to_lowercase().contains(&name.to_lowercase())
                    })
                    .copied()
                    .take(3)
                    .collect();

                if similar.is_empty() {
                    format!("Available tools: {}", available.join(", "))
                } else {
                    format!("Did you mean: {}?", similar.join(", "))
                }
            };

            Ok(InspectToolParamsOutput {
                found: false,
                tool_name: args.tool_name,
                schema: format!("Tool not found. {}", suggestions),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_schemas() -> HashMap<String, serde_json::Value> {
        let mut schemas = HashMap::new();
        schemas.insert(
            "mezmo_list_pipelines".to_string(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "Max pipelines to return" }
                },
                "required": []
            }),
        );
        schemas.insert(
            "mezmo_get_topology".to_string(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pipeline_id": { "type": "string", "description": "Pipeline ID" }
                },
                "required": ["pipeline_id"]
            }),
        );
        schemas
    }

    #[tokio::test]
    async fn test_inspect_found() {
        let tool = InspectToolParamsTool::new(sample_schemas());
        let result = tool
            .call(InspectToolParamsArgs {
                tool_name: "mezmo_list_pipelines".to_string(),
            })
            .await
            .unwrap();

        assert!(result.found);
        assert_eq!(result.tool_name, "mezmo_list_pipelines");
        assert!(result.schema.contains("limit"));
    }

    #[tokio::test]
    async fn test_inspect_not_found() {
        let tool = InspectToolParamsTool::new(sample_schemas());
        let result = tool
            .call(InspectToolParamsArgs {
                tool_name: "nonexistent".to_string(),
            })
            .await
            .unwrap();

        assert!(!result.found);
        assert!(result.schema.contains("Tool not found"));
    }

    #[tokio::test]
    async fn test_inspect_suggestions() {
        let tool = InspectToolParamsTool::new(sample_schemas());
        let result = tool
            .call(InspectToolParamsArgs {
                tool_name: "mezmo".to_string(),
            })
            .await
            .unwrap();

        assert!(!result.found);
        // Should suggest similar tools
        assert!(result.schema.contains("Did you mean"));
    }

    #[tokio::test]
    async fn test_inspect_empty_schemas() {
        let tool = InspectToolParamsTool::new(HashMap::new());
        let result = tool
            .call(InspectToolParamsArgs {
                tool_name: "anything".to_string(),
            })
            .await
            .unwrap();

        assert!(!result.found);
        assert!(result.schema.contains("No tools are available"));
    }

    #[tokio::test]
    async fn test_definition() {
        let tool = InspectToolParamsTool::new(HashMap::new());
        let def = tool.definition("".to_string()).await;
        assert_eq!(def.name, "inspect_tool_params");
        assert!(def.description.contains("parameter schema"));
    }
}
