//! Fallback Tool Call Parser for Ollama Models
//!
//! This module provides parsing functionality for tool calls that Ollama models
//! sometimes output as text content instead of proper tool_call structures.
//!
//! # Problem
//!
//! Ollama models (especially with multi-parameter tools) sometimes output tool calls
//! as text content in various formats:
//! - JSON: `{"name": "tool_name", "parameters": {...}}`
//! - Hermes XML: `<tool_call>{"name": "tool_name", ...}</tool_call>`
//! - Pythonic: `[tool_name(arg1="value")]`
//! - Qwen XML: `<function=tool_name><parameter=arg>value</parameter></function>`
//!
//! # Known Model Issues
//!
//! ## Qwen3-Coder Malformed Output
//!
//! Qwen3-coder models have documented bugs where they produce malformed XML:
//! - Missing `<tool_call>` opening tag
//! - Missing `</parameter>` closing tags
//! - Using `</tool_call>` instead of `</function>` as closing tag
//!
//! Example malformed output from qwen3-coder:30b-128k:
//! ```text
//! <function=browser_get_state> <parameter=include_screenshot> True </tool_call>
//! ```
//!
//! This parser handles these variants. See:
//! - <https://github.com/QwenLM/Qwen3-Coder/issues/475>
//! - <https://github.com/lmstudio-ai/lmstudio-bug-tracker/issues/825>
//! - <https://github.com/ggml-org/llama.cpp/issues/15012>
//!
//! ## Workarounds
//!
//! 1. **System prompt guidance**: Adding explicit format instructions can improve
//!    compliance (~85% improvement reported), but won't eliminate all malformed output.
//!
//! 2. **Updated GGUF files**: Unsloth provides fixed GGUF files with corrected chat
//!    templates: <https://huggingface.co/unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF>
//!
//! 3. **Hermes-style format**: Qwen docs recommend Hermes-style JSON tool calling
//!    for better reliability: <https://qwen.readthedocs.io/en/latest/framework/function_call.html>
//!
//! # Usage
//!
//! ```ignore
//! use aura::{parse_fallback_tool_calls, ParsedToolCall};
//!
//! let available_tools = vec!["quick_tool".to_string(), "slow_task".to_string()];
//! let content = r#"{"name": "quick_tool", "parameters": {"message": "hello"}}"#;
//!
//! if let Some(tool_calls) = parse_fallback_tool_calls(content, &available_tools) {
//!     for call in tool_calls {
//!         println!("Tool: {}, Args: {}", call.name, call.arguments);
//!     }
//! }
//! ```

use crate::string_utils::truncate_for_log;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;
use tracing::{debug, trace};

const EMPTY_JSON_OBJECT: &str = "{}";

/// A parsed tool call extracted from text content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedToolCall {
    pub name: String,
    /// JSON-encoded arguments
    pub arguments: String,
}

/// JSON format tool call structure
#[derive(Debug, Deserialize)]
struct JsonToolCall {
    name: String,
    #[serde(alias = "parameters", alias = "arguments")]
    #[serde(default)]
    args: Option<Value>,
}

// Pre-compiled regex patterns for better performance
// Hermes XML: <tool_call>{...json...}</tool_call>
// Uses greedy [\s\S]* to capture full JSON including nested braces.
// The </tool_call> boundary ensures we don't over-match.
static HERMES_TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<tool_call>\s*(\{[\s\S]*\})\s*</tool_call>").expect("Invalid regex pattern")
});

static PYTHONIC_TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[([a-zA-Z_][a-zA-Z0-9_]*)\((.*?)\)\]").expect("Invalid regex pattern")
});

// Qwen XML function format: <function=name>params</function>
// Also handles qwen3-coder bug where </tool_call> is used instead of </function>
// See: https://github.com/QwenLM/Qwen3-Coder/issues/475
static QWEN_FUNCTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<function=([a-zA-Z_][a-zA-Z0-9_]*)>([\s\S]*?)(?:</function>|</tool_call>)")
        .expect("Invalid regex pattern")
});

