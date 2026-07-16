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
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, Notify};

use super::persistence::{ExecutionPersistence, ToolCallRecord, lock_persistence};
use crate::mcp_response::CallOutcome;
use crate::tool_wrapper::{
    ToolCallContext, ToolWrapper, TransformArgsResult, TransformOutputResult,
};

/// The namespaced field name for reasoning (signals framework/internal field).
const REASONING_FIELD: &str = "_aura_reasoning";

/// Per-call metadata key stashed in `extracted` so `transform_output` and
/// `on_complete` can rendezvous on the same entry in `raw_outputs`.
const CALL_ID_FIELD: &str = "_persistence_call_id";

/// RAII guard that decrements the in-flight counter and notifies drain waiters
/// on drop. Guarantees the counter is decremented even on early returns or
/// panics inside `on_complete`.
struct DrainGuard {
    counter: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Release);
        self.notify.notify_one();
    }
}

/// Tool wrapper that captures reasoning and persists execution details.
///
/// This wrapper:
/// 1. Adds an optional `_aura_reasoning` field to tool schemas
/// 2. Extracts reasoning from args before calling the inner tool
/// 3. Captures RAW tool output in `transform_output` (runs before any wrapper
///    that rewrites the output, e.g. `ScratchpadWrapper`)
/// 4. Writes the record in `on_complete` using the captured raw output plus
///    the `duration_ms` that only the completion hook receives
/// 5. Promotes qualifying tool outputs to artifact files (size/duration threshold)
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
    /// In-flight write counter (shared across all wrappers in an iteration)
    in_flight: Arc<AtomicUsize>,
    /// Notification channel for drain waiters
    drain_notify: Arc<Notify>,
    /// Worker name for artifact filenames (None → "default")
    worker_name: Option<String>,
    /// Iteration snapshot at wrapper construction time
    iteration: usize,
    /// Whether persistence is enabled (snapshotted at construction)
    persistence_enabled: bool,
    /// Character threshold for tool output promotion (0 = promote all)
    size_threshold: usize,
    /// Duration threshold in ms for tool output promotion (0 = disabled)
    duration_threshold_ms: u64,
    /// Per-wrapper call counter for deterministic artifact filenames
    call_counter: Arc<AtomicUsize>,
}

/// Construction parameters for `PersistenceWrapper`.
pub struct PersistenceWrapperParams {
    pub persistence: Arc<Mutex<ExecutionPersistence>>,
    pub in_flight: Arc<AtomicUsize>,
    pub drain_notify: Arc<Notify>,
    pub worker_name: Option<String>,
    pub iteration: usize,
    pub persistence_enabled: bool,
    pub size_threshold: usize,
    pub duration_threshold_ms: u64,
}

