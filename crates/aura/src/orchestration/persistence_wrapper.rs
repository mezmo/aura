//! Persistence tool wrapper for orchestration.
//!
//! Wraps MCP tools to capture reasoning and persist execution details.
//! The wrapper adds an optional `_aura_reasoning` field to tool schemas,
//! extracts it during execution, and writes records to ExecutionPersistence.
//! The field is intentionally not in the schema's `required` array so
//! quantized/smaller models that omit it don't break their ReAct loop.
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
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex;

use super::persistence::{ExecutionPersistence, ToolCallRecord};
use crate::mcp_response::CallOutcome;
use crate::tool_wrapper::{
    ToolCallContext, ToolWrapper, TransformArgsResult, TransformOutputResult,
};

/// The namespaced field name for reasoning (signals framework/internal field).
const REASONING_FIELD: &str = "_aura_reasoning";

/// Per-call metadata key stashed in `extracted` so `transform_output` and
/// `on_complete` can rendezvous on the same entry in `raw_outputs`.
const CALL_ID_FIELD: &str = "_persistence_call_id";

/// Tool wrapper that captures reasoning and persists execution details.
///
/// This wrapper:
/// 1. Adds an optional `_aura_reasoning` field to tool schemas
/// 2. Extracts reasoning from args before calling the inner tool
/// 3. Captures RAW tool output in `transform_output` (runs before any wrapper
///    that rewrites the output, e.g. `ScratchpadWrapper`)
/// 4. Writes the record in `on_complete` using the captured raw output plus
///    the `duration_ms` that only the completion hook receives
///
/// Only used in orchestration mode for worker agents.
#[derive(Clone)]
pub struct PersistenceWrapper {
    /// Shared persistence manager for writing records
    persistence: Arc<Mutex<ExecutionPersistence>>,
    /// Cache keyed by `_persistence_call_id`. Entries are inserted in
    /// `transform_output` and removed in `on_complete`. Bounded because each
    /// tool call inserts and removes exactly one entry.
    raw_outputs: Arc<StdMutex<HashMap<String, String>>>,
}

impl PersistenceWrapper {
    pub fn new(persistence: Arc<Mutex<ExecutionPersistence>>) -> Self {
        Self {
            persistence,
            raw_outputs: Arc::new(StdMutex::new(HashMap::new())),
        }
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
        // A fresh call id per invocation lets `transform_output` and
        // `on_complete` share state via `raw_outputs` without colliding when
        // the same tool is invoked multiple times in one attempt.
        let call_id = uuid::Uuid::new_v4().to_string();
        let extracted = serde_json::json!({
            "reasoning": reasoning.unwrap_or_default(),
            CALL_ID_FIELD: call_id,
        });

        TransformArgsResult {
            args: clean_args,
            extracted: Some(extracted),
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
                "Tool call with empty _aura_reasoning (allowed, reasoning is optional)"
            );
        }

        Ok(())
    }

    /// Captures the RAW tool output into `raw_outputs`. Must run BEFORE any
    /// wrapper that rewrites the output (e.g. `ScratchpadWrapper` replacing a
    /// large payload with a pointer) so `on_complete` can persist what the
    /// tool actually produced. The caller orders wrappers so this one runs
    /// first in reverse-iteration (i.e. PersistenceWrapper is last in the
    /// ComposedWrapper list).
    fn transform_output(
        &self,
        output: String,
        _outcome: &CallOutcome,
        _ctx: &ToolCallContext,
        extracted: Option<&Value>,
    ) -> TransformOutputResult {
        if let Some(call_id) = find_field(extracted, CALL_ID_FIELD) {
            self.raw_outputs
                .lock()
                .unwrap()
                .insert(call_id.to_string(), output.clone());
        }
        TransformOutputResult::new(output)
    }

    async fn on_complete(
        &self,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
        result: Result<&str, &str>,
        duration_ms: u64,
    ) {
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

        let reasoning = find_field(extracted, "reasoning").unwrap_or("").to_string();
        let raw_output = find_field(extracted, CALL_ID_FIELD)
            .and_then(|id| self.raw_outputs.lock().unwrap().remove(id));

        // Prefer the raw output captured in `transform_output` over the
        // possibly-transformed `result` the framework hands us on success.
        // On error, `result` is authoritative (transform_output didn't run).
        let (output, error) = match result {
            Ok(_) => (raw_output.or_else(|| result.ok().map(String::from)), None),
            Err(e) => (None, Some(e.to_string())),
        };

        let record = ToolCallRecord {
            tool: ctx.tool_name.clone(),
            arguments: ctx
                .metadata
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
            reasoning,
            output,
            error,
            duration_ms,
        };

        let persistence_guard = self.persistence.lock().await;
        if let Err(e) = persistence_guard
            .append_tool_call(task_id, attempt, &record)
            .await
        {
            tracing::warn!("Failed to persist tool call for {}: {}", ctx.tool_name, e);
        }
    }
}

