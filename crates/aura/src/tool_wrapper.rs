//! Generic tool wrapper trait for composable tool transformations.
//!
//! This module provides a trait-based approach to wrapping tools with
//! additional functionality like:
//! - Schema modification (adding fields, changing types)
//! - Input argument transformation
//! - Output transformation
//! - Side effects (logging, persistence, metrics)
//!
//! # Design Goals
//!
//! 1. **Generic** - Not tied to any specific use case (orchestration, time conversion, etc.)
//! 2. **Composable** - Multiple wrappers can be chained
//! 3. **Async-friendly** - Transformations can be async (e.g., for persistence)
//! 4. **Rig-compatible** - Works with Rig's `Tool` trait
//!
//! # Example Use Cases
//!
//! - **Persistence**: Add `_aura_reasoning` field, persist tool calls
//! - **Time conversion**: Auto-convert time fields between formats
//! - **Metrics**: Track tool call duration and success rates
//! - **Validation**: Add schema validation before tool execution
//!
//! # Example
//!
//! ```ignore
//! use aura::tool_wrapper::{ToolWrapper, ToolWrapperConfig, WrappedTool};
//!
//! // Create a wrapper that adds metrics
//! struct MetricsWrapper { /* ... */ }
//!
//! impl ToolWrapper for MetricsWrapper {
//!     fn wrap_schema(&self, schema: Value) -> Value { schema }
//!     fn transform_args(&self, args: Value) -> Value { args }
//!     fn transform_output(&self, output: String) -> String { output }
//! }
//!
//! // Wrap a tool
//! let wrapped = WrappedTool::new(inner_tool, Arc::new(metrics_wrapper));
//! ```

use async_trait::async_trait;
use rig::tool::{Tool as RigTool, ToolError};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::config::ToolContextFactory;
use crate::mcp_response::CallOutcome;

/// Context passed to wrapper methods during tool execution.
///
/// Contains metadata about the current tool call that wrappers
/// can use for logging, persistence, or transformation decisions.
#[derive(Debug, Clone, Default)]
pub struct ToolCallContext {
    /// Tool name being called
    pub tool_name: String,
    /// The ID of the orchestrator or worker initiating the tool call
    pub tool_initiator_id: String,
    /// Optional correlation ID for tracing
    pub correlation_id: Option<String>,
    /// Optional task context (for orchestration)
    pub task_id: Option<usize>,
    /// Optional attempt number (for retries)
    pub attempt: Option<usize>,
    /// Custom metadata that wrappers can use
    pub metadata: Option<Value>,
}

impl ToolCallContext {
    /// Create a new context with just the tool name.
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            ..Default::default()
        }
    }

    /// Set correlation ID for tracing.
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Set task context for orchestration.
    pub fn with_task_context(
        mut self,
        task_id: usize,
        tool_initiator_id: String,
        attempt: usize,
    ) -> Self {
        self.task_id = Some(task_id);
        self.attempt = Some(attempt);
        self.tool_initiator_id = tool_initiator_id;
        self
    }

    /// Set custom metadata.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Result of argument transformation.
///
/// Wrappers can extract data from args (like reasoning) while
/// returning cleaned args for the inner tool.
#[derive(Debug, Clone)]
pub struct TransformArgsResult {
    /// Arguments to pass to the inner tool (possibly modified)
    pub args: Value,
    /// Data extracted from args that the wrapper wants to keep
    /// (e.g., reasoning text, metadata fields)
    pub extracted: Option<Value>,
}

impl TransformArgsResult {
    /// Create result with just transformed args.
    pub fn new(args: Value) -> Self {
        Self {
            args,
            extracted: None,
        }
    }

    /// Create result with args and extracted data.
    pub fn with_extracted(args: Value, extracted: Value) -> Self {
        Self {
            args,
            extracted: Some(extracted),
        }
    }
}

/// Result of output transformation.
///
/// Wrappers can modify the output and/or produce side effects.
#[derive(Debug, Clone)]
pub struct TransformOutputResult {
    /// Output to return (possibly modified)
    pub output: String,
    /// Whether the transformation succeeded
    pub success: bool,
    /// Optional error message if transformation failed but we still return output
    pub warning: Option<String>,
}

impl TransformOutputResult {
    /// Create successful result with output.
    pub fn new(output: String) -> Self {
        Self {
            output,
            success: true,
            warning: None,
        }
    }

    /// Create result with a warning (non-fatal issue).
    pub fn with_warning(output: String, warning: impl Into<String>) -> Self {
        Self {
            output,
            success: true,
            warning: Some(warning.into()),
        }
    }
}

