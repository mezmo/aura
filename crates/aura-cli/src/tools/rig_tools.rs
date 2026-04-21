//! Adapts CLI local tools to rig's `Tool` trait for standalone mode.
//!
//! Each CLI tool (Shell, Read, ListFiles, etc.) is wrapped as a rig `Tool`
//! so it can be registered on an aura Agent alongside MCP tools. The agent's
//! internal multi-turn loop then handles all tool execution automatically.

use aura::RigTool;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::tools::definitions::client_tool_definitions;

/// Error type for CLI tool execution.
#[derive(Debug, thiserror::Error)]
pub enum CliToolError {
    #[error("{0}")]
    ExecutionError(String),
}

/// A single CLI tool adapted to rig's `Tool` trait.
///
/// Uses dynamic dispatch via `name()` override (same pattern as
/// `HttpMcpToolAdaptor` and `DynamicVectorSearchTool` in the aura crate).
pub struct CliToolAdaptor {
    tool_name: String,
    description: String,
    parameters: serde_json::Value,
}

/// Generic args — the CLI tools already parse JSON internally,
/// so we just pass the raw JSON object through.
#[derive(Debug, Deserialize, Serialize)]
pub struct CliToolArgs(BTreeMap<String, serde_json::Value>);

impl RigTool for CliToolAdaptor {
    const NAME: &'static str = "cli_tool_adaptor";

    type Error = CliToolError;
    type Args = CliToolArgs;
    type Output = String;

    fn name(&self) -> String {
        self.tool_name.clone()
    }

    async fn definition(&self, _prompt: String) -> aura::RigToolDefinition {
        aura::RigToolDefinition {
            name: self.tool_name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let args_json = serde_json::to_string(&args.0)
            .map_err(|e| CliToolError::ExecutionError(format!("Failed to serialize args: {e}")))?;

        crate::tools::execute_tool(&self.tool_name, &args_json)
            .map_err(|e| CliToolError::ExecutionError(e.to_string()))
    }
}

/// Create rig `Tool` adaptors for all CLI local tools (except CompactContext,
/// which is a meta-operation handled by the REPL).
///
/// Returns a vec of boxed `ToolDyn` objects ready to register on an agent.
pub fn cli_tools_as_rig_tools() -> Vec<Box<dyn aura::ToolDyn>> {
    let definitions = client_tool_definitions();

    definitions
        .into_iter()
        .filter(|def| {
            // CompactContext is a REPL meta-operation, not an LLM tool in standalone mode
            def.function.name != "CompactContext"
        })
        .map(|def| {
            let adaptor = CliToolAdaptor {
                tool_name: def.function.name,
                description: def.function.description,
                parameters: def.function.parameters,
            };
            Box::new(adaptor) as Box<dyn aura::ToolDyn>
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // cli_tools_as_rig_tools
    // -----------------------------------------------------------------------

    #[test]
    fn cli_tools_returns_seven() {
        let tools = cli_tools_as_rig_tools();
        assert_eq!(tools.len(), 7, "expected 7 tools (8 minus CompactContext)");
    }

    #[test]
    fn excludes_compact_context() {
        let tools = cli_tools_as_rig_tools();
        let names: Vec<String> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"CompactContext".to_string()));
    }

    #[test]
    fn expected_tool_names() {
        let tools = cli_tools_as_rig_tools();
        let mut names: Vec<String> = tools.iter().map(|t| t.name()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "FileInfo",
                "FindFiles",
                "ListFiles",
                "Read",
                "SearchFiles",
                "Shell",
                "Update"
            ]
        );
    }

    // -----------------------------------------------------------------------
    // CliToolAdaptor
    // -----------------------------------------------------------------------

    #[test]
    fn adaptor_name_returns_tool_name() {
        let adaptor = CliToolAdaptor {
            tool_name: "Shell".to_string(),
            description: "test".to_string(),
            parameters: serde_json::json!({}),
        };
        // name() overrides const NAME and returns the dynamic tool_name
        assert_eq!(RigTool::name(&adaptor), "Shell");
    }

    #[tokio::test]
    async fn adaptor_definition_has_correct_fields() {
        let adaptor = CliToolAdaptor {
            tool_name: "Read".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let def = RigTool::definition(&adaptor, "test".to_string()).await;
        assert_eq!(def.name, "Read");
        assert_eq!(def.description, "Read a file");
        assert_eq!(def.parameters, serde_json::json!({"type": "object"}));
    }

    #[tokio::test]
    async fn adaptor_call_shell_echo() {
        let adaptor = CliToolAdaptor {
            tool_name: "Shell".to_string(),
            description: "".to_string(),
            parameters: serde_json::json!({}),
        };
        let mut args = BTreeMap::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("echo hello_test".to_string()),
        );
        let result = RigTool::call(&adaptor, CliToolArgs(args)).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("hello_test"));
    }

    #[tokio::test]
    async fn adaptor_call_read_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "line1\nline2\n").unwrap();

        let adaptor = CliToolAdaptor {
            tool_name: "Read".to_string(),
            description: "".to_string(),
            parameters: serde_json::json!({}),
        };
        let mut args = BTreeMap::new();
        args.insert(
            "file_path".to_string(),
            serde_json::Value::String(file_path.to_string_lossy().to_string()),
        );
        let result = RigTool::call(&adaptor, CliToolArgs(args)).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
    }

    #[tokio::test]
    async fn adaptor_call_missing_args_returns_error() {
        let adaptor = CliToolAdaptor {
            tool_name: "Read".to_string(),
            description: "".to_string(),
            parameters: serde_json::json!({}),
        };
        // Missing required "file_path" argument
        let result = RigTool::call(&adaptor, CliToolArgs(BTreeMap::new())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn tool_dyn_call_shell() {
        let tools = cli_tools_as_rig_tools();
        let shell = tools.iter().find(|t| t.name() == "Shell").unwrap();
        let result = shell
            .call(r#"{"command":"echo dyn_test"}"#.to_string())
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("dyn_test"));
    }

    #[tokio::test]
    async fn tool_dyn_call_invalid_json_returns_error() {
        let tools = cli_tools_as_rig_tools();
        let shell = tools.iter().find(|t| t.name() == "Shell").unwrap();
        let result = shell.call("not valid json".to_string()).await;
        assert!(result.is_err());
    }
}
