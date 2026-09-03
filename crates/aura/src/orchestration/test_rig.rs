//! Stubbed-model test rig for park/reify (P44, commit 2). Test-only: a
//! scripted [`rig::completion::CompletionModel`], a recording stub tool, and
//! the harness that drives them through the same worker-stream shape
//! production uses.
//!
//! # Contract (the verified seam)
//!
//! - Turn boundaries are deterministic: the streaming hook's
//!   `on_completion_call` fires before *every* model turn, and tools execute
//!   sequentially within a turn (rig fork, the streaming multi-turn loop).
//!   One [`ScriptedTurn`] is one model turn: `stream()` pops the next turn and
//!   delivers it as a `StreamingCompletionResponse`, so turn *n* of the loop
//!   always sees scripted turn *n*, and turn *n+1* always sees turn *n*'s
//!   tool results in its request history.
//! - A gated call's lifecycle, when commit 3 wires it, is: the park arm
//!   raises the approval request, the recorded decision rules the resume
//!   consult. Bounding-line vocabulary (DECISIONS-2026-09-03 item 2):
//!   approvals **request**, decisions **rule**; "ticket" is retired.
//!
//! # Fidelity boundary
//!
//! [`drive_worker`] runs the exact shape
//! [`crate::orchestration::orchestrator::Orchestrator::stream_and_forward`]
//! reaches for a park-mode worker — `stream_chat` + the park-aware
//! [`crate::streaming_request_hook::StreamingRequestHook`] + the rig tool
//! server (`tool_server_handle.call_tool`) — over the scripted model. Full
//! `execute_task` runs go through the worker-model injection seam
//! ([`install_worker_overrides`]): the orchestrator's cfg(test) prelude in
//! `build_worker_provider_agent` builds the worker from a queued override's
//! scripted model and registers its tools through the worker's own wrapper
//! chain, so execute_task-level tests drive the production path unmodified.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, GetTokenUsage, Usage,
};
use rig::message::{AssistantContent, ToolCall, ToolFunction};
use rig::streaming::{
    RawStreamingChoice, RawStreamingToolCall, StreamedUserContent, StreamingChat,
    StreamingCompletionResponse,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};

use crate::streaming_request_hook::{StreamingRequestHook, UsageState};

/// The stub tool's registered name: innocuous by construction, matching no
/// `require_approval` glob, so the smoke run exercises zero park machinery.
pub(crate) const ECHO_TOOL_NAME: &str = "echo_tool";

/// Result text the stub tool returns for every invocation.
pub(crate) const ECHO_TOOL_RESULT: &str = "applied successfully";

/// The stub tool's result as the loop delivers it back to the model: rig
/// JSON-serializes tool outputs, so a plain string arrives quoted. What the
/// gate's sentinel replacement and denial feedback must produce to be
/// indistinguishable from a live chain result.
pub(crate) fn echo_tool_result_wire() -> String {
    serde_json::to_string(ECHO_TOOL_RESULT).expect("a plain string serializes")
}

// ============================================================================
// Scripted model
// ============================================================================

/// One model turn: the full assistant response the loop consumes before the
/// next turn boundary. `text` streams as `Message` chunks (before any tool
/// calls, mirroring real providers); `tool_calls` stream as complete
/// `ToolCall` chunks and are executed sequentially by the loop in script
/// order.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScriptedTurn {
    text: Option<String>,
    tool_calls: Vec<ScriptedToolCall>,
    /// Fail the provider stream after this turn's tool calls ran, instead of
    /// completing the turn. The pinned loop yields the error and ends the
    /// stream without another `on_completion_call`, which is the
    /// provider-error orphan window.
    stream_failed: bool,
}

impl ScriptedTurn {
    /// A turn that answers with final text (the loop terminates on it).
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            tool_calls: Vec::new(),
            stream_failed: false,
        }
    }

    /// A turn that issues tool calls (the loop executes them and starts the
    /// next turn).
    pub(crate) fn tool_calls(calls: Vec<ScriptedToolCall>) -> Self {
        Self {
            text: None,
            tool_calls: calls,
            stream_failed: false,
        }
    }

    /// A turn that issues tool calls and then fails the provider stream
    /// mid-turn, before the loop's next `on_completion_call` can fire — the
    /// deterministic stand-in for a provider stream error after tools have
    /// run (the pinned loop's mid-turn error break).
    pub(crate) fn tool_calls_then_stream_failure(calls: Vec<ScriptedToolCall>) -> Self {
        Self {
            text: None,
            tool_calls: calls,
            stream_failed: true,
        }
    }

    /// Attach text alongside this turn's tool calls.
    #[allow(dead_code)] // reserved: text+tool-call turn scripts, not yet consumed
    pub(crate) fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

