//! Middleware engine behind [`WrappedTool`](crate::tool_wrapper::WrappedTool).
//!
//! A call passes through a [`MiddlewareStack`] in three phases. The forward
//! phase prepares it: every layer, outermost first, transforms the
//! arguments, writes the context, then validates. The onion phase runs it:
//! each layer's [`ToolMiddleware::call`] wraps the next layer's through
//! [`Next`], down to the inner tool, so a layer sees the request on the way
//! in and the reply on the way out, the outermost layer last. Then every
//! layer's `on_complete` is told the result, outermost first, without the
//! caller waiting for it.
//!
//! The forward phase stays apart from the onion because layers read what
//! earlier ones prepared before any gate runs: the HITL gate keys its
//! decision on arguments the persistence layer has already stripped and
//! takes the call's intent from the context that layer wrote, and the
//! observer announces the call before the gate decides on it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use rig::completion::ToolDefinition;
use rig::tool::{Tool as RigTool, ToolError};
use serde_json::Value;

use crate::approver_headers::{APPROVER_OVERRIDES, ApproverHeaders, OverrideApplicationError};
use crate::config::ToolContextFactory;
use crate::tool_error_detection::is_reported_error;
use crate::tool_wrapper::{PreCallOutcome, ToolCallContext, ToolWrapper, TransformArgsResult};

/// A prepared tool call.
#[non_exhaustive]
pub struct ToolRequest {
    /// Arguments for the inner tool.
    pub args: Value,
    pub ctx: Arc<ToolCallContext>,
    /// Data extracted from the raw arguments.
    pub extracted: Option<Arc<Value>>,
    approver_overrides: Option<ApproverHeaders>,
}

impl ToolRequest {
    /// Carry approver identity into the inner call.
    ///
    /// One call runs under one identity: a second producer fails the call
    /// rather than letting layer order choose whose identity it runs under.
    pub fn set_approver_overrides(
        &mut self,
        overrides: ApproverHeaders,
    ) -> Result<(), CallFailure> {
        if self.approver_overrides.is_some() {
            return Err(CallFailure::Rejected(ToolError::ToolCallError(Box::new(
                OverrideApplicationError::DoubleOverride,
            ))));
        }
        self.approver_overrides = Some(overrides);
        Ok(())
    }
}

/// The result of a call that produced one.
#[non_exhaustive]
pub enum ToolReply {
    /// The inner tool's output, and whether its raw output reports a failure.
    Tool { output: String, is_error: bool },
    /// A layer's answer in place of the inner tool's.
    ShortCircuit { output: String },
}

impl ToolReply {
    pub fn into_output(self) -> String {
        match self {
            Self::Tool { output, .. } | Self::ShortCircuit { output } => output,
        }
    }
}

/// Why a call produced no reply.
pub enum CallFailure {
    /// The inner tool's error.
    Tool(ToolError),
    /// A layer's refusal of the call.
    Rejected(ToolError),
    /// A call its caller stopped waiting for.
    Abandoned,
}

impl CallFailure {
    fn into_error(self) -> ToolError {
        match self {
            Self::Tool(e) | Self::Rejected(e) => e,
            Self::Abandoned => ToolError::ToolCallError("tool call abandoned by its caller".into()),
        }
    }
}

/// One layer of a [`MiddlewareStack`].
#[async_trait]
pub trait ToolMiddleware: Send + Sync {
    /// Modify the tool's parameter schema.
    fn wrap_schema(&self, schema: Value) -> Value {
        schema
    }

    /// Transform the arguments, keeping aside anything the layer added to
    /// the schema.
    fn transform_args(&self, args: Value, _ctx: &ToolCallContext) -> TransformArgsResult {
        TransformArgsResult::new(args)
    }

    /// Surface fields from the extracted data onto the context.
    fn write_context(&self, _extracted: Option<&Value>, _ctx: &mut ToolCallContext) {}

    /// Reject the call before any layer's `call` runs.
    fn validate_args(
        &self,
        _args: &Value,
        _extracted: Option<&Value>,
        _ctx: &ToolCallContext,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    /// Run the call: act on the request, hand it to `next`, act on the reply.
    ///
    /// Passes the request straight on. An override reaches the inner tool
    /// only through `next`; it may answer without calling `next`, but must
    /// never run the inner tool itself. Every other method passes through
    /// by default too, so a layer implements only the phases it takes part
    /// in.
    async fn call(&self, req: ToolRequest, next: Next<'_>) -> Result<ToolReply, CallFailure> {
        next.run(req).await
    }

    /// Observe the call's final result.
    async fn on_complete(
        &self,
        _ctx: &ToolCallContext,
        _extracted: Option<&Value>,
        _result: Result<&str, &str>,
        _duration_ms: u64,
    ) {
    }
}

/// The rest of the onion below a layer.
pub struct Next<'a> {
    rest: &'a [Arc<dyn ToolMiddleware>],
    terminal: &'a dyn Terminal,
}

impl Next<'_> {
    /// Hand the request to the next layer, or to the inner tool when this
    /// layer was the innermost.
    pub async fn run(self, req: ToolRequest) -> Result<ToolReply, CallFailure> {
        match self.rest.split_first() {
            Some((layer, rest)) => {
                layer
                    .call(
                        req,
                        Next {
                            rest,
                            terminal: self.terminal,
                        },
                    )
                    .await
            }
            None => self.terminal.invoke(req).await,
        }
    }
}

/// The innermost step of the onion: the tool itself.
#[async_trait]
trait Terminal: Send + Sync {
    async fn invoke(&self, req: ToolRequest) -> Result<ToolReply, CallFailure>;
}

