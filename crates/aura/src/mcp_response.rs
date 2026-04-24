/*!
 * Shared MCP Tool Response Processing
 *
 * Provides unified handling of MCP tool results across all transport types
 * (HTTP Streamable, STDIO). Handles both structured content (FastMCP)
 * and traditional text-based responses.
 */

use anyhow::Result;
use base64::Engine;
use rmcp::model::CallToolResult;
use tracing::{debug, info, warn};

const MCP_ERROR_PREFIX: &str = "Tool returned an error: ";

/// JSON-RPC 2.0 error codes that indicate input/schema validation failures.
const SCHEMA_ERROR_CODES: [i32; 3] = [
    -32602, // InvalidParams — malformed or missing required arguments
    -32600, // InvalidRequest — structurally invalid JSON-RPC request
    -32700, // ParseError — not valid JSON at all
];

/// Classified outcome of an MCP tool call, preserving error-kind fidelity
/// that would otherwise be lost during stringification.
///
/// Downstream consumers (e.g., duplicate-call guard) can branch on this
/// structurally instead of parsing string prefixes.
#[derive(Debug, Clone, PartialEq)]
pub enum CallOutcome {
    /// Tool executed successfully.
    Success(String),

    /// JSON-RPC protocol-level error indicating input validation failure
    /// (e.g., `-32602 InvalidParams`).
    SchemaError { content: String, code: i32 },

    /// Tool error that is NOT clearly a schema/validation failure.
    /// Includes application-level `is_error: true` responses (ambiguous)
    /// and non-schema JSON-RPC errors (e.g., `-32603 InternalError`).
    GeneralToolError { content: String, code: Option<i32> },
}

impl CallOutcome {
    pub fn content(&self) -> &str {
        match self {
            Self::Success(s) => s,
            Self::SchemaError { content, .. } | Self::GeneralToolError { content, .. } => content,
        }
    }

    pub fn is_error(&self) -> bool {
        !matches!(self, Self::Success(_))
    }

    pub fn is_schema_error(&self) -> bool {
        matches!(self, Self::SchemaError { .. })
    }

    /// Convert to the legacy prefixed-string format consumed by existing callers.
    pub fn into_prefixed_string(self) -> String {
        match self {
            Self::Success(s) => s,
            Self::SchemaError { content, .. } | Self::GeneralToolError { content, .. } => {
                format!("{MCP_ERROR_PREFIX}{content}")
            }
        }
    }

    /// Classify from a prefixed tool-output string (reverse of `into_prefixed_string`).
    /// Used at the WrappedTool boundary where only a String is available.
    pub fn classify_from_output(output: &str) -> Self {
        if let Some(msg) = output.strip_prefix(MCP_ERROR_PREFIX) {
            Self::GeneralToolError {
                content: msg.to_string(),
                code: None,
            }
        } else {
            Self::Success(output.to_string())
        }
    }

    /// Construct from a JSON-RPC error, mapping error codes to the
    /// appropriate variant.
    pub fn from_jsonrpc_error(code: i32, message: &str) -> Self {
        if SCHEMA_ERROR_CODES.contains(&code) {
            Self::SchemaError {
                content: message.to_string(),
                code,
            }
        } else {
            Self::GeneralToolError {
                content: message.to_string(),
                code: Some(code),
            }
        }
    }
}

/// Maximum size (in bytes) for extracted resource content.
/// Content exceeding this limit is truncated with a notice.
const MAX_RESOURCE_CONTENT_BYTES: usize = 100_000;

