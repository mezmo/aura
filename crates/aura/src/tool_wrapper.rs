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
//! use std::sync::Arc;
//!
//! use aura::mcp::CallOutcome;
//! use aura::tool_wrapper::{
//!     ToolCallContext, ToolWrapper, TransformOutputResult, WrappedTool,
//! };
//! use serde_json::Value;
//!
//! // A wrapper that observes tool output; every method has a passthrough
//! // default, so override only what you need.
//! struct MetricsWrapper;
//!
//! #[async_trait::async_trait]
//! impl ToolWrapper for MetricsWrapper {
//!     async fn transform_output(
//!         &self,
//!         output: String,
//!         _outcome: &CallOutcome,
//!         _ctx: &ToolCallContext,
//!         _extracted: Option<&Value>,
//!     ) -> TransformOutputResult {
//!         TransformOutputResult::new(output)
//!     }
//! }
//!
//! // Wrap a tool (inner_tool: any rig Tool with Value args / String output)
//! let wrapped = WrappedTool::new(inner_tool, Arc::new(MetricsWrapper));
//! ```

use async_trait::async_trait;
use rig::tool::{Tool as RigTool, ToolError};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::config::ToolContextFactory;
use crate::mcp::CallOutcome;

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
    /// Agent's authored reasoning for the pending tool call.
    pub tool_call_intent: Option<String>,
    /// Park-owned execution state for this call: reservation lease,
    /// cancellation token, and task tracker. `None` keeps the current
    /// unscoped behavior for every non-park call.
    pub execution_scope: Option<Arc<crate::orchestration::RunExecutionScope>>,
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

    /// Attach park-owned execution state (reservation lease, cancellation,
    /// task tracker). Park calls carry the scope so detached work registers
    /// and fences under it; non-park calls leave it unset.
    #[must_use]
    pub fn with_execution_scope(
        mut self,
        scope: Arc<crate::orchestration::RunExecutionScope>,
    ) -> Self {
        self.execution_scope = Some(scope);
        self
    }
}