impl PersistenceWrapper {
    pub fn new(params: PersistenceWrapperParams) -> Self {
        Self {
            persistence: params.persistence,
            raw_outputs: Arc::new(StdMutex::new(HashMap::new())),
            in_flight: params.in_flight,
            drain_notify: params.drain_notify,
            worker_name: params.worker_name,
            iteration: params.iteration,
            persistence_enabled: params.persistence_enabled,
            size_threshold: params.size_threshold,
            duration_threshold_ms: params.duration_threshold_ms,
            call_counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn effective_worker_name(&self) -> &str {
        self.worker_name.as_deref().unwrap_or("default")
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
        let call_id = uuid::Uuid::new_v4().to_string();
        let call_idx = self.call_counter.fetch_add(1, Ordering::Relaxed);

        let extracted = serde_json::json!({
            "reasoning": reasoning.unwrap_or_default(),
            CALL_ID_FIELD: call_id,
            "call_idx": call_idx
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
            tracing::debug!(
                tool = %ctx.tool_name,
                "Tool call with empty _aura_reasoning (allowed, reasoning is optional)"
            );
        }

        Ok(())
    }

    /// Captures the RAW tool output into `raw_outputs` (must run BEFORE any
    /// wrapper that rewrites the output, e.g. `ScratchpadWrapper`), then
    /// appends an artifact footer when size-based promotion qualifies.
    ///
    /// Suppressed when persistence is disabled (no backing file will exist).
    /// Duration-based promotion is handled in `on_complete` — those artifacts
    /// are written for observability but lack an inline footer because the
    /// output has already been returned to the LLM by that point.
    fn transform_output(
        &self,
        output: String,
        _outcome: &CallOutcome,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
    ) -> TransformOutputResult {
        // 1. Cache raw output for persistence (before any footer append)
        if let Some(call_id) = find_field(extracted, CALL_ID_FIELD) {
            self.raw_outputs
                .lock()
                .unwrap()
                .insert(call_id.to_string(), output.clone());
        }

        // 2. Artifact footer (only if persistence enabled + size qualifies)
        if !self.persistence_enabled {
            return TransformOutputResult::new(output);
        }

        let call_idx = extract_call_idx(extracted);
        let should_promote = self.size_threshold == 0 || output.len() > self.size_threshold;

        if !should_promote {
            return TransformOutputResult::new(output);
        }

        let worker = super::persistence::sanitize_filename_component(self.effective_worker_name());
        let tool = super::persistence::sanitize_filename_component(&ctx.tool_name);
        let task_id = ctx.task_id.unwrap_or(0);
        let filename = format!(
            "task-{}-{}-iter-{}-{}-{}-output.txt",
            task_id, worker, self.iteration, tool, call_idx
        );

        let footer = format!("\n\n[Tool output saved to artifact: {}]", filename);
        TransformOutputResult::new(format!("{}{}", output, footer))
    }

    #[tracing::instrument(
        name = "persistence.on_complete",
        skip(self, ctx, extracted, result),
        fields(
            tool = %ctx.tool_name,
            task_id = ?ctx.task_id,
            attempt = ?ctx.attempt,
            duration_ms,
            promoted = tracing::field::Empty,
        )
    )]
    async fn on_complete(
        &self,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
        result: Result<&str, &str>,
        duration_ms: u64,
    ) {
        self.in_flight.fetch_add(1, Ordering::Acquire);
        let _guard = DrainGuard {
            counter: self.in_flight.clone(),
            notify: self.drain_notify.clone(),
        };

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

        // Artifact promotion: use raw output from cache (clean, no footer)
        let call_idx = extract_call_idx(extracted);
        let output_text = output.as_deref().unwrap_or("");
        // Strip any artifact footer appended by transform_output so we write
        // clean output and compute thresholds on the original size.
        let output_clean = output_text
            .rfind("\n\n[Tool output saved to artifact: ")
            .map(|pos| &output_text[..pos])
            .unwrap_or(output_text);
        let should_promote = match (
            output_clean.len(),
            self.size_threshold,
            self.duration_threshold_ms,
        ) {
            (_, 0, _) => true,
            (len, thresh, _) if len > thresh => true,
            (_, _, 0) => false,
            (_, _, thresh) if duration_ms > thresh => true,
            _ => false,
        };

        tracing::Span::current().record("promoted", should_promote && result.is_ok());

        // Single lock acquisition for both artifact write and tool call append
        let persistence_guard = lock_persistence(&self.persistence, "tool_output_write").await;

        // Write artifact file if promoted
        let artifact_filename = if should_promote && result.is_ok() {
            match persistence_guard
                .write_tool_output_artifact(
                    task_id,
                    self.effective_worker_name(),
                    self.iteration,
                    &ctx.tool_name,
                    call_idx,
                    output_clean,
                )
                .await
            {
                Ok(filename) => Some(filename),
                Err(e) => {
                    tracing::warn!(
                        "Failed to write tool output artifact for {}: {}",
                        ctx.tool_name,
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        // Build tool call record (store clean output without footer)
        let record = ToolCallRecord {
            tool: ctx.tool_name.clone(),
            arguments: ctx
                .metadata
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
            reasoning,
            output: output.map(|o| {
                o.rfind("\n\n[Tool output saved to artifact: ")
                    .map(|pos| o[..pos].to_string())
                    .unwrap_or(o)
            }),
            error,
            duration_ms,
            artifact_filename,
        };
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

/// Extract the call_idx from the extracted metadata (set during transform_args).
fn extract_call_idx(extracted: Option<&Value>) -> usize {
    extracted
        .and_then(|v| {
            if let Some(arr) = v.as_array() {
                arr.iter()
                    .find_map(|item| item.get("call_idx"))
                    .and_then(|v| v.as_u64())
            } else {
                v.get("call_idx").and_then(|v| v.as_u64())
            }
        })
        .unwrap_or(0) as usize
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

    fn test_wrapper(persistence: Arc<Mutex<ExecutionPersistence>>) -> PersistenceWrapper {
        PersistenceWrapper::new(PersistenceWrapperParams {
            persistence,
            in_flight: Arc::new(AtomicUsize::new(0)),
            drain_notify: Arc::new(Notify::new()),
            worker_name: None,
            iteration: 1,
            persistence_enabled: false,
            size_threshold: 500,
            duration_threshold_ms: 5000,
        })
    }

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
        let wrapper = test_wrapper(persistence);

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

        // Extracted should contain reasoning and call_idx
        assert!(result.extracted.is_some());
        let extracted = result.extracted.unwrap();
        assert_eq!(extracted["reasoning"], "test reasoning");
        assert_eq!(extracted["call_idx"], 0);
    }

    #[test]
    fn test_persistence_wrapper_wrap_schema() {
        use tokio::sync::Mutex;

        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let wrapper = test_wrapper(persistence);

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
            let wrapper = Arc::new(test_wrapper(persistence.clone()));
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
            let wrapper = Arc::new(test_wrapper(persistence.clone()));
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
            let wrapper = Arc::new(test_wrapper(persistence.clone()));
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
            let wrapper = Arc::new(test_wrapper(persistence.clone()));
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
            let wrapper = Arc::new(test_wrapper(persistence.clone()));
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
            let wrapper = Arc::new(test_wrapper(persistence.clone()));
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
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let (in_flight, drain_notify) = {
            let p = persistence.lock().await;
            (p.in_flight_counter(), p.drain_notify())
        };
        let wrapper = PersistenceWrapper::new(PersistenceWrapperParams {
            persistence,
            in_flight,
            drain_notify,
            worker_name: None,
            iteration: 1,
            persistence_enabled: false,
            size_threshold: 0,
            duration_threshold_ms: 0,
        });

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

        let persistence_inner = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let (in_flight, drain_notify) = {
            let p = persistence_inner.lock().await;
            (p.in_flight_counter(), p.drain_notify())
        };
        let persistence = Arc::new(PersistenceWrapper::new(PersistenceWrapperParams {
            persistence: persistence_inner,
            in_flight,
            drain_notify,
            worker_name: None,
            iteration: 1,
            persistence_enabled: false,
            size_threshold: 0,
            duration_threshold_ms: 0,
        }));
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

    #[test]
    fn test_drain_guard_decrements_on_drop() {
        let counter = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());

        counter.fetch_add(1, Ordering::Release);
        assert_eq!(counter.load(Ordering::Acquire), 1);

        {
            let _guard = DrainGuard {
                counter: counter.clone(),
                notify: notify.clone(),
            };
            assert_eq!(counter.load(Ordering::Acquire), 1);
        }
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn test_drain_guard_decrements_on_early_return() {
        let counter = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());

        counter.fetch_add(1, Ordering::Release);

        let do_work = || -> Option<()> {
            let _guard = DrainGuard {
                counter: counter.clone(),
                notify: notify.clone(),
            };
            // Simulate early return before any work
            None?;
            Some(())
        };

        let _ = do_work();
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn test_on_complete_increments_and_decrements_in_flight() {
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let (in_flight, drain_notify) = {
            let p = persistence.lock().await;
            (p.in_flight_counter(), p.drain_notify())
        };
        let wrapper = PersistenceWrapper::new(PersistenceWrapperParams {
            persistence: persistence.clone(),
            in_flight: in_flight.clone(),
            drain_notify,
            worker_name: None,
            iteration: 1,
            persistence_enabled: false,
            size_threshold: 500,
            duration_threshold_ms: 5000,
        });

        assert_eq!(in_flight.load(Ordering::Acquire), 0);

        let ctx = ToolCallContext::new("test_tool").with_task_context(0, "worker".to_string(), 1);
        wrapper.on_complete(&ctx, None, Ok("output"), 100).await;

        assert_eq!(in_flight.load(Ordering::Acquire), 0);
    }

    // ========================================================================
    // Tool Output Promotion Tests
    // ========================================================================

    fn promotion_wrapper(
        worker_name: Option<String>,
        iteration: usize,
        persistence_enabled: bool,
        size_threshold: usize,
        duration_threshold_ms: u64,
    ) -> PersistenceWrapper {
        PersistenceWrapper::new(PersistenceWrapperParams {
            persistence: Arc::new(Mutex::new(ExecutionPersistence::disabled())),
            in_flight: Arc::new(AtomicUsize::new(0)),
            drain_notify: Arc::new(Notify::new()),
            worker_name,
            iteration,
            persistence_enabled,
            size_threshold,
            duration_threshold_ms,
        })
    }

    async fn enabled_wrapper(
        persistence: Arc<Mutex<ExecutionPersistence>>,
        worker_name: Option<String>,
        size_threshold: usize,
        duration_threshold_ms: u64,
    ) -> PersistenceWrapper {
        let (in_flight, drain_notify) = {
            let p = persistence.lock().await;
            (p.in_flight_counter(), p.drain_notify())
        };
        PersistenceWrapper::new(PersistenceWrapperParams {
            persistence,
            in_flight,
            drain_notify,
            worker_name,
            iteration: 1,
            persistence_enabled: true,
            size_threshold,
            duration_threshold_ms,
        })
    }

    #[test]
    fn test_transform_output_appends_footer_when_size_exceeded() {
        let wrapper = promotion_wrapper(Some("sre".to_string()), 2, true, 10, 5000);

        let ctx = ToolCallContext::new("log_search").with_task_context(0, "sre".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "", "call_idx": 0});
        let outcome = CallOutcome::Success(String::new());
        let long_output = "x".repeat(20);

        let result =
            wrapper.transform_output(long_output.clone(), &outcome, &ctx, Some(&extracted));
        assert!(result.output.contains(
            "[Tool output saved to artifact: task-0-sre-iter-2-log-search-0-output.txt]"
        ));
        assert!(result.output.starts_with(&long_output));
    }

    #[test]
    fn test_transform_output_no_footer_when_below_threshold() {
        let wrapper = promotion_wrapper(Some("sre".to_string()), 1, true, 500, 5000);

        let ctx = ToolCallContext::new("log_search").with_task_context(0, "sre".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "", "call_idx": 0});
        let outcome = CallOutcome::Success(String::new());

        let result =
            wrapper.transform_output("short output".to_string(), &outcome, &ctx, Some(&extracted));
        assert_eq!(result.output, "short output");
        assert!(!result.output.contains("[Tool output saved to artifact"));
    }

    #[test]
    fn test_transform_output_promotes_all_when_size_zero() {
        let wrapper = promotion_wrapper(None, 1, true, 0, 5000);

        let ctx = ToolCallContext::new("my_tool").with_task_context(3, "w".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "", "call_idx": 0});
        let outcome = CallOutcome::Success(String::new());

        let result = wrapper.transform_output("tiny".to_string(), &outcome, &ctx, Some(&extracted));
        assert!(result.output.contains(
            "[Tool output saved to artifact: task-3-default-iter-1-my-tool-0-output.txt]"
        ));
    }

    #[test]
    fn test_call_counter_increments_across_calls() {
        let wrapper = promotion_wrapper(Some("worker".to_string()), 1, false, 0, 5000);

        let ctx = ToolCallContext::new("tool_a").with_task_context(0, "worker".to_string(), 1);

        let result1 = wrapper.transform_args(serde_json::json!({"key": "val"}), &ctx);
        assert_eq!(result1.extracted.as_ref().unwrap()["call_idx"], 0);

        let result2 = wrapper.transform_args(serde_json::json!({"key": "val2"}), &ctx);
        assert_eq!(result2.extracted.as_ref().unwrap()["call_idx"], 1);
    }

    #[tokio::test]
    async fn test_on_complete_writes_artifact_when_size_exceeded() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(Mutex::new(
            ExecutionPersistence::new(temp_dir.path().join("memory"), None)
                .await
                .unwrap(),
        ));
        let wrapper =
            enabled_wrapper(persistence.clone(), Some("research".to_string()), 10, 5000).await;

        let ctx = ToolCallContext::new("kb_search").with_task_context(0, "research".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "test", "call_idx": 0});
        let long_output = "x".repeat(50);

        wrapper
            .on_complete(&ctx, Some(&extracted), Ok(&long_output), 100)
            .await;

        let p = persistence.lock().await;
        let artifacts = p.list_artifacts().await.unwrap();
        assert!(artifacts.contains(&"task-0-research-iter-1-kb-search-0-output.txt".to_string()));

        let content = p
            .read_artifact("task-0-research-iter-1-kb-search-0-output.txt")
            .await
            .unwrap();
        assert_eq!(content.len(), 50);
    }

    #[tokio::test]
    async fn test_on_complete_writes_artifact_when_duration_exceeded() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(Mutex::new(
            ExecutionPersistence::new(temp_dir.path().join("memory"), None)
                .await
                .unwrap(),
        ));
        let wrapper = enabled_wrapper(persistence.clone(), Some("sre".to_string()), 500, 100).await;

        let ctx = ToolCallContext::new("log_search").with_task_context(0, "sre".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "test", "call_idx": 0});

        wrapper
            .on_complete(&ctx, Some(&extracted), Ok("short"), 200)
            .await;

        let p = persistence.lock().await;
        let artifacts = p.list_artifacts().await.unwrap();
        assert!(artifacts.contains(&"task-0-sre-iter-1-log-search-0-output.txt".to_string()));
    }