/// The wrapped tool and its caller's presence flag.
struct InnerCall<T> {
    inner: T,
    caller_gone: Arc<AtomicBool>,
}

#[async_trait]
impl<T> Terminal for InnerCall<T>
where
    T: RigTool<Args = Value, Output = String, Error = ToolError> + Send + Sync + Clone + 'static,
{
    /// The tool runs in its own task, so a panic in it is an error of the
    /// call rather than of the onion, under the request's approver identity.
    /// A caller that left before the tool started, or before its result
    /// came back, gets no further work done on its behalf: the result is
    /// dropped as [`CallFailure::Abandoned`] without reaching any layer.
    async fn invoke(&self, req: ToolRequest) -> Result<ToolReply, CallFailure> {
        if self.caller_gone.load(Ordering::SeqCst) {
            return Err(CallFailure::Abandoned);
        }
        let ToolRequest {
            args,
            approver_overrides,
            ..
        } = req;
        let inner = self.inner.clone();
        let span = tracing::Span::current();
        let handle = tokio::spawn(tracing::Instrument::instrument(
            APPROVER_OVERRIDES.scope(approver_overrides, async move { inner.call(args).await }),
            span,
        ));
        let result = match handle.await {
            Ok(r) => r,
            Err(join_error) => Err(ToolError::ToolCallError(join_error.into())),
        };
        if self.caller_gone.load(Ordering::SeqCst) {
            return Err(CallFailure::Abandoned);
        }
        match result {
            Ok(output) => {
                let is_error = is_reported_error(&output);
                Ok(ToolReply::Tool { output, is_error })
            }
            Err(error) => Err(CallFailure::Tool(error)),
        }
    }
}

/// How a stack hands the forward phase's extracted values to its layers.
#[derive(Clone, Copy)]
enum ExtractedShape {
    /// The single layer's own value, as it produced it.
    Direct,
    /// An array of every layer's value, outermost first.
    Array,
}

/// The layers a tool call passes through, outermost first.
#[derive(Clone)]
pub struct MiddlewareStack {
    layers: Arc<[Arc<dyn ToolMiddleware>]>,
    extracted: ExtractedShape,
}

impl MiddlewareStack {
    /// A stack of native layers, outermost first.
    pub fn new(layers: Vec<Arc<dyn ToolMiddleware>>) -> Self {
        Self {
            layers: layers.into(),
            extracted: ExtractedShape::Array,
        }
    }

    /// The stack a [`ToolWrapper`] describes.
    ///
    /// A composed wrapper becomes one [`WrapperLayer`] per wrapper it holds,
    /// so native layers can be placed among them. A wrapper that holds a
    /// composition of its own stays one layer: its layers see the extracted
    /// values nested the way that composition builds them. Any other wrapper
    /// is the stack's only layer and sees its extracted value directly.
    pub fn from_wrapper(root: Arc<dyn ToolWrapper>) -> Self {
        match root.composed_layers() {
            Some(wrappers) => Self::new(
                wrappers
                    .iter()
                    .map(|w| Arc::new(WrapperLayer(Arc::clone(w))) as Arc<dyn ToolMiddleware>)
                    .collect(),
            ),
            None => Self {
                layers: Arc::from([Arc::new(WrapperLayer(root)) as Arc<dyn ToolMiddleware>]),
                extracted: ExtractedShape::Direct,
            },
        }
    }

    pub fn layers(&self) -> &[Arc<dyn ToolMiddleware>] {
        &self.layers
    }

    /// The stack with `inner`'s layers inside every layer it has.
    pub fn around(self, inner: MiddlewareStack) -> Self {
        let mut layers = self.layers.to_vec();
        layers.extend(inner.layers.iter().cloned());
        Self::new(layers)
    }

    /// The stack with `layer` outside every layer it has.
    pub fn wrapped_by(self, layer: Arc<dyn ToolMiddleware>) -> Self {
        let mut layers = Vec::with_capacity(self.layers.len() + 1);
        layers.push(layer);
        layers.extend(self.layers.iter().cloned());
        Self::new(layers)
    }

    /// Every layer's schema change, outermost first.
    fn wrap_schema(&self, mut schema: Value) -> Value {
        for layer in self.layers.iter() {
            schema = layer.wrap_schema(schema);
        }
        schema
    }

    /// The forward phase: each layer transforms the arguments the previous
    /// one produced, the clean arguments go on the context as `metadata`,
    /// and each layer writes the context with every layer's extracted
    /// values in view.
    fn prepare(&self, mut args: Value, ctx: &mut ToolCallContext) -> (Value, Option<Value>) {
        let mut all = Vec::new();
        for layer in self.layers.iter() {
            let result = layer.transform_args(args, ctx);
            args = result.args;
            if let Some(extracted) = result.extracted {
                all.push(extracted);
            }
        }
        let extracted = match self.extracted {
            ExtractedShape::Direct => all.pop(),
            ExtractedShape::Array if all.is_empty() => None,
            ExtractedShape::Array => Some(Value::Array(all)),
        };
        ctx.metadata = Some(args.clone());
        for layer in self.layers.iter() {
            layer.write_context(extracted.as_ref(), ctx);
        }
        (args, extracted)
    }

    /// The first layer's objection, outermost first.
    fn validate(
        &self,
        args: &Value,
        extracted: Option<&Value>,
        ctx: &ToolCallContext,
    ) -> Result<(), ToolError> {
        for layer in self.layers.iter() {
            layer.validate_args(args, extracted, ctx)?;
        }
        Ok(())
    }

