//! Persistence tool wrapper for orchestration.
//!
//! Wraps MCP tools to capture reasoning and persist execution details.
//! The wrapper modifies tool schemas to require a `_aura_reasoning` field,
//! extracts it during execution, and writes records to ExecutionPersistence.
//!
//! This wrapper is only used for orchestrator workers, not regular agents.
//!
//! ## Implementation
//!
//! This module provides `PersistenceWrapper`, which implements the generic
//! `ToolWrapper` trait. It can be used with `WrappedTool<T>` to wrap any
//! Rig-compatible tool.

use async_trait::async_trait;
use rig::tool::ToolError;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::persistence::{ExecutionPersistence, ToolCallRecord};
use crate::tool_wrapper::{
    ToolCallContext, ToolWrapper, TransformArgsResult, TransformOutputResult,
};

/// The namespaced field name for reasoning (signals framework/internal field).
const REASONING_FIELD: &str = "_aura_reasoning";

/// Tool wrapper that captures reasoning and persists execution details.
///
/// This wrapper:
/// 1. Modifies tool schemas to add a required `_aura_reasoning` field
/// 2. Extracts reasoning from args before calling the inner tool
/// 3. Records execution details to ExecutionPersistence via `on_complete`
///
/// Only used in orchestration mode for worker agents.
#[derive(Clone)]
pub struct PersistenceWrapper {
    /// Shared persistence manager for writing records
    persistence: Arc<Mutex<ExecutionPersistence>>,
}

impl PersistenceWrapper {
    pub fn new(persistence: Arc<Mutex<ExecutionPersistence>>) -> Self {
        Self { persistence }
    }
}

#[async_trait]
impl ToolWrapper for PersistenceWrapper {
    fn wrap_schema(&self, mut schema: Value) -> Value {
        add_reasoning_to_schema(&mut schema);
        schema
    }

    fn transform_args(&self, args: Value, _ctx: &ToolCallContext) -> TransformArgsResult {
        let (reasoning, clean_args) = extract_reasoning(args);

        // Store reasoning in extracted field for use in on_complete
        let extracted = reasoning.map(|r| {
            serde_json::json!({
                "reasoning": r
            })
        });

        TransformArgsResult {
            args: clean_args,
            extracted,
        }
    }

    fn validate_args(
        &self,
        _args: &Value,
        extracted: Option<&Value>,
        ctx: &ToolCallContext,
    ) -> Result<(), ToolError> {
        let reasoning = extracted
            .and_then(|v| {
                // Handle both direct object and array from ComposedWrapper
                if let Some(arr) = v.as_array() {
                    arr.iter()
                        .find_map(|item| item.get("reasoning"))
                        .and_then(|v| v.as_str())
                } else {
                    v.get("reasoning").and_then(|v| v.as_str())
                }
            })
            .unwrap_or("");

        if reasoning.trim().is_empty() {
            tracing::warn!(
                tool = %ctx.tool_name,
                "Rejected tool call: empty _aura_reasoning"
            );
            return Err(ToolError::ToolCallError(
                "REQUIRED: You MUST provide a non-empty '_aura_reasoning' field explaining \
                 your reasoning for this tool call. Retry this tool call with a detailed \
                 reasoning string describing what you are trying to accomplish."
                    .into(),
            ));
        }

        Ok(())
    }