/// Trait for wrapping tools with additional functionality.
///
/// Implementors can modify tool schemas, transform arguments,
/// transform outputs, and perform side effects like persistence.
///
/// All methods have default implementations that pass through unchanged,
/// so you only need to implement the methods relevant to your use case.
#[async_trait]
pub trait ToolWrapper: Send + Sync {
    /// Modify the tool's JSON schema definition.
    ///
    /// Called once when the tool definition is requested.
    /// Use this to add fields (e.g. `_aura_reasoning`) or modify
    /// field types.
    ///
    /// # Arguments
    /// * `schema` - The inner tool's parameter schema
    ///
    /// # Returns
    /// Modified schema (or unchanged if no modification needed)
    fn wrap_schema(&self, schema: Value) -> Value {
        schema
    }

    /// Transform input arguments before tool execution.
    ///
    /// Called before each tool invocation. Use this to:
    /// - Extract fields you added via `wrap_schema`
    /// - Convert field formats (e.g., time zones)
    /// - Validate or sanitize inputs
    ///
    /// # Arguments
    /// * `args` - Arguments from the LLM
    /// * `ctx` - Context about the current tool call
    ///
    /// # Returns
    /// Transformed args and optionally extracted data
    fn transform_args(&self, args: Value, _ctx: &ToolCallContext) -> TransformArgsResult {
        TransformArgsResult::new(args)
    }