    /// Tell every layer the result, outermost first, on a task of its own so
    /// the caller never waits for it.
    fn complete(
        &self,
        ctx: Arc<ToolCallContext>,
        extracted: Option<Arc<Value>>,
        result: Result<String, String>,
        duration_ms: u64,
    ) {
        let layers = Arc::clone(&self.layers);
        tokio::spawn(async move {
            let result = match &result {
                Ok(output) => Ok(output.as_str()),
                Err(error) => Err(error.as_str()),
            };
            for layer in layers.iter() {
                layer
                    .on_complete(&ctx, extracted.as_deref(), result, duration_ms)
                    .await;
            }
        });
    }
}

/// A [`ToolWrapper`] as one layer.
pub struct WrapperLayer(pub Arc<dyn ToolWrapper>);

#[async_trait]
impl ToolMiddleware for WrapperLayer {
    fn wrap_schema(&self, schema: Value) -> Value {
        self.0.wrap_schema(schema)
    }

    fn transform_args(&self, args: Value, ctx: &ToolCallContext) -> TransformArgsResult {
        self.0.transform_args(args, ctx)
    }

    fn write_context(&self, extracted: Option<&Value>, ctx: &mut ToolCallContext) {
        self.0.write_context(extracted, ctx);
    }

    fn validate_args(
        &self,
        args: &Value,
        extracted: Option<&Value>,
        ctx: &ToolCallContext,
    ) -> Result<(), ToolError> {
        self.0.validate_args(args, extracted, ctx)
    }

    /// `pre_call` on the way down; `transform_output` or `handle_error` on
    /// the way up. A short-circuit from below passes up untouched, and an
    /// abandoned call gets no transform at all.
    async fn call(&self, mut req: ToolRequest, next: Next<'_>) -> Result<ToolReply, CallFailure> {
        match self.0.pre_call(&req.args, &req.ctx).await {
            Ok(PreCallOutcome::Proceed { overrides }) => {
                if let Some(overrides) = overrides {
                    req.set_approver_overrides(overrides)?;
                }
            }
            Ok(PreCallOutcome::ShortCircuit { output }) => {
                return Ok(ToolReply::ShortCircuit { output });
            }
            Err(error) => return Err(CallFailure::Rejected(error)),
        }
        let ctx = Arc::clone(&req.ctx);
        let extracted = req.extracted.clone();
        match next.run(req).await {
            Ok(ToolReply::Tool { output, is_error }) => {
                let transformed = self
                    .0
                    .transform_output(output, is_error, &ctx, extracted.as_deref())
                    .await;
                if let Some(warning) = &transformed.warning {
                    tracing::warn!("Tool wrapper warning for {}: {}", ctx.tool_name, warning);
                }
                Ok(ToolReply::Tool {
                    output: transformed.output,
                    is_error,
                })
            }
            Ok(short_circuit) => Ok(short_circuit),
            Err(CallFailure::Tool(error)) => Err(CallFailure::Tool(self.0.handle_error(
                error,
                &ctx,
                extracted.as_deref(),
            ))),
            Err(other) => Err(other),
        }
    }

    async fn on_complete(
        &self,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
        result: Result<&str, &str>,
        duration_ms: u64,
    ) {
        self.0
            .on_complete(ctx, extracted, result, duration_ms)
            .await;
    }
}

/// The caller's presence flag, held by the future awaiting the call.
struct CallerGuard(Arc<AtomicBool>);

impl Drop for CallerGuard {
    /// The future awaiting the call is gone, and with it the caller.
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A rig tool run through a [`MiddlewareStack`].
#[derive(Clone)]
pub struct MiddlewareTool<T> {
    inner: T,
    stack: MiddlewareStack,
    context_factory: Option<ToolContextFactory>,
}

impl<T> MiddlewareTool<T> {
    pub fn new(inner: T, stack: MiddlewareStack) -> Self {
        Self {
            inner,
            stack,
            context_factory: None,
        }
    }

    /// Build each call's context from the tool name instead of the default
    /// bare one.
    pub fn with_context_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn(&str) -> ToolCallContext + Send + Sync + 'static,
    {
        self.context_factory = Some(Arc::new(factory));
        self
    }
}

impl<T> RigTool for MiddlewareTool<T>
where
    T: RigTool<Args = Value, Output = String, Error = ToolError> + Send + Sync + Clone + 'static,
{
    type Error = ToolError;
    type Args = Value;
    type Output = String;

    const NAME: &'static str = "middleware_tool";

    fn name(&self) -> String {
        self.inner.name()
    }

    #[allow(refining_impl_trait)]
    fn definition(
        &self,
        prompt: String,
    ) -> Pin<Box<dyn Future<Output = ToolDefinition> + Send + Sync + '_>> {
        let inner = self.inner.clone();
        let stack = self.stack.clone();
        Box::pin(async move {
            let mut def = inner.definition(prompt).await;
            def.parameters = stack.wrap_schema(def.parameters);
            def
        })
    }

    #[allow(refining_impl_trait)]
    fn call(
        &self,
        args: Self::Args,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + '_>> {
        Box::pin(run_call(
            self.inner.clone(),
            self.stack.clone(),
            self.context_factory.clone(),
            self.inner.name(),
            args,
        ))
    }
}

/// What `on_complete` is told about a reply. A reply that reports a failure
/// completes as `Err`, so no layer has to classify the text again.
fn reported(output: &str, is_error: bool) -> Result<String, String> {
    if is_error {
        Err(output.to_string())
    } else {
        Ok(output.to_string())
    }
}