    fn transform_output(
        &self,
        output: String,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> TransformOutputResult {
        // Output passes through unchanged
        TransformOutputResult::new(output)
    }

    fn handle_error(
        &self,
        error: ToolError,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> ToolError {
        // Error passes through unchanged
        error
    }

    async fn on_complete(
        &self,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
        result: Result<&str, &str>,
        duration_ms: u64,
    ) {
        // Extract task context (required for persistence)
        let (task_id, attempt) = match (ctx.task_id, ctx.attempt) {
            (Some(tid), Some(att)) => (tid, att),
            _ => {
                tracing::debug!(
                    "Skipping persistence for {} - no task context",
                    ctx.tool_name
                );
                return;
            }
        };

        // Extract reasoning from the extracted data
        // Handle both direct object and array from ComposedWrapper
        let reasoning = extracted
            .and_then(|v| {
                if let Some(arr) = v.as_array() {
                    arr.iter()
                        .find_map(|item| item.get("reasoning"))
                        .and_then(|v| v.as_str())
                } else {
                    v.get("reasoning").and_then(|v| v.as_str())
                }
            })
            .unwrap_or("")
            .to_string();

        // Extract original args from metadata if available
        let arguments = ctx
            .metadata
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));

        // Build tool call record
        let record = ToolCallRecord {
            tool: ctx.tool_name.clone(),
            arguments,
            reasoning,
            output: result.ok().map(String::from),
            error: result.err().map(String::from),
            duration_ms,
        };

        // Persist asynchronously (best effort, don't block on failure)
        let persistence_guard = self.persistence.lock().await;
        if let Err(e) = persistence_guard
            .append_tool_call(task_id, attempt, &record)
            .await
        {
            tracing::warn!("Failed to persist tool call for {}: {}", ctx.tool_name, e);
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Add a required `_aura_reasoning` field to a JSON schema.
///
/// Modifies the schema in-place to:
/// 1. Add a `_aura_reasoning` property of type string with description
/// 2. Add `_aura_reasoning` to the required array
pub fn add_reasoning_to_schema(schema: &mut Value) {
    if let Value::Object(obj) = schema {
        // Ensure "properties" exists
        let properties = obj
            .entry("properties")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));

        if let Value::Object(props) = properties {
            // Add reasoning field definition
            props.insert(
                REASONING_FIELD.to_string(),
                serde_json::json!({
                    "type": "string",
                    "minLength": 1,
                    "description": "REQUIRED. Explain your reasoning for calling this tool with these specific arguments. What are you trying to accomplish?"
                }),
            );
        }

        // Ensure "required" exists and includes "_aura_reasoning"
        let required = obj
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));

        if let Value::Array(req_arr) = required {
            let reasoning_val = Value::String(REASONING_FIELD.to_string());
            if !req_arr.contains(&reasoning_val) {
                req_arr.push(reasoning_val);
            }
        }
    }
}

/// Extract reasoning from tool arguments, returning (reasoning, cleaned_args).
///
/// The cleaned args have the `_aura_reasoning` field removed so the inner tool
/// receives only its expected arguments.
pub fn extract_reasoning(mut args: Value) -> (Option<String>, Value) {
    let reasoning = if let Value::Object(ref mut obj) = args {
        obj.remove(REASONING_FIELD)
            .and_then(|v| v.as_str().map(String::from))
    } else {
        None
    };

    (reasoning, args)
}

#[cfg(test)]
mod tests {
    use crate::WrappedTool;
    use rig::tool::{Tool as RigTool, ToolError};

    use super::*;