/// Fetch a top-level string field from the `extracted` value. Handles both
/// direct objects and the array form that `ComposedWrapper` produces when
/// multiple wrappers each contribute extracted data.
fn find_field<'a>(extracted: Option<&'a Value>, field: &str) -> Option<&'a str> {
    extracted.and_then(|v| {
        if let Some(arr) = v.as_array() {
            arr.iter()
                .find_map(|item| item.get(field))
                .and_then(|v| v.as_str())
        } else {
            v.get(field).and_then(|v| v.as_str())
        }
    })
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Add an optional `_aura_reasoning` field to a JSON schema.
///
/// The field is intentionally not in the schema's `required` array and
/// carries no `minLength` constraint — empty/absent values must not cause
/// strict-validation providers (e.g. OpenAI structured-outputs) to reject
/// the call. The "REQUIRED" prefix in the description is guidance for the
/// model only; `validate_args` warns on empty but never rejects.
pub fn add_reasoning_to_schema(schema: &mut Value) {
    if let Value::Object(obj) = schema {
        let properties = obj
            .entry("properties")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));

        if let Value::Object(props) = properties {
            props.insert(
                REASONING_FIELD.to_string(),
                serde_json::json!({
                    "type": "string",
                    "description": "REQUIRED. Explain your reasoning for calling this tool with these specific arguments. What are you trying to accomplish?"
                }),
            );
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

        // Reasoning should NOT be in required array (optional to avoid breaking quantized models)
        let required = schema["required"].as_array().unwrap();
        assert!(!required.contains(&Value::String(REASONING_FIELD.to_string())));
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

        // Check pipeline_id still required, but reasoning is NOT required (optional)
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&Value::String("pipeline_id".to_string())));
        assert!(!required.contains(&Value::String(REASONING_FIELD.to_string())));
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

        // Reasoning should not be in required at all (optional)
        let required = schema["required"].as_array().unwrap();
        let reasoning_count = required
            .iter()
            .filter(|v| v == &&Value::String(REASONING_FIELD.to_string()))
            .count();
        assert_eq!(reasoning_count, 0);
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
        assert!(
            !clean_args
                .as_object()
                .unwrap()
                .contains_key(REASONING_FIELD)
        );
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
        assert!(
            !result
                .args
                .as_object()
                .unwrap()
                .contains_key("_aura_reasoning")
        );
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
        assert!(!required.contains(&Value::String("_aura_reasoning".to_string())));
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
            !required.contains(&Value::String("_aura_reasoning".to_string())),
            "Schema should NOT require _aura_reasoning (optional)"
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
    async fn test_wrapped_tool_accepts_missing_reasoning() {
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
        assert!(
            result.is_ok(),
            "Should accept tool call without reasoning (optional)"
        );
    }

    #[tokio::test]
    async fn test_wrapped_tool_accepts_empty_reasoning() {
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
            result.is_ok(),
            "Should accept tool call with empty reasoning (optional)"
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

    /// transform_output caches the raw output under the per-call id; on_complete
    /// retrieves it. Verifies the rendezvous that lets persistence record the
    /// raw tool output even when a later wrapper (e.g. scratchpad) rewrites it.
    #[tokio::test]
    async fn test_raw_output_cached_and_retrieved_by_call_id() {
        let wrapper =
            PersistenceWrapper::new(Arc::new(Mutex::new(ExecutionPersistence::disabled())));

        let ctx = ToolCallContext::new("tool").with_task_context(1, "w".into(), 0);
        let TransformArgsResult { extracted, .. } =
            wrapper.transform_args(serde_json::json!({}), &ctx);
        let extracted = extracted.expect("transform_args always produces extracted");
        let call_id = extracted[CALL_ID_FIELD]
            .as_str()
            .expect("call id field present")
            .to_string();

        let raw = "gigantic raw tool output that scratchpad would rewrite to a pointer";
        let outcome = crate::mcp_response::CallOutcome::Success(raw.to_string());
        let _ = wrapper.transform_output(raw.to_string(), &outcome, &ctx, Some(&extracted));

        // Cached under the call id until on_complete consumes it.
        assert_eq!(
            wrapper.raw_outputs.lock().unwrap().get(&call_id),
            Some(&raw.to_string()),
        );

        // on_complete pulls the raw output (via the call id) rather than using
        // the transformed `result` string it's handed. Since persistence is
        // disabled, we can't inspect the written record — instead we assert
        // the cache is drained, which proves the lookup happened.
        let transformed = "[scratchpad: pointer ...]";
        wrapper
            .on_complete(&ctx, Some(&extracted), Ok(transformed), 42)
            .await;
        assert!(
            wrapper.raw_outputs.lock().unwrap().is_empty(),
            "on_complete should drain the per-call cache entry"
        );
    }

    /// End-to-end ordering test through `ComposedWrapper`. Locks down the
    /// load-bearing convention from `orchestrator.rs::create_worker` that
    /// `PersistenceWrapper` is placed AFTER `ScratchpadWrapper` in the vec
    /// so its `transform_output` runs FIRST under reverse iteration and sees
    /// the RAW payload, while the LLM-facing output is the scratchpad
    /// pointer. A future reorder of the wrapper vec — or a change to
    /// `ComposedWrapper`'s reverse iteration — will fail this test.
    #[tokio::test]
    async fn test_composed_persistence_after_scratchpad_captures_raw() {
        use crate::scratchpad::context_budget::TiktokenCounter;
        use crate::scratchpad::{ContextBudget, ScratchpadStorage, ScratchpadWrapper};
        use crate::tool_wrapper::ComposedWrapper;

        let tmp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-compose-1")
                .await
                .unwrap(),
        );
        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, Arc::new(counter));

        let scratchpad_tools = HashMap::from([("big_tool".to_string(), 10_usize)]);
        let scratchpad: Arc<dyn ToolWrapper> = Arc::new(ScratchpadWrapper::new(
            scratchpad_tools,
            storage.clone(),
            budget,
        ));

        let persistence = Arc::new(PersistenceWrapper::new(Arc::new(Mutex::new(
            ExecutionPersistence::disabled(),
        ))));
        let persistence_dyn: Arc<dyn ToolWrapper> = persistence.clone();

        // Same ordering as `orchestrator.rs::create_worker`: scratchpad
        // before persistence in the vec → persistence's `transform_output`
        // runs first under reverse iteration → sees raw → caches → then
        // scratchpad rewrites to the pointer.
        let composed = ComposedWrapper::new(vec![scratchpad, persistence_dyn]);

        let ctx = ToolCallContext::new("big_tool").with_task_context(7, "worker_xyz".into(), 0);
        let TransformArgsResult { extracted, .. } =
            composed.transform_args(serde_json::json!({}), &ctx);
        let extracted = extracted.expect("composed transform_args produces extracted");

        // Use varied content to avoid tokenizer compression masking the threshold.
        let raw: String = (0..500).map(|i| format!("entry_{} ", i)).collect();
        let outcome = crate::mcp_response::CallOutcome::Success(raw.clone());
        let result = composed.transform_output(raw.clone(), &outcome, &ctx, Some(&extracted));

        assert!(
            result.output.contains("[scratchpad:"),
            "scratchpad must rewrite output to a pointer for the LLM, got: {}",
            &result.output[..result.output.len().min(120)]
        );

        let call_id = find_field(Some(&extracted), CALL_ID_FIELD)
            .expect("composed extracted must carry _persistence_call_id");
        let cached = persistence
            .raw_outputs
            .lock()
            .unwrap()
            .get(call_id)
            .cloned();
        assert_eq!(
            cached.as_deref(),
            Some(raw.as_str()),
            "persistence must cache the RAW payload, not the scratchpad pointer"
        );
    }
}