/// Drive one call through the three phases.
///
/// The onion runs on a task of its own, under the caller's span, so a gate
/// that is mid-decision or a transform that is mid-flight finishes even if
/// the caller stops waiting; the tool itself runs on a further task inside
/// it (see [`InnerCall`]). A panic anywhere in the onion fails the call
/// without any layer's `handle_error`. `on_complete` is told about every
/// result except one the caller had already walked away from before it was
/// decided — a reply that was transformed after the caller left is still
/// reported, since the transform ran to completion on its behalf.
async fn run_call<T>(
    inner: T,
    stack: MiddlewareStack,
    context_factory: Option<ToolContextFactory>,
    tool_name: String,
    args: Value,
) -> Result<String, ToolError>
where
    T: RigTool<Args = Value, Output = String, Error = ToolError> + Send + Sync + Clone + 'static,
{
    let start = Instant::now();
    let elapsed_ms = move || start.elapsed().as_millis() as u64;

    let mut ctx = context_factory
        .as_ref()
        .map(|f| f(&tool_name))
        .unwrap_or_else(|| ToolCallContext::new(&tool_name));
    let (args, extracted) = stack.prepare(args, &mut ctx);
    let ctx = Arc::new(ctx);
    let extracted = extracted.map(Arc::new);

    if let Err(error) = stack.validate(&args, extracted.as_deref(), &ctx) {
        stack.complete(ctx, extracted, Err(error.to_string()), elapsed_ms());
        return Err(error);
    }

    let guard = CallerGuard(Arc::new(AtomicBool::new(false)));
    let caller_gone = Arc::clone(&guard.0);
    let req = ToolRequest {
        args,
        ctx: Arc::clone(&ctx),
        extracted: extracted.clone(),
        approver_overrides: None,
    };
    let onion_stack = stack.clone();
    let (onion_ctx, onion_extracted) = (Arc::clone(&ctx), extracted.clone());
    let onion = tokio::spawn(tracing::Instrument::instrument(
        async move {
            let (ctx, extracted) = (onion_ctx, onion_extracted);
            let terminal = InnerCall {
                inner,
                caller_gone: Arc::clone(&caller_gone),
            };
            let result = Next {
                rest: &onion_stack.layers,
                terminal: &terminal,
            }
            .run(req)
            .await;
            let duration_ms = elapsed_ms();
            let caller_present = || !caller_gone.load(Ordering::SeqCst);
            match result {
                Ok(ToolReply::Tool { output, is_error }) => {
                    onion_stack.complete(ctx, extracted, reported(&output, is_error), duration_ms);
                    Ok(output)
                }
                Ok(ToolReply::ShortCircuit { output }) => {
                    if caller_present() {
                        let is_error = is_reported_error(&output);
                        onion_stack.complete(
                            ctx,
                            extracted,
                            reported(&output, is_error),
                            duration_ms,
                        );
                    }
                    Ok(output)
                }
                Err(CallFailure::Tool(error)) => {
                    onion_stack.complete(ctx, extracted, Err(error.to_string()), duration_ms);
                    Err(error)
                }
                Err(CallFailure::Rejected(error)) => {
                    if caller_present() {
                        onion_stack.complete(ctx, extracted, Err(error.to_string()), duration_ms);
                    }
                    Err(error)
                }
                Err(abandoned @ CallFailure::Abandoned) => Err(abandoned.into_error()),
            }
        },
        tracing::Span::current(),
    ));

    let result = match onion.await {
        Ok(result) => result,
        Err(join_error) => {
            let error = ToolError::ToolCallError(join_error.into());
            stack.complete(ctx, extracted, Err(error.to_string()), elapsed_ms());
            Err(error)
        }
    };
    drop(guard);
    result
}
#[cfg(test)]
mod parity {
    //! Hook-order parity: every scenario a wrapped call can take, recorded
    //! through three scripted layers around a scripted inner tool. The
    //! sequences pin the engine's observable behaviour: which hooks run, in
    //! what order, what `on_complete` is told, and what a caller that walks
    //! away mid-call leaves running.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use rig::tool::{Tool as RigTool, ToolError};
    use serde_json::{Value, json};
    use tokio::sync::Notify;

    use super::{
        CallFailure, MiddlewareStack, MiddlewareTool, Next, ToolMiddleware, ToolReply, ToolRequest,
        WrapperLayer,
    };
    use crate::approver_headers::tests::captured_overrides;
    use crate::tool_wrapper::{
        ComposedWrapper, PreCallOutcome, ToolCallContext, ToolWrapper, TransformArgsResult,
        TransformOutputResult, WrappedTool,
    };

    type Log = Arc<Mutex<Vec<String>>>;

    fn note(log: &Log, entry: impl Into<String>) {
        log.lock().unwrap().push(entry.into());
    }

    fn entries(log: &Log) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    /// A rendezvous: the holder announces it has `entered`, then waits to be
    /// `release`d. Lets a test drop the call future at a known point.
    #[derive(Default)]
    struct Hold {
        entered: Notify,
        release: Notify,
    }

    impl Hold {
        async fn park(&self) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }

    #[derive(Clone, Default)]
    enum Pre {
        #[default]
        Proceed,
        Produce(&'static str),
        Reject,
        ShortCircuit,
        Panic,
        Hold(Arc<Hold>),
    }

    #[derive(Clone, Default)]
    enum Post {
        #[default]
        Pass,
        Panic,
        Hold(Arc<Hold>),
    }