/// Extract text or decoded blob content from an MCP ResourceContents.
///
/// - `TextResourceContents`: returns the text directly
/// - `BlobResourceContents`: base64-decodes and returns as UTF-8 if the MIME type
///   indicates text (`text/*`, `application/json`, `application/xml`, etc.);
///   otherwise returns a metadata placeholder.
///
/// Content exceeding `MAX_RESOURCE_CONTENT_BYTES` is truncated.
pub fn extract_resource_contents(resource: &rmcp::model::ResourceContents) -> String {
    let raw = match resource {
        rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
        rmcp::model::ResourceContents::BlobResourceContents {
            uri,
            blob,
            mime_type,
            ..
        } => {
            let is_text_mime = mime_type.as_deref().is_some_and(|m| {
                m.starts_with("text/")
                    || m == "application/json"
                    || m == "application/xml"
                    || m == "application/yaml"
            });
            if !is_text_mime {
                return format!(
                    "[Binary resource: {uri} (mime: {})]",
                    mime_type.as_deref().unwrap_or("unknown")
                );
            }
            match base64::engine::general_purpose::STANDARD.decode(blob) {
                Ok(bytes) => String::from_utf8(bytes)
                    .unwrap_or_else(|_| format!("[Binary resource: {uri} (not valid UTF-8)]")),
                Err(_) => {
                    format!("[Binary resource: {uri} (invalid base64)]")
                }
            }
        }
    };

    if raw.len() > MAX_RESOURCE_CONTENT_BYTES {
        let truncated = &raw[..raw.floor_char_boundary(MAX_RESOURCE_CONTENT_BYTES)];
        format!(
            "{truncated}\n\n[Resource truncated: showing {MAX_RESOURCE_CONTENT_BYTES} of {} bytes]",
            raw.len()
        )
    } else {
        raw
    }
}