/// One scripted tool call, delivered as a complete `RawStreamingToolCall`.
#[derive(Clone, Debug)]
pub(crate) struct ScriptedToolCall {
    id: String,
    call_id: Option<String>,
    name: String,
    arguments: serde_json::Value,
}

impl ScriptedToolCall {
    /// A tool call with the rig-assigned `id` the loop keys tool results by.
    pub(crate) fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            call_id: None,
            name: name.into(),
            arguments,
        }
    }

    /// Set the provider-side `call_id` (what the park gate records on a
    /// pending call and what the continuation's sentinel replacement keys on).
    pub(crate) fn with_call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }
}

/// The streaming response payload the scripted model reports per turn.
/// Fixed figures keep usage assertions deterministic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScriptedFinalResponse;

impl GetTokenUsage for ScriptedFinalResponse {
    fn token_usage(&self) -> Option<Usage> {
        Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
        })
    }
}

/// A completion model playing a fixed script of turns. Each `stream()` (or
/// `completion()`) call serves the next [`ScriptedTurn`]; every served
/// request is recorded for turn-boundary assertions. An exhausted script is a
/// provider error — the deterministic stand-in for a provider stream failure.
#[derive(Clone, Default)]
pub(crate) struct ScriptedCompletionModel {
    script: Arc<Mutex<VecDeque<ScriptedTurn>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ScriptedCompletionModel {
    pub(crate) fn new(turns: Vec<ScriptedTurn>) -> Self {
        Self {
            script: Arc::new(Mutex::new(VecDeque::from(turns))),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every request the loop built, in turn order. `len()` is the number of
    /// turns the hook's `on_completion_call` fronted.
    pub(crate) fn requests(&self) -> Arc<Mutex<Vec<CompletionRequest>>> {
        Arc::clone(&self.requests)
    }

    fn record_and_take(&self, request: CompletionRequest) -> Result<ScriptedTurn, CompletionError> {
        self.requests
            .lock()
            .expect("scripted-model request log")
            .push(request);
        self.script
            .lock()
            .expect("scripted-model script")
            .pop_front()
            .ok_or_else(|| {
                CompletionError::ProviderError(
                    "scripted model: script exhausted before the loop finished".to_string(),
                )
            })
    }

    fn turn_stream(turn: &ScriptedTurn) -> StreamingCompletionResponse<ScriptedFinalResponse> {
        let mut items =
            Vec::<Result<RawStreamingChoice<ScriptedFinalResponse>, CompletionError>>::new();
        if let Some(text) = &turn.text {
            items.push(Ok(RawStreamingChoice::Message(text.clone())));
        }
        for call in &turn.tool_calls {
            items.push(Ok(RawStreamingChoice::ToolCall(RawStreamingToolCall {
                id: call.id.clone(),
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                signature: None,
                additional_params: None,
            })));
        }
        if turn.stream_failed {
            items.push(Err(CompletionError::ProviderError(
                "scripted model: provider stream failed mid-turn".to_string(),
            )));
        } else {
            items.push(Ok(RawStreamingChoice::FinalResponse(ScriptedFinalResponse)));
        }
        StreamingCompletionResponse::stream(Box::pin(futures::stream::iter(items)))
    }

    fn turn_choice(turn: &ScriptedTurn) -> rig::OneOrMany<AssistantContent> {
        let mut choice = Vec::<AssistantContent>::new();
        if let Some(text) = &turn.text {
            choice.push(AssistantContent::text(text));
        }
        for call in &turn.tool_calls {
            choice.push(AssistantContent::ToolCall(ToolCall {
                id: call.id.clone(),
                call_id: call.call_id.clone(),
                function: ToolFunction {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
                signature: None,
                additional_params: None,
            }));
        }
        if choice.is_empty() {
            choice.push(AssistantContent::text(""));
        }
        rig::OneOrMany::many(choice).expect("scripted turn always yields one content item")
    }
}

impl CompletionModel for ScriptedCompletionModel {
    type Response = ();
    type StreamingResponse = ScriptedFinalResponse;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self::default()
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        let turn = self.record_and_take(request)?;
        Ok(CompletionResponse {
            choice: Self::turn_choice(&turn),
            usage: Usage::new(),
            raw_response: (),
        })
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        let turn = self.record_and_take(request)?;
        Ok(Self::turn_stream(&turn))
    }
}

// ============================================================================
// Worker-model injection seam (commit 3)
// ============================================================================

/// A scripted rig agent — the concrete type the `#[cfg(test)]` variant of
/// `ProviderAgent` wraps.
pub(crate) type ScriptedAgent = rig::agent::Agent<ScriptedCompletionModel>;

/// One queued worker-model override: the scripted model the next
/// `build_worker_provider_agent` call builds the worker from, plus tools
/// registered alongside it. The orchestrator's cfg(test) prelude wraps each
/// extra tool in the worker's own wrapper chain (gate included) before
/// registering it, so a scripted worker's tool calls are gated exactly like
/// live ones.
pub(crate) struct WorkerOverride {
    pub(crate) model: ScriptedCompletionModel,
    pub(crate) extra_tools: Vec<Box<dyn rig::tool::ToolDyn>>,
}

/// Take-once override queue: a test installs one override per worker build
/// it will drive, in order.
static WORKER_OVERRIDES: OnceLock<Mutex<VecDeque<WorkerOverride>>> = OnceLock::new();

/// Queue worker-model overrides. FIFO: the *n*-th worker build after this
/// call consumes the *n*-th override. Tests that use the seam must serialize
/// against each other — the queue is process-global.
pub(crate) fn install_worker_overrides(overrides: Vec<WorkerOverride>) {
    let queue = WORKER_OVERRIDES.get_or_init(|| Mutex::new(VecDeque::new()));
    queue
        .lock()
        .expect("worker-override lock")
        .extend(overrides);
}

/// Pop the next override, if one is queued. Consumed by the orchestrator's
/// cfg(test) prelude in `build_worker_provider_agent`.
pub(crate) fn take_worker_override() -> Option<WorkerOverride> {
    let queue = WORKER_OVERRIDES.get_or_init(|| Mutex::new(VecDeque::new()));
    queue.lock().expect("worker-override lock").pop_front()
}

/// Adapter presenting a boxed dynamic tool as a concrete [`rig::tool::Tool`]
/// with the `Value`/`String`/`ToolError` shape the worker wrapper chain
/// wraps, so an override's tools re-enter the same `WrappedTool` path the
/// MCP tools take. `Clone` over an `Arc` because `WrappedTool` requires it.
#[derive(Clone)]
pub(crate) struct DynToolAsTool(Arc<dyn rig::tool::ToolDyn>);

impl DynToolAsTool {
    pub(crate) fn new(tool: Box<dyn rig::tool::ToolDyn>) -> Self {
        Self(Arc::from(tool))
    }
}

impl rig::tool::Tool for DynToolAsTool {
    const NAME: &'static str = "dyn_tool_as_tool";

    type Error = rig::tool::ToolError;
    type Args = serde_json::Value;
    type Output = String;

    fn name(&self) -> String {
        rig::tool::ToolDyn::name(&*self.0)
    }

    async fn definition(&self, prompt: String) -> rig::completion::ToolDefinition {
        // The boxed dyn future is only `Send`; `Shared` makes awaiting it
        // satisfy the `Tool` trait's `Sync` future bound (the shared state is
        // behind an `Arc` + mutex, so the wrapper future is `Sync`).
        use futures::FutureExt as _;
        self.0.definition(prompt).shared().await
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // The inner ToolDyn result is already JSON-encoded (the blanket impl
        // serializes the tool's output on the way out). Decode string outputs
        // so the wrapper boundary's own serialization is the only one applied
        // — byte-identical to a concrete `Tool` registration. Non-string
        // outputs keep their encoded text.
        let encoded = self.0.call(args.to_string()).await?;
        match serde_json::from_str::<serde_json::Value>(&encoded) {
            Ok(serde_json::Value::String(text)) => Ok(text),
            _ => Ok(encoded),
        }
    }
}

// ============================================================================
// Stub tool
// ============================================================================

/// One recorded invocation of the stub tool. The rig's `call_id` correlation
/// lives in [`StreamRun::tool_results`] — rig's `Tool::call` receives only the
/// arguments, so the tool itself cannot observe the call id.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolInvocation {
    pub(crate) arguments: serde_json::Value,
    pub(crate) result: String,
}

/// Permissive argument shape: any JSON object round-trips, so scripts can
/// carry arbitrary tool payloads (e.g. a `kubectl_apply`-shaped call) without
/// a schema per test.
#[derive(Debug, Deserialize)]
pub(crate) struct FreeformArgs {
    #[serde(flatten)]
    pub(crate) fields: serde_json::Map<String, serde_json::Value>,
}

/// Holds one stub-tool invocation open until [`StallHook::release`], for the
/// commit-3 race tests that need a mid-invocation observation point. Inert
/// unless the tool was built `with_stall`.
#[derive(Clone, Default)]
pub(crate) struct StallHook {
    inner: Arc<StallInner>,
}

#[derive(Default)]
struct StallInner {
    /// Fired when an invocation has been recorded and is about to hold.
    entered: Notify,
    /// Flipped before waking every waiter; the flag makes the release
    /// idempotent and races on it impossible.
    released: AtomicBool,
    release: Notify,
}

impl StallHook {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Resolves once an invocation is recorded and holding. Permits fired
    /// before the waiter registers are honored (`Notify` stores one).
    pub(crate) async fn wait_entered(&self) {
        self.inner.entered.notified().await;
    }

    /// Let the held invocation return.
    pub(crate) fn release(&self) {
        self.inner.released.store(true, Ordering::Release);
        self.inner.release.notify_waiters();
    }
}

/// The stub tool: records every invocation (arguments, returned result) into
/// a shared log the test keeps a handle to, and returns a fixed result
/// string. Implements the same `rig::tool::Tool` shape the orchestration
/// tools (`submit_result`, `read_artifact`, …) register through.
pub(crate) struct RecordingTool {
    result: String,
    invocations: Arc<Mutex<Vec<ToolInvocation>>>,
    stall: Option<StallHook>,
    /// The name the tool registers under. Defaults to [`ECHO_TOOL_NAME`];
    /// renamed instances give scripts an ungated sibling tool.
    registered_name: String,
}

impl RecordingTool {
    pub(crate) fn new(invocations: Arc<Mutex<Vec<ToolInvocation>>>) -> Self {
        Self {
            result: ECHO_TOOL_RESULT.to_string(),
            invocations,
            stall: None,
            registered_name: ECHO_TOOL_NAME.to_string(),
        }
    }

    /// Register under a different name: the default name is what the park
    /// tests' glob matches, so a depth-exhaustion script needs an ungated
    /// sibling tool to burn turns with.
    pub(crate) fn with_name(mut self, name: &str) -> Self {
        self.registered_name = name.to_string();
        self
    }

    /// Arm the stall hook: invocations record, then hold until
    /// [`StallHook::release`]. Opt-in — the default tool never holds.
    pub(crate) fn with_stall(mut self, stall: StallHook) -> Self {
        self.stall = Some(stall);
        self
    }
}

impl rig::tool::Tool for RecordingTool {
    const NAME: &'static str = ECHO_TOOL_NAME;

    type Error = std::convert::Infallible;
    type Args = FreeformArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: self.name(),
            description: "Test stand-in: records the call and echoes a fixed result.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let arguments = serde_json::Value::Object(args.fields);
        self.invocations
            .lock()
            .expect("tool invocation log")
            .push(ToolInvocation {
                arguments: arguments.clone(),
                result: self.result.clone(),
            });

        if let Some(stall) = &self.stall {
            stall.inner.entered.notify_one();
            if !stall.inner.released.load(Ordering::Acquire) {
                let notified = stall.inner.release.notified();
                // Re-check after registering the waiter so a release racing
                // this await cannot be missed.
                if !stall.inner.released.load(Ordering::Acquire) {
                    notified.await;
                }
            }
        }

        Ok(self.result.clone())
    }
}

// ============================================================================
// Worker harness
// ============================================================================

/// A worker rig: the scripted-model agent with the stub tool registered on
/// the rig tool server, plus the shared handles tests assert through.
pub(crate) struct WorkerRig {
    pub(crate) agent: rig::agent::Agent<ScriptedCompletionModel>,
    pub(crate) model: ScriptedCompletionModel,
    pub(crate) invocations: Arc<Mutex<Vec<ToolInvocation>>>,
    /// Handle to the tool's stall hook; only live when built via
    /// [`worker_rig_with_stall`]. Reserved for race tests driving the
    /// worker-stream level directly.
    #[allow(dead_code)] // reserved: stall-driven race tests
    pub(crate) stall: StallHook,
}

/// Assemble a worker rig from a script of turns. The stub tool never stalls.
pub(crate) fn worker_rig(turns: Vec<ScriptedTurn>) -> WorkerRig {
    worker_rig_inner(turns, false)
}

/// A worker rig whose stub tool holds every invocation open until
/// [`StallHook::release`]. The rig's `stall` handle shares the hook the
/// tool holds; tests synchronize on it instead of sleeping.
#[allow(dead_code)] // reserved: stall-driven race tests
pub(crate) fn worker_rig_with_stall(turns: Vec<ScriptedTurn>) -> WorkerRig {
    worker_rig_inner(turns, true)
}

fn worker_rig_inner(turns: Vec<ScriptedTurn>, stalled: bool) -> WorkerRig {
    let model = ScriptedCompletionModel::new(turns);
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let stall = StallHook::new();
    let mut tool = RecordingTool::new(invocations.clone());
    if stalled {
        tool = tool.with_stall(stall.clone());
    }
    let agent = rig::agent::AgentBuilder::new(model.clone())
        .name("rig-worker")
        .preamble("test worker preamble")
        .tool(tool)
        .build();
    WorkerRig {
        agent,
        model,
        invocations,
        stall,
    }
}

/// One tool execution as the loop reported it, with the call-id correlation
/// the tool itself cannot see.
#[derive(Clone, Debug)]
pub(crate) struct ToolResultRecord {
    pub(crate) id: String,
    pub(crate) call_id: Option<String>,
    pub(crate) result: String,
}

/// Outcome of one [`drive_worker`] run: the worker-stream equivalent of a
/// task execution. Reaching [`MultiTurnStreamItem::FinalResponse`] is the
/// loop's completion signal; its text is what survives into the task result
/// upstream.
pub(crate) struct StreamRun {
    pub(crate) final_text: Option<String>,
    /// The loop's aggregated usage (`FinalResponse.usage`).
    pub(crate) usage: Usage,
    pub(crate) tool_results: Vec<ToolResultRecord>,
    /// External cancellation handle (the hook's watch channel), for the
    /// commit-3 race tests.
    #[allow(dead_code)] // commit 3: race tests cancel mid-stream
    pub(crate) cancel_tx: watch::Sender<bool>,
    /// The park-aware hook's usage state, asserting hook compatibility.
    pub(crate) usage_state: UsageState,
}

impl std::fmt::Debug for StreamRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRun")
            .field("final_text", &self.final_text)
            .field("usage", &self.usage)
            .field("tool_results", &self.tool_results)
            .finish_non_exhaustive()
    }
}