    #[derive(Clone, Default)]
    struct Script {
        reject_validation: bool,
        extract: Option<Value>,
        pre: Pre,
        post: Post,
    }

    struct Layer {
        id: &'static str,
        log: Log,
        script: Script,
    }

    #[async_trait]
    impl ToolWrapper for Layer {
        fn wrap_schema(&self, mut schema: Value) -> Value {
            note(&self.log, format!("{}.schema", self.id));
            schema[self.id] = json!(true);
            schema
        }

        fn transform_args(&self, mut args: Value, _ctx: &ToolCallContext) -> TransformArgsResult {
            note(&self.log, format!("{}.args", self.id));
            args[self.id] = json!("seen");
            TransformArgsResult {
                args,
                extracted: self.script.extract.clone(),
            }
        }

        fn write_context(&self, _extracted: Option<&Value>, ctx: &mut ToolCallContext) {
            note(&self.log, format!("{}.ctx", self.id));
            ctx.correlation_id = Some(format!(
                "{}{}",
                ctx.correlation_id.as_deref().unwrap_or(""),
                self.id
            ));
        }

        fn validate_args(
            &self,
            _args: &Value,
            _extracted: Option<&Value>,
            _ctx: &ToolCallContext,
        ) -> Result<(), ToolError> {
            note(&self.log, format!("{}.validate", self.id));
            if self.script.reject_validation {
                return Err(ToolError::ToolCallError(
                    format!("{} rejects", self.id).into(),
                ));
            }
            Ok(())
        }

        async fn pre_call(
            &self,
            _args: &Value,
            _ctx: &ToolCallContext,
        ) -> Result<PreCallOutcome, ToolError> {
            note(&self.log, format!("{}.pre", self.id));
            match &self.script.pre {
                Pre::Proceed => Ok(PreCallOutcome::Proceed { overrides: None }),
                Pre::Produce(user) => Ok(PreCallOutcome::Proceed {
                    overrides: Some(captured_overrides("x-forwarded-user", user)),
                }),
                Pre::Reject => Err(ToolError::ToolCallError(
                    format!("{} pre rejects", self.id).into(),
                )),
                Pre::ShortCircuit => Ok(PreCallOutcome::ShortCircuit {
                    output: format!("{} short-circuit", self.id),
                }),
                Pre::Panic => panic!("{} pre panics", self.id),
                Pre::Hold(hold) => {
                    hold.park().await;
                    note(&self.log, format!("{}.pre.released", self.id));
                    Ok(PreCallOutcome::Proceed { overrides: None })
                }
            }
        }

        async fn transform_output(
            &self,
            output: String,
            is_error: bool,
            _ctx: &ToolCallContext,
            _extracted: Option<&Value>,
        ) -> TransformOutputResult {
            note(
                &self.log,
                format!("{}.out({})", self.id, if is_error { "err" } else { "ok" }),
            );
            match &self.script.post {
                Post::Pass => {}
                Post::Panic => panic!("{} out panics", self.id),
                Post::Hold(hold) => {
                    hold.park().await;
                    note(&self.log, format!("{}.out.released", self.id));
                }
            }
            TransformOutputResult::new(format!("{output}<{}", self.id))
        }

        fn handle_error(
            &self,
            error: ToolError,
            _ctx: &ToolCallContext,
            _extracted: Option<&Value>,
        ) -> ToolError {
            note(&self.log, format!("{}.err", self.id));
            ToolError::ToolCallError(format!("{error}<{}", self.id).into())
        }

        async fn on_complete(
            &self,
            ctx: &ToolCallContext,
            extracted: Option<&Value>,
            result: Result<&str, &str>,
            _duration_ms: u64,
        ) {
            let (tag, text) = match result {
                Ok(o) => ("ok", o),
                Err(e) => ("err", e),
            };
            note(
                &self.log,
                format!(
                    "{}.done({tag}:{text}|corr={}|meta={}|ex={})",
                    self.id,
                    ctx.correlation_id.as_deref().unwrap_or("-"),
                    ctx.metadata
                        .as_ref()
                        .map(Value::to_string)
                        .unwrap_or_else(|| "-".into()),
                    extracted
                        .map(Value::to_string)
                        .unwrap_or_else(|| "-".into()),
                ),
            );
        }
    }

    #[derive(Clone, Default)]
    enum InnerScript {
        #[default]
        Ok,
        Err,
        Panic,
        Hold(Arc<Hold>),
    }

    #[derive(Clone)]
    struct Inner {
        log: Log,
        script: InnerScript,
        ran: Arc<AtomicBool>,
    }

    impl RigTool for Inner {
        const NAME: &'static str = "inner";
        type Error = ToolError;
        type Args = Value;
        type Output = String;

        async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
            rig::completion::ToolDefinition {
                name: Self::NAME.to_string(),
                description: String::new(),
                parameters: json!({ "type": "object" }),
            }
        }

        async fn call(&self, args: Value) -> Result<String, ToolError> {
            self.ran.store(true, Ordering::SeqCst);
            let user = crate::approver_headers::current_approver_overrides()
                .map(|o| format!("{o:?}"))
                .unwrap_or_else(|| "-".into());
            note(&self.log, format!("inner(args={args},overrides={user})"));
            match &self.script {
                InnerScript::Ok => Ok("raw".to_string()),
                InnerScript::Err => Err(ToolError::ToolCallError("inner failed".into())),
                InnerScript::Panic => panic!("inner panics"),
                InnerScript::Hold(hold) => {
                    hold.park().await;
                    note(&self.log, "inner.released");
                    Ok("raw".to_string())
                }
            }
        }
    }