    /// Transform output after tool execution.
    ///
    /// Called after each successful tool invocation. Use this to:
    /// - Convert field formats in the response
    /// - Add metadata to the response
    /// - Trigger side effects (logging, metrics)
    ///
    /// # Arguments
    /// * `output` - Output from the inner tool
    /// * `ctx` - Context about the current tool call
    /// * `extracted` - Data extracted during `transform_args`
    ///
    /// # Returns
    /// Transformed output
    fn transform_output(
        &self,
        output: String,
        _outcome: &CallOutcome,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> TransformOutputResult {
        TransformOutputResult::new(output)
    }

    /// Handle tool execution errors.
    ///
    /// Called when the inner tool returns an error. Use this to:
    /// - Log errors
    /// - Transform error messages
    /// - Trigger alerts
    ///
    /// # Arguments
    /// * `error` - Error from the inner tool
    /// * `ctx` - Context about the current tool call
    /// * `extracted` - Data extracted during `transform_args`
    ///
    /// # Returns
    /// The error (possibly transformed)
    fn handle_error(
        &self,
        error: ToolError,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
    ) -> ToolError {
        error
    }

    /// Validate arguments after transformation but before tool execution.
    ///
    /// Called after `transform_args` in `WrappedTool::call`. If this returns
    /// an error, the tool call is rejected without executing the inner tool,
    /// and the error is returned to the LLM for potential retry.
    ///
    /// `on_complete` is still called on validation failure so wrappers can
    /// clean up (e.g., emit CallCompleted events).
    ///
    /// # Arguments
    /// * `args` - The cleaned arguments (after transform_args)
    /// * `extracted` - Data extracted during `transform_args`
    /// * `ctx` - Context about the current tool call
    ///
    /// # Returns
    /// `Ok(())` to proceed, or `Err(ToolError)` to reject the call
    fn validate_args(
        &self,
        _args: &Value,
        _extracted: Option<&Value>,
        _ctx: &ToolCallContext,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    /// Async hook called after tool completion (success or failure).
    ///
    /// Use this for async side effects like:
    /// - Persisting tool call records
    /// - Sending metrics to external services
    /// - Async logging
    ///
    /// This is called after `transform_output` or `handle_error`.
    /// Also called on validation failure from `validate_args`.
    /// Failures here are logged but don't affect the tool result.
    ///
    /// # Arguments
    /// * `ctx` - Context about the current tool call
    /// * `extracted` - Data extracted during `transform_args`
    /// * `result` - The final result (output or error message)
    /// * `duration_ms` - How long the tool call took
    async fn on_complete(
        &self,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
        _result: Result<&str, &str>,
        _duration_ms: u64,
    ) {
        // Default: no-op
    }
}

/// A tool wrapped with a `ToolWrapper` implementation.
///
/// This struct implements Rig's `Tool` trait, delegating to the inner
/// tool while applying transformations from the wrapper.
#[derive(Clone)]
pub struct WrappedTool<T>
where
    T: RigTool + Send + Sync + Clone,
{
    inner: T,
    wrapper: Arc<dyn ToolWrapper>,
    /// Optional context factory for creating per-call context
    context_factory: Option<ToolContextFactory>,
}

impl<T> WrappedTool<T>
where
    T: RigTool + Send + Sync + Clone,
{
    /// Create a new wrapped tool.
    pub fn new(inner: T, wrapper: Arc<dyn ToolWrapper>) -> Self {
        Self {
            inner,
            wrapper,
            context_factory: None,
        }
    }

    /// Set a context factory for creating per-call context.
    ///
    /// The factory receives the tool name and should return a `ToolCallContext`.
    pub fn with_context_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn(&str) -> ToolCallContext + Send + Sync + 'static,
    {
        self.context_factory = Some(Arc::new(factory));
        self
    }
}

impl<T> RigTool for WrappedTool<T>
where
    T: RigTool<Args = Value, Output = String, Error = ToolError> + Send + Sync + Clone + 'static,
{
    type Error = ToolError;
    type Args = Value;
    type Output = String;

    const NAME: &'static str = "wrapped_tool";

    fn name(&self) -> String {
        self.inner.name()
    }

    #[allow(refining_impl_trait)]
    fn definition(
        &self,
        prompt: String,
    ) -> Pin<Box<dyn Future<Output = rig::completion::ToolDefinition> + Send + Sync + '_>> {
        let inner = self.inner.clone();
        let wrapper = self.wrapper.clone();

        Box::pin(async move {
            let mut def = inner.definition(prompt).await;
            def.parameters = wrapper.wrap_schema(def.parameters);
            def
        })
    }

    #[allow(refining_impl_trait)]
    fn call(
        &self,
        args: Self::Args,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + Sync + '_>> {
        let inner = self.inner.clone();
        let wrapper = self.wrapper.clone();
        let tool_name = self.inner.name();
        let context_factory = self.context_factory.clone();

        Box::pin(async move {
            let start = std::time::Instant::now();

            // Create context
            let mut ctx = context_factory
                .as_ref()
                .map(|f| f(&tool_name))
                .unwrap_or_else(|| ToolCallContext::new(&tool_name));

            // Transform args
            let transform_result = wrapper.transform_args(args, &ctx);
            let clean_args = transform_result.args;
            let extracted = transform_result.extracted;

            // Store clean args on context so on_complete can persist them
            ctx.metadata = Some(clean_args.clone());

            // Validate args (wrappers can reject tool calls here)
            if let Err(validation_error) =
                wrapper.validate_args(&clean_args, extracted.as_ref(), &ctx)
            {
                let duration_ms = start.elapsed().as_millis() as u64;
                let error_msg = validation_error.to_string();

                // Still call on_complete so wrappers can clean up
                // (e.g., observer emits CallCompleted for the orphaned CallStarted)
                let wrapper_clone = wrapper.clone();
                let ctx_clone = ctx.clone();
                let extracted_clone = extracted.clone();
                tokio::spawn(async move {
                    wrapper_clone
                        .on_complete(
                            &ctx_clone,
                            extracted_clone.as_ref(),
                            Err(&error_msg),
                            duration_ms,
                        )
                        .await;
                });

                return Err(validation_error);
            }

            // Call inner tool (spawn to handle non-Sync futures).
            // Propagate the current span so mcp.tool_call nests under execute_tool.
            let inner_clone = inner.clone();
            let args_clone = clean_args.clone();
            let tool_span = tracing::Span::current();
            let result_handle = tokio::spawn(tracing::Instrument::instrument(
                async move { inner_clone.call(args_clone).await },
                tool_span,
            ));

            let result = match result_handle.await {
                Ok(r) => r,
                Err(join_error) => Err(ToolError::ToolCallError(join_error.into())),
            };

            let duration_ms = start.elapsed().as_millis() as u64;

            // Transform result
            match result {
                Ok(output) => {
                    let outcome = CallOutcome::classify_from_output(&output);
                    let transformed =
                        wrapper.transform_output(output, &outcome, &ctx, extracted.as_ref());

                    if let Some(warning) = &transformed.warning {
                        tracing::warn!("Tool wrapper warning for {}: {}", tool_name, warning);
                    }

                    // Spawn async completion hook (fire-and-forget, don't block the response)
                    let wrapper_clone = wrapper.clone();
                    let ctx_clone = ctx.clone();
                    let extracted_clone = extracted.clone();
                    let output_clone = transformed.output.clone();
                    tokio::spawn(async move {
                        wrapper_clone
                            .on_complete(
                                &ctx_clone,
                                extracted_clone.as_ref(),
                                Ok(&output_clone),
                                duration_ms,
                            )
                            .await;
                    });

                    Ok(transformed.output)
                }
                Err(error) => {
                    let transformed_error = wrapper.handle_error(error, &ctx, extracted.as_ref());
                    let error_msg = transformed_error.to_string();

                    // Spawn async completion hook (fire-and-forget, don't block the response)
                    let wrapper_clone = wrapper.clone();
                    let ctx_clone = ctx.clone();
                    let extracted_clone = extracted.clone();
                    tokio::spawn(async move {
                        wrapper_clone
                            .on_complete(
                                &ctx_clone,
                                extracted_clone.as_ref(),
                                Err(&error_msg),
                                duration_ms,
                            )
                            .await;
                    });

                    Err(transformed_error)
                }
            }
        })
    }
}

/// Compose multiple wrappers into a single wrapper.
///
/// Wrappers are applied in order:
/// - Schema: first wrapper's output feeds into second, etc.
/// - Args: first wrapper transforms, then second, etc.
/// - Output: last wrapper transforms first, then second-to-last, etc. (reverse)
/// - Errors: same as output (reverse order)
/// - on_complete: all wrappers called in parallel
///
/// Asymmetry to know about when composing your own wrapper with a built-in
/// one (e.g. scratchpad, persistence): schema/args walk the vec forward, but
/// output/error walk it in reverse. So a wrapper placed *after* the
/// built-in in the vec sees the **raw** tool output but a
/// **built-in-modified** schema and args (e.g. extra scratchpad fields
/// stripped from args before your wrapper runs). Audit / logging wrappers
/// that ignore schema and args are unaffected; wrappers that introspect
/// schema or transform args need to account for this.
pub struct ComposedWrapper {
    wrappers: Vec<Arc<dyn ToolWrapper>>,
}

impl ComposedWrapper {
    /// Create a new composed wrapper from a list of wrappers.
    pub fn new(wrappers: Vec<Arc<dyn ToolWrapper>>) -> Self {
        Self { wrappers }
    }
}

#[async_trait]
impl ToolWrapper for ComposedWrapper {
    fn wrap_schema(&self, mut schema: Value) -> Value {
        for wrapper in &self.wrappers {
            schema = wrapper.wrap_schema(schema);
        }
        schema
    }