    #[test]
    fn test_add_reasoning_to_empty_schema() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        });

        add_reasoning_to_schema(&mut schema);

        // Check reasoning property was added
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key(REASONING_FIELD));
        assert_eq!(props[REASONING_FIELD]["type"], "string");

        // Check reasoning is in required array
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&Value::String(REASONING_FIELD.to_string())));
    }

    #[test]
    fn test_add_reasoning_to_existing_schema() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "pipeline_id": {
                    "type": "string",
                    "description": "The pipeline ID"
                }
            },
            "required": ["pipeline_id"]
        });

        add_reasoning_to_schema(&mut schema);

        // Check original property preserved
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("pipeline_id"));

        // Check reasoning property was added
        assert!(props.contains_key(REASONING_FIELD));

        // Check both are in required array
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&Value::String("pipeline_id".to_string())));
        assert!(required.contains(&Value::String(REASONING_FIELD.to_string())));
    }

    #[test]
    fn test_add_reasoning_idempotent() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        });

        add_reasoning_to_schema(&mut schema);
        add_reasoning_to_schema(&mut schema);

        // Should only have one reasoning field
        let required = schema["required"].as_array().unwrap();
        let reasoning_count = required
            .iter()
            .filter(|v| v == &&Value::String(REASONING_FIELD.to_string()))
            .count();
        assert_eq!(reasoning_count, 1);
    }

    #[test]
    fn test_extract_reasoning_present() {
        let args = serde_json::json!({
            "pipeline_id": "abc123",
            "_aura_reasoning": "I need to analyze this pipeline because..."
        });

        let (reasoning, clean_args) = extract_reasoning(args);

        assert_eq!(
            reasoning,
            Some("I need to analyze this pipeline because...".to_string())
        );
        assert_eq!(clean_args["pipeline_id"], "abc123");
        assert!(!clean_args
            .as_object()
            .unwrap()
            .contains_key(REASONING_FIELD));
    }

    #[test]
    fn test_extract_reasoning_absent() {
        let args = serde_json::json!({
            "pipeline_id": "abc123"
        });

        let (reasoning, clean_args) = extract_reasoning(args);

        assert!(reasoning.is_none());
        assert_eq!(clean_args["pipeline_id"], "abc123");
    }

    #[test]
    fn test_extract_reasoning_non_object() {
        let args = serde_json::json!("just a string");

        let (reasoning, clean_args) = extract_reasoning(args);

        assert!(reasoning.is_none());
        assert_eq!(clean_args, "just a string");
    }

    #[test]
    fn test_persistence_wrapper_transform_args() {
        use tokio::sync::Mutex;

        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapper = PersistenceWrapper::new(persistence);

        let args = serde_json::json!({
            "param": "value",
            "_aura_reasoning": "test reasoning"
        });

        let ctx = ToolCallContext::new("test_tool");
        let result = wrapper.transform_args(args, &ctx);

        // Args should have reasoning removed
        assert!(!result
            .args
            .as_object()
            .unwrap()
            .contains_key("_aura_reasoning"));
        assert_eq!(result.args["param"], "value");

        // Extracted should contain reasoning
        assert!(result.extracted.is_some());
        let extracted = result.extracted.unwrap();
        assert_eq!(extracted["reasoning"], "test reasoning");
    }

    #[test]
    fn test_persistence_wrapper_wrap_schema() {
        use tokio::sync::Mutex;

        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapper = PersistenceWrapper::new(persistence);

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "x": {"type": "number"}
            },
            "required": ["x"]
        });

        let modified = wrapper.wrap_schema(schema);

        let props = modified["properties"].as_object().unwrap();
        assert!(props.contains_key("_aura_reasoning"));

        let required = modified["required"].as_array().unwrap();
        assert!(required.contains(&Value::String("_aura_reasoning".to_string())));
    }

    use std::future::Future;
    use std::pin::Pin;

    #[derive(Clone)]
    struct MockTool {
        name: String,
        response: String,
    }

    impl MockTool {
        fn new(name: &str, response: &str) -> Self {
            Self {
                name: name.to_string(),
                response: response.to_string(),
            }
        }
    }

    impl RigTool for MockTool {
        type Error = ToolError;
        type Args = Value;
        type Output = String;

        const NAME: &'static str = "mock_tool";

        fn name(&self) -> String {
            self.name.clone()
        }

        #[allow(refining_impl_trait)]
        fn definition(
            &self,
            _prompt: String,
        ) -> Pin<Box<dyn Future<Output = rig::completion::ToolDefinition> + Send + Sync + '_>>
        {
            let name = self.name.clone();
            Box::pin(async move {
                rig::completion::ToolDefinition {
                    name,
                    description: "A mock tool for testing".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "input": {"type": "string"}
                        },
                        "required": ["input"]
                    }),
                }
            })
        }

        #[allow(refining_impl_trait)]
        fn call(
            &self,
            _args: Self::Args,
        ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + Sync + '_>>
        {
            let response = self.response.clone();
            Box::pin(async move { Ok(response) })
        }
    }

    #[tokio::test]
    async fn test_wrapped_tool_schema_has_reasoning() {
        let mock = MockTool::new("test_tool", "response");
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapped = {
            let wrapper = Arc::new(PersistenceWrapper::new(persistence.clone()));
            let initiator = "initiator".to_string();
            WrappedTool::new(mock, wrapper).with_context_factory(move |tool_name| {
                ToolCallContext::new(tool_name).with_task_context(0, initiator.clone(), 1)
            })
        };

        let def = wrapped.definition("test".to_string()).await;

        assert_eq!(def.name, "test_tool");
        let props = def.parameters["properties"].as_object().unwrap();
        assert!(
            props.contains_key("_aura_reasoning"),
            "Schema should have _aura_reasoning property"
        );

        let required = def.parameters["required"].as_array().unwrap();
        assert!(
            required.contains(&Value::String("_aura_reasoning".to_string())),
            "Schema should require _aura_reasoning"
        );
    }

    #[tokio::test]
    async fn test_wrapped_tool_executes_with_reasoning_stripped() {
        let mock = MockTool::new("calculator", "42");
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapped = {
            let wrapper = Arc::new(PersistenceWrapper::new(persistence.clone()));
            let initiator = "initiator".to_string();
            WrappedTool::new(mock, wrapper).with_context_factory(move |tool_name| {
                ToolCallContext::new(tool_name).with_task_context(1, initiator.clone(), 1)
            })
        };

        let args = serde_json::json!({
            "input": "test_input",
            "_aura_reasoning": "Testing the calculator because I need to verify math."
        });

        let result = wrapped.call(args).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "42");
    }

    #[tokio::test]
    async fn test_wrapped_tool_rejects_missing_reasoning() {
        let mock = MockTool::new("echo", "echoed");
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapped = {
            let wrapper = Arc::new(PersistenceWrapper::new(persistence.clone()));
            let initiator = "initiator".to_string();
            WrappedTool::new(mock, wrapper).with_context_factory(move |tool_name| {
                ToolCallContext::new(tool_name).with_task_context(2, initiator.clone(), 1)
            })
        };

        let args = serde_json::json!({
            "input": "hello"
        });

        let result = wrapped.call(args).await;
        assert!(result.is_err(), "Should reject tool call without reasoning");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("_aura_reasoning"),
            "Error should mention _aura_reasoning: {err}"
        );
    }

    #[tokio::test]
    async fn test_wrapped_tool_rejects_empty_reasoning() {
        let mock = MockTool::new("echo", "echoed");
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapped = {
            let wrapper = Arc::new(PersistenceWrapper::new(persistence.clone()));
            let initiator = "initiator".to_string();
            WrappedTool::new(mock, wrapper).with_context_factory(move |tool_name| {
                ToolCallContext::new(tool_name).with_task_context(2, initiator.clone(), 1)
            })
        };

        let args = serde_json::json!({
            "input": "hello",
            "_aura_reasoning": ""
        });

        let result = wrapped.call(args).await;
        assert!(
            result.is_err(),
            "Should reject tool call with empty reasoning"
        );
    }

    #[tokio::test]
    async fn test_wrapped_tool_accepts_valid_reasoning() {
        let mock = MockTool::new("echo", "echoed");
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapped = {
            let wrapper = Arc::new(PersistenceWrapper::new(persistence.clone()));
            let initiator = "initiator".to_string();
            WrappedTool::new(mock, wrapper).with_context_factory(move |tool_name| {
                ToolCallContext::new(tool_name).with_task_context(2, initiator.clone(), 1)
            })
        };

        let args = serde_json::json!({
            "input": "hello",
            "_aura_reasoning": "I need to echo this input to verify the tool works."
        });

        let result = wrapped.call(args).await;
        assert!(
            result.is_ok(),
            "Should accept tool call with valid reasoning"
        );
        assert_eq!(result.unwrap(), "echoed");
    }

    #[tokio::test]
    async fn test_wrapped_tool_preserves_name() {
        let mock = MockTool::new("custom_name", "response");
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapped = {
            let wrapper = Arc::new(PersistenceWrapper::new(persistence.clone()));
            let initiator = "initiator".to_string();
            WrappedTool::new(mock, wrapper).with_context_factory(move |tool_name| {
                ToolCallContext::new(tool_name).with_task_context(0, initiator.clone(), 1)
            })
        };

        assert_eq!(wrapped.name(), "custom_name");
    }
}