    #[tokio::test]
    async fn test_on_complete_no_artifact_when_below_thresholds() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(Mutex::new(
            ExecutionPersistence::new(temp_dir.path().join("memory"), None)
                .await
                .unwrap(),
        ));
        let wrapper =
            enabled_wrapper(persistence.clone(), Some("worker".to_string()), 500, 5000).await;

        let ctx = ToolCallContext::new("simple_tool").with_task_context(0, "worker".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "test", "call_idx": 0});

        wrapper
            .on_complete(&ctx, Some(&extracted), Ok("short"), 100)
            .await;

        let p = persistence.lock().await;
        let artifacts = p.list_artifacts().await.unwrap();
        assert!(artifacts.is_empty());
    }

    #[tokio::test]
    async fn test_on_complete_no_artifact_when_duration_disabled() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(Mutex::new(
            ExecutionPersistence::new(temp_dir.path().join("memory"), None)
                .await
                .unwrap(),
        ));
        let wrapper =
            enabled_wrapper(persistence.clone(), Some("worker".to_string()), 500, 0).await;

        let ctx = ToolCallContext::new("slow_tool").with_task_context(0, "worker".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "test", "call_idx": 0});

        wrapper
            .on_complete(&ctx, Some(&extracted), Ok("short"), 999999)
            .await;

        let p = persistence.lock().await;
        let artifacts = p.list_artifacts().await.unwrap();
        assert!(artifacts.is_empty());
    }

    #[test]
    fn test_extract_call_idx_from_direct_object() {
        let extracted = serde_json::json!({"reasoning": "test", "call_idx": 5});
        assert_eq!(extract_call_idx(Some(&extracted)), 5);
    }

    #[test]
    fn test_extract_call_idx_from_composed_array() {
        let extracted = serde_json::json!([
            {"observer": true},
            {"reasoning": "test", "call_idx": 3}
        ]);
        assert_eq!(extract_call_idx(Some(&extracted)), 3);
    }

    #[test]
    fn test_extract_call_idx_missing() {
        assert_eq!(extract_call_idx(None), 0);
        assert_eq!(extract_call_idx(Some(&serde_json::json!({}))), 0);
    }

    #[test]
    fn test_tool_output_artifact_filename_sanitization() {
        let wrapper = promotion_wrapper(Some("SRE/Ops Worker".to_string()), 1, true, 0, 5000);

        let ctx = ToolCallContext::new("my_search tool").with_task_context(0, "sre".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "", "call_idx": 0});
        let outcome = CallOutcome::Success(String::new());

        let result =
            wrapper.transform_output("output".to_string(), &outcome, &ctx, Some(&extracted));
        assert!(
            result
                .output
                .contains("task-0-sre-ops-worker-iter-1-my-search-tool-0-output.txt")
        );
    }

    #[test]
    fn test_transform_output_no_footer_when_persistence_disabled() {
        let wrapper = promotion_wrapper(Some("sre".to_string()), 1, false, 0, 5000);

        let ctx = ToolCallContext::new("log_search").with_task_context(0, "sre".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "", "call_idx": 0});
        let outcome = CallOutcome::Success(String::new());

        let result = wrapper.transform_output(
            "big output here".to_string(),
            &outcome,
            &ctx,
            Some(&extracted),
        );
        assert_eq!(result.output, "big output here");
        assert!(!result.output.contains("[Tool output saved to artifact"));
    }

    #[tokio::test]
    async fn test_on_complete_artifact_filename_none_when_not_promoted() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(Mutex::new(
            ExecutionPersistence::new(temp_dir.path().join("memory"), None)
                .await
                .unwrap(),
        ));
        let wrapper =
            enabled_wrapper(persistence.clone(), Some("worker".to_string()), 500, 5000).await;

        let ctx = ToolCallContext::new("simple_tool").with_task_context(0, "worker".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "test", "call_idx": 0});

        wrapper
            .on_complete(&ctx, Some(&extracted), Ok("short"), 100)
            .await;

        let p = persistence.lock().await;
        let iter_path = p.run_path().join("iteration-1");
        let tool_calls_path = iter_path.join("task-0.attempt-1.tool-calls.json");
        let content = tokio::fs::read_to_string(&tool_calls_path).await.unwrap();
        let records: Vec<ToolCallRecord> = serde_json::from_str(&content).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].artifact_filename.is_none());
    }

    #[tokio::test]
    async fn test_on_complete_artifact_filename_set_when_promoted() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(Mutex::new(
            ExecutionPersistence::new(temp_dir.path().join("memory"), None)
                .await
                .unwrap(),
        ));
        let wrapper = enabled_wrapper(persistence.clone(), Some("sre".to_string()), 10, 5000).await;

        let ctx = ToolCallContext::new("log_search").with_task_context(0, "sre".to_string(), 1);
        let extracted = serde_json::json!({"reasoning": "test", "call_idx": 0});
        let long_output = "x".repeat(50);

        wrapper
            .on_complete(&ctx, Some(&extracted), Ok(&long_output), 100)
            .await;

        let p = persistence.lock().await;
        let iter_path = p.run_path().join("iteration-1");
        let tool_calls_path = iter_path.join("task-0.attempt-1.tool-calls.json");
        let content = tokio::fs::read_to_string(&tool_calls_path).await.unwrap();
        let records: Vec<ToolCallRecord> = serde_json::from_str(&content).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].artifact_filename.as_deref(),
            Some("task-0-sre-iter-1-log-search-0-output.txt")
        );
    }
}