    struct Harness {
        tool: WrappedTool<Inner>,
        log: Log,
        ran: Arc<AtomicBool>,
    }

    fn layer(id: &'static str, log: &Log, script: Script) -> Arc<dyn ToolWrapper> {
        Arc::new(Layer {
            id,
            log: log.clone(),
            script,
        })
    }

    /// Three layers `a`, `b`, `c` composed as one root wrapper.
    fn composed(scripts: [Script; 3], inner: InnerScript) -> Harness {
        let log: Log = Default::default();
        let [a, b, c] = scripts;
        let root = Arc::new(ComposedWrapper::new(vec![
            layer("a", &log, a),
            layer("b", &log, b),
            layer("c", &log, c),
        ]));
        with_root(root, log, inner)
    }

    fn with_root(root: Arc<dyn ToolWrapper>, log: Log, inner: InnerScript) -> Harness {
        let ran = Arc::new(AtomicBool::new(false));
        let tool = WrappedTool::new(
            Inner {
                log: log.clone(),
                script: inner,
                ran: ran.clone(),
            },
            root,
        )
        .with_context_factory(|name| ToolCallContext::new(name).with_correlation_id("req:"));
        Harness { tool, log, ran }
    }

    fn pre(pre: Pre) -> Script {
        Script {
            pre,
            ..Default::default()
        }
    }

    fn post(post: Post) -> Script {
        Script {
            post,
            ..Default::default()
        }
    }

