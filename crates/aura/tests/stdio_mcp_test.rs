#![cfg(feature = "integration-stdio")]

//! Integration test for STDIO MCP transport using the official
//! `@modelcontextprotocol/server-everything` test fixture.
//!
//! Requires `mcp-server-everything` to be installed and on PATH.
//! In CI this is pre-installed in the Docker test image.

use aura::{config::McpServerConfig, mcp::McpManager};
use std::collections::HashMap;

const EVERYTHING_BIN: &str = "mcp-server-everything";

#[tokio::test]
async fn test_stdio_mcp_connection_and_tool_execution() {
    // This test is gated by `--features integration-stdio`.
    // If the fixture binary is missing, fail loudly rather than silently skip.
    let binary_ok = std::process::Command::new("which")
        .arg(EVERYTHING_BIN)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        binary_ok,
        "{EVERYTHING_BIN} not found on PATH. \
         Install with: npm install -g @modelcontextprotocol/server-everything@2026.1.26"
    );

    let mcp_config = aura::config::McpConfig {
        sanitize_schemas: false,
        servers: [(
            "test_stdio".to_string(),
            McpServerConfig::Stdio {
                cmd: vec![EVERYTHING_BIN.to_string()],
                args: vec!["stdio".to_string()],
                env: HashMap::new(),
                description: Some("Everything MCP server for STDIO testing".to_string()),
                scratchpad: HashMap::new(),
            },
        )]
        .into_iter()
        .collect(),
    };

    let manager = McpManager::initialize_from_config(&mcp_config)
        .await
        .expect("Failed to initialize STDIO MCP manager");

    // Verify server connected
    assert_eq!(
        manager.server_info.len(),
        1,
        "Expected one server_info entry"
    );
    assert_eq!(manager.stdio_clients.len(), 1, "Expected one stdio client");
    assert!(
        !manager.stdio_tools.is_empty(),
        "Expected at least one stdio tool"
    );

    // Verify tool discovered
    let tool_names = manager.get_available_tool_names();
    assert!(
        tool_names.contains(&"echo".to_string()),
        "Expected 'echo' tool to be discovered"
    );

    // Verify per-server tracking
    let tools_per_server = manager.tool_names_per_server();
    assert!(
        tools_per_server.contains_key("test_stdio"),
        "Expected per-server tracking for 'test_stdio'"
    );
    assert!(
        tools_per_server["test_stdio"].contains(&"echo".to_string()),
        "Expected 'echo' in per-server tools"
    );

    // Verify tool execution via fallback path
    let result = manager
        .execute_fallback_tool("echo", r#"{"message": "hello stdio"}"#)
        .await
        .expect("Failed to execute echo tool");
    assert!(
        result.contains("hello stdio"),
        "Tool result should contain echoed message. Got: {result}"
    );
}