/// Drive one worker stream the way `stream_and_forward` drives a park-mode
/// worker: `stream_chat` with the park-aware `StreamingRequestHook` attached,
/// full multi-turn depth. The hook is inert here — no blocked cell is
/// registered under the stream's request id.
pub(crate) async fn drive_worker(
    rig: &WorkerRig,
    prompt: &str,
    history: Vec<rig::completion::Message>,
    max_depth: usize,
) -> Result<StreamRun, Box<dyn std::error::Error + Send + Sync>> {
    let request_id = format!("rig_{}", uuid::Uuid::new_v4().simple());
    let (hook, cancel_tx, usage_state) =
        StreamingRequestHook::with_scratchpad_budget(Duration::from_secs(60), request_id, None);

    let mut stream = rig
        .agent
        .stream_chat(prompt, history)
        .with_hook(hook)
        .multi_turn(max_depth)
        .await;

    let mut run = StreamRun {
        final_text: None,
        usage: Usage::new(),
        tool_results: Vec::new(),
        cancel_tx,
        usage_state,
    };

    while let Some(item) = stream.next().await {
        match item.map_err(Box::<dyn std::error::Error + Send + Sync>::from)? {
            MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult(tr)) => {
                run.tool_results.push(ToolResultRecord {
                    id: tr.id,
                    call_id: tr.call_id,
                    result: tool_result_text(&tr.content),
                });
            }
            MultiTurnStreamItem::FinalResponse(final_response) => {
                run.final_text = Some(final_response.response().to_string());
                run.usage = final_response.usage();
            }
            _ => {}
        }
    }

    Ok(run)
}