    /// Give fire-and-forget hooks and detached tasks time to finish.
    async fn settle() {
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            tokio::task::yield_now().await;
        }
    }

    const FORWARD: [&str; 9] = [
        "a.args",
        "b.args",
        "c.args",
        "a.ctx",
        "b.ctx",
        "c.ctx",
        "a.validate",
        "b.validate",
        "c.validate",
    ];

    fn seq(parts: &[&[&str]]) -> Vec<String> {
        parts
            .iter()
            .flat_map(|p| p.iter().map(|s| s.to_string()))
            .collect()
    }

    const ARGS_SEEN: &str = r#"{"a":"seen","b":"seen","c":"seen","x":1}"#;

    fn done(id: &str, result: &str) -> String {
        format!("{id}.done({result}|corr=req:abc|meta={ARGS_SEEN}|ex=-)")
    }

    async fn call(h: &Harness) -> Result<String, ToolError> {
        let r = h.tool.call(json!({ "x": 1 })).await;
        settle().await;
        r
    }

    #[tokio::test]
    async fn success_runs_every_hook_in_composed_order() {
        let h = composed(Default::default(), InnerScript::Ok);
        let out = call(&h).await.unwrap();
        assert_eq!(out, "raw<c<b<a");
        assert_eq!(
            entries(&h.log),
            seq(&[
                &FORWARD,
                &["a.pre", "b.pre", "c.pre"],
                &[&format!("inner(args={ARGS_SEEN},overrides=-)")],
                &["c.out(ok)", "b.out(ok)", "a.out(ok)"],
                &[
                    &done("a", "ok:raw<c<b<a"),
                    &done("b", "ok:raw<c<b<a"),
                    &done("c", "ok:raw<c<b<a"),
                ],
            ])
        );
    }

    #[tokio::test]
    async fn schema_is_wrapped_forward() {
        let h = composed(Default::default(), InnerScript::Ok);
        let def = h.tool.definition(String::new()).await;
        assert_eq!(def.name, "inner");
        assert_eq!(
            def.parameters,
            json!({ "type": "object", "a": true, "b": true, "c": true })
        );
        assert_eq!(
            entries(&h.log),
            seq(&[&["a.schema", "b.schema", "c.schema"]])
        );
    }

    #[tokio::test]
    async fn inner_error_is_handled_in_reverse_then_completed() {
        let h = composed(Default::default(), InnerScript::Err);
        let err = call(&h).await.unwrap_err().to_string();
        assert!(err.ends_with("inner failed<c<b<a"), "got: {err}");
        let log = entries(&h.log);
        assert_eq!(&log[9..12], &["a.pre", "b.pre", "c.pre"]);
        assert!(log[12].starts_with("inner("));
        assert_eq!(&log[13..16], &["c.err", "b.err", "a.err"]);
        assert_eq!(
            &log[16..],
            &[
                done("a", &format!("err:{err}")),
                done("b", &format!("err:{err}")),
                done("c", &format!("err:{err}")),
            ]
        );
    }

    #[tokio::test]
    async fn inner_panic_is_an_inner_error() {
        let h = composed(Default::default(), InnerScript::Panic);
        let err = call(&h).await.unwrap_err().to_string();
        assert!(
            err.contains("panic") && err.ends_with("<c<b<a"),
            "got: {err}"
        );
        let log = entries(&h.log);
        assert_eq!(&log[13..16], &["c.err", "b.err", "a.err"]);
        assert_eq!(log.len(), 19, "three completions follow: {log:?}");
    }

    #[tokio::test]
    async fn validation_failure_stops_before_pre_call_but_completes() {
        let h = composed(
            [
                Default::default(),
                Script {
                    reject_validation: true,
                    ..Default::default()
                },
                Default::default(),
            ],
            InnerScript::Ok,
        );
        let err = call(&h).await.unwrap_err().to_string();
        assert!(err.contains("b rejects"));
        assert!(!h.ran.load(Ordering::SeqCst));
        assert_eq!(
            entries(&h.log),
            seq(&[
                &FORWARD[..8],
                &[
                    &done("a", &format!("err:{err}")),
                    &done("b", &format!("err:{err}")),
                    &done("c", &format!("err:{err}")),
                ],
            ])
        );
    }

    #[tokio::test]
    async fn short_circuit_skips_later_gates_inner_and_transforms() {
        let h = composed(
            [
                Default::default(),
                pre(Pre::ShortCircuit),
                Default::default(),
            ],
            InnerScript::Ok,
        );
        let out = call(&h).await.unwrap();
        assert_eq!(out, "b short-circuit");
        assert!(!h.ran.load(Ordering::SeqCst));
        assert_eq!(
            entries(&h.log),
            seq(&[
                &FORWARD,
                &["a.pre", "b.pre"],
                &[
                    &done("a", "ok:b short-circuit"),
                    &done("b", "ok:b short-circuit"),
                    &done("c", "ok:b short-circuit"),
                ],
            ])
        );
    }

    #[tokio::test]
    async fn pre_call_rejection_bypasses_handle_error() {
        let h = composed(
            [Default::default(), pre(Pre::Reject), Default::default()],
            InnerScript::Ok,
        );
        let err = call(&h).await.unwrap_err().to_string();
        assert_eq!(err, "ToolCallError: b pre rejects");
        assert!(!h.ran.load(Ordering::SeqCst));
        assert_eq!(
            entries(&h.log),
            seq(&[
                &FORWARD,
                &["a.pre", "b.pre"],
                &[
                    &done("a", &format!("err:{err}")),
                    &done("b", &format!("err:{err}")),
                    &done("c", &format!("err:{err}")),
                ],
            ])
        );
    }

    #[tokio::test]
    async fn pre_call_panic_fails_the_call_without_handle_error() {
        let h = composed(
            [Default::default(), pre(Pre::Panic), Default::default()],
            InnerScript::Ok,
        );
        let err = call(&h).await.unwrap_err().to_string();
        assert!(err.contains("panic"), "got: {err}");
        assert!(!h.ran.load(Ordering::SeqCst));
        let log = entries(&h.log);
        assert_eq!(&log[9..11], &["a.pre", "b.pre"]);
        assert!(!log.iter().any(|e| e.ends_with(".err")));
        assert_eq!(log.len(), 14, "three completions follow: {log:?}");
        assert!(log[11..].iter().all(|e| e.contains("err:")));
    }

    #[tokio::test]
    async fn one_producer_scopes_its_identity_into_the_inner_call() {
        let h = composed(
            [
                Default::default(),
                pre(Pre::Produce("alice")),
                Default::default(),
            ],
            InnerScript::Ok,
        );
        call(&h).await.unwrap();
        let inner = entries(&h.log)
            .into_iter()
            .find(|e| e.starts_with("inner("))
            .unwrap();
        assert!(
            inner.contains(r#"captured_names: ["x-forwarded-user"]"#),
            "got: {inner}"
        );
    }

    #[tokio::test]
    async fn two_producers_reject_after_the_second_gate() {
        let h = composed(
            [
                pre(Pre::Produce("alice")),
                Default::default(),
                pre(Pre::Produce("bob")),
            ],
            InnerScript::Ok,
        );
        let err = call(&h).await.unwrap_err().to_string();
        assert!(err.contains("conflicting approver identity"), "got: {err}");
        assert!(!h.ran.load(Ordering::SeqCst));
        let log = entries(&h.log);
        assert_eq!(&log[9..12], &["a.pre", "b.pre", "c.pre"]);
        assert_eq!(log.len(), 15, "three completions follow: {log:?}");
        assert!(log[12..].iter().all(|e| e.contains("err:")));
    }

    #[tokio::test]
    async fn transform_panic_fails_the_call_without_handle_error() {
        let h = composed(
            [Default::default(), post(Post::Panic), Default::default()],
            InnerScript::Ok,
        );
        let err = call(&h).await.unwrap_err().to_string();
        assert!(err.contains("panic"), "got: {err}");
        let log = entries(&h.log);
        assert_eq!(&log[13..15], &["c.out(ok)", "b.out(ok)"]);
        assert!(!log.iter().any(|e| e == "a.out(ok)" || e.ends_with(".err")));
        assert_eq!(log.len(), 18, "three completions follow: {log:?}");
        assert!(log[15..].iter().all(|e| e.contains("err:")));
    }

    /// Drop the call future once `hold` reports it was entered.
    async fn abandon_at(h: &Harness, hold: &Hold) {
        let fut = h.tool.call(json!({ "x": 1 }));
        tokio::select! {
            r = fut => panic!("call finished before the hold: {r:?}"),
            _ = hold.entered.notified() => {}
        }
    }

    #[tokio::test]
    async fn abandoned_during_pre_call_finishes_the_gates_and_nothing_else() {
        let hold = Arc::new(Hold::default());
        let h = composed(
            [
                Default::default(),
                pre(Pre::Hold(hold.clone())),
                Default::default(),
            ],
            InnerScript::Ok,
        );
        abandon_at(&h, &hold).await;
        hold.release.notify_one();
        settle().await;
        assert!(!h.ran.load(Ordering::SeqCst), "inner must not start");
        assert_eq!(
            entries(&h.log),
            seq(&[&FORWARD, &["a.pre", "b.pre", "b.pre.released", "c.pre"]])
        );
    }

    #[tokio::test]
    async fn abandoned_during_inner_drops_transforms_and_completions() {
        let hold = Arc::new(Hold::default());
        let h = composed(Default::default(), InnerScript::Hold(hold.clone()));
        abandon_at(&h, &hold).await;
        hold.release.notify_one();
        settle().await;
        let log = entries(&h.log);
        assert_eq!(log.len(), 14, "{log:?}");
        assert_eq!(log[13], "inner.released");
    }

    #[tokio::test]
    async fn abandoned_during_transform_still_transforms_and_completes() {
        let hold = Arc::new(Hold::default());
        let h = composed(
            [
                Default::default(),
                post(Post::Hold(hold.clone())),
                Default::default(),
            ],
            InnerScript::Ok,
        );
        abandon_at(&h, &hold).await;
        hold.release.notify_one();
        settle().await;
        let log = entries(&h.log);
        assert_eq!(
            &log[13..],
            &[
                "c.out(ok)".to_string(),
                "b.out(ok)".to_string(),
                "b.out.released".to_string(),
                "a.out(ok)".to_string(),
                done("a", "ok:raw<c<b<a"),
                done("b", "ok:raw<c<b<a"),
                done("c", "ok:raw<c<b<a"),
            ]
        );
    }

    fn extracting(id: &'static str, log: &Log) -> Arc<dyn ToolWrapper> {
        layer(
            id,
            log,
            Script {
                extract: Some(json!({ id: 1 })),
                ..Default::default()
            },
        )
    }

    fn extracted_seen(log: &Log, id: &str) -> String {
        let entry = entries(log)
            .into_iter()
            .find(|e| e.starts_with(&format!("{id}.done(")))
            .unwrap();
        entry
            .rsplit("|ex=")
            .next()
            .unwrap()
            .trim_end_matches(')')
            .to_string()
    }

    #[tokio::test]
    async fn a_lone_wrapper_sees_its_extracted_value_directly() {
        let log: Log = Default::default();
        let h = with_root(extracting("a", &log), log, InnerScript::Ok);
        call(&h).await.unwrap();
        assert_eq!(extracted_seen(&h.log, "a"), r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn composed_wrappers_see_an_array_and_nesting_is_preserved() {
        let log: Log = Default::default();
        let inner = Arc::new(ComposedWrapper::new(vec![
            extracting("b", &log),
            extracting("c", &log),
        ]));
        let root = Arc::new(ComposedWrapper::new(vec![extracting("a", &log), inner]));
        let h = with_root(root, log, InnerScript::Ok);
        call(&h).await.unwrap();
        assert_eq!(
            extracted_seen(&h.log, "a"),
            r#"[{"a":1},[{"b":1},{"c":1}]]"#
        );
        assert_eq!(
            extracted_seen(&h.log, "c"),
            r#"[{"a":1},[{"b":1},{"c":1}]]"#
        );
    }

    /// A layer written against the middleware trait directly, placed among
    /// adapted wrappers.
    struct Native {
        log: Log,
    }

    #[async_trait]
    impl ToolMiddleware for Native {
        async fn call(&self, req: ToolRequest, next: Next<'_>) -> Result<ToolReply, CallFailure> {
            note(&self.log, "n.in");
            let reply = next.run(req).await;
            note(&self.log, "n.out");
            match reply {
                Ok(ToolReply::Tool { output, is_error }) => Ok(ToolReply::Tool {
                    output: format!("{output}<n"),
                    is_error,
                }),
                other => other,
            }
        }

        async fn on_complete(
            &self,
            _ctx: &ToolCallContext,
            _extracted: Option<&Value>,
            result: Result<&str, &str>,
            _duration_ms: u64,
        ) {
            note(&self.log, format!("n.done({})", result.unwrap_or("err")));
        }
    }

    #[tokio::test]
    async fn a_native_layer_interleaves_with_adapted_wrappers() {
        let log: Log = Default::default();
        let stack = MiddlewareStack::new(vec![
            Arc::new(WrapperLayer(layer("a", &log, Default::default()))),
            Arc::new(Native { log: log.clone() }),
            Arc::new(WrapperLayer(layer("c", &log, Default::default()))),
        ]);
        let tool = MiddlewareTool::new(
            Inner {
                log: log.clone(),
                script: InnerScript::Ok,
                ran: Default::default(),
            },
            stack,
        );
        let out = tool.call(json!({ "x": 1 })).await.unwrap();
        settle().await;
        assert_eq!(out, "raw<c<n<a");
        let args = r#"{"a":"seen","c":"seen","x":1}"#;
        assert_eq!(
            entries(&log),
            seq(&[
                &[
                    "a.args",
                    "c.args",
                    "a.ctx",
                    "c.ctx",
                    "a.validate",
                    "c.validate"
                ],
                &["a.pre", "n.in", "c.pre"],
                &[&format!("inner(args={args},overrides=-)")],
                &["c.out(ok)", "n.out", "a.out(ok)"],
                &[
                    &format!("a.done(ok:raw<c<n<a|corr=ac|meta={args}|ex=-)"),
                    "n.done(raw<c<n<a)",
                    &format!("c.done(ok:raw<c<n<a|corr=ac|meta={args}|ex=-)"),
                ],
            ])
        );
    }

    #[tokio::test]
    async fn without_a_factory_the_context_is_the_tool_name() {
        let log: Log = Default::default();
        let tool = WrappedTool::new(
            Inner {
                log: log.clone(),
                script: InnerScript::Ok,
                ran: Default::default(),
            },
            layer("a", &log, Default::default()),
        );
        tool.call(json!({})).await.unwrap();
        settle().await;
        let done = entries(&log).pop().unwrap();
        assert!(done.contains("corr=a|"), "got: {done}");
    }
}