    fn transform_args(&self, mut args: Value, ctx: &ToolCallContext) -> TransformArgsResult {
        let mut all_extracted = Vec::new();

        for wrapper in &self.wrappers {
            let result = wrapper.transform_args(args, ctx);
            args = result.args;
            if let Some(extracted) = result.extracted {
                all_extracted.push(extracted);
            }
        }

        TransformArgsResult {
            args,
            extracted: if all_extracted.is_empty() {
                None
            } else {
                Some(Value::Array(all_extracted))
            },
        }
    }

    fn validate_args(
        &self,
        args: &Value,
        extracted: Option<&Value>,
        ctx: &ToolCallContext,
    ) -> Result<(), ToolError> {
        for wrapper in &self.wrappers {
            wrapper.validate_args(args, extracted, ctx)?;
        }
        Ok(())
    }

    fn transform_output(
        &self,
        mut output: String,
        outcome: &CallOutcome,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
    ) -> TransformOutputResult {
        for wrapper in self.wrappers.iter().rev() {
            let result = wrapper.transform_output(output, outcome, ctx, extracted);
            output = result.output;
        }
        TransformOutputResult::new(output)
    }

    fn handle_error(
        &self,
        mut error: ToolError,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
    ) -> ToolError {
        // Apply in reverse order
        for wrapper in self.wrappers.iter().rev() {
            error = wrapper.handle_error(error, ctx, extracted);
        }
        error
    }