/// Blank (empty or whitespace-only) reasoning counts as absent.
pub(crate) fn non_blank(s: &str) -> Option<&str> {
    (!s.trim().is_empty()).then_some(s)
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

/// Result of an async pre-call gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCallOutcome {
    /// Continue to the wrapped tool, optionally carrying approver header
    /// overrides captured at approval. `None` everywhere except an approved
    /// config-gated call whose webhook response carried the configured
    /// identity headers.
    Proceed {
        overrides: Option<crate::approver_headers::ApproverHeaders>,
    },
    /// Skip the wrapped tool and return this output as a successful tool result.
    ShortCircuit { output: String },
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
    async fn transform_output(
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

    /// Populate advisory context fields from data extracted during
    /// `transform_args`.
    ///
    /// Called once between `transform_args` and `pre_call` in
    /// `WrappedTool::call`, after the composed `extracted` payload is
    /// available. Default is a no-op; wrappers that capture cross-cutting
    /// data in `transform_args` (e.g. `PersistenceWrapper` stashes the
    /// pre-strip reasoning) override this to surface it as a typed field
    /// on the context rather than requiring every reader to search the
    /// `extracted` `Value` blob by string key.
    fn write_context(&self, _extracted: Option<&Value>, _ctx: &mut ToolCallContext) {}

    /// Async hook called before tool execution.
    ///
    /// Use this for async gates that must run before the tool executes, such as
    /// approval workflows that call external services or park for a human
    /// decision.
    ///
    /// Returns [`PreCallOutcome::Proceed`] to continue, [`PreCallOutcome::ShortCircuit`]
    /// to skip the inner tool with a model-visible result, or `Err(ToolError)`
    /// to reject the call as a true tool error.
    ///
    /// `WrappedTool::call` invokes this after `validate_args` and before the
    /// inner tool runs. Like `validate_args`, a rejection still runs
    /// `on_complete` so wrappers can clean up.
    async fn pre_call(
        &self,
        _args: &Value,
        _ctx: &ToolCallContext,
    ) -> Result<PreCallOutcome, ToolError> {
        Ok(PreCallOutcome::Proceed { overrides: None })
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

fn spawn_scoped<F>(
    scope: &Option<Arc<crate::orchestration::RunExecutionScope>>,
    future: F,
) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match scope {
        Some(scope) => scope.spawn_tracked(future),
        None => tokio::spawn(future),
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
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + '_>> {
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

            // Let wrappers surface advisory fields from extracted data onto
            // the context (e.g. PersistenceWrapper publishes the pre-strip
            // reasoning as `tool_call_intent` for the HITL gate). Runs after
            // transform_args (which produces `extracted`) and before pre_call
            // (which reads it).
            wrapper.write_context(extracted.as_ref(), &mut ctx);

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
                let validate_scope = ctx.execution_scope.clone();
                spawn_scoped(&validate_scope, async move {
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

            // Async pre-call gate (e.g. HITL approval): may call an external
            // service or park for a human decision before the tool runs.
            // Spawned so the gate survives caller cancellation.
            let pre_wrapper = wrapper.clone();
            let pre_args = clean_args.clone();
            let pre_ctx = ctx.clone();
            let pre_span = tracing::Span::current();
            let pre_scope = ctx.execution_scope.clone();
            let pre_handle = spawn_scoped(
                &pre_scope,
                tracing::Instrument::instrument(
                    async move { pre_wrapper.pre_call(&pre_args, &pre_ctx).await },
                    pre_span,
                ),
            );
            let pre_call_result = match pre_handle.await {
                Ok(r) => r,
                Err(join_error) => Err(ToolError::ToolCallError(join_error.into())),
            };
            let approver_overrides = match pre_call_result {
                // Approver overrides are scoped into the task-local inside
                // the inner-call spawn below — task-locals do not cross a
                // spawn, the value is known here after pre_call, and a
                // demanded override silently degrading to None would
                // proceed under cached identity, which is fail-open.
                Ok(PreCallOutcome::Proceed { overrides }) => overrides,
                Ok(PreCallOutcome::ShortCircuit { output }) => {
                    let duration_ms = start.elapsed().as_millis() as u64;

                    let wrapper_clone = wrapper.clone();
                    let ctx_clone = ctx.clone();
                    let extracted_clone = extracted.clone();
                    let output_clone = output.clone();
                    let short_circuit_scope = ctx.execution_scope.clone();
                    spawn_scoped(&short_circuit_scope, async move {
                        wrapper_clone
                            .on_complete(
                                &ctx_clone,
                                extracted_clone.as_ref(),
                                Ok(&output_clone),
                                duration_ms,
                            )
                            .await;
                    });

                    return Ok(output);
                }
                Err(pre_call_error) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let error_msg = pre_call_error.to_string();

                    let wrapper_clone = wrapper.clone();
                    let ctx_clone = ctx.clone();
                    let extracted_clone = extracted.clone();
                    let pre_error_scope = ctx.execution_scope.clone();
                    spawn_scoped(&pre_error_scope, async move {
                        wrapper_clone
                            .on_complete(
                                &ctx_clone,
                                extracted_clone.as_ref(),
                                Err(&error_msg),
                                duration_ms,
                            )
                            .await;
                    });

                    return Err(pre_call_error);
                }
            };

            // Call inner tool in a spawned task to isolate panics.
            // Propagate the current span so mcp.tool_call nests under execute_tool.
            let inner_clone = inner.clone();
            let args_clone = clean_args.clone();
            let tool_span = tracing::Span::current();
            let inner_scope = ctx.execution_scope.clone();
            let result_handle = spawn_scoped(
                &inner_scope,
                tracing::Instrument::instrument(
                    crate::approver_headers::APPROVER_OVERRIDES
                        .scope(approver_overrides, async move {
                            inner_clone.call(args_clone).await
                        }),
                    tool_span,
                ),
            );
            let result = match result_handle.await {
                Ok(r) => r,
                Err(join_error) => Err(ToolError::ToolCallError(join_error.into())),
            };

            let duration_ms = start.elapsed().as_millis() as u64;

            // Transform result
            match result {
                Ok(output) => {
                    // Supervise the transform + completion hook in a spawned task
                    // so it runs to completion even if the request is cancelled.
                    let wrapper_clone = wrapper.clone();
                    let ctx_clone = ctx.clone();
                    let extracted_clone = extracted.clone();
                    let span = tracing::Span::current();
                    let transform_scope = ctx.execution_scope.clone();
                    let transform_handle = spawn_scoped(
                        &transform_scope,
                        tracing::Instrument::instrument(
                            async move {
                                let outcome = CallOutcome::classify_from_output(&output);
                                let transformed = wrapper_clone
                                    .transform_output(
                                        output,
                                        &outcome,
                                        &ctx_clone,
                                        extracted_clone.as_ref(),
                                    )
                                    .await;

                                // Fire-and-forget completion hook.
                                let output_clone = transformed.output.clone();
                                let nested_scope = ctx_clone.execution_scope.clone();
                                spawn_scoped(&nested_scope, async move {
                                    wrapper_clone
                                        .on_complete(
                                            &ctx_clone,
                                            extracted_clone.as_ref(),
                                            Ok(&output_clone),
                                            duration_ms,
                                        )
                                        .await;
                                });

                                transformed
                            },
                            span,
                        ),
                    );
                    let transformed = match transform_handle.await {
                        Ok(t) => t,
                        Err(join_error) => {
                            // Emit failure completion on transform panic to close state.
                            let error = ToolError::ToolCallError(join_error.into());
                            let error_msg = error.to_string();
                            let wrapper_clone = wrapper.clone();
                            let ctx_clone = ctx.clone();
                            let extracted_clone = extracted.clone();
                            let panic_scope = ctx.execution_scope.clone();
                            spawn_scoped(&panic_scope, async move {
                                wrapper_clone
                                    .on_complete(
                                        &ctx_clone,
                                        extracted_clone.as_ref(),
                                        Err(&error_msg),
                                        duration_ms,
                                    )
                                    .await;
                            });
                            return Err(error);
                        }
                    };

                    if let Some(warning) = &transformed.warning {
                        tracing::warn!("Tool wrapper warning for {}: {}", tool_name, warning);
                    }

                    Ok(transformed.output)
                }
                Err(error) => {
                    let transformed_error = wrapper.handle_error(error, &ctx, extracted.as_ref());
                    let error_msg = transformed_error.to_string();

                    // Spawn async completion hook (fire-and-forget, don't block the response)
                    let wrapper_clone = wrapper.clone();
                    let ctx_clone = ctx.clone();
                    let extracted_clone = extracted.clone();
                    let error_scope = ctx.execution_scope.clone();
                    spawn_scoped(&error_scope, async move {
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

    fn write_context(&self, extracted: Option<&Value>, ctx: &mut ToolCallContext) {
        for wrapper in &self.wrappers {
            wrapper.write_context(extracted, ctx);
        }
    }

    async fn pre_call(
        &self,
        args: &Value,
        ctx: &ToolCallContext,
    ) -> Result<PreCallOutcome, ToolError> {
        // Forward to each wrapper in order, short-circuiting on the first
        // non-proceed outcome. Without this, a composed gate (e.g. HITL) would
        // silently inherit the no-op default and never run.
        //
        // At most one wrapper may produce approver overrides. Identity is
        // never chosen by wrapper order, so a second producer is a
        // deterministic error in release and debug alike, not last-wins.
        let mut overrides: Option<crate::approver_headers::ApproverHeaders> = None;
        for wrapper in &self.wrappers {
            match wrapper.pre_call(args, ctx).await? {
                PreCallOutcome::Proceed {
                    overrides: produced,
                } => match (overrides.is_some(), produced) {
                    (true, Some(_)) => {
                        return Err(ToolError::ToolCallError(Box::new(
                            crate::approver_headers::OverrideApplicationError::DoubleOverride,
                        )));
                    }
                    (false, Some(produced)) => overrides = Some(produced),
                    (_, None) => {}
                },
                outcome @ PreCallOutcome::ShortCircuit { .. } => return Ok(outcome),
            }
        }
        Ok(PreCallOutcome::Proceed { overrides })
    }

    async fn transform_output(
        &self,
        mut output: String,
        outcome: &CallOutcome,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
    ) -> TransformOutputResult {
        for wrapper in self.wrappers.iter().rev() {
            let result = wrapper
                .transform_output(output, outcome, ctx, extracted)
                .await;
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

    #[tokio::test]
    async fn test_noop_wrapper_passthrough() {
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
        let result = wrapper
            .transform_output(output.clone(), &outcome, &ctx, None)
            .await;
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

    #[derive(Clone)]
    struct RecordingInner {
        ran: Arc<std::sync::atomic::AtomicBool>,
    }

    impl RigTool for RecordingInner {
        const NAME: &'static str = "recording_inner";
        type Error = ToolError;
        type Args = Value;
        type Output = String;

        async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
            rig::completion::ToolDefinition {
                name: Self::NAME.to_string(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }

        async fn call(&self, _args: Value) -> Result<String, ToolError> {
            self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("ran".to_string())
        }
    }

    struct RejectingPreCall;

    #[async_trait]
    impl ToolWrapper for RejectingPreCall {
        async fn pre_call(
            &self,
            _args: &Value,
            _ctx: &ToolCallContext,
        ) -> Result<PreCallOutcome, ToolError> {
            Err(ToolError::ToolCallError(
                "rejected by pre_call".to_string().into(),
            ))
        }
    }

    struct ShortCircuitPreCall;

    #[async_trait]
    impl ToolWrapper for ShortCircuitPreCall {
        async fn pre_call(
            &self,
            _args: &Value,
            _ctx: &ToolCallContext,
        ) -> Result<PreCallOutcome, ToolError> {
            Ok(PreCallOutcome::ShortCircuit {
                output: "blocked with feedback".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn pre_call_rejection_blocks_inner_tool() {
        use std::sync::atomic::Ordering;

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner = RecordingInner { ran: ran.clone() };
        let wrapped = WrappedTool::new(inner, Arc::new(RejectingPreCall) as Arc<dyn ToolWrapper>);

        let result = wrapped.call(serde_json::json!({})).await;

        assert!(
            result.is_err(),
            "pre_call rejection must reject the tool call"
        );
        assert!(
            !ran.load(Ordering::SeqCst),
            "inner tool must not run when pre_call rejects"
        );
    }

    #[tokio::test]
    async fn pre_call_short_circuit_returns_feedback_without_inner_tool_error() {
        use std::sync::atomic::Ordering;

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner = RecordingInner { ran: ran.clone() };
        let wrapped =
            WrappedTool::new(inner, Arc::new(ShortCircuitPreCall) as Arc<dyn ToolWrapper>);

        let result = wrapped.call(serde_json::json!({})).await;

        assert_eq!(result.unwrap(), "blocked with feedback");
        assert!(
            !ran.load(Ordering::SeqCst),
            "inner tool must not run when pre_call short-circuits"
        );
    }

    #[tokio::test]
    async fn composed_pre_call_short_circuits_on_first_rejection() {
        use std::sync::atomic::Ordering;

        struct Reject;
        #[async_trait]
        impl ToolWrapper for Reject {
            async fn pre_call(
                &self,
                _a: &Value,
                _c: &ToolCallContext,
            ) -> Result<PreCallOutcome, ToolError> {
                Err(ToolError::ToolCallError("rejected".to_string().into()))
            }
        }

        struct Record(Arc<std::sync::atomic::AtomicBool>);
        #[async_trait]
        impl ToolWrapper for Record {
            async fn pre_call(
                &self,
                _a: &Value,
                _c: &ToolCallContext,
            ) -> Result<PreCallOutcome, ToolError> {
                self.0.store(true, Ordering::SeqCst);
                Ok(PreCallOutcome::Proceed { overrides: None })
            }
        }

        let second_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let composed = ComposedWrapper::new(vec![
            Arc::new(Reject) as Arc<dyn ToolWrapper>,
            Arc::new(Record(second_ran.clone())) as Arc<dyn ToolWrapper>,
        ]);

        let ctx = ToolCallContext::new("t");
        let result = composed.pre_call(&serde_json::json!({}), &ctx).await;

        assert!(
            result.is_err(),
            "composed pre_call must surface the rejection"
        );
        assert!(
            !second_ran.load(Ordering::SeqCst),
            "wrappers after the first rejection must not run"
        );
    }

    /// Composition is a value-loss seam: it builds its own `Proceed`, so the
    /// gate's captured identity survives only if aggregation carries it.
    mod composed_overrides {
        use super::*;
        use crate::approver_headers::tests::captured_overrides;

        struct Produces(&'static str);

        #[async_trait]
        impl ToolWrapper for Produces {
            async fn pre_call(
                &self,
                _a: &Value,
                _c: &ToolCallContext,
            ) -> Result<PreCallOutcome, ToolError> {
                Ok(PreCallOutcome::Proceed {
                    overrides: Some(captured_overrides("x-forwarded-user", self.0)),
                })
            }
        }

        struct Passive;

        #[async_trait]
        impl ToolWrapper for Passive {
            async fn pre_call(
                &self,
                _a: &Value,
                _c: &ToolCallContext,
            ) -> Result<PreCallOutcome, ToolError> {
                Ok(PreCallOutcome::Proceed { overrides: None })
            }
        }

        async fn compose(wrappers: Vec<Arc<dyn ToolWrapper>>) -> Result<PreCallOutcome, ToolError> {
            ComposedWrapper::new(wrappers)
                .pre_call(&serde_json::json!({}), &ToolCallContext::new("t"))
                .await
        }

        #[tokio::test]
        async fn the_single_producers_identity_survives_its_passive_neighbours() {
            let outcome = compose(vec![
                Arc::new(Passive),
                Arc::new(Produces("alice")),
                Arc::new(Passive),
            ])
            .await
            .expect("one producer composes cleanly");

            assert_eq!(
                outcome,
                PreCallOutcome::Proceed {
                    overrides: Some(captured_overrides("x-forwarded-user", "alice")),
                },
            );
        }

        /// Two producers would make wrapper order decide whose identity the
        /// call runs under. The call fails instead.
        #[tokio::test]
        async fn two_producers_fail_the_call_rather_than_pick_one() {
            let error = compose(vec![Arc::new(Produces("alice")), Arc::new(Produces("bob"))])
                .await
                .expect_err("two producers must not resolve to either identity");

            assert!(
                error.to_string().contains("conflicting approver identity"),
                "the error must name the conflict, got: {error}",
            );
        }
    }
}

/// LIFETIME L2 goldens (unit L2, contract "Ownership and lifetime"). Three
/// behavioral reds pin TRACKER MEMBERSHIP: under a scoped call, every
/// fire-and-forget detached tail a `WrappedTool::call` spawns (completion
/// hooks, the inner tool) must be registered with the scope's task tracker,
/// so `scope.drain()` may not return while any live tail is still running.
/// Today each tail is a bare `tokio::spawn`, unregistered, so tests 1-3 fail
/// at their drain-membership assertion and flip green when the integration
/// routes the spawns through `RunExecutionScope::spawn_tracked`. Test 4 is a
/// CHARACTERIZATION, not a golden: it pins the non-park (no scope) behavior
/// that must stay exactly as-is through the integration.
///
/// (LEASE liveness through a tail is deliberately NOT pinned here: a ctx
/// carrying a scope already hands every spawned tail an `Arc` clone that
/// holds the reservation lease, so that property is incidentally true today
/// and belongs to the L1 goldens.)
#[cfg(test)]
mod lifetime_goldens {
    use super::*;

    use crate::orchestration::{ReservationTable, RunExecutionScope, RunId};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Long enough for a spawned tail to make visible progress, short
    /// enough that a hang or a regressed drain fails fast instead of
    /// hanging the suite.
    const GATE_TICK: Duration = Duration::from_millis(500);

    /// Distinct, probe-free run ids parsed through `RunId`'s `FromStr`
    /// (same pattern as the lifetime.rs unit tests).
    fn run_id(uuid: &'static str) -> RunId {
        uuid.parse().expect("well-formed run id")
    }

    /// A scope built over one freshly admitted run, as the resume grant
    /// establishes it (`ReservationTable::admit` + `RunExecutionScope::new`).
    /// The test keeps its own `Arc<RunExecutionScope>` clone for `drain()`;
    /// wrappers keep state in their own `Arc` fields (ctx.metadata is
    /// overwritten at the `WrappedTool::call` seam, so nothing rides on it).
    fn scoped(run: &'static str) -> Arc<RunExecutionScope> {
        let table = ReservationTable::new();
        let lease = table.admit(run_id(run)).expect("admission");
        RunExecutionScope::new(lease)
    }

    /// Spin until a tail's handshake flag is set, bounded so a tail that
    /// never signals cannot hang the test.
    async fn wait_entered(entered: &Arc<AtomicBool>, label: &str) {
        tokio::time::timeout(GATE_TICK, async {
            while !entered.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{label} tail never signaled `entered`"));
    }

    /// A do-nothing inner tool whose Ok path returns a fixed output.
    #[derive(Clone)]
    struct OkInner;

    impl RigTool for OkInner {
        const NAME: &'static str = "lifetime_goldens_ok_inner";
        type Error = ToolError;
        type Args = Value;
        type Output = String;

        async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
            rig::completion::ToolDefinition {
                name: Self::NAME.to_string(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }

        async fn call(&self, _args: Value) -> Result<String, ToolError> {
            Ok("inner output".to_string())
        }
    }

    /// A gated inner tool: signals `gated` through one channel the moment it
    /// is entered, then blocks on a second channel until it is released.
    /// Both channels are `Notify` (stored permits), so the signal is never
    /// lost regardless of waiter registration order.
    #[derive(Clone)]
    struct GatedInner {
        gated: Arc<tokio::sync::Notify>,
        blocker: Arc<tokio::sync::Notify>,
    }

    impl RigTool for GatedInner {
        const NAME: &'static str = "lifetime_goldens_gated_inner";
        type Error = ToolError;
        type Args = Value;
        type Output = String;

        async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
            rig::completion::ToolDefinition {
                name: Self::NAME.to_string(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }

        async fn call(&self, _args: Value) -> Result<String, ToolError> {
            self.gated.notify_one();
            self.blocker.notified().await;
            Ok("inner output".to_string())
        }
    }

    /// A wrapper whose `on_complete` blocks on a gate channel, with an
    /// `entered` handshake set BEFORE the block so the test can prove the
    /// tail is live before asserting drain still waits. `transform_output`
    /// is a passthrough override so the Ok path runs both seams. When
    /// `short_circuit` is set, `pre_call` returns
    /// `PreCallOutcome::ShortCircuit` with that output instead of
    /// proceeding to the inner tool.
    struct GatedCompletion {
        short_circuit: Option<String>,
        gate: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        entered: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl ToolWrapper for GatedCompletion {
        async fn pre_call(
            &self,
            _args: &Value,
            _ctx: &ToolCallContext,
        ) -> Result<PreCallOutcome, ToolError> {
            match &self.short_circuit {
                Some(output) => Ok(PreCallOutcome::ShortCircuit {
                    output: output.clone(),
                }),
                None => Ok(PreCallOutcome::Proceed { overrides: None }),
            }
        }
        async fn transform_output(
            &self,
            output: String,
            _outcome: &CallOutcome,
            _ctx: &ToolCallContext,
            _extracted: Option<&Value>,
        ) -> TransformOutputResult {
            TransformOutputResult::new(output)
        }

        async fn on_complete(
            &self,
            _ctx: &ToolCallContext,
            _extracted: Option<&Value>,
            _result: Result<&str, &str>,
            _duration_ms: u64,
        ) {
            let gate = self.gate.lock().expect("gate cell lock").take();
            self.entered.store(true, Ordering::SeqCst);
            if let Some(gate) = gate {
                let _ = gate.await;
            }
        }
    }

    /// A do-nothing wrapper (the default pass-throughs).
    struct PassThroughWrapper;

    #[async_trait::async_trait]
    impl ToolWrapper for PassThroughWrapper {}

    // Golden 1: the plain-fire-and-forget on_complete tail on the
    // short-circuit path. `pre_call` returns `ShortCircuit`, so the call
    // body spawns the hook DIRECTLY (the non-nested tail) and returns
    // `Ok(output)` without touching the inner tool. RED today: that tail's
    // spawn is unregistered, so `drain()` returns while the hook is still
    // blocked with its gate closed.
    #[tokio::test]
    async fn drain_waits_for_scoped_completion_hook_tail() {
        let scope = scoped("f39a5be0-1b6e-4d02-8c74-2e6d9a30f5b1");
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let entered = Arc::new(AtomicBool::new(false));
        let wrapper: Arc<dyn ToolWrapper> = Arc::new(GatedCompletion {
            short_circuit: Some("short-circuited".to_string()),
            gate: Arc::new(std::sync::Mutex::new(Some(gate))),
            entered: Arc::clone(&entered),
        });
        let scope_for_factory = Arc::clone(&scope);
        let wrapped = WrappedTool::new(OkInner, wrapper).with_context_factory(move |_| {
            ToolCallContext::new("gated_hook_tool")
                .with_execution_scope(Arc::clone(&scope_for_factory))
        });

        // Drive the call through the pre_call short-circuit; the
        // fire-and-forget on_complete tail is spawned directly in the call
        // body and outlives this await.
        let outcome = tokio::time::timeout(GATE_TICK, wrapped.call(serde_json::json!({})))
            .await
            .expect("scoped call completes within GATE_TICK")
            .expect("short-circuited call succeeded");
        assert_eq!(outcome, "short-circuited");

        // Handshake first: the tail has entered its hook, and the drain
        // assertion below must be judged only after that.
        wait_entered(&entered, "completion hook").await;

        assert!(
            tokio::time::timeout(GATE_TICK, scope.drain())
                .await
                .is_err(),
            "drain must NOT complete while the scoped on_complete tail is still blocked on its gate",
        );

        release.send(()).expect("gate still open for release");
        tokio::time::timeout(GATE_TICK, scope.drain())
            .await
            .expect("drain completes once the hook's gate opens");
    }

    // Golden 2: the NESTED on_complete the transform path spawns inside its
    // spawned task. Same red shape as golden 1; the nested spawn is the one
    // the gate holds.
    #[tokio::test]
    async fn drain_waits_for_scoped_nested_completion_hook() {
        let scope = scoped("9d47c1a2-3fb8-4625-b0ea-64c10f8d7e93");
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let entered = Arc::new(AtomicBool::new(false));
        let wrapper: Arc<dyn ToolWrapper> = Arc::new(GatedCompletion {
            short_circuit: None,
            gate: Arc::new(std::sync::Mutex::new(Some(gate))),
            entered: Arc::clone(&entered),
        });
        let scope_for_factory = Arc::clone(&scope);
        let wrapped = WrappedTool::new(OkInner, wrapper).with_context_factory(move |_| {
            ToolCallContext::new("nested_hook_tool")
                .with_execution_scope(Arc::clone(&scope_for_factory))
        });

        let outcome = tokio::time::timeout(GATE_TICK, wrapped.call(serde_json::json!({})))
            .await
            .expect("scoped call completes within GATE_TICK")
            .expect("scoped call succeeded");
        assert_eq!(outcome, "inner output");

        wait_entered(&entered, "nested completion hook").await;

        assert!(
            tokio::time::timeout(GATE_TICK, scope.drain())
                .await
                .is_err(),
            "drain must NOT complete while the nested on_complete tail inside the transform path is still blocked on its gate",
        );

        release.send(()).expect("gate still open for release");
        tokio::time::timeout(GATE_TICK, scope.drain())
            .await
            .expect("drain completes once the nested hook's gate opens");
    }

    // Golden 3: caller cancellation against a gated inner tool. The inner
    // tool signals `gated` before blocking; the test aborts the outer call
    // task only after that signal, with the gate still closed. RED today:
    // the inner tool's spawn is unregistered, so `drain()` returns while
    // the inner tool still runs.
    #[tokio::test]
    async fn drain_waits_for_scoped_inner_tool_after_caller_drops() {
        let scope = scoped("5b8e2f7c-4a91-4d36-9c02-e71b0a4f63d8");
        let gated = Arc::new(tokio::sync::Notify::new());
        let blocker = Arc::new(tokio::sync::Notify::new());
        let inner = GatedInner {
            gated: Arc::clone(&gated),
            blocker: Arc::clone(&blocker),
        };
        let scope_for_factory = Arc::clone(&scope);
        let wrapped = WrappedTool::new(inner, Arc::new(PassThroughWrapper) as Arc<dyn ToolWrapper>)
            .with_context_factory(move |_| {
                ToolCallContext::new("gated_inner_tool")
                    .with_execution_scope(Arc::clone(&scope_for_factory))
            });

        // Caller-side: spawn the call future, then cancel it while the
        // inner tool is gated.
        let caller = tokio::spawn(async move { wrapped.call(serde_json::json!({})).await });

        tokio::time::timeout(GATE_TICK, gated.notified())
            .await
            .expect("inner tool signaled `gated` within GATE_TICK");

        caller.abort();

        assert!(
            tokio::time::timeout(GATE_TICK, scope.drain())
                .await
                .is_err(),
            "drain must NOT complete while the scoped inner tool still runs after the caller aborts",
        );

        blocker.notify_one();
        tokio::time::timeout(GATE_TICK, scope.drain())
            .await
            .expect("drain completes once the inner tool is released");
    }

    // CHARACTERIZATION, not a golden: a plain ctx (no execution_scope) keeps
    // today's unscoped behavior — a full Ok call whose on_complete fires —
    // both now and after the scoped integration. This is the non-park
    // regression guard for the L2 integration.
    #[tokio::test]
    async fn unscoped_call_keeps_current_unscoped_behavior() {
        struct RecordingCompletion {
            completed: Arc<std::sync::Mutex<Vec<String>>>,
        }

        #[async_trait::async_trait]
        impl ToolWrapper for RecordingCompletion {
            async fn on_complete(
                &self,
                _ctx: &ToolCallContext,
                _extracted: Option<&Value>,
                result: Result<&str, &str>,
                _duration_ms: u64,
            ) {
                let result = result.map(str::to_string).map_err(str::to_string);
                self.completed
                    .lock()
                    .expect("completions lock")
                    .push(format!("{result:?}"));
            }
        }

        let completed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let wrapper: Arc<dyn ToolWrapper> = Arc::new(RecordingCompletion {
            completed: Arc::clone(&completed),
        });
        let wrapped = WrappedTool::new(OkInner, Arc::clone(&wrapper) as Arc<dyn ToolWrapper>)
            .with_context_factory(|_| ToolCallContext::new("unscoped_tool"));

        let outcome = tokio::time::timeout(GATE_TICK, wrapped.call(serde_json::json!({})))
            .await
            .expect("unscoped call completes within GATE_TICK")
            .expect("unscoped call succeeded");
        assert_eq!(outcome, "inner output");

        // The fire-and-forget on_complete still ran; give it a bounded
        // window instead of racing it.
        tokio::time::timeout(GATE_TICK, async {
            while completed.lock().expect("completions lock").is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("unscoped on_complete ran within GATE_TICK");
        assert_eq!(
            *completed.lock().expect("completions lock"),
            vec![r#"Ok("inner output")"#.to_string()],
        );
    }
}