/// Extract the result from an MCP tool call response
///
/// This function handles two types of MCP tool responses:
/// 1. **Structured Content** (FastMCP with x-fastmcp-wrap-result):
///    - Returns JSON data in `structured_content` field
///    - Used by tools like `list_metrics` that return structured data
///    - Serialized to pretty-printed JSON string
/// 2. **Text Content** (traditional MCP):
///    - Returns text/images/resources in `content` field
///    - Extracted and joined into a single string
///
/// ## Arguments
/// * `result` - The CallToolResult from the MCP server
/// * `tool_name` - Name of the tool (for logging)
///
/// ## Returns
/// * `Ok(String)` - The tool result as a string (JSON for structured, text for unstructured)
/// * `Err(anyhow::Error)` - If result processing fails
///
/// ## FastMCP x-fastmcp-wrap-result Extension
/// FastMCP tools with `x-fastmcp-wrap-result: true` in their outputSchema
/// return results in `structured_content` field. This extension wraps
/// non-object return values in a `{"result": <value>}` structure.
///
/// Example: `list_metrics` tool with outputSchema:
/// ```json
/// {
///   "type": "object",
///   "properties": {
///     "result": {"type": "array", "items": {"type": "string"}}
///   },
///   "x-fastmcp-wrap-result": true
/// }
/// ```
/// Returns: `{"result": ["metric1", "metric2", ...]}`
pub fn extract_tool_result(result: CallToolResult, tool_name: &str) -> Result<CallOutcome> {
    let is_error = result.is_error.unwrap_or(false);
    if is_error {
        warn!("Tool '{}' returned error result", tool_name);
    }

    if let Some(structured) = result.structured_content {
        debug!(
            "Tool '{}' has structured_content: {}",
            tool_name,
            serde_json::to_string(&structured).unwrap_or_else(|_| "invalid".to_string())
        );

        let json_str = serde_json::to_string_pretty(&structured).unwrap_or_else(|e| {
            warn!(
                "Failed to serialize structured_content for '{}': {}",
                tool_name, e
            );
            // Fallback to non-pretty print
            structured.to_string()
        });

        info!(
            "Tool '{}' returned structured content ({} bytes)",
            tool_name,
            json_str.len()
        );
        debug!(
            "   Structured content preview: {}",
            if json_str.len() > 200 {
                format!("{}...", &json_str[..200])
            } else {
                json_str.clone()
            }
        );

        return if is_error {
            Ok(CallOutcome::GeneralToolError {
                content: json_str,
                code: None,
            })
        } else {
            Ok(CallOutcome::Success(json_str))
        };
    }

    debug!(
        "Tool '{}' using text content extraction (no structured_content)",
        tool_name
    );

    let content = result
        .content
        .into_iter()
        .map(|content_item| match content_item.raw {
            rmcp::model::RawContent::Text(text) => text.text,
            rmcp::model::RawContent::Image(img) => {
                format!("[Image: {} ({})]", img.mime_type, img.data.len())
            }
            rmcp::model::RawContent::Resource(res) => extract_resource_contents(&res.resource),
            rmcp::model::RawContent::ResourceLink(link) => {
                format!("[Resource link: {} ({})]", link.name, link.uri)
            }
            _ => "[Unsupported content type]".to_string(),
        })
        .collect::<Vec<String>>()
        .join("\n");

    info!(
        "Tool '{}' returned text content ({} bytes)",
        tool_name,
        content.len()
    );
    debug!(
        "   Content preview: {}",
        if content.len() > 200 {
            format!("{}...", &content[..200])
        } else {
            content.clone()
        }
    );

    if is_error {
        Ok(CallOutcome::GeneralToolError {
            content,
            code: None,
        })
    } else {
        Ok(CallOutcome::Success(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{
        Content, RawContent, RawEmbeddedResource, RawResource, RawTextContent, ResourceContents,
    };
    use serde_json::{Value, json};

    #[test]
    fn test_extract_structured_content() {
        let result = CallToolResult {
            content: vec![],
            structured_content: Some(json!({
                "result": ["metric1", "metric2", "metric3"]
            })),
            is_error: None,
            meta: None,
        };

        let outcome = extract_tool_result(result, "list_metrics").unwrap();
        assert!(!outcome.is_error());
        let extracted = outcome.content();

        assert!(extracted.contains("\"result\""));
        assert!(extracted.contains("metric1"));
        assert!(extracted.contains("metric2"));
        assert!(extracted.contains("metric3"));

        let parsed: Value = serde_json::from_str(extracted).unwrap();
        assert_eq!(parsed["result"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_extract_text_content() {
        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Text(RawTextContent {
                    text: "Hello, world!".to_string(),
                    meta: None,
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        let outcome = extract_tool_result(result, "echo").unwrap();
        assert_eq!(outcome, CallOutcome::Success("Hello, world!".to_string()));
    }

    #[test]
    fn test_prioritizes_structured_over_text() {
        // When both are present, structured_content takes priority
        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Text(RawTextContent {
                    text: "Fallback text".to_string(),
                    meta: None,
                }),
                annotations: None,
            }],
            structured_content: Some(json!({"data": "structured"})),
            is_error: None,
            meta: None,
        };

        let outcome = extract_tool_result(result, "test").unwrap();
        let content = outcome.content();

        assert!(content.contains("structured"));
        assert!(!content.contains("Fallback text"));
    }

    #[test]
    fn test_empty_content() {
        let result = CallToolResult {
            content: vec![],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        let outcome = extract_tool_result(result, "empty").unwrap();
        assert_eq!(outcome, CallOutcome::Success("".to_string()));
    }

    #[test]
    fn test_error_text_content_classified() {
        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Text(RawTextContent {
                    text: "Connection refused".to_string(),
                    meta: None,
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: Some(true),
            meta: None,
        };

        let outcome = extract_tool_result(result, "failing_tool").unwrap();
        assert!(outcome.is_error());
        assert!(!outcome.is_schema_error());
        assert_eq!(outcome.content(), "Connection refused");

        let prefixed = outcome.into_prefixed_string();
        assert!(prefixed.starts_with("Tool returned an error:"));
    }

    #[test]
    fn test_error_structured_content_classified() {
        let result = CallToolResult {
            content: vec![],
            structured_content: Some(
                json!({"error_code": "AUTH_FAILED", "message": "Invalid token"}),
            ),
            is_error: Some(true),
            meta: None,
        };

        let outcome = extract_tool_result(result, "auth_tool").unwrap();
        assert!(outcome.is_error());
        assert!(outcome.content().contains("AUTH_FAILED"));
    }

    #[test]
    fn test_success_not_prefixed() {
        // When is_error is false, the result should NOT be prefixed
        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Text(RawTextContent {
                    text: "Success message".to_string(),
                    meta: None,
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        };

        let outcome = extract_tool_result(result, "test").unwrap();
        assert_eq!(outcome, CallOutcome::Success("Success message".to_string()));
    }

    // --- Embedded Resource extraction tests ---

    #[test]
    fn test_extract_text_resource_content() {
        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Resource(RawEmbeddedResource {
                    meta: None,
                    resource: ResourceContents::TextResourceContents {
                        uri: "repo://owner/repo/contents/README.md".to_string(),
                        mime_type: Some("text/markdown".to_string()),
                        text: "# Hello World\nThis is a readme.".to_string(),
                        meta: None,
                    },
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        let extracted = extract_tool_result(result, "get_file")
            .unwrap()
            .into_prefixed_string();
        assert_eq!(extracted, "# Hello World\nThis is a readme.");
    }

    #[test]
    fn test_extract_blob_resource_text_mime() {
        use base64::Engine;
        let text = "console.log('hello');";
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);

        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Resource(RawEmbeddedResource {
                    meta: None,
                    resource: ResourceContents::BlobResourceContents {
                        uri: "repo://owner/repo/contents/index.js".to_string(),
                        mime_type: Some("text/javascript".to_string()),
                        blob: encoded,
                        meta: None,
                    },
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        let extracted = extract_tool_result(result, "get_file")
            .unwrap()
            .into_prefixed_string();
        assert_eq!(extracted, "console.log('hello');");
    }

    #[test]
    fn test_extract_blob_resource_binary_mime() {
        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Resource(RawEmbeddedResource {
                    meta: None,
                    resource: ResourceContents::BlobResourceContents {
                        uri: "repo://owner/repo/contents/image.png".to_string(),
                        mime_type: Some("image/png".to_string()),
                        blob: "iVBORw0KGgo=".to_string(),
                        meta: None,
                    },
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        let extracted = extract_tool_result(result, "get_file")
            .unwrap()
            .into_prefixed_string();
        assert!(extracted.starts_with("[Binary resource:"));
        assert!(extracted.contains("image/png"));
    }

    #[test]
    fn test_extract_blob_resource_json_mime() {
        use base64::Engine;
        let json_text = r#"{"key": "value"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json_text);

        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Resource(RawEmbeddedResource {
                    meta: None,
                    resource: ResourceContents::BlobResourceContents {
                        uri: "repo://owner/repo/contents/data.json".to_string(),
                        mime_type: Some("application/json".to_string()),
                        blob: encoded,
                        meta: None,
                    },
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        let extracted = extract_tool_result(result, "get_file")
            .unwrap()
            .into_prefixed_string();
        assert_eq!(extracted, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_resource_link_placeholder() {
        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::ResourceLink(RawResource {
                    uri: "repo://owner/repo/contents/big-file.md".to_string(),
                    name: "big-file.md".to_string(),
                    title: None,
                    description: None,
                    mime_type: None,
                    size: None,
                    icons: None,
                    meta: None,
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        let extracted = extract_tool_result(result, "get_file")
            .unwrap()
            .into_prefixed_string();
        assert!(extracted.contains("Resource link:"));
        assert!(extracted.contains("big-file.md"));
        assert!(extracted.contains("repo://"));
    }

    #[test]
    fn test_extract_text_resource_truncation() {
        let large_text = "x".repeat(MAX_RESOURCE_CONTENT_BYTES + 1000);

        let contents = ResourceContents::TextResourceContents {
            uri: "file:///large.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            text: large_text.clone(),
            meta: None,
        };

        let extracted = extract_resource_contents(&contents);
        assert!(extracted.len() < large_text.len());
        assert!(extracted.contains("[Resource truncated:"));
    }

    #[test]
    fn test_mixed_text_and_resource_content() {
        let result = CallToolResult {
            content: vec![
                Content {
                    raw: RawContent::Text(RawTextContent {
                        text: "Successfully downloaded file".to_string(),
                        meta: None,
                    }),
                    annotations: None,
                },
                Content {
                    raw: RawContent::Resource(RawEmbeddedResource {
                        meta: None,
                        resource: ResourceContents::TextResourceContents {
                            uri: "repo://owner/repo/contents/file.md".to_string(),
                            mime_type: Some("text/markdown".to_string()),
                            text: "# File Content".to_string(),
                            meta: None,
                        },
                    }),
                    annotations: None,
                },
            ],
            structured_content: None,
            is_error: None,
            meta: None,
        };

        let extracted = extract_tool_result(result, "get_file")
            .unwrap()
            .into_prefixed_string();
        assert!(extracted.contains("Successfully downloaded file"));
        assert!(extracted.contains("# File Content"));
    }

    // --- CallOutcome tests ---

    #[test]
    fn test_call_outcome_success() {
        let outcome = CallOutcome::Success("result".to_string());
        assert!(!outcome.is_error());
        assert!(!outcome.is_schema_error());
        assert_eq!(outcome.content(), "result");
        assert_eq!(outcome.into_prefixed_string(), "result");
    }

    #[test]
    fn test_call_outcome_schema_error() {
        let outcome = CallOutcome::SchemaError {
            content: "Invalid params".to_string(),
            code: -32602,
        };
        assert!(outcome.is_error());
        assert!(outcome.is_schema_error());
        assert_eq!(outcome.content(), "Invalid params");
        assert_eq!(
            outcome.into_prefixed_string(),
            "Tool returned an error: Invalid params"
        );
    }

    #[test]
    fn test_call_outcome_general_tool_error() {
        let outcome = CallOutcome::GeneralToolError {
            content: "Internal error".to_string(),
            code: Some(-32603),
        };
        assert!(outcome.is_error());
        assert!(!outcome.is_schema_error());
        assert_eq!(outcome.content(), "Internal error");
    }

    #[test]
    fn test_call_outcome_application_level_error_is_general() {
        let result = CallToolResult {
            content: vec![Content {
                raw: RawContent::Text(RawTextContent {
                    text: "rate limited".to_string(),
                    meta: None,
                }),
                annotations: None,
            }],
            structured_content: None,
            is_error: Some(true),
            meta: None,
        };

        let outcome = extract_tool_result(result, "test").unwrap();
        assert!(matches!(
            outcome,
            CallOutcome::GeneralToolError { code: None, .. }
        ));
        assert_eq!(outcome.content(), "rate limited");
    }

    #[test]
    fn test_from_jsonrpc_error_invalid_params() {
        let outcome = CallOutcome::from_jsonrpc_error(-32602, "missing field: x");
        assert!(outcome.is_schema_error());
        assert_eq!(outcome.content(), "missing field: x");
        assert!(matches!(
            outcome,
            CallOutcome::SchemaError { code: -32602, .. }
        ));
    }

    #[test]
    fn test_from_jsonrpc_error_internal() {
        let outcome = CallOutcome::from_jsonrpc_error(-32603, "server crashed");
        assert!(!outcome.is_schema_error());
        assert!(outcome.is_error());
        assert!(matches!(
            outcome,
            CallOutcome::GeneralToolError {
                code: Some(-32603),
                ..
            }
        ));
    }

    #[test]
    fn test_classify_from_output_success() {
        let outcome = CallOutcome::classify_from_output("normal result");
        assert_eq!(outcome, CallOutcome::Success("normal result".to_string()));
    }

    #[test]
    fn test_classify_from_output_error_prefix() {
        let outcome = CallOutcome::classify_from_output("Tool returned an error: 503");
        assert!(outcome.is_error());
        assert_eq!(outcome.content(), "503");
    }

    #[test]
    fn test_classify_roundtrip() {
        let original = CallOutcome::GeneralToolError {
            content: "timeout".to_string(),
            code: None,
        };
        let prefixed = original.clone().into_prefixed_string();
        let reconstructed = CallOutcome::classify_from_output(&prefixed);
        assert_eq!(reconstructed.content(), original.content());
        assert!(reconstructed.is_error());
    }
}