static QWEN_PARAMETER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<parameter=([a-zA-Z_][a-zA-Z0-9_]*)>\s*([\s\S]*?)\s*</parameter>")
        .expect("Invalid regex pattern")
});

// Alternate format: <parameter=name> value (no closing tag)
// Used by qwen3-coder which has known bugs with malformed XML output.
// Captures value until next `<` tag. Works with captures_iter for multiple params.
// See module docs for issue links and workarounds.
static QWEN_PARAMETER_OPEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<parameter=([a-zA-Z_][a-zA-Z0-9_]*)>\s*([^<]+)").expect("Invalid regex pattern")
});

/// Parse text content for fallback tool calls.
///
/// Attempts to extract tool calls from text content that models may have
/// output instead of proper structured tool calls. Only matches tools in
/// `available_tools`.
///
/// # Supported Formats
///
/// 1. **JSON**: `{"name": "tool_name", "parameters": {...}}` or `{"name": "tool_name", "arguments": {...}}`
/// 2. **Hermes XML**: `<tool_call>{"name": "tool_name", ...}</tool_call>`
/// 3. **Pythonic**: `[tool_name(arg1="value", arg2=123)]` (llama3.2 format)
/// 4. **Qwen XML**: `<function=tool_name><parameter=arg>value</parameter></function>` (qwen3 format)
pub fn parse_fallback_tool_calls(
    content: &str,
    available_tools: &[String],
) -> Option<Vec<ParsedToolCall>> {
    let content = content.trim();

    if content.is_empty() {
        return None;
    }

    trace!(
        "Attempting to parse fallback tool calls from content: {}...",
        truncate_for_log(content, 100).0
    );

    // Try each format in order of likelihood
    // 1. JSON format (most common for Ollama)
    if let Some(calls) = try_parse_json(content, available_tools) {
        debug!("Parsed {} tool call(s) using JSON format", calls.len());
        return Some(calls);
    }

    // 2. Hermes XML format
    if let Some(calls) = try_parse_hermes(content, available_tools) {
        debug!(
            "Parsed {} tool call(s) using Hermes XML format",
            calls.len()
        );
        return Some(calls);
    }

    // 3. Pythonic format
    if let Some(calls) = try_parse_pythonic(content, available_tools) {
        debug!("Parsed {} tool call(s) using Pythonic format", calls.len());
        return Some(calls);
    }

    // 4. Qwen XML format
    if let Some(calls) = try_parse_qwen(content, available_tools) {
        debug!("Parsed {} tool call(s) using Qwen XML format", calls.len());
        return Some(calls);
    }

    trace!("No tool calls found in content");
    None
}