fn tool_result_text(content: &rig::OneOrMany<rig::message::ToolResultContent>) -> String {
    content
        .iter()
        .map(|c| match c {
            rig::message::ToolResultContent::Text(text) => text.text.clone(),
            rig::message::ToolResultContent::Image(_) => "[Image content]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================================================================
// Orchestrator fixture (commit 3 scaffolding)
// ============================================================================

/// A worker definition wired for the rig: no MCP tools (`mcp_filter = []`),
/// default depth, no overrides. Returns `(name, config)` for insertion into
/// [`OrchestrationConfig::workers`]. The execute_task-level tests build their
/// worker configs inline (they need a settable turn depth), so this stays
/// reserved.
#[allow(dead_code)] // reserved: rig-level worker definition fixture
pub(crate) fn worker_definition(
    name: &str,
    description: &str,
    preamble: &str,
) -> (String, super::WorkerConfig) {
    (
        name.to_string(),
        super::WorkerConfig {
            description: description.to_string(),
            preamble: preamble.to_string(),
            mcp_filter: Some(vec![]),
            vector_stores: vec![],
            turn_depth: None,
            llm: None,
            scratchpad: None,
            skills: None,
        },
    )
}

/// A park-mode orchestrator over an in-memory approval store, persisted under
/// `memory_dir` — the construction pattern the orchestrator's own park tests
/// use, minus the run id (that lives behind the orchestrator's private
/// persistence handle, readable only from the orchestrator's own test
/// module). The execute_task-level fixtures live in that test module for the
/// same reason, so this stays reserved.
#[allow(dead_code)] // reserved: rig-level orchestrator fixture
pub(crate) async fn park_orchestrator_in(
    memory_dir: &Path,
) -> (
    super::Orchestrator,
    Arc<crate::session_store::InMemoryApprovalStore>,
    crate::hitl::PendingApprovals,
) {
    use crate::hitl::PendingApprovals;
    use crate::session_store::InMemoryEventBus;

    let store = Arc::new(crate::session_store::InMemoryApprovalStore::new());
    let registry = PendingApprovals::with_backend(store.clone(), Arc::new(InMemoryEventBus::new()));
    let (worker_name, worker) = worker_definition(
        "operations",
        "Runs the scripted tool",
        "You apply changes with the echo tool.",
    );
    let mut workers = std::collections::HashMap::new();
    workers.insert(worker_name, worker);
    let config = crate::config::AgentRuntimeConfig {
        hitl: Some(crate::hitl::HitlRuntime {
            patterns: Arc::from([aura_config::GlobPattern::new("kubectl_*").unwrap()]),
            route: Arc::new(crate::hitl::DecisionRoute::Conversational {
                registry: registry.clone(),
                timeout: Duration::from_secs(3600),
            }),
            park_enabled: true,
        }),
        memory_dir: Some(memory_dir.to_string_lossy().into_owned()),
        session_id: Some("park-sess".to_string()),
        request_id: Some(format!("req_rig_{}", uuid::Uuid::new_v4().simple())),
        orchestration: Some(super::OrchestrationConfig {
            enabled: true,
            workers,
            ..Default::default()
        }),
        ..crate::config::AgentRuntimeConfig::default()
    };
    let orchestrator = super::Orchestrator::new(config)
        .await
        .expect("orchestrator builds");
    (orchestrator, store, registry)
}

// ============================================================================
// Tests
// ============================================================================

/// The consuming smoke test (P44 pull addendum 2026-09-03): the rig is never
/// dead infrastructure, and Gate A gets its fidelity baseline. One worker,
/// one ungated tool, zero park machinery: turn 1 emits the tool call, turn 2
/// (seeing the tool result) emits the final text. Asserts the tool ran
/// exactly once with the scripted arguments, the loop completed, and the
/// final result text survived.
#[tokio::test]
async fn smoke_scripted_worker_ungated_tool_round_trip() {
    let rig = worker_rig(vec![
        ScriptedTurn::tool_calls(vec![
            ScriptedToolCall::new(
                "call_0",
                ECHO_TOOL_NAME,
                serde_json::json!({"namespace": "prod"}),
            )
            .with_call_id("call_id_0"),
        ]),
        ScriptedTurn::text("applied the manifest to prod"),
    ]);

    let run = drive_worker(&rig, "apply the manifest", Vec::new(), 4)
        .await
        .expect("the scripted worker stream completes");

    // The tool ran exactly once, with the scripted arguments, and returned
    // the scripted result.
    let invocations = rig.invocations.lock().expect("tool invocation log");
    assert_eq!(invocations.len(), 1, "exactly one tool invocation");
    assert_eq!(
        invocations[0].arguments,
        serde_json::json!({"namespace": "prod"})
    );
    assert_eq!(invocations[0].result, ECHO_TOOL_RESULT);
    drop(invocations);

    // Turn boundaries are deterministic: the hook fronted exactly two model
    // turns, and the second turn's request carries the tool result — the
    // model saw what the tool returned before answering.
    let request_log = rig.model.requests();
    let requests = request_log.lock().expect("request log");
    assert_eq!(requests.len(), 2, "one request per scripted turn");
    let second_turn_saw_result = requests[1].chat_history.iter().any(|m| {
        serde_json::to_string(m)
            .expect("message serializes")
            .contains(ECHO_TOOL_RESULT)
    });
    assert!(
        second_turn_saw_result,
        "turn 2 must see the tool result text in its prompt/history"
    );
    drop(requests);

    // Call-id correlation survives the loop.
    assert_eq!(run.tool_results.len(), 1, "one tool result round trip");
    assert_eq!(run.tool_results[0].id, "call_0");
    assert_eq!(run.tool_results[0].call_id.as_deref(), Some("call_id_0"));
    assert_eq!(run.tool_results[0].result, echo_tool_result_wire());

    // The task completed: the loop reached FinalResponse, and the final
    // result text is the turn-2 answer.
    assert_eq!(
        run.final_text.as_deref(),
        Some("applied the manifest to prod")
    );

    // Loop aggregation sums both turns' fixed usage.
    assert_eq!(run.usage.input_tokens, 20);
    assert_eq!(run.usage.output_tokens, 10);

    // The park-aware hook fired (compatibility) and captured the final text
    // turn's usage — inert otherwise: no blocked cell was registered, so the
    // stream ran to completion instead of parking.
    assert_eq!(run.usage_state.get_final_usage(), (10, 5, 15));
}

/// The provider-error trigger commit 3's dual-trigger orphan coverage needs:
/// an exhausted script surfaces as a provider stream error through the hook
/// after the tools it served have already run.
#[tokio::test]
async fn script_exhaustion_surfaces_as_a_provider_stream_error() {
    let rig = worker_rig(vec![
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_0",
            ECHO_TOOL_NAME,
            serde_json::json!({"attempt": 1}),
        )]),
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_1",
            ECHO_TOOL_NAME,
            serde_json::json!({"attempt": 2}),
        )]),
    ]);

    let error = drive_worker(&rig, "keep applying", Vec::new(), 4)
        .await
        .expect_err("an exhausted script fails the stream");

    assert!(
        error.to_string().contains("script exhausted"),
        "the provider error must be the script-exhaustion one, got: {error}"
    );
    let invocations = rig.invocations.lock().expect("tool invocation log");
    assert_eq!(invocations.len(), 2, "both scripted tool calls executed");
}