    async fn on_complete(
        &self,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
        result: Result<&str, &str>,
        duration_ms: u64,
    ) {
        // Call all wrappers (could parallelize with join_all if needed)
        for wrapper in &self.wrappers {
            wrapper
                .on_complete(ctx, extracted, result, duration_ms)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoOpWrapper;

    #[async_trait]
    impl ToolWrapper for NoOpWrapper {}

    #[test]
    fn test_tool_call_context_builder() {
        let ctx = ToolCallContext::new("test_tool")
            .with_correlation_id("req-123")
            .with_task_context(1, String::from("initiator"), 2)
            .with_metadata(serde_json::json!({"key": "value"}));

        assert_eq!(ctx.tool_name, "test_tool");
        assert_eq!(ctx.correlation_id, Some("req-123".to_string()));
        assert_eq!(ctx.task_id, Some(1));
        assert_eq!(ctx.attempt, Some(2));
        assert!(ctx.metadata.is_some());
    }

    #[test]
    fn test_transform_args_result() {
        let args = serde_json::json!({"x": 1});
        let result = TransformArgsResult::new(args.clone());
        assert_eq!(result.args, args);
        assert!(result.extracted.is_none());

        let extracted = serde_json::json!({"reasoning": "test"});
        let result = TransformArgsResult::with_extracted(args.clone(), extracted.clone());
        assert_eq!(result.args, args);
        assert_eq!(result.extracted, Some(extracted));
    }

    #[test]
    fn test_transform_output_result() {
        let result = TransformOutputResult::new("output".to_string());
        assert_eq!(result.output, "output");
        assert!(result.success);
        assert!(result.warning.is_none());

        let result = TransformOutputResult::with_warning("output".to_string(), "minor issue");
        assert_eq!(result.output, "output");
        assert!(result.success);
        assert_eq!(result.warning, Some("minor issue".to_string()));
    }

    #[test]
    fn test_noop_wrapper_passthrough() {
        let wrapper = NoOpWrapper;

        // Schema unchanged
        let schema = serde_json::json!({"type": "object"});
        assert_eq!(wrapper.wrap_schema(schema.clone()), schema);

        // Args unchanged
        let args = serde_json::json!({"x": 1});
        let ctx = ToolCallContext::new("test");
        let result = wrapper.transform_args(args.clone(), &ctx);
        assert_eq!(result.args, args);
        assert!(result.extracted.is_none());

        // Output unchanged
        let output = "test output".to_string();
        let outcome = CallOutcome::Success(output.clone());
        let result = wrapper.transform_output(output.clone(), &outcome, &ctx, None);
        assert_eq!(result.output, output);
    }

    struct SchemaModifyingWrapper;

    #[async_trait]
    impl ToolWrapper for SchemaModifyingWrapper {
        fn wrap_schema(&self, mut schema: Value) -> Value {
            if let Value::Object(ref mut obj) = schema {
                obj.insert("modified".to_string(), Value::Bool(true));
            }
            schema
        }
    }

    #[test]
    fn test_schema_modifying_wrapper() {
        let wrapper = SchemaModifyingWrapper;
        let schema = serde_json::json!({"type": "object"});
        let modified = wrapper.wrap_schema(schema);

        assert_eq!(modified["type"], "object");
        assert_eq!(modified["modified"], true);
    }

    struct ExtractingWrapper;

    #[async_trait]
    impl ToolWrapper for ExtractingWrapper {
        fn transform_args(&self, mut args: Value, _ctx: &ToolCallContext) -> TransformArgsResult {
            let extracted = if let Value::Object(ref mut obj) = args {
                obj.remove("_extract_me")
            } else {
                None
            };

            TransformArgsResult { args, extracted }
        }
    }

    #[test]
    fn test_extracting_wrapper() {
        let wrapper = ExtractingWrapper;
        let args = serde_json::json!({
            "real_arg": "value",
            "_extract_me": "extracted_value"
        });

        let ctx = ToolCallContext::new("test");
        let result = wrapper.transform_args(args, &ctx);

        // _extract_me should be removed from args
        assert!(result.args.get("_extract_me").is_none());
        assert_eq!(result.args["real_arg"], "value");

        // And captured in extracted
        assert_eq!(
            result.extracted,
            Some(Value::String("extracted_value".to_string()))
        );
    }

    #[test]
    fn test_composed_wrapper_schema() {
        struct AddFieldA;
        #[async_trait]
        impl ToolWrapper for AddFieldA {
            fn wrap_schema(&self, mut schema: Value) -> Value {
                if let Value::Object(ref mut obj) = schema {
                    obj.insert("field_a".to_string(), Value::Bool(true));
                }
                schema
            }
        }

        struct AddFieldB;
        #[async_trait]
        impl ToolWrapper for AddFieldB {
            fn wrap_schema(&self, mut schema: Value) -> Value {
                if let Value::Object(ref mut obj) = schema {
                    obj.insert("field_b".to_string(), Value::Bool(true));
                }
                schema
            }
        }

        let composed = ComposedWrapper::new(vec![
            Arc::new(AddFieldA) as Arc<dyn ToolWrapper>,
            Arc::new(AddFieldB) as Arc<dyn ToolWrapper>,
        ]);

        let schema = serde_json::json!({"type": "object"});
        let modified = composed.wrap_schema(schema);

        assert_eq!(modified["field_a"], true);
        assert_eq!(modified["field_b"], true);
    }
}