/// Try to parse JSON format tool calls.
///
/// Supports:
/// - `{"name": "...", "parameters": {...}}`
/// - `{"name": "...", "arguments": {...}}`
/// - Multiple JSON objects on separate lines
fn try_parse_json(content: &str, available_tools: &[String]) -> Option<Vec<ParsedToolCall>> {
    let mut results = Vec::new();

    // First, try to parse the entire content as a single JSON object
    if let Ok(call) = serde_json::from_str::<JsonToolCall>(content) {
        if available_tools.contains(&call.name) {
            let arguments = call
                .args
                .map(|v| serde_json::to_string(&v).unwrap_or_default())
                .unwrap_or_else(|| EMPTY_JSON_OBJECT.to_string());

            results.push(ParsedToolCall {
                name: call.name,
                arguments,
            });
            return Some(results);
        }
    }

    // Try to find JSON objects within the content (handle markdown code blocks, etc.)
    // Look for patterns like ```json ... ``` or standalone JSON
    let json_content = extract_json_from_content(content);
    for json_str in json_content {
        if let Ok(call) = serde_json::from_str::<JsonToolCall>(&json_str) {
            if available_tools.contains(&call.name) {
                let arguments = call
                    .args
                    .map(|v| serde_json::to_string(&v).unwrap_or_default())
                    .unwrap_or_else(|| EMPTY_JSON_OBJECT.to_string());

                results.push(ParsedToolCall {
                    name: call.name,
                    arguments,
                });
            }
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// Extract JSON strings from content, handling code blocks and inline JSON.
fn extract_json_from_content(content: &str) -> Vec<String> {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Try to extract from markdown code blocks
    static CODE_BLOCK_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"```(?:json)?\s*\n?([\s\S]*?)\n?```").expect("Invalid regex"));

    for cap in CODE_BLOCK_RE.captures_iter(content) {
        if let Some(json_content) = cap.get(1) {
            let json_str = json_content.as_str().trim().to_string();
            if !json_str.is_empty() && seen.insert(json_str.clone()) {
                results.push(json_str);
            }
        }
    }

    // Also try to find standalone JSON objects (brace-matching)
    // Skip if we already found JSON in code blocks
    if results.is_empty() {
        let chars: Vec<char> = content.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            if chars[i] == '{' {
                // Try to find matching closing brace
                if let Some(json_str) = extract_balanced_braces(&chars, i) {
                    // Verify it's valid JSON with a "name" field
                    if json_str.contains("\"name\"") && seen.insert(json_str.clone()) {
                        results.push(json_str);
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
    }

    results
}

/// Extract a balanced JSON object starting at the given position.
fn extract_balanced_braces(chars: &[char], start: usize) -> Option<String> {
    if start >= chars.len() || chars[start] != '{' {
        return None;
    }

    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, &ch) in chars.iter().enumerate().skip(start) {
        if escape_next {
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if in_string => {
                escape_next = true;
            }
            '"' => {
                in_string = !in_string;
            }
            '{' if !in_string => {
                depth += 1;
            }
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(chars[start..=i].iter().collect());
                }
            }
            _ => {}
        }
    }

    None
}

/// Try to parse Hermes XML format tool calls.
///
/// Format: `<tool_call>{"name": "...", ...}</tool_call>`
fn try_parse_hermes(content: &str, available_tools: &[String]) -> Option<Vec<ParsedToolCall>> {
    let mut results = Vec::new();

    for cap in HERMES_TOOL_CALL_RE.captures_iter(content) {
        if let Some(json_match) = cap.get(1) {
            if let Ok(call) = serde_json::from_str::<JsonToolCall>(json_match.as_str()) {
                if available_tools.contains(&call.name) {
                    let arguments = call
                        .args
                        .map(|v| serde_json::to_string(&v).unwrap_or_default())
                        .unwrap_or_else(|| EMPTY_JSON_OBJECT.to_string());

                    results.push(ParsedToolCall {
                        name: call.name,
                        arguments,
                    });
                }
            }
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// Try to parse Pythonic format tool calls (llama3.2 format).
///
/// Format: `[tool_name(arg1="value", arg2=123)]`
fn try_parse_pythonic(content: &str, available_tools: &[String]) -> Option<Vec<ParsedToolCall>> {
    let mut results = Vec::new();

    for cap in PYTHONIC_TOOL_CALL_RE.captures_iter(content) {
        let tool_name = cap.get(1).map(|m| m.as_str().to_string());
        let args_str = cap.get(2).map(|m| m.as_str());

        if let (Some(name), Some(args)) = (tool_name, args_str) {
            if available_tools.contains(&name) {
                // Parse Python-style arguments into JSON
                let arguments = parse_pythonic_args(args);
                results.push(ParsedToolCall { name, arguments });
            }
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// Parse Python-style function arguments into a JSON string.
///
/// Handles: `arg1="value", arg2=123, arg3=true`
fn parse_pythonic_args(args_str: &str) -> String {
    let mut result: HashMap<String, Value> = HashMap::new();

    if args_str.trim().is_empty() {
        return EMPTY_JSON_OBJECT.to_string();
    }

    // Simple regex-based parsing for key=value pairs
    static ARG_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(\w+)\s*=\s*(?:"([^"]*)"|'([^']*)'|(\d+(?:\.\d+)?)|(\w+))"#)
            .expect("Invalid regex")
    });

    for cap in ARG_RE.captures_iter(args_str) {
        if let Some(key) = cap.get(1) {
            let key = key.as_str().to_string();

            // Try each capture group in order: double-quoted, single-quoted, number, identifier
            let value = if let Some(s) = cap.get(2) {
                Value::String(s.as_str().to_string())
            } else if let Some(s) = cap.get(3) {
                Value::String(s.as_str().to_string())
            } else if let Some(n) = cap.get(4) {
                // Try to parse as number
                if let Ok(i) = n.as_str().parse::<i64>() {
                    Value::Number(i.into())
                } else if let Ok(f) = n.as_str().parse::<f64>() {
                    serde_json::Number::from_f64(f)
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                } else {
                    Value::String(n.as_str().to_string())
                }
            } else if let Some(ident) = cap.get(5) {
                // Handle boolean-like identifiers
                match ident.as_str().to_lowercase().as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "none" | "null" => Value::Null,
                    other => Value::String(other.to_string()),
                }
            } else {
                continue;
            };

            result.insert(key, value);
        }
    }

    serde_json::to_string(&result).unwrap_or_else(|_| EMPTY_JSON_OBJECT.to_string())
}

/// Try to parse Qwen XML format tool calls (qwen3 format).
///
/// Format: `<function=tool_name><parameter=arg>value</parameter>...</function>`
fn try_parse_qwen(content: &str, available_tools: &[String]) -> Option<Vec<ParsedToolCall>> {
    let mut results = Vec::new();

    for cap in QWEN_FUNCTION_RE.captures_iter(content) {
        let tool_name = cap.get(1).map(|m| m.as_str().to_string());
        let params_content = cap.get(2).map(|m| m.as_str());

        if let (Some(name), Some(params_str)) = (tool_name, params_content) {
            if available_tools.contains(&name) {
                let arguments = parse_qwen_parameters(params_str);
                results.push(ParsedToolCall { name, arguments });
            }
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// Parse Qwen-style XML parameters into a JSON string.
///
/// Handles two formats:
/// - Closed: `<parameter=name>value</parameter>`
/// - Open: `<parameter=name> value` (no closing tag, used by qwen3-coder)
fn parse_qwen_parameters(params_str: &str) -> String {
    let mut result: HashMap<String, Value> = HashMap::new();

    // First try the standard closed format: <parameter=name>value</parameter>
    for cap in QWEN_PARAMETER_RE.captures_iter(params_str) {
        if let (Some(key), Some(value)) = (cap.get(1), cap.get(2)) {
            let key = key.as_str().to_string();
            let value_str = value.as_str().trim();
            result.insert(key, parse_parameter_value(value_str));
        }
    }

    // Fallback: if no closed-format parameters found, try open format.
    // We don't mix formats to avoid double-parsing edge cases.
    if result.is_empty() {
        for cap in QWEN_PARAMETER_OPEN_RE.captures_iter(params_str) {
            if let (Some(key), Some(value)) = (cap.get(1), cap.get(2)) {
                let key = key.as_str().to_string();
                let value_str = value.as_str().trim();
                result.insert(key, parse_parameter_value(value_str));
            }
        }
    }

    serde_json::to_string(&result).unwrap_or_else(|_| EMPTY_JSON_OBJECT.to_string())
}

/// Parse a parameter value string into a JSON Value.
///
/// Coerces string values to appropriate JSON types:
/// - Integers: "123" -> Number(123)
/// - Floats: "1.5" -> Number(1.5)
/// - Booleans: "true"/"false" (case-insensitive) -> Bool
/// - Null: "null"/"none" (case-insensitive) -> Null
/// - Everything else -> String
fn parse_parameter_value(value_str: &str) -> Value {
    // Try to parse as JSON value (number, bool, null), otherwise string
    if let Ok(n) = value_str.parse::<i64>() {
        Value::Number(n.into())
    } else if let Ok(f) = value_str.parse::<f64>() {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::String(value_str.to_string()))
    } else {
        match value_str.to_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" | "none" => Value::Null,
            _ => Value::String(value_str.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tools() -> Vec<String> {
        vec![
            "quick_tool".to_string(),
            "slow_task_with_progress".to_string(),
            "get_weather".to_string(),
            "searxng_web_search".to_string(),
        ]
    }

    #[test]
    fn test_json_format_with_parameters() {
        let content = r#"{"name": "quick_tool", "parameters": {"message": "hello"}}"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "quick_tool");
        assert_eq!(calls[0].arguments, r#"{"message":"hello"}"#);
    }

    #[test]
    fn test_json_format_with_arguments() {
        let content = r#"{"name": "quick_tool", "arguments": {"message": "hello"}}"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "quick_tool");
        assert_eq!(calls[0].arguments, r#"{"message":"hello"}"#);
    }

    #[test]
    fn test_json_format_complex_arguments() {
        let content = r#"{"name": "slow_task_with_progress", "parameters": {"duration_seconds": 5, "steps": 10}}"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "slow_task_with_progress");

        // Parse the arguments to verify
        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["duration_seconds"], 5);
        assert_eq!(args["steps"], 10);
    }

    #[test]
    fn test_json_format_in_markdown_code_block() {
        let content = r#"I'll call the tool for you:

```json
{"name": "quick_tool", "parameters": {"message": "test"}}
```
"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "quick_tool");
    }

    #[test]
    fn test_json_format_no_arguments() {
        let content = r#"{"name": "quick_tool"}"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "quick_tool");
        assert_eq!(calls[0].arguments, "{}");
    }

    #[test]
    fn test_hermes_xml_format() {
        let content =
            r#"<tool_call>{"name": "quick_tool", "parameters": {"message": "hello"}}</tool_call>"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "quick_tool");
        assert_eq!(calls[0].arguments, r#"{"message":"hello"}"#);
    }

    #[test]
    fn test_hermes_xml_format_with_whitespace() {
        let content = r#"<tool_call>
  {"name": "quick_tool", "parameters": {"message": "hello"}}
</tool_call>"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "quick_tool");
    }

    #[test]
    fn test_hermes_xml_multiple_tool_calls() {
        let content = r#"<tool_call>{"name": "quick_tool", "parameters": {"message": "first"}}</tool_call>
<tool_call>{"name": "get_weather", "parameters": {"city": "NYC"}}</tool_call>"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "quick_tool");
        assert_eq!(calls[1].name, "get_weather");
    }

    #[test]
    fn test_hermes_xml_nested_json() {
        // Regression test: nested JSON objects must be captured fully
        // Previously failed with lazy .*? which stopped at first }
        let content = r#"<tool_call>{"name": "quick_tool", "parameters": {"nested": {"deep": "value"}}}</tool_call>"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some(), "Should parse nested JSON");
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "quick_tool");

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["nested"]["deep"], "value");
    }

    #[test]
    fn test_pythonic_format_simple() {
        let content = r#"[get_weather(city="NYC")]"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["city"], "NYC");
    }

    #[test]
    fn test_pythonic_format_multiple_args() {
        let content = r#"[slow_task_with_progress(duration_seconds=5, steps=10)]"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "slow_task_with_progress");

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["duration_seconds"], 5);
        assert_eq!(args["steps"], 10);
    }

    #[test]
    fn test_pythonic_format_boolean_args() {
        let content = r#"[quick_tool(message="test", enabled=true)]"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["message"], "test");
        assert_eq!(args["enabled"], true);
    }

    #[test]
    fn test_pythonic_format_single_quotes() {
        let content = r#"[get_weather(city='London')]"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["city"], "London");
    }

    #[test]
    fn test_unknown_tool_rejected() {
        let content = r#"{"name": "unknown_tool", "parameters": {"message": "hello"}}"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_none(), "Unknown tool should be rejected");
    }

    #[test]
    fn test_empty_content_returns_none() {
        let result = parse_fallback_tool_calls("", &test_tools());
        assert!(result.is_none());

        let result = parse_fallback_tool_calls("   ", &test_tools());
        assert!(result.is_none());
    }

    #[test]
    fn test_plain_text_returns_none() {
        let content = "Hello, I can help you with that. Let me think about it.";
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(
            result.is_none(),
            "Plain text should not be parsed as tool call"
        );
    }

    #[test]
    fn test_malformed_json_returns_none() {
        let content = r#"{"name": "quick_tool", "parameters": {"message": }"#; // Invalid JSON
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_none(), "Malformed JSON should not parse");
    }

    #[test]
    fn test_json_without_name_returns_none() {
        let content = r#"{"parameters": {"message": "hello"}}"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_none(), "JSON without name field should not parse");
    }

    #[test]
    fn test_json_embedded_in_text() {
        let content = r#"Sure, I'll call the tool now. {"name": "quick_tool", "parameters": {"message": "hello"}} Done!"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "quick_tool");
    }

    #[test]
    fn test_nested_json_in_arguments() {
        let content = r#"{"name": "quick_tool", "parameters": {"data": {"nested": "value"}}}"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["data"]["nested"], "value");
    }

    #[test]
    fn test_empty_available_tools() {
        let content = r#"{"name": "quick_tool", "parameters": {"message": "hello"}}"#;
        let result = parse_fallback_tool_calls(content, &[]);

        assert!(result.is_none(), "Should reject when no tools available");
    }

    #[test]
    fn test_qwen_xml_format() {
        let content = r#"<function=searxng_web_search>
<parameter=query>companies closing in Wallace NH</parameter>
<parameter=language>en</parameter>
</function>"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "searxng_web_search");

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["query"], "companies closing in Wallace NH");
        assert_eq!(args["language"], "en");
    }

    #[test]
    fn test_qwen_xml_format_with_numbers() {
        let content = r#"<function=searxng_web_search>
<parameter=query>test query</parameter>
<parameter=pageno>1</parameter>
<parameter=safesearch>1</parameter>
</function>"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["query"], "test query");
        assert_eq!(args["pageno"], 1);
        assert_eq!(args["safesearch"], 1);
    }

    #[test]
    fn test_qwen_xml_format_with_trailing_tool_call_tag() {
        // Real-world example from qwen3-coder with trailing </tool_call>
        let content = r#"<function=searxng_web_search>
<parameter=language>
en
</parameter>
<parameter=pageno>
1
</parameter>
<parameter=query>
companies closing or downsizing in Wallace NH
</parameter>
<parameter=safesearch>
1
</parameter>
<parameter=time_range>
month
</parameter>
</function>
</tool_call>"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "searxng_web_search");

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["language"], "en");
        assert_eq!(args["pageno"], 1);
        assert_eq!(
            args["query"],
            "companies closing or downsizing in Wallace NH"
        );
        assert_eq!(args["safesearch"], 1);
        assert_eq!(args["time_range"], "month");
    }

    #[test]
    fn test_qwen_xml_format_unknown_tool() {
        let content = r#"<function=unknown_tool>
<parameter=arg>value</parameter>
</function>"#;
        let result = parse_fallback_tool_calls(content, &test_tools());

        assert!(result.is_none(), "Unknown tool should be rejected");
    }

    #[test]
    fn test_qwen_xml_format_open_parameters_with_tool_call_closing() {
        // Real output from qwen3-coder:30b-128k (LOG-22998)
        // Model omits </parameter> closing tags and uses </tool_call> instead of </function>
        let content =
            r#"<function=browser_get_state> <parameter=include_screenshot> True   </tool_call>"#;

        let tools = vec!["browser_get_state".to_string()];
        let result = parse_fallback_tool_calls(content, &tools);

        assert!(result.is_some(), "Should parse qwen3-coder format");
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "browser_get_state");

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["include_screenshot"], true);
    }

    #[test]
    fn test_qwen_xml_format_open_parameters_multiple() {
        // Synthetic test for multiple open-format parameters in same call
        // Validates regex correctly captures each param until next `<` tag
        let content = r#"<function=browser_navigate> <parameter=url> https://example.com <parameter=wait_for_load> true </tool_call>"#;

        let tools = vec!["browser_navigate".to_string()];
        let result = parse_fallback_tool_calls(content, &tools);

        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "browser_navigate");

        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["url"], "https://example.com");
        assert_eq!(args["wait_for_load"], true);
    }
}