/// The stall hook: a held invocation is observable (recorded, stream
/// suspended) and releasable, with no sleeps — the race tests synchronize on
/// the hook's notifications alone.
#[tokio::test]
async fn stall_hook_holds_and_releases_a_tool_invocation() {
    let rig = worker_rig_with_stall(vec![
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_0",
            ECHO_TOOL_NAME,
            serde_json::json!({"namespace": "staging"}),
        )]),
        ScriptedTurn::text("released and finished"),
    ]);

    let invocations = rig.invocations.clone();
    let stall = rig.stall.clone();
    let drive =
        tokio::spawn(async move { drive_worker(&rig, "hold the apply", Vec::new(), 4).await });

    // The invocation records and holds before the next turn starts.
    tokio::time::timeout(Duration::from_secs(5), stall.wait_entered())
        .await
        .expect("the tool invocation registers itself while held");
    assert_eq!(
        invocations.lock().expect("tool invocation log").len(),
        1,
        "the invocation is recorded while the call is held open"
    );

    stall.release();
    let run = tokio::time::timeout(Duration::from_secs(5), drive)
        .await
        .expect("the stream finishes after release")
        .expect("drive task joins")
        .expect("the stream completes after release");

    assert_eq!(run.final_text.as_deref(), Some("released and finished"));
    assert_eq!(run.tool_results.len(), 1);
    assert_eq!(run.tool_results[0].result, echo_tool_result_wire());
}
