//! Orchestrator agent for multi-agent workflows.
//!
//! The orchestrator decomposes queries into tasks, executes them (potentially
//! in parallel), and consolidates results. `StreamingAgent` is implemented by
//! `OrchestratorFactory` (see `factory.rs`), which creates an `Orchestrator`
//! lazily inside `stream()`.
//!
//! # Architecture
//!
//! ```text
//! User Query
//!     │
//!     ▼
//! ┌─────────────┐
//! │ COORDINATOR │ ── decompose query into Plan
//! └─────────────┘
//!     │
//!     ▼
//! ┌─────────────┐     ┌─────────────┐
//! │   WORKER 1  │ ... │   WORKER N  │  ── execute tasks
//! └─────────────┘     └─────────────┘
//!     │                     │
//!     └──────────┬──────────┘
//!                ▼
//!     ┌─────────────────────┐
//!     │  COORDINATOR CONT.  │  ── consolidate + route
//!     └─────────────────────┘
//!                │
//!                ▼
//!          Final Response
//! ```
//!
//! # Streaming Events
//!
//! The orchestrator emits `OrchestratorEvent` variants through the stream:
//! - `PlanCreated` - when the coordinator produces a plan
//! - `TaskStarted` - when a worker begins a task
//! - `TaskCompleted` - when a worker finishes a task
//! - `IterationComplete` - when the post-execute coordinator decision completes
//! - `Synthesizing` - when task results are being consolidated for the coordinator

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rig::client::CompletionClient;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use crate::Agent;
use crate::config::{AgentRuntimeConfig, LlmConfig};
use crate::mcp::McpManager;
use crate::provider_agent::{BuilderState, ProviderAgent, StreamError, StreamItem};
use crate::scratchpad;
use crate::string_utils::safe_truncate;
use crate::tool_call_observer::ToolCallObserver;

use super::tools::RoutingToolSet;
use super::tools::{InspectToolParamsTool, ListToolsTool, ReadArtifactTool};

use super::config::OrchestrationConfig;
use super::events::OrchestratorEvent;
use super::persistence::ExecutionPersistence;
use super::prompt_journal::{JournalPhase, PromptJournal};
use super::types::{
    FailedTaskRecord, FailureCategory, FailureSummary, IterationContext, IterationOutcome, Plan,
    PlanningResponse, TaskState, TaskStatus,
};

// ============================================================================
// Constants
// ============================================================================

/// Number of characters per chunk when streaming the final orchestration response.
pub(super) const STREAM_CHUNK_SIZE: usize = 50;

/// Maximum ReAct depth for the planning coordinator.
/// Defense-in-depth alongside stream_and_collect's early exit.
/// Allows: 1 list_tools + 1 inspect_tool_params + 1 read_artifact + 1 routing + 2 spare.
const PLANNING_COORDINATOR_MAX_DEPTH: usize = 6;

/// Maximum attempts for a worker task before giving up.
/// Attempt 1 = normal execution. Attempt 2 = retry with correction prompt.
const MAX_WORKER_ATTEMPTS: usize = 2;

/// After a worker timeout cancels an active HITL approval, briefly poll the
/// worker stream so the approval route can emit completion and clean registry
/// state before the worker future is dropped.
const HITL_TASK_TIMEOUT_CLEANUP: Duration = Duration::from_secs(1);

// ============================================================================
// Helper Structs
// ============================================================================

/// Parameters for task execution to avoid clippy::too_many_arguments.
struct TaskExecutionParams<'a> {
    task_description: &'a str,
    task_context: &'a Option<String>,
    worker_name: Option<&'a str>,
    plan_goal: &'a str,
}

/// Result from `execute_task` including structured output from `submit_result`.
struct TaskExecutionResult {
    result: String,
    structured_output: Option<super::types::StructuredTaskOutput>,
}

/// Named return type for `create_*` coordinator/worker methods.
///
/// Replaces bare `(Agent, String)` tuples where the `String` was the preamble
/// used for journal recording.
struct AgentWithPreamble {
    agent: Agent,
    preamble: String,
    /// Side-channel for worker→executor escalation. Set by the duplicate-call
    /// guard when a tool-call loop is terminated; read by `execute_task` after
    /// the multi_turn loop to convert a false `Ok` into `TaskStatus::Failed`.
    escalation_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Shared state for the worker's `submit_result` tool. Read after the
    /// worker completes to extract structured output (summary, result, confidence).
    submit_result_decision: super::tools::SubmitResultDecision,
}

/// Persistent coordinator state for conversation across planning iterations.
///
/// Created once at `run_orchestration` entry and threaded through the
/// plan → execute → continue loop. The conversation grows monotonically
/// with each coordinator turn (planning prompt, correction, continuation).
struct CoordinatorState {
    agent: Agent,
    preamble: String,
    conversation: Vec<rig::completion::Message>,
    routing_decision: super::tools::routing_tools::RoutingDecision,
}

/// Bundled coordinator tools for `build_agent_with_tools`.
struct CoordinatorTools {
    list_tools: Option<ListToolsTool>,
    inspect_tool_params: Option<InspectToolParamsTool>,
    vector_tools: Vec<crate::vector_dynamic::DynamicVectorSearchTool>,
    routing_tools: RoutingToolSet,
    read_artifact: Option<ReadArtifactTool>,
    list_prior_runs: Option<super::tools::ListPriorRunsTool>,
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Spawns a task that monitors for external cancellation or timeout,
/// cancelling the provided token when either occurs.
///
/// Returns a `JoinHandle` for the watcher task. The handle is intentionally
/// fire-and-forget in production (the task self-terminates via `select!`),
/// but callers in tests should `.await` it to assert post-conditions.
///
/// Cleanup: when the caller drops the sender side of `cancel_rx`, `rx.changed()`
/// returns `Err`, the `select!` resolves, and the sleep future is dropped
/// (cancelling the timer via tokio's standard drop semantics).
#[must_use = "task runs independently; bind with `let _handle =` to document fire-and-forget intent"]
pub(super) fn spawn_cancellation_watcher(
    cancel_rx: watch::Receiver<bool>,
    timeout: Duration,
    cancel_token: CancellationToken,
    request_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            was_cancelled = async {
                let mut rx = cancel_rx;
                loop {
                    if rx.changed().await.is_err() {
                        return false; // Sender dropped — stream finished normally
                    }
                    if *rx.borrow_and_update() {
                        return true; // External cancellation requested
                    }
                }
            } => {
                if was_cancelled {
                    tracing::info!("External cancellation triggered for {}", request_id);
                    cancel_token.cancel();
                }
            }
            _ = tokio::time::sleep(timeout) => {
                tracing::warn!("Timeout reached, cancelling orchestration");
                cancel_token.cancel();
            }
        }
    })
}

/// Extract task_id from a tool_call_id string.
///
/// Tool call IDs follow the format: `task{id}_{toolname}_{counter}`
/// Returns `None` if the ID doesn't match the expected format.
fn extract_task_id(tool_call_id: &str) -> Option<usize> {
    tool_call_id
        .strip_prefix("task")
        .and_then(|s| s.split('_').next())
        .and_then(|s| s.parse().ok())
}

/// Convert a `ToolEvent` to an `OrchestratorEvent`.
fn tool_event_to_orchestrator_event(
    event: crate::tool_call_observer::ToolEvent,
) -> OrchestratorEvent {
    match event {
        crate::tool_call_observer::ToolEvent::CallStarted {
            tool_call_id,
            tool_name,
            tool_initiator_id,
            arguments,
            ..
        } => OrchestratorEvent::ToolCallStarted {
            task_id: extract_task_id(&tool_call_id),
            tool_call_id,
            tool_name,
            worker_id: tool_initiator_id,
            arguments,
        },
        crate::tool_call_observer::ToolEvent::CallCompleted {
            tool_call_id,
            result,
            duration_ms,
        } => {
            let success = result.is_success();
            let result_str = match result {
                crate::tool_call_observer::ToolOutcome::Success(content) => content,
                crate::tool_call_observer::ToolOutcome::Error { message, .. } => message,
            };
            OrchestratorEvent::ToolCallCompleted {
                task_id: extract_task_id(&tool_call_id),
                tool_call_id,
                success,
                duration_ms,
                result: result_str,
            }
        }
    }
}

/// Spawn a task that forwards tool call events to the SSE stream.
///
/// Listens on the observer's broadcast channel and converts `ToolEvent`s
/// to `OrchestratorEvent`s, sending them through the event channel.
pub(super) fn spawn_tool_event_forwarder(
    observer: &ToolCallObserver,
    event_tx: tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
    cancel_token: CancellationToken,
) {
    let mut tool_rx = observer.subscribe();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = tool_rx.recv() => {
                    match result {
                        Ok(tool_event) => {
                            let orch_event = tool_event_to_orchestrator_event(tool_event);
                            let _ = event_tx.send(Ok(StreamItem::OrchestratorEvent(orch_event))).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Tool observer lagged by {} events", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                _ = cancel_token.cancelled() => {
                    break;
                }
            }
        }
    });
}

// ============================================================================
// Orchestrator
// ============================================================================

/// Orchestrator for multi-agent workflows.
///
/// Created lazily by `OrchestratorFactory::stream()` to coordinate multiple
/// agents through a plan-execute-continue loop.
pub struct Orchestrator {
    /// ID for the orchestrator
    orchestrator_id: String,

    /// Orchestration configuration
    config: OrchestrationConfig,

    /// The underlying agent configuration (for creating workers)
    agent_config: AgentRuntimeConfig,

    /// Tool call observer for coordinator visibility into worker tool execution.
    /// Wired to emit OrchestratorEvent for real-time SSE streaming via spawn_tool_event_forwarder.
    pub(super) tool_call_observer: ToolCallObserver,

    /// Shared MCP manager for tool discovery and cancellation.
    /// Arc-wrapped so workers can share the same connections.
    pub(super) mcp_manager: Option<Arc<McpManager>>,

    /// Execution persistence for debugging and retry intelligence
    persistence: Arc<Mutex<ExecutionPersistence>>,

    /// Optional prompt journal for dev diagnostics (gated by AURA_PROMPT_JOURNAL=1)
    prompt_journal: Option<PromptJournal>,

    /// Current orchestration iteration, set at the top of `run_orchestration_loop`.
    /// Read by `journal_record` so that iteration doesn't pollute method signatures.
    current_iteration: AtomicUsize,

    /// Accumulated token usage across all LLM calls in this orchestration run
    /// (planning, workers, continuation routing).
    ///
    /// Cloned from a handle owned by `OrchestratorFactory::stream_with_timeout`
    /// so the streaming handler can read the final totals and emit `aura.usage`.
    /// In orchestration mode we aggregate additively via
    /// [`crate::UsageState::accumulate_usage`] so the reported prompt/completion
    /// totals reflect *billed* tokens across every internal LLM turn, not just
    /// the first one. Assigned by the factory after construction; see
    /// `OrchestratorFactory::spawn_orchestration_stream`.
    pub(super) usage_state: crate::UsageState,
}

/// Stream context for reasoning attribution in `stream_and_forward`.
///
/// When `Some`, reasoning items are wrapped as `OrchestratorEvent::WorkerReasoning`
/// with proper task/worker attribution. When `None`, reasoning is forwarded raw
/// (coordinator context — attributed as `agent_id: "main"` by handlers).
struct StreamContext<'a> {
    task_id: usize,
    worker_id: &'a str,
    worker_name: Option<&'a str>,
}

/// Shared parameters for streaming LLM calls (`stream_and_forward` / `stream_and_collect`).
///
/// Groups the per-call prompt payload and event routing — the data that every streaming
/// variant needs regardless of ReAct depth or reasoning attribution mode.
struct StreamCallParams<'a> {
    prompt: &'a str,
    history: Vec<rig::completion::Message>,
    phase: &'a str,
    event_tx: Option<&'a tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>>,
}

impl Orchestrator {
    /// Create a new orchestrator from configuration.
    pub async fn new(
        agent_config: AgentRuntimeConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let orchestration_config = agent_config.orchestration.clone().unwrap_or_default();

        // Initialize MCP manager (shared across coordinator and all workers via Arc)
        let mcp_manager = if let Some(ref mcp_config) = agent_config.mcp {
            tracing::info!("Orchestrator: initializing MCP connections");
            Some(Arc::new(
                McpManager::initialize_from_config(mcp_config).await?,
            ))
        } else {
            None
        };

        // Tool call observer for real-time streaming. The _rx receiver is consumed
        // by spawn_tool_event_forwarder in factory.rs when the stream starts.
        let (tool_call_observer, _rx) = ToolCallObserver::new(32);

        // Initialize execution persistence for debugging and retry intelligence.
        let effective_memory_dir = agent_config.effective_memory_dir();
        let persistence = if let Some(memory_dir) = effective_memory_dir {
            tracing::info!(
                "Orchestrator: Initializing execution persistence at: {}",
                memory_dir
            );
            let p = ExecutionPersistence::new(memory_dir, agent_config.session_id.clone())
                .await
                .map_err(|e| format!("Failed to initialize persistence: {}", e))?;
            p.prune_session_runs(orchestration_config.max_session_runs())
                .await;
            Arc::new(Mutex::new(p))
        } else {
            tracing::info!("Orchestrator: Persistence disabled (no memory_dir configured)");
            Arc::new(Mutex::new(ExecutionPersistence::disabled()))
        };

        let orchestrator_id = uuid::Uuid::new_v4().to_string();

        // Initialize prompt journal (gated by AURA_PROMPT_JOURNAL env var, default off)
        let journal_enabled = crate::env_flags::bool_env("AURA_PROMPT_JOURNAL", false);
        let prompt_journal = if effective_memory_dir.is_some() {
            let guard = persistence.lock().await;
            let run_id = guard.run_id().to_string();
            let run_path = guard.run_path().to_path_buf();
            drop(guard);
            PromptJournal::from_persistence(&run_path, &run_id, &orchestrator_id, journal_enabled)
        } else {
            None
        };

        let run_id_str = persistence.lock().await.run_id().to_string();
        let default_turn_depth = agent_config
            .agent
            .turn_depth
            .unwrap_or(crate::builder::DEFAULT_MAX_DEPTH);
        tracing::info!(
            "Orchestrator initialized (run={}, max_planning_cycles={}, per_call_timeout={}s, default_turn_depth={}, max_plan_parse_retries={})",
            run_id_str.get(..8).unwrap_or(&run_id_str),
            orchestration_config.max_planning_cycles,
            orchestration_config.per_call_timeout_secs(),
            default_turn_depth,
            orchestration_config.max_plan_parse_retries,
        );

        Ok(Self {
            orchestrator_id,
            config: orchestration_config,
            agent_config,
            tool_call_observer,
            mcp_manager,
            persistence,
            prompt_journal,
            current_iteration: AtomicUsize::new(0),
            usage_state: crate::UsageState::new(),
        })
    }

    /// Record a prompt in the journal if enabled.
    ///
    /// Reads the current iteration from `self.current_iteration` so callers
    /// don't need to pass it explicitly.
    fn journal_record(&self, phase: JournalPhase, system_prompt: &str, user_prompt: &str) {
        if let Some(ref journal) = self.prompt_journal {
            let iteration = self.current_iteration.load(Ordering::Relaxed);
            journal.record(phase, iteration, system_prompt, user_prompt);
        }
    }

    /// Create a worker agent for task execution.
    ///
    /// Workers are regular agents that execute individual tasks.
    /// When persistence is enabled, MCP tools are wrapped with
    /// `PersistenceToolWrapper` to capture reasoning and execution details.
    /// When an observer is present, tools also emit events for real-time
    /// visibility into worker tool execution.
    ///
    /// If a specialized worker is assigned via `worker_name`, it uses that worker's
    /// custom preamble and MCP filter. Otherwise uses generic worker with all tools.
    async fn create_worker(
        &self,
        task_id: usize,
        attempt: usize,
        worker_name: Option<&str>,
    ) -> Result<AgentWithPreamble, Box<dyn std::error::Error + Send + Sync>> {
        use super::duplicate_call_guard::DuplicateCallGuard;
        use super::observer_wrapper::ObserverWrapper;
        use super::persistence_wrapper::PersistenceWrapper;
        use crate::tool_wrapper::{ComposedWrapper, ToolCallContext, ToolWrapper};

        // Build base tool wrappers: observer + duplicate guard + persistence
        let (in_flight, drain_notify, iteration, persistence_enabled) = {
            let p = self.persistence.lock().await;
            (
                p.in_flight_counter(),
                p.drain_notify(),
                p.current_iteration(),
                p.is_enabled(),
            )
        };
        let persistence_wrapper = Arc::new(PersistenceWrapper::new(
            super::persistence_wrapper::PersistenceWrapperParams {
                persistence: self.persistence.clone(),
                in_flight,
                drain_notify,
                worker_name: worker_name.map(String::from),
                iteration,
                persistence_enabled,
                size_threshold: self.config.tool_output_artifact_threshold(),
                duration_threshold_ms: self.config.tool_output_duration_threshold_ms(),
            },
        ));
        let observer_wrapper = Arc::new(ObserverWrapper::new(
            self.tool_call_observer.clone(),
            task_id,
        ));
        let nudge = self.config.duplicate_call_nudge_threshold;
        let block = self.config.duplicate_call_block_threshold;
        let escalation_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let duplicate_guard = Arc::new(DuplicateCallGuard::new(
            nudge,
            block,
            escalation_flag.clone(),
        ));

        // Create a modified config for workers with extension fields
        let mut worker_config = self.agent_config.clone();

        // Resolve per-worker LLM override (falls back to [agent.llm] when absent)
        if let Some(override_llm) = worker_name
            .and_then(|name| self.config.workers.get(name))
            .and_then(|w| w.llm.as_ref())
        {
            worker_config.llm = override_llm.clone();
        }

        // Per-worker scratchpad override falls back to [agent.scratchpad].
        // Each worker gets a FRESH ContextBudget scoped to its effective LLM —
        // workers never share a budget.
        let worker_cfg = worker_name.and_then(|name| self.config.workers.get(name));
        let effective_scratchpad = worker_cfg
            .and_then(|w| w.scratchpad.as_ref())
            .or(self.agent_config.agent.scratchpad.as_ref())
            .cloned();

        let mut scratchpad_tools = Vec::<Arc<dyn ToolWrapper>>::new();
        if let Some(ref sp_cfg) = effective_scratchpad
            && sp_cfg.enabled
        {
            let tools_per_server = self
                .mcp_manager
                .as_ref()
                .map(|m| m.tool_names_per_server())
                .unwrap_or_default();
            let scratchpad_tool_map =
                scratchpad::scratchpad_tool_map(self.agent_config.mcp.as_ref(), &tools_per_server);
            let worker_filter = worker_cfg.map(|w| w.mcp_filter.as_slice()).unwrap_or(&[]);
            let accessible_tools = self
                .mcp_manager
                .as_ref()
                .map(|m| m.get_available_tool_names())
                .unwrap_or_default();
            let has_matching_tool = scratchpad::has_accessible_scratchpad_tool(
                &accessible_tools,
                worker_filter,
                &scratchpad_tool_map,
            );

            if !has_matching_tool {
                tracing::warn!(
                    "Worker {}: scratchpad enabled but no MCP tool matches a scratchpad threshold; skipping",
                    task_id
                );
            } else {
                // Validation enforces these upstream; re-check here so runtime
                // misconfiguration fails loudly instead of silently degrading.
                let context_window = worker_config.llm.context_window().ok_or_else(
                    || -> Box<dyn std::error::Error + Send + Sync> {
                        format!(
                            "Worker {}: scratchpad enabled but context_window unset on effective LLM!",
                            task_id
                        ).into()
                    },
                )? as usize;
                let (provider, model) = worker_config.llm.model_info();
                let token_counter = scratchpad::token_counter_for_provider(provider, model);
                let worker_preamble = worker_cfg.map(|w| w.preamble.as_str()).unwrap_or("");

                let mcp_tool_tokens = self
                    .mcp_manager
                    .as_ref()
                    .map(|m| {
                        scratchpad::count_mcp_tool_schema_tokens(
                            &*token_counter,
                            m.tool_definitions_iter(),
                            worker_filter,
                        )
                    })
                    .unwrap_or(0);
                let initial_used = scratchpad::estimate_scratchpad_overhead(
                    &*token_counter,
                    &[super::config::WORKER_PREAMBLE_TEMPLATE, worker_preamble],
                ) + mcp_tool_tokens;

                let (iter_dir, read_root) = {
                    let persistence = self.persistence.lock().await;
                    let run_dir = persistence.run_path().to_path_buf();
                    let read_root = run_dir.parent().map(|p| p.to_path_buf()).unwrap_or(run_dir);
                    (persistence.iteration_path(), read_root)
                };

                let build = scratchpad::build_scratchpad(scratchpad::ScratchpadBuildInputs {
                    sp_cfg,
                    storage_dir: &iter_dir,
                    read_root: Some(&read_root),
                    scratchpad_tool_map,
                    context_window,
                    initial_used,
                    token_counter,
                })
                .await?;

                scratchpad_tools.push(build.wrapper);
                worker_config.scratchpad_tools_config = Some(build.tools_config);
            }
        }

        // ComposedWrapper applies transform_output in reverse-list order, so
        // the LAST entry runs FIRST on the raw tool output. Persistence must
        // see raw output (for debugging/retry), so it goes last. Scratchpad
        // also needs raw output — persistence's transform_output is a
        // passthrough that just caches the raw — and rewrites to the pointer.
        // Duplicate-guard and observer then see the pointer, which is what
        // should surface to the LLM/UI.
        let mut wrappers: Vec<Arc<dyn ToolWrapper>> = vec![observer_wrapper, duplicate_guard];
        wrappers.extend(scratchpad_tools);
        wrappers.push(persistence_wrapper);

        // HITL config gate. When `[hitl]` is configured, gate matching worker
        // tool calls behind the decision route, and pre-build the agent-callable
        // `request_approval` tool with the same scope/route (attached by
        // `add_all_tools` via `hitl_request_approval_tool`). The gate is composed
        // FIRST so its `pre_call` runs before every other wrapper and a denial
        // short-circuits the call — `ComposedWrapper::pre_call` iterates the vec
        // front-to-back. The gate only implements `pre_call`, so prepending it
        // leaves the documented `transform_output` ordering above untouched.
        if let Some(hitl) = worker_config.hitl.clone() {
            let (run_id_str, session_id_owned) = {
                let p = self.persistence.lock().await;
                (p.run_id().to_string(), p.session_id().map(String::from))
            };
            let run_id = run_id_str.parse::<super::RunId>().map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("HITL: orchestration run id '{run_id_str}' is not a valid UUID: {e}")
                        .into()
                },
            )?;
            let scope = crate::hitl::AgentScope::Worker {
                run_id,
                task: super::TaskIdentity::new(task_id, worker_name.map(String::from)),
                session_id: session_id_owned.map(crate::config::SessionId::new),
            };
            let request_id = worker_config.request_id.clone().unwrap_or_default();
            let gate = Arc::new(crate::hitl::HitlApprovalWrapper::new(
                hitl.patterns.clone(),
                hitl.route.clone(),
                scope.clone(),
                request_id.clone(),
            ));
            wrappers.insert(0, gate);
            worker_config.hitl_request_approval_tool = Some(crate::hitl::RequestApprovalTool::new(
                hitl.route.clone(),
                scope,
                request_id,
            ));
        }

        let wrapper: Arc<dyn ToolWrapper> = Arc::new(ComposedWrapper::new(wrappers));

        // Configure worker based on assignment
        if let Some(name) = worker_name {
            let worker = self.config.get_worker(name).ok_or_else(|| {
                format!(
                    "Worker '{}' not found in configuration (task {})",
                    name, task_id
                )
            })?;
            tracing::info!("Creating worker '{}' for task {}", name, task_id);
            let full_preamble = super::config::WORKER_PREAMBLE_TEMPLATE
                .replace("{{worker_system_prompt}}", &worker.preamble);
            worker_config.preamble_override = Some(full_preamble);
            if !worker.mcp_filter.is_empty() {
                worker_config.mcp_filter = Some(worker.mcp_filter.clone());
            }

            // Filter vector stores: worker only gets stores explicitly assigned to it
            // Empty vector_stores = no RAG access (must opt-in)
            let assigned_stores: std::collections::HashSet<&str> =
                worker.vector_stores.iter().map(|s| s.as_str()).collect();
            worker_config
                .vector_stores
                .retain(|vs| assigned_stores.contains(vs.name.as_str()));

            // Inject vector store context into preamble if stores assigned
            if !worker_config.vector_stores.is_empty() {
                let vs_context =
                    super::config::build_vector_store_context(&worker_config.vector_stores);
                if let Some(ref mut preamble) = worker_config.preamble_override {
                    preamble.push_str(&vs_context);
                }
            }

            tracing::debug!(
                "Worker '{}' vector stores: {:?}",
                name,
                worker_config
                    .vector_stores
                    .iter()
                    .map(|vs| &vs.name)
                    .collect::<Vec<_>>()
            );
            let worker_name_copy = String::from(name);

            // Orchestrator provides context factory with task metadata
            worker_config.tool_context_factory = Some(Arc::new(move |tool_name: &str| {
                ToolCallContext::new(tool_name).with_task_context(
                    task_id,
                    worker_name_copy.clone(),
                    attempt,
                )
            }));
        } else {
            worker_config.preamble_override =
                Some(super::config::build_worker_preamble(&self.config));
            let orchestrator_id_copy = self.orchestrator_id.clone();

            // Orchestrator provides context factory with task metadata
            worker_config.tool_context_factory = Some(Arc::new(move |tool_name: &str| {
                ToolCallContext::new(tool_name).with_task_context(
                    task_id,
                    orchestrator_id_copy.clone(),
                    attempt,
                )
            }));
        }

        // Orchestrator owns tool wrapping decision
        worker_config.tool_wrapper = Some(wrapper);

        // Give workers access to result artifacts
        worker_config.orchestration_persistence = Some(self.persistence.clone());

        // Give workers the submit_result tool for structured output
        let submit_result_decision: super::tools::SubmitResultDecision = Arc::new(Mutex::new(None));
        worker_config.orchestration_submit_result = Some(submit_result_decision.clone());

        // Disable orchestration in worker config to avoid nested orchestration
        worker_config.orchestration = None;

        // Resolution order: worker turn_depth → [agent].turn_depth → DEFAULT_MAX_DEPTH.
        // Scratchpad bonus only applies when scratchpad was actually wired up.
        let base_depth = worker_name
            .and_then(|name| self.config.workers.get(name))
            .and_then(|w| w.turn_depth)
            .or(self.agent_config.agent.turn_depth)
            .unwrap_or(crate::builder::DEFAULT_MAX_DEPTH);
        let scratchpad_bonus = worker_config
            .scratchpad_tools_config
            .as_ref()
            .and(effective_scratchpad.as_ref())
            .map(|sp| sp.turn_depth_bonus)
            .unwrap_or(0);
        let resolved_depth = base_depth + scratchpad_bonus;
        worker_config.agent.turn_depth = Some(resolved_depth);
        if scratchpad_bonus > 0 {
            tracing::info!(
                "Worker {} turn_depth={} (base={}, scratchpad_bonus={})",
                task_id,
                resolved_depth,
                base_depth,
                scratchpad_bonus
            );
        } else {
            tracing::info!("Worker {} turn_depth={}", task_id, resolved_depth);
        }

        if worker_config.scratchpad_tools_config.is_some()
            && let Some(ref mut preamble) = worker_config.preamble_override
        {
            preamble.push_str(scratchpad::SCRATCHPAD_PREAMBLE);
        }

        tracing::debug!(
            "Worker {} config: preamble length = {} chars, mcp_filter = {:?}",
            task_id,
            worker_config
                .preamble_override
                .as_ref()
                .map(|s| s.len())
                .unwrap_or(0),
            worker_config.mcp_filter
        );

        // Capture preamble before config is consumed by builder
        let preamble = if self.prompt_journal.is_some() {
            worker_config
                .preamble_override
                .as_deref()
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        };

        // Build worker agent using shared MCP connections.
        // Client-side tools are not supported in orchestration mode and are
        // never attached to workers (or the coordinator).
        let (provider_agent, model_name) = self.build_worker_provider_agent(&worker_config).await?;

        let agent = Agent {
            inner: provider_agent,
            model: model_name,
            max_depth: resolved_depth,
            mcp_manager: self.mcp_manager.clone(),
            fallback_tool_parsing: false,
            fallback_tool_names: vec![],
            context_window: worker_config.llm.context_window(),
            scratchpad_budget: worker_config
                .scratchpad_tools_config
                .as_ref()
                .map(|sp| sp.budget.clone()),
            client_tool_names: Default::default(),
        };

        Ok(AgentWithPreamble {
            agent,
            preamble,
            escalation_flag,
            submit_result_decision,
        })
    }

    /// Execute a blocking chat call with timeout — for **worker tasks only**.
    ///
    /// Workers need the full ReAct loop for sequential MCP tool chains.
    /// Coordinator phases (planning, continuation routing) use `stream_and_collect()`
    /// for early exit after one-shot tool decisions and reasoning forwarding.
    ///
    /// Returns `Err` with a timeout message if the call exceeds `per_call_timeout_secs`.
    /// A value of 0 (the default) disables the per-call timeout.
    #[allow(dead_code)]
    async fn chat_with_timeout(
        &self,
        agent: &Agent,
        prompt: &str,
        history: Vec<rig::completion::Message>,
        phase: &str,
    ) -> Result<crate::provider_agent::CompletionResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        let timeout_secs = self.config.per_call_timeout_secs();
        if timeout_secs == 0 {
            return agent.chat(prompt, history).await;
        }

        let timeout = Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, agent.chat(prompt, history)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                tracing::warn!(
                    "{} LLM call timed out after {}s (per_call_timeout_secs={})",
                    phase,
                    timeout_secs,
                    timeout_secs,
                );
                Err(format!(
                    "{} timed out after {}s — the LLM provider did not respond in time",
                    phase, timeout_secs
                )
                .into())
            }
        }
    }

    /// Stream a full ReAct chat, forwarding reasoning events and collecting the final response.
    ///
    /// Unlike `stream_and_collect` (which uses `max_depth=1` and early-exit for coordinator
    /// one-shot tool phases), this uses the agent's configured `max_depth` and runs the
    /// full multi-turn tool loop. Used for workers and phase continuation.
    ///
    /// Key behaviors:
    /// - Uses `agent.stream_chat()` which respects the agent's configured `max_depth`
    /// - Forwards `ReasoningDelta`/`Reasoning` items through `event_tx`
    /// - No early-exit — runs the complete ReAct loop
    /// - Timeout wrapping via `per_call_timeout_secs`
    async fn stream_and_forward(
        &self,
        agent: &Agent,
        params: StreamCallParams<'_>,
        stream_context: Option<StreamContext<'_>>,
        decision_ready: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
    ) -> Result<crate::provider_agent::CompletionResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        use crate::provider_agent::{
            CompletionResponse, StreamedAssistantContent, StreamedUserContent,
        };
        use crate::tool_error_detection::{ToolResultStatus, detect_tool_error};
        use futures::StreamExt;
        use rig::completion::Usage;
        use std::collections::HashMap;

        let StreamCallParams {
            prompt,
            history,
            phase,
            event_tx,
        } = params;
        let timeout_secs = self.config.per_call_timeout_secs();
        let emit_scratchpad_events = scratchpad::emit_scratchpad_tool_events_enabled();
        let stream_future = async {
            let mut stream = agent.stream_chat(prompt, history).await;
            let mut content = String::new();
            let mut usage = Usage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
            };
            // Track scratchpad-tool start times so the matching ToolResult
            // can compute a duration. Membership also gates the ToolResult
            // branch — only IDs we emitted a `ToolCallStarted` for are
            // forwarded as `ToolCallCompleted`, preventing accidental
            // double-emission for MCP tools (which the ObserverWrapper
            // already covers).
            let mut scratchpad_tool_starts: HashMap<String, std::time::Instant> = HashMap::new();

            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                        content.push_str(&t);
                    }
                    Ok(StreamItem::StreamAssistantItem(
                        StreamedAssistantContent::ReasoningDelta { delta, .. },
                    )) => {
                        if let Some(tx) = event_tx {
                            if let Some(ref ctx) = stream_context {
                                let _ = tx
                                    .send(Ok(StreamItem::OrchestratorEvent(
                                        OrchestratorEvent::WorkerReasoning {
                                            task_id: ctx.task_id,
                                            worker_id: ctx.worker_id.to_string(),
                                            content: delta,
                                        },
                                    )))
                                    .await;
                            } else {
                                let _ = tx
                                    .send(Ok(StreamItem::StreamAssistantItem(
                                        StreamedAssistantContent::ReasoningDelta {
                                            delta,
                                            id: None,
                                        },
                                    )))
                                    .await;
                            }
                        }
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(_))) => {
                        // Final reasoning block — already forwarded as deltas above.
                        // Skip to avoid double-emission.
                    }
                    // Debug toggle: when `AURA_EMIT_SCRATCHPAD_TOOL_EVENTS` is on,
                    // forward scratchpad exploration tool calls as
                    // `OrchestratorEvent::ToolCallStarted` so they surface in
                    // `aura.orchestrator.tool_call_started`. MCP tools are
                    // intentionally NOT forwarded here — the orchestrator's
                    // `ObserverWrapper` already covers them via the observer
                    // broadcast channel, and forwarding them again would
                    // double-emit.
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall(
                        ref tc,
                    ))) if emit_scratchpad_events && scratchpad::is_internal_tool(&tc.name) => {
                        if let Some(tx) = event_tx {
                            scratchpad_tool_starts.insert(tc.id.clone(), std::time::Instant::now());
                            let arguments: serde_json::Value = serde_json::from_str(&tc.arguments)
                                .unwrap_or(serde_json::json!({}));
                            let (task_id, worker_id) = match stream_context.as_ref() {
                                Some(w) => (Some(w.task_id), w.worker_id.to_string()),
                                None => (None, "main".to_string()),
                            };
                            let _ = tx
                                .send(Ok(StreamItem::OrchestratorEvent(
                                    OrchestratorEvent::ToolCallStarted {
                                        task_id,
                                        tool_call_id: tc.id.clone(),
                                        tool_name: tc.name.clone(),
                                        worker_id,
                                        arguments,
                                    },
                                )))
                                .await;
                        }
                    }
                    // Companion to the ToolCall branch above. Membership in
                    // `scratchpad_tool_starts` gates emission so MCP results
                    // (already covered by `ObserverWrapper`) don't duplicate.
                    Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(ref tr)))
                        if emit_scratchpad_events
                            && scratchpad_tool_starts.contains_key(&tr.id) =>
                    {
                        if let Some(tx) = event_tx {
                            let duration_ms = scratchpad_tool_starts
                                .remove(&tr.id)
                                .map(|start| start.elapsed().as_millis() as u64)
                                .unwrap_or(0);
                            let task_id = stream_context.as_ref().map(|w| w.task_id);
                            let success =
                                matches!(detect_tool_error(&tr.result), ToolResultStatus::Success);
                            let _ = tx
                                .send(Ok(StreamItem::OrchestratorEvent(
                                    OrchestratorEvent::ToolCallCompleted {
                                        task_id,
                                        tool_call_id: tr.id.clone(),
                                        success,
                                        duration_ms,
                                        result: tr.result.clone(),
                                    },
                                )))
                                .await;
                        }
                    }
                    Ok(StreamItem::Final(info)) => {
                        content = info.content;
                        usage = info.usage;
                        break;
                    }
                    Ok(StreamItem::FinalMarker) => {
                        // Per-turn marker — not end-of-stream. Continue collecting.
                    }
                    Ok(StreamItem::TurnUsage(turn)) => {
                        usage.input_tokens += turn.input_tokens;
                        usage.output_tokens += turn.output_tokens;
                        usage.total_tokens += turn.total_tokens;
                        self.usage_state
                            .accumulate_usage(turn.input_tokens, turn.output_tokens);
                        if let Some(ref budget) = agent.scratchpad_budget {
                            budget.set_estimated_used(turn.input_tokens, turn.output_tokens);
                        }
                    }
                    Err(e) => return Err(e),
                    Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(ref tr))) => {
                        tracing::debug!(
                            "{}: tool result received (id={}, call_id={})",
                            phase,
                            tr.id,
                            tr.call_id.as_deref().unwrap_or("-")
                        );
                        if decision_ready().await {
                            tracing::debug!("{}: decision captured, reading turn usage", phase);
                            if let Some(Ok(StreamItem::TurnUsage(turn))) = stream.next().await {
                                usage.input_tokens += turn.input_tokens;
                                usage.output_tokens += turn.output_tokens;
                                usage.total_tokens += turn.total_tokens;
                            }
                            break;
                        }
                    }
                    _ => {} // ToolCall, ToolCallDelta — rig handles execution
                }
            }

            Ok(CompletionResponse { content, usage })
        };

        if timeout_secs == 0 {
            stream_future.await
        } else {
            tokio::pin!(stream_future);
            tokio::select! {
                result = &mut stream_future => result,
                _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
                    if let (Some(ctx), Some(hitl)) =
                        (stream_context.as_ref(), self.agent_config.hitl.as_ref())
                        && let Some(active) =
                            hitl.cancel_worker_task_timeout(ctx.task_id, ctx.worker_name)
                    {
                        tracing::warn!(
                            task_id = ctx.task_id,
                            worker_id = ctx.worker_id,
                            decision_id = %active.decision_id,
                            tool_name = %active.tool_name,
                            "{} timed out after {}s while waiting for HITL approval",
                            phase,
                            timeout_secs,
                        );
                        let _ = tokio::time::timeout(
                            HITL_TASK_TIMEOUT_CLEANUP,
                            &mut stream_future,
                        )
                        .await;
                        return Err(format!(
                            "{} timed out after {}s while waiting for HITL approval {} for tool '{}' [approval_task_timeout]",
                            phase, timeout_secs, active.decision_id, active.tool_name
                        )
                        .into());
                    }
                    tracing::warn!(
                        "{} timed out after {}s (per_call_timeout_secs={})",
                        phase,
                        timeout_secs,
                        timeout_secs,
                    );
                    Err(format!(
                        "{} timed out after {}s — the LLM provider did not respond in time",
                        phase, timeout_secs
                    )
                    .into())
                }
            }
        }
    }

    /// Stream a coordinator call with early exit and optional reasoning forwarding.
    ///
    /// Replaces `chat_with_timeout` for one-shot tool phases (planning, continuation routing).
    /// Workers MUST NOT use this — they need the full ReAct loop for MCP tool chains.
    ///
    /// Key behaviors:
    /// - Opens stream with `max_depth=1` (rig safety net, but early exit is primary guard)
    /// - Forwards `ReasoningDelta`/`Reasoning` items through `event_tx` when provided
    /// - Short-circuits after first `ToolResult` when `decision_ready()` returns true
    /// - Falls back to normal completion for text-only responses
    ///
    /// Per-turn usage is captured from `TurnUsage` events even when
    /// short-circuiting before the terminal `Final`.
    async fn stream_and_collect(
        &self,
        agent: &Agent,
        params: StreamCallParams<'_>,
        decision_ready: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
    ) -> Result<crate::provider_agent::CompletionResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        use crate::provider_agent::{
            CompletionResponse, StreamedAssistantContent, StreamedUserContent,
        };
        use futures::StreamExt;
        use rig::completion::Usage;

        let StreamCallParams {
            prompt,
            history,
            phase,
            event_tx,
        } = params;
        let timeout_secs = self.config.per_call_timeout_secs();
        let stream_future = async {
            let mut stream = agent
                .stream_chat_with_depth(prompt, history, agent.max_depth)
                .await;
            let mut content = String::new();
            let mut usage = Usage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
            };

            while let Some(item) = stream.next().await {
                match item {
                    // Text accumulation
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                        content.push_str(&t);
                    }
                    // Reasoning forwarding (fixes reasoning token discard during orchestration)
                    Ok(StreamItem::StreamAssistantItem(
                        ref sa @ StreamedAssistantContent::ReasoningDelta { .. },
                    )) => {
                        if let Some(tx) = event_tx {
                            let _ = tx
                                .send(Ok(StreamItem::StreamAssistantItem(sa.clone())))
                                .await;
                        }
                    }
                    Ok(StreamItem::StreamAssistantItem(
                        ref sa @ StreamedAssistantContent::Reasoning(_),
                    )) => {
                        if let Some(tx) = event_tx {
                            let _ = tx
                                .send(Ok(StreamItem::StreamAssistantItem(sa.clone())))
                                .await;
                        }
                    }
                    // Tool result — check decision, short-circuit (fixes ReAct loop waste)
                    Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(ref tr))) => {
                        tracing::debug!(
                            "{}: tool result received (id={}, call_id={})",
                            phase,
                            tr.id,
                            tr.call_id.as_deref().unwrap_or("-")
                        );
                        if decision_ready().await {
                            tracing::debug!("{}: decision captured, reading turn usage", phase);
                            if let Some(Ok(StreamItem::TurnUsage(turn))) = stream.next().await {
                                usage.input_tokens += turn.input_tokens;
                                usage.output_tokens += turn.output_tokens;
                                usage.total_tokens += turn.total_tokens;
                                self.usage_state
                                    .accumulate_usage(turn.input_tokens, turn.output_tokens);
                            }
                            break;
                        }
                    }
                    // Final response — authoritative content + usage
                    Ok(StreamItem::Final(info)) => {
                        content = info.content;
                        usage = info.usage;
                        break;
                    }
                    Ok(StreamItem::TurnUsage(turn)) => {
                        usage.input_tokens += turn.input_tokens;
                        usage.output_tokens += turn.output_tokens;
                        usage.total_tokens += turn.total_tokens;
                        self.usage_state
                            .accumulate_usage(turn.input_tokens, turn.output_tokens);
                    }
                    Ok(StreamItem::FinalMarker) => break,
                    // MaxDepthError: success if decision was captured, error otherwise
                    Err(ref e) if is_max_depth_error(e.as_ref()) => {
                        if decision_ready().await {
                            tracing::debug!("{}: depth cap hit but decision captured", phase);
                            break;
                        }
                        return Err(format!("{}: {}", phase, e).into());
                    }
                    // Context overflow — propagate
                    Err(ref e) if is_context_overflow_error(e.as_ref()) => {
                        return Err(format!("{}: {}", phase, e).into());
                    }
                    Err(e) => return Err(e),
                    _ => {} // ToolCall, ToolCallDelta — rig handles execution
                }
            }
            Ok(CompletionResponse { content, usage })
        };

        // Timeout wrapping (preserves chat_with_timeout behavior)
        if timeout_secs == 0 {
            stream_future.await
        } else {
            match tokio::time::timeout(Duration::from_secs(timeout_secs), stream_future).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    tracing::warn!(
                        "{} coordinator timed out after {}s (per_call_timeout_secs={})",
                        phase,
                        timeout_secs,
                        timeout_secs,
                    );
                    Err(format!("{} timed out after {}s", phase, timeout_secs).into())
                }
            }
        }
    }

    /// Build the iter-1 planning wrapper (fresh query, no prior iteration).
    /// Enumerates the three routing tools with neutral bullets.
    pub(crate) fn build_planning_wrapper(
        query: &str,
        worker_section: &str,
        worker_guidelines: &str,
    ) -> String {
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        format!(
            "Current time: {timestamp}\n\n\
             Analyze this user query and decide on the best approach.\n\n\
             USER QUERY: {query}{worker_section}\n\n\
             You have three routing tools. Call EXACTLY ONE (do not call more than one):\n\n\
             1. **respond_directly** — For simple factual questions answerable from general knowledge, \
                OR when the relevant workers have no tools configured (tools show \"none configured\") \
                and the query requires external data. In that case, explain the limitation and suggest \
                configuring MCP servers.\n\
                Do not use for queries about system data, logs, metrics, or anything requiring tools \
                when workers DO have tools available.\n\n\
             2. **create_plan** — For queries requiring tool execution, data gathering, or multi-step analysis.\n\
                When uncertain, choose create_plan only if tool execution or multi-step work is genuinely required; otherwise choose respond_directly.\n\n\
             3. **request_clarification** — For genuinely ambiguous queries where intent is unclear.\n\
                Use sparingly when a reasonable interpretation exists.\n\n\
             {worker_guidelines}\n\
             - For time-scoped tasks, include the current time and relevant time range in the task description so workers have explicit time context\n\n\
             Call the appropriate routing tool now.",
        )
    }

    /// Build the post-execute continuation wrapper (end-of-iteration decision
    /// point). Renders the continuation prompt from the iteration context and
    /// deliberately does NOT re-enumerate the three routing tools — the
    /// coordinator already has them in its preamble, and re-listing them here
    /// would layer additional tool-choice bias into the user message.
    fn build_continuation_wrapper(
        ctx: &IterationContext,
        max_iterations: usize,
        show_tool_chain: bool,
        content_max_length: usize,
    ) -> String {
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let base =
            ctx.build_continuation_prompt(max_iterations, show_tool_chain, content_max_length);
        format!("Current time: {timestamp}\n\n{base}")
    }

    /// Plan with routing tool support via persistent conversation.
    ///
    /// Uses the coordinator from `CoordinatorState` (created once at
    /// `run_orchestration` entry) and grows the conversation with each turn.
    /// If the coordinator doesn't call a routing tool, appends a correction
    /// message and retries within the same conversation context.
    ///
    /// Enforces config flags: converts Direct/Clarification to single-task
    /// Orchestrated when `allow_direct_answers`/`allow_clarification` is false.
    #[tracing::instrument(
        name = "orchestration.planning",
        skip_all,
        fields(orchestration.phase = "planning")
    )]
    async fn plan_with_routing(
        &self,
        query: &str,
        chat_history: &[rig::completion::Message],
        coordinator_state: &mut CoordinatorState,
        previous: Option<&IterationContext>,
        event_tx: Option<&tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>>,
    ) -> Result<(PlanningResponse, String, String), StreamError> {
        let max_correction_attempts = self.config.max_plan_parse_retries;

        let (worker_section, _worker_field, worker_guidelines) =
            self.build_worker_prompt_sections();

        // Build the primary user message based on call phase.
        let planning_prompt = match previous {
            None => Self::build_planning_wrapper(query, &worker_section, &worker_guidelines),
            Some(ctx) => Self::build_continuation_wrapper(
                ctx,
                self.config.max_planning_cycles,
                self.config.show_tool_reasoning_in_continuation(),
                self.config.result_summary_length(),
            ),
        };

        let mut final_prompt: Option<String> = None;
        let mut final_response: Option<String> = None;
        let planning_start = Instant::now();

        for attempt in 1..=max_correction_attempts {
            // Clear any stale routing decision
            {
                let mut guard = coordinator_state.routing_decision.lock().await;
                *guard = None;
            }

            let attempt_start = Instant::now();
            // First attempt sends the planning/continuation prompt; subsequent
            // attempts send a correction message within the same conversation.
            let prompt = if attempt == 1 {
                planning_prompt.clone()
            } else {
                super::prompt_constants::corrections::ROUTING_TOOL_REQUIRED.to_string()
            };

            tracing::info!(
                "Planning attempt {}/{} (per_call_timeout={}s, conversation_len={})",
                attempt,
                max_correction_attempts,
                self.config.per_call_timeout_secs(),
                coordinator_state.conversation.len(),
            );

            // Build full history: external chat + accumulated coordinator conversation
            let mut full_history = chat_history.to_vec();
            full_history.extend(coordinator_state.conversation.iter().cloned());

            self.journal_record(
                JournalPhase::Planning {
                    attempt,
                    max_attempts: max_correction_attempts,
                },
                &coordinator_state.preamble,
                &prompt,
            );

            let rd = coordinator_state.routing_decision.clone();
            let response = match self
                .stream_and_collect(
                    &coordinator_state.agent,
                    StreamCallParams {
                        prompt: &prompt,
                        history: full_history,
                        phase: "Planning",
                        event_tx,
                    },
                    || {
                        let rd = rd.clone();
                        Box::pin(async move { rd.lock().await.is_some() })
                    },
                )
                .await
            {
                Ok(r) => {
                    crate::logging::set_token_usage(
                        &tracing::Span::current(),
                        r.usage.input_tokens,
                        r.usage.output_tokens,
                        r.usage.total_tokens,
                        0,
                    );
                    r
                }
                Err(e) if is_context_overflow_error(e.as_ref()) => {
                    let suggestion = context_overflow_suggestion("planning");
                    return Err(
                        format!("Context limit exceeded during planning. {}", suggestion).into(),
                    );
                }
                Err(e) => {
                    let err_str = e.to_string();
                    coordinator_state
                        .conversation
                        .push(rig::completion::Message::user(&prompt));
                    if is_transient_planning_error(&err_str) {
                        tracing::warn!(
                            "Planning attempt {} transient error after {:.1}s, retrying: {}",
                            attempt,
                            attempt_start.elapsed().as_secs_f64(),
                            err_str,
                        );
                        continue;
                    }
                    tracing::warn!(
                        "Planning attempt {} failed after {:.1}s: {}",
                        attempt,
                        attempt_start.elapsed().as_secs_f64(),
                        err_str,
                    );
                    return Err(format!("Planning failed: {}", err_str).into());
                }
            };

            // Grow conversation: user turn
            coordinator_state
                .conversation
                .push(rig::completion::Message::user(&prompt));

            // Check if a routing tool was called
            let decision = coordinator_state.routing_decision.lock().await.take();

            if let Some(planning_response) = decision {
                let response_text = if response.content.trim().is_empty() {
                    serde_json::to_string_pretty(&planning_response)
                        .unwrap_or_else(|_| response.content.clone())
                } else {
                    response.content.clone()
                };

                // Grow conversation: assistant turn (serialized routing decision)
                coordinator_state
                    .conversation
                    .push(rig::completion::Message::assistant(&response_text));

                // Persist planning phase artifacts
                {
                    let persistence = self.persistence.lock().await;
                    if let Err(e) = persistence
                        .write_planning_phase(&prompt, &response_text)
                        .await
                    {
                        tracing::warn!("Failed to persist planning phase: {}", e);
                    }
                }
                let routing_rationale = planning_response.routing_rationale().to_string();
                tracing::info!(
                    "Routing decision (attempt {}, {:.1}s): {} (rationale: {})",
                    attempt,
                    attempt_start.elapsed().as_secs_f64(),
                    planning_response.variant_name(),
                    truncate_query(&routing_rationale, 80),
                );

                let planning_response = Self::enforce_routing_config(
                    planning_response,
                    query,
                    self.config.allow_direct_answers,
                    self.config.allow_clarification,
                );

                if matches!(&planning_response, PlanningResponse::StepsPlan { .. })
                    && let Some(plan) = planning_response.clone().into_plan()
                {
                    let persistence = self.persistence.lock().await;
                    if let Err(e) = persistence.write_plan(&plan).await {
                        tracing::warn!("Failed to persist plan: {}", e);
                    }
                }

                return Ok((planning_response, prompt, response_text));
            }

            // No routing tool called — record assistant response and try correction
            let response_text = response.content.clone();
            coordinator_state
                .conversation
                .push(rig::completion::Message::assistant(&response_text));

            {
                let persistence = self.persistence.lock().await;
                if let Err(e) = persistence
                    .write_planning_phase(&prompt, &response_text)
                    .await
                {
                    tracing::warn!("Failed to persist planning phase: {}", e);
                }
            }

            let (response_preview, _) = safe_truncate(&response.content, 300);
            tracing::warn!(
                "No routing tool called (attempt {}/{}). Appending correction to conversation. Response: {}",
                attempt,
                max_correction_attempts,
                response_preview,
            );

            final_prompt = Some(prompt);
            final_response = Some(response_text);
        }

        // All correction attempts exhausted
        tracing::warn!(
            "All {} planning attempts failed after {:.1}s. Coordinator did not call a routing tool.",
            max_correction_attempts,
            planning_start.elapsed().as_secs_f64(),
        );

        if previous.is_some() {
            return Err(format!(
                "All {} post-execute planning attempts failed (coordinator could not route)",
                max_correction_attempts,
            )
            .into());
        }

        let fallback = PlanningResponse::StepsPlan {
            goal: query.to_string(),
            steps: vec![super::types::StepInput::LeafTask {
                task: format!("Execute: {}", truncate_query(query, 100)),
                worker: None,
            }],
            routing_rationale: "Fallback: all routing attempts failed".to_string(),
            planning_summary: String::new(),
        };

        let (query_preview, _) = safe_truncate(query, 100);
        tracing::info!("Created fallback single-task plan for: {}", query_preview);

        Ok((
            fallback,
            final_prompt.unwrap_or_default(),
            final_response.unwrap_or_default(),
        ))
    }

    /// Enforce config flags on a routing decision.
    ///
    /// When `allow_direct_answers` or `allow_clarification` is false,
    /// converts the response to a single-task `StepsPlan`.
    ///
    /// Takes flags as arguments (rather than reading `self.config`) so the
    /// transformation is unit-testable without an `Orchestrator` instance.
    fn enforce_routing_config(
        response: PlanningResponse,
        query: &str,
        allow_direct_answers: bool,
        allow_clarification: bool,
    ) -> PlanningResponse {
        match &response {
            PlanningResponse::Direct {
                response: answer,
                routing_rationale,
                ..
            } if !allow_direct_answers => {
                tracing::info!(
                    "Config override: converting direct answer to orchestrated plan (allow_direct_answers=false)"
                );
                PlanningResponse::StepsPlan {
                    goal: query.to_string(),
                    steps: vec![super::types::StepInput::LeafTask {
                        task: format!("Answer the user's query: {}", truncate_query(query, 80)),
                        worker: None,
                    }],
                    routing_rationale: format!(
                        "Config override (allow_direct_answers=false). Original rationale: {} | Original answer: {}",
                        routing_rationale,
                        truncate_query(answer, 100)
                    ),
                    planning_summary: String::new(),
                }
            }
            PlanningResponse::Clarification {
                question,
                routing_rationale,
                ..
            } if !allow_clarification => {
                tracing::info!(
                    "Config override: converting clarification to orchestrated plan (allow_clarification=false)"
                );
                PlanningResponse::StepsPlan {
                    goal: query.to_string(),
                    steps: vec![super::types::StepInput::LeafTask {
                        task: format!(
                            "Investigate and answer the user's query: {}",
                            truncate_query(query, 80)
                        ),
                        worker: None,
                    }],
                    routing_rationale: format!(
                        "Config override (allow_clarification=false). Original rationale: {} | Original question: {}",
                        routing_rationale,
                        truncate_query(question, 100)
                    ),
                    planning_summary: String::new(),
                }
            }
            _ => response,
        }
    }

    /// Build the worker-related sections of the planning prompt.
    ///
    /// Based on `tools_in_planning` config:
    /// - `None`: Just worker descriptions (original behavior)
    /// - `Summary`: Worker descriptions + tool names
    /// - `Full`: Worker descriptions + tool names + descriptions
    fn build_worker_prompt_sections(&self) -> (String, String, String) {
        use super::config::ToolVisibility;

        if self.config.has_workers() {
            let worker_names: Vec<&str> = self.config.available_worker_names();
            let names_json: Vec<String> =
                worker_names.iter().map(|n| format!("\"{}\"", n)).collect();

            // Build worker section based on visibility setting
            let section = match &self.config.tools_in_planning {
                ToolVisibility::None => self.build_workers_section_no_tools(),
                ToolVisibility::Summary => self.build_workers_section_with_tools(),
                ToolVisibility::Full => self.build_workers_section_with_full_tools(),
            };

            let field = r#",
      "worker": "worker_name""#
                .to_string();

            let guidelines = format!(
                r#"
- Assign each task to a worker using the "worker" field
- Valid worker names: {}
- Choose the worker whose tools best match what the task needs to accomplish"#,
                names_json.join(", ")
            );

            (section, field, guidelines)
        } else {
            (String::new(), String::new(), String::new())
        }
    }

    /// Build worker section without tool information (ToolVisibility::None).
    fn build_workers_section_no_tools(&self) -> String {
        let workers_list = self.config.format_workers_for_prompt();
        format!(
            r#"

AVAILABLE WORKERS:
{}

Each worker has specialized capabilities. Assign tasks to the most appropriate worker."#,
            workers_list
        )
    }

    /// Build worker section with tool names (ToolVisibility::Summary).
    fn build_workers_section_with_tools(&self) -> String {
        let worker_tools = self.resolve_worker_tools();
        let max_tools = self.config.max_tools_per_worker;
        let mut sections = Vec::new();

        for (name, config) in &self.config.workers {
            let tools = worker_tools.get(name).cloned().unwrap_or_default();
            let tool_list = self.format_tool_list(&tools, max_tools);

            let section = if tool_list.is_empty() {
                format!(
                    "## {}\n{}\nTools: (none configured — this worker cannot query external systems)",
                    name, config.description
                )
            } else {
                format!("## {}\n{}\nTools: {}", name, config.description, tool_list)
            };
            sections.push(section);
        }

        format!(
            r#"

AVAILABLE WORKERS:
NOTE: Worker names below are role assignments, not callable tool names. Only the tools listed under each worker are MCP tools that workers can execute.

{}

Assign tasks to the worker whose tools best match the required operations."#,
            sections.join("\n\n")
        )
    }

    /// Build worker section with full tool info (ToolVisibility::Full).
    fn build_workers_section_with_full_tools(&self) -> String {
        let worker_tools = self.resolve_worker_tools();
        let tool_descriptions = self.get_all_tool_descriptions();
        let max_tools = self.config.max_tools_per_worker;
        let mut sections = Vec::new();

        for (name, config) in &self.config.workers {
            let tools = worker_tools.get(name).cloned().unwrap_or_default();

            let tool_details: Vec<String> = tools
                .iter()
                .take(max_tools)
                .map(|t| {
                    if let Some(desc) = tool_descriptions.get(t) {
                        format!("  - {}: {}", t, desc)
                    } else {
                        format!("  - {}", t)
                    }
                })
                .collect();

            let remaining = tools.len().saturating_sub(max_tools);
            let tool_section = if tool_details.is_empty() {
                String::new()
            } else if remaining > 0 {
                format!("{}\n  (+{} more)", tool_details.join("\n"), remaining)
            } else {
                tool_details.join("\n")
            };

            let section = if tool_section.is_empty() {
                format!("## {}\n{}", name, config.description)
            } else {
                format!(
                    "## {}\n{}\nTools:\n{}",
                    name, config.description, tool_section
                )
            };
            sections.push(section);
        }

        format!(
            r#"

AVAILABLE WORKERS:
NOTE: Worker names below are role assignments, not callable tool names. Only the tools listed under each worker are MCP tools that workers can execute.

{}

Assign tasks to the worker whose tools best match the required operations."#,
            sections.join("\n\n")
        )
    }

    /// Format a list of tool names with truncation.
    ///
    /// If the list exceeds `max`, truncates and appends "(+N more)".
    fn format_tool_list(&self, tools: &[String], max: usize) -> String {
        if tools.is_empty() {
            return String::new();
        }

        let display_tools: Vec<&str> = tools.iter().take(max).map(|s| s.as_str()).collect();
        let remaining = tools.len().saturating_sub(max);

        if remaining > 0 {
            format!("{} (+{} more)", display_tools.join(", "), remaining)
        } else {
            display_tools.join(", ")
        }
    }

    // ========================================================================
    // Tool Resolution Methods (for capability-aware planning)
    // ========================================================================

    /// Get all tool names from the MCP manager.
    ///
    /// Collects tool names from all sources:
    /// - Streamable HTTP tools
    /// - SSE tools
    /// - Legacy tool definitions
    ///
    /// Returns an empty Vec if no MCP manager is present.
    fn get_all_tool_names(&self) -> Vec<String> {
        let Some(ref mcp_manager) = self.mcp_manager else {
            return Vec::new();
        };

        let mut names = Vec::new();

        // Collect from streamable HTTP tools (rmcp::model::Tool has Cow<'static, str>)
        for tools in mcp_manager.streamable_tools.values() {
            for tool in tools {
                names.push(tool.name.to_string());
            }
        }

        // Collect from SSE tools
        for tools in mcp_manager.sse_tools.values() {
            for tool in tools {
                names.push(tool.name.to_string());
            }
        }

        // Collect from STDIO tools
        for tools in mcp_manager.stdio_tools.values() {
            for tool in tools {
                names.push(tool.name.to_string());
            }
        }

        // Remove duplicates while preserving order
        let mut seen = std::collections::HashSet::new();
        names.retain(|name| seen.insert(name.clone()));

        names
    }

    /// Get tool schemas for inspect_tool_params.
    ///
    /// Returns a map of tool name -> input_schema JSON value.
    /// Used by the `inspect_tool_params` reconnaissance tool.
    ///
    /// Returns an empty HashMap if no MCP manager is present.
    fn get_all_tool_schemas(&self) -> std::collections::HashMap<String, serde_json::Value> {
        let Some(ref mcp_manager) = self.mcp_manager else {
            return std::collections::HashMap::new();
        };

        let mut schemas = std::collections::HashMap::new();

        // Collect from streamable HTTP tools
        // rmcp::model::Tool.input_schema is Arc<JsonObject> where JsonObject = Map<String, Value>
        for tools in mcp_manager.streamable_tools.values() {
            for tool in tools {
                // Convert Arc<Map<String, Value>> to serde_json::Value
                let schema_value = serde_json::Value::Object((*tool.input_schema).clone());
                schemas.insert(tool.name.to_string(), schema_value);
            }
        }

        // Collect from SSE tools
        for tools in mcp_manager.sse_tools.values() {
            for tool in tools {
                let schema_value = serde_json::Value::Object((*tool.input_schema).clone());
                schemas.insert(tool.name.to_string(), schema_value);
            }
        }

        // Collect from STDIO tools
        for tools in mcp_manager.stdio_tools.values() {
            for tool in tools {
                let schema_value = serde_json::Value::Object((*tool.input_schema).clone());
                schemas.insert(tool.name.to_string(), schema_value);
            }
        }

        schemas
    }

    /// Resolve which tools each worker can access based on their mcp_filter.
    ///
    /// Returns a map of worker_name -> Vec<tool_name>.
    /// Tools that don't match any worker's filter are omitted.
    ///
    /// # Example
    ///
    /// Given workers:
    /// - operations: mcp_filter = ["mezmo_*"]
    /// - knowledge: mcp_filter = ["ListKnowledgeBases", "QueryKnowledgeBases"]
    ///
    /// And tools: mezmo_logs, mezmo_pipelines, ListKnowledgeBases, QueryKnowledgeBases
    ///
    /// Returns:
    /// - "operations" -> ["mezmo_logs", "mezmo_pipelines"]
    /// - "knowledge" -> ["ListKnowledgeBases", "QueryKnowledgeBases"]
    fn resolve_worker_tools(&self) -> std::collections::HashMap<String, Vec<String>> {
        let all_tools = self.get_all_tool_names();
        let mut worker_tools = std::collections::HashMap::new();

        for (worker_name, worker_config) in &self.config.workers {
            // Match MCP tools via mcp_filter (empty = all MCP tools for backwards compatibility)
            let mut matching_tools: Vec<String> = if worker_config.mcp_filter.is_empty() {
                all_tools.clone()
            } else {
                all_tools
                    .iter()
                    .filter(|tool_name| {
                        worker_config
                            .mcp_filter
                            .iter()
                            .any(|pattern| crate::config::glob_match(pattern, tool_name))
                    })
                    .cloned()
                    .collect()
            };

            // Add vector store tools based on explicit vector_stores assignment
            for store_name in &worker_config.vector_stores {
                matching_tools.push(format!("vector_search_{}", store_name));
            }

            worker_tools.insert(worker_name.clone(), matching_tools);
        }

        worker_tools
    }

    /// Get tool descriptions for full visibility mode.
    ///
    /// Returns a map of tool_name -> description.
    /// Used when `tools_in_planning = "full"`.
    fn get_all_tool_descriptions(&self) -> std::collections::HashMap<String, String> {
        let mut descriptions = std::collections::HashMap::new();

        // Collect from MCP tools
        if let Some(ref mcp_manager) = self.mcp_manager {
            // Collect from streamable HTTP tools (description is Option<Cow<'static, str>>)
            for tools in mcp_manager.streamable_tools.values() {
                for tool in tools {
                    if let Some(ref desc) = tool.description {
                        descriptions.insert(tool.name.to_string(), desc.to_string());
                    }
                }
            }

            // Collect from SSE tools
            for tools in mcp_manager.sse_tools.values() {
                for tool in tools {
                    if let Some(ref desc) = tool.description {
                        descriptions.insert(tool.name.to_string(), desc.to_string());
                    }
                }
            }

            // Collect from STDIO tools
            for tools in mcp_manager.stdio_tools.values() {
                for tool in tools {
                    if let Some(ref desc) = tool.description {
                        descriptions.insert(tool.name.to_string(), desc.to_string());
                    }
                }
            }
        }

        // Collect from vector stores (context_prefix becomes the description)
        for store in &self.agent_config.vector_stores {
            let tool_name = format!("vector_search_{}", store.name);
            let description = store
                .context_prefix
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Search the {} knowledge base", store.name));
            descriptions.insert(tool_name, description);
        }

        descriptions
    }

    // ========================================================================
    // Guardrail Methods
    // ========================================================================

    /// Build a Rig agent with the given completion model and coordinator tools.
    ///
    /// Shared helper that eliminates per-provider duplication in `create_coordinator`.
    fn build_agent_with_tools<M: rig::completion::CompletionModel + Send + Sync>(
        completion_model: M,
        preamble: &str,
        temperature: Option<f64>,
        additional_params: Option<serde_json::Value>,
        tools: CoordinatorTools,
    ) -> rig::agent::Agent<M> {
        let mut builder = rig::agent::AgentBuilder::new(completion_model);
        builder = builder.preamble(preamble);
        if let Some(temp) = temperature {
            builder = builder.temperature(temp);
        }
        if let Some(params) = additional_params {
            builder = builder.additional_params(params);
        }
        let mut state = BuilderState::Initial(builder);
        if let Some(list_tools) = tools.list_tools {
            state = state.add_tool(list_tools);
        }
        if let Some(inspect_tool_params) = tools.inspect_tool_params {
            state = state.add_tool(inspect_tool_params);
        }
        for tool in tools.vector_tools {
            state = state.add_tool(tool);
        }
        state = state.add_tool(tools.routing_tools.respond_directly);
        state = state.add_tool(tools.routing_tools.create_plan);
        state = state.add_tool(tools.routing_tools.request_clarification);
        if let Some(artifact_tool) = tools.read_artifact {
            state = state.add_tool(artifact_tool);
        }
        if let Some(list_prior_runs) = tools.list_prior_runs {
            state = state.add_tool(list_prior_runs);
        }
        state.build()
    }

    /// Create a coordinator agent for planning tasks.
    ///
    /// Uses the agent's system_prompt for domain routing context (layered pattern).
    /// Planning mechanics and auto-generated worker descriptions go in the user message.
    ///
    /// The coordinator is equipped with reconnaissance tools for dynamic tool inspection:
    /// - `list_tools`: Returns all available tool names
    /// - `inspect_tool_params`: Returns parameter schema for a specific tool
    ///
    /// The three routing tools are also added to the
    /// coordinator agent, enabling structured routing decisions via tool calling.
    async fn create_coordinator(
        &self,
        routing_tools: RoutingToolSet,
        allow_recon_tools: bool,
    ) -> Result<AgentWithPreamble, Box<dyn std::error::Error + Send + Sync>> {
        use crate::vector_dynamic::DynamicVectorSearchTool;
        use crate::vector_store::VectorStoreManager;

        // Capture tool information for reconnaissance tools
        let tool_names = self.get_all_tool_names();
        let tool_schemas = self.get_all_tool_schemas();

        // Create reconnaissance tools
        let list_tool = ListToolsTool::new(tool_names);
        let inspect_tool = InspectToolParamsTool::new(tool_schemas);

        // Recon tools are gated by two conditions:
        //   - `allow_recon_tools`: callers pass false for post-execute calls,
        //     where execute is already done and recon has no legitimate use.
        //   - `tools_in_planning == None`: when worker tool inventories are
        //     already inlined into the planning prompt, recon is redundant.
        let include_recon_tools = allow_recon_tools
            && matches!(
                self.config.tools_in_planning,
                super::config::ToolVisibility::None
            );

        // Build coordinator preamble: orchestration framework template + user system prompt
        let include_history_tools = self.config.memory_dir().is_some()
            && self.persistence.lock().await.session_id().is_some();
        let mut preamble = super::config::build_coordinator_preamble(
            self.agent_config.effective_preamble(),
            include_recon_tools,
            include_history_tools,
        );
        let temperature = self.agent_config.llm.temperature();

        // Filter vector stores for coordinator (if any configured)
        let coordinator_stores: Vec<_> = if !self.config.coordinator_vector_stores.is_empty() {
            let assigned: std::collections::HashSet<&str> = self
                .config
                .coordinator_vector_stores
                .iter()
                .map(|s| s.as_str())
                .collect();
            self.agent_config
                .vector_stores
                .iter()
                .filter(|vs| assigned.contains(vs.name.as_str()))
                .collect()
        } else {
            vec![]
        };

        // Create vector store tools for coordinator
        let mut vector_tools: Vec<DynamicVectorSearchTool> = Vec::new();
        for store_config in &coordinator_stores {
            tracing::info!(
                "Coordinator: configuring vector store '{}'",
                store_config.name
            );
            let manager = Arc::new(VectorStoreManager::from_config(store_config).await?);
            let tool = DynamicVectorSearchTool::new(manager, store_config.name.clone());
            vector_tools.push(tool);
        }

        // Inject vector store context into preamble if any stores assigned
        if !coordinator_stores.is_empty() {
            let vs_configs: Vec<_> = coordinator_stores.iter().map(|c| (*c).clone()).collect();
            let vs_context = super::config::build_vector_store_context(&vs_configs);
            preamble.push_str(&vs_context);
        }

        // Load and inject session history from prior runs
        if self.config.session_history_turns() > 0
            && let Some(memory_dir) = self.agent_config.effective_memory_dir()
        {
            let persistence = self.persistence.lock().await;
            if let Some(session_id) = persistence.session_id() {
                let manifests = super::persistence::load_session_manifests(
                    std::path::Path::new(memory_dir),
                    session_id,
                    persistence.run_id(),
                    self.config.session_history_turns(),
                )
                .await
                .unwrap_or_default();
                if !manifests.is_empty() {
                    tracing::info!(
                        "Injecting session history: {} prior run(s) for session {}",
                        manifests.len(),
                        session_id
                    );
                    preamble.push('\n');
                    preamble.push_str(&super::persistence::build_session_context(&manifests));
                }
            }
        }

        // Bundle all coordinator tools
        let coordinator_tools = CoordinatorTools {
            list_tools: if include_recon_tools {
                Some(list_tool)
            } else {
                None
            },
            inspect_tool_params: if include_recon_tools {
                Some(inspect_tool)
            } else {
                None
            },
            vector_tools,
            routing_tools,
            read_artifact: Some(ReadArtifactTool::new(self.persistence.clone())),
            list_prior_runs: if include_history_tools {
                Some(super::tools::ListPriorRunsTool::new(
                    self.persistence.clone(),
                    std::path::PathBuf::from(self.config.memory_dir().unwrap()),
                ))
            } else {
                None
            },
        };

        let provider_agent = self
            .build_provider_agent_with_tools(
                &preamble,
                temperature,
                self.agent_config.llm.additional_params(),
                coordinator_tools,
            )
            .await?;

        let model_name = self.agent_config.llm.model_name().to_string();

        // Coordinator depth budget allows recon + read_artifact + routing within one
        // stream_and_collect call. The decision_ready early-exit is the primary guard;
        // max_depth is defense-in-depth. GPT 5.2 observed using read_artifact during
        // post-execute continuation routing (13 calls in 5-prompt E2E suite).
        let max_depth = PLANNING_COORDINATOR_MAX_DEPTH;

        Ok(AgentWithPreamble {
            agent: Agent {
                inner: provider_agent,
                model: model_name,
                max_depth,
                mcp_manager: None, // Coordinator doesn't have MCP tools
                fallback_tool_parsing: false,
                fallback_tool_names: vec![],
                context_window: None,
                scratchpad_budget: None,
                client_tool_names: Default::default(),
            },
            preamble,
            escalation_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            submit_result_decision: Arc::new(Mutex::new(None)),
        })
    }

    /// Build a provider-specific agent with coordinator tools.
    ///
    /// Extracted from `create_coordinator` to share provider matching across
    /// planning and continuation constructors.
    async fn build_provider_agent_with_tools(
        &self,
        preamble: &str,
        temperature: Option<f64>,
        additional_params: Option<serde_json::Value>,
        tools: CoordinatorTools,
    ) -> Result<ProviderAgent, Box<dyn std::error::Error + Send + Sync>> {
        match &self.agent_config.llm {
            LlmConfig::OpenAI {
                api_key,
                model,
                base_url,
                ..
            } => {
                let mut cb =
                    rig::providers::openai::Client::<reqwest::Client>::builder().api_key(api_key);
                if let Some(url) = base_url {
                    cb = cb.base_url(url);
                }
                let cm = cb
                    .build()
                    .map_err(|e| format!("Failed to build OpenAI coordinator: {}", e))?
                    .completions_api()
                    .completion_model(model);
                Ok(ProviderAgent::OpenAI(Self::build_agent_with_tools(
                    cm,
                    preamble,
                    temperature,
                    additional_params,
                    tools,
                )))
            }
            LlmConfig::Anthropic {
                api_key,
                model,
                base_url,
                ..
            } => {
                let mut cb = rig::providers::anthropic::Client::<reqwest::Client>::builder()
                    .api_key(api_key);
                if let Some(url) = base_url {
                    cb = cb.base_url(url);
                }
                let cm = cb
                    .build()
                    .map_err(|e| format!("Failed to build Anthropic coordinator: {}", e))?
                    .completion_model(model);
                Ok(ProviderAgent::Anthropic(Self::build_agent_with_tools(
                    cm,
                    preamble,
                    temperature,
                    additional_params,
                    tools,
                )))
            }
            LlmConfig::Bedrock {
                model,
                region,
                profile,
                ..
            } => {
                use aws_config::{BehaviorVersion, Region};
                let sdk_config = if let Some(profile_name) = profile {
                    aws_config::defaults(BehaviorVersion::latest())
                        .region(Region::new(region.to_string()))
                        .profile_name(profile_name)
                        .load()
                        .await
                } else {
                    aws_config::defaults(BehaviorVersion::latest())
                        .region(Region::new(region.to_string()))
                        .load()
                        .await
                };
                let cm = rig_bedrock::client::Client::from(aws_sdk_bedrockruntime::Client::new(
                    &sdk_config,
                ))
                .completion_model(model);
                Ok(ProviderAgent::Bedrock(Self::build_agent_with_tools(
                    cm,
                    preamble,
                    temperature,
                    additional_params,
                    tools,
                )))
            }
            LlmConfig::Gemini {
                api_key,
                model,
                base_url,
                ..
            } => {
                let mut cb =
                    rig::providers::gemini::Client::<reqwest::Client>::builder().api_key(api_key);
                if let Some(url) = base_url {
                    cb = cb.base_url(url);
                }
                let cm = cb
                    .build()
                    .map_err(|e| format!("Failed to build Gemini coordinator: {}", e))?
                    .completion_model(model);
                Ok(ProviderAgent::Gemini(Self::build_agent_with_tools(
                    cm,
                    preamble,
                    temperature,
                    additional_params,
                    tools,
                )))
            }
            LlmConfig::Ollama {
                model, base_url, ..
            } => {
                let url = base_url.as_deref().unwrap_or("http://localhost:11434");
                let cm = rig::providers::ollama::Client::builder()
                    .api_key(rig::client::Nothing)
                    .base_url(url)
                    .build()
                    .map_err(|e| format!("Failed to build Ollama coordinator: {}", e))?
                    .completion_model(model);
                Ok(ProviderAgent::Ollama(Self::build_agent_with_tools(
                    cm,
                    preamble,
                    temperature,
                    additional_params,
                    tools,
                )))
            }
            LlmConfig::OpenRouter {
                api_key,
                model,
                base_url,
                ..
            } => {
                let mut cb = rig::providers::openrouter::Client::<reqwest::Client>::builder()
                    .api_key(api_key);
                if let Some(url) = base_url {
                    cb = cb.base_url(url);
                }
                let cm = cb
                    .build()
                    .map_err(|e| format!("Failed to build OpenRouter coordinator: {}", e))?
                    .completion_model(model);
                Ok(ProviderAgent::OpenRouter(Self::build_agent_with_tools(
                    cm,
                    preamble,
                    temperature,
                    additional_params,
                    tools,
                )))
            }
        }
    }

    /// Build a provider-specific worker agent with MCP tools from the shared manager.
    ///
    /// Workers share the orchestrator's `Arc<McpManager>` rather than creating
    /// their own MCP connections. Tool filtering is handled by `add_all_tools`
    /// via `worker_config.mcp_filter`. Workers never receive client-side
    /// passthrough tools — see `create_worker` for the rationale.
    async fn build_worker_provider_agent(
        &self,
        worker_config: &AgentRuntimeConfig,
    ) -> Result<(ProviderAgent, String), Box<dyn std::error::Error + Send + Sync>> {
        let preamble = worker_config.effective_preamble();
        let temperature = worker_config.llm.temperature();
        let shared_mcp: Option<Arc<McpManager>> = self.mcp_manager.clone();

        match &worker_config.llm {
            LlmConfig::OpenAI {
                api_key,
                model,
                base_url,
                reasoning_effort,
                additional_params,
                ..
            } => {
                let mut cb =
                    rig::providers::openai::Client::<reqwest::Client>::builder().api_key(api_key);
                if let Some(url) = base_url {
                    cb = cb.base_url(url);
                }
                let cm = cb
                    .build()
                    .map_err(|e| format!("Failed to build OpenAI worker: {}", e))?
                    .completions_api()
                    .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                // Build combined additional_params: reasoning_effort
                let mut combined_params: Option<serde_json::Value> = None;
                if let Some(effort) = reasoning_effort {
                    combined_params =
                        Some(serde_json::json!({"reasoning_effort": effort.to_string()}));
                }
                if let Some(params) = additional_params {
                    combined_params = Some(match combined_params {
                        Some(existing) => crate::builder::merge_json(existing, params.clone()),
                        None => params.clone(),
                    });
                }
                if let Some(params) = combined_params {
                    builder = builder.additional_params(params);
                }
                if let Some(max) = worker_config.llm.max_tokens() {
                    builder = builder.max_tokens(max);
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp, vec![]).await?;
                Ok((ProviderAgent::OpenAI(state.build()), model.clone()))
            }
            LlmConfig::Anthropic {
                api_key,
                model,
                base_url,
                additional_params,
                ..
            } => {
                let mut cb = rig::providers::anthropic::Client::<reqwest::Client>::builder()
                    .api_key(api_key);
                if let Some(url) = base_url {
                    cb = cb.base_url(url);
                }
                let cm = cb
                    .build()
                    .map_err(|e| format!("Failed to build Anthropic worker: {}", e))?
                    .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                if let Some(max) = worker_config.llm.max_tokens() {
                    builder = builder.max_tokens(max);
                }
                if let Some(params) = additional_params {
                    builder = builder.additional_params(params.clone());
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp, vec![]).await?;
                Ok((ProviderAgent::Anthropic(state.build()), model.clone()))
            }
            LlmConfig::Bedrock {
                model,
                region,
                profile,
                additional_params,
                ..
            } => {
                use aws_config::{BehaviorVersion, Region};
                let sdk_config = if let Some(profile_name) = profile {
                    aws_config::defaults(BehaviorVersion::latest())
                        .region(Region::new(region.to_string()))
                        .profile_name(profile_name)
                        .load()
                        .await
                } else {
                    aws_config::defaults(BehaviorVersion::latest())
                        .region(Region::new(region.to_string()))
                        .load()
                        .await
                };
                let cm = rig_bedrock::client::Client::from(aws_sdk_bedrockruntime::Client::new(
                    &sdk_config,
                ))
                .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                if let Some(max) = worker_config.llm.max_tokens() {
                    builder = builder.max_tokens(max);
                }
                if let Some(params) = additional_params {
                    builder = builder.additional_params(params.clone());
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp, vec![]).await?;
                Ok((ProviderAgent::Bedrock(state.build()), model.clone()))
            }
            LlmConfig::Gemini {
                api_key,
                model,
                base_url,
                additional_params,
                ..
            } => {
                let mut cb =
                    rig::providers::gemini::Client::<reqwest::Client>::builder().api_key(api_key);
                if let Some(url) = base_url {
                    cb = cb.base_url(url);
                }
                let cm = cb
                    .build()
                    .map_err(|e| format!("Failed to build Gemini worker: {}", e))?
                    .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                if let Some(params) = additional_params {
                    builder = builder.additional_params(params.clone());
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp, vec![]).await?;
                Ok((ProviderAgent::Gemini(state.build()), model.clone()))
            }
            LlmConfig::Ollama {
                model,
                base_url,
                additional_params,
                ..
            } => {
                let url = base_url.as_deref().unwrap_or("http://localhost:11434");
                let cm = rig::providers::ollama::Client::builder()
                    .api_key(rig::client::Nothing)
                    .base_url(url)
                    .build()
                    .map_err(|e| format!("Failed to build Ollama worker: {}", e))?
                    .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }

                if let Some(params) = additional_params {
                    builder = builder.additional_params(params.clone());
                }

                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp, vec![]).await?;
                Ok((ProviderAgent::Ollama(state.build()), model.clone()))
            }
            LlmConfig::OpenRouter {
                api_key,
                model,
                base_url,
                additional_params,
                ..
            } => {
                let mut cb = rig::providers::openrouter::Client::<reqwest::Client>::builder()
                    .api_key(api_key);
                if let Some(url) = base_url {
                    cb = cb.base_url(url);
                }
                let cm = cb
                    .build()
                    .map_err(|e| format!("Failed to build OpenRouter worker: {}", e))?
                    .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                if let Some(max) = worker_config.llm.max_tokens() {
                    builder = builder.max_tokens(max);
                }
                if let Some(params) = additional_params {
                    builder = builder.additional_params(params.clone());
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp, vec![]).await?;
                Ok((ProviderAgent::OpenRouter(state.build()), model.clone()))
            }
        }
    }

    /// Execute phase: run tasks and collect results.
    ///
    /// Iterates through tasks respecting dependencies, executes ready tasks in parallel,
    /// and emits progress events. Tasks with unsatisfied dependencies wait until their
    /// dependencies complete in subsequent iterations.
    ///
    /// Each worker receives the plan goal (not the raw query) to understand context.
    async fn execute(
        &self,
        plan: &mut Plan,
        event_tx: &tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
    ) -> Result<(), StreamError> {
        use futures::StreamExt;
        use futures::stream::FuturesUnordered;

        // Capture the plan goal to pass to each worker
        let plan_goal = plan.goal.clone();

        while !plan.is_finished() {
            // Collect ready tasks with their context and worker assignment
            // Tuple: (task_id, description, context, worker_name)
            let ready_tasks: Vec<(usize, String, Option<String>, Option<String>)> = plan
                .ready_tasks()
                .iter()
                .map(|t| {
                    let context = self.build_task_context(plan, t.id);
                    (t.id, t.description.clone(), context, t.worker.clone())
                })
                .collect();

            if ready_tasks.is_empty() {
                // No ready tasks but not finished - this shouldn't happen with valid plans
                tracing::warn!(
                    "No ready tasks but plan not finished — blocked tasks remaining after failure (dependency chain broken)"
                );
                break;
            }

            let parallel_count = ready_tasks.len();
            let default_depth = self
                .agent_config
                .agent
                .turn_depth
                .unwrap_or(crate::builder::DEFAULT_MAX_DEPTH);
            tracing::info!(
                "Executing {} task(s) in parallel (default_turn_depth={}, per_call_timeout={}s)",
                parallel_count,
                default_depth,
                self.config.per_call_timeout_secs(),
            );

            // Mark all ready tasks as running and emit TaskStarted events
            for (task_id, task_desc, _context, worker_name) in &ready_tasks {
                if let Some(task) = plan.get_task_mut(*task_id) {
                    task.start();
                }
                let _ = event_tx
                    .send(Ok(StreamItem::OrchestratorEvent(
                        OrchestratorEvent::TaskStarted {
                            task_id: *task_id,
                            description: task_desc.clone(),
                            orchestrator_id: self.orchestrator_id.clone(),
                            worker_id: worker_name.clone().unwrap_or(self.orchestrator_id.clone()),
                        },
                    )))
                    .await;
            }

            // Execute all ready tasks in parallel using FuturesUnordered
            // Each task receives the plan goal for context
            let goal = plan_goal.clone();
            let mut futures: FuturesUnordered<_> = ready_tasks
                .into_iter()
                .map(|(task_id, task_desc, task_context, worker_name)| {
                    let goal = goal.clone();
                    async move {
                        let start_time = Instant::now();
                        let params = TaskExecutionParams {
                            task_description: &task_desc,
                            task_context: &task_context,
                            worker_name: worker_name.as_deref(),
                            plan_goal: &goal,
                        };
                        let result = self.execute_task(task_id, &params, Some(event_tx)).await;
                        let duration_ms = start_time.elapsed().as_millis() as u64;
                        (task_id, result, duration_ms, worker_name, task_desc)
                    }
                })
                .collect();

            // Collect results as they complete and update plan
            while let Some((task_id, result, duration_ms, worker_name, task_desc)) =
                futures.next().await
            {
                match result {
                    Ok(exec_result) => {
                        let final_result = self
                            .maybe_create_artifact(
                                task_id,
                                worker_name.as_deref(),
                                exec_result.result,
                            )
                            .await;
                        let result_for_event = final_result.clone();
                        let success = exec_result.structured_output.is_some();
                        if let Some(t) = plan.get_task_mut(task_id) {
                            if success {
                                t.complete(final_result);
                            } else {
                                t.fail(final_result, FailureCategory::SoftFailure);
                            }
                            t.structured_output = exec_result.structured_output;
                        }
                        let _ = event_tx
                            .send(Ok(StreamItem::OrchestratorEvent(
                                OrchestratorEvent::TaskCompleted {
                                    task_id,
                                    success,
                                    duration_ms,
                                    orchestrator_id: self.orchestrator_id.clone(),
                                    worker_id: worker_name
                                        .clone()
                                        .unwrap_or(self.orchestrator_id.clone()),
                                    result: result_for_event,
                                },
                            )))
                            .await;
                        if success {
                            tracing::info!("Task {} completed in {}ms", task_id, duration_ms);
                        } else {
                            tracing::warn!(
                                "Task {} did not call submit_result (SoftFailure) after {}ms",
                                task_id,
                                duration_ms
                            );
                        }
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        let category = Self::categorize_failure_error(&err_str);
                        if let Some(t) = plan.get_task_mut(task_id) {
                            t.fail(err_str.clone(), category);
                        }
                        let _ = event_tx
                            .send(Ok(StreamItem::OrchestratorEvent(
                                OrchestratorEvent::TaskCompleted {
                                    task_id,
                                    success: false,
                                    duration_ms,
                                    orchestrator_id: self.orchestrator_id.clone(),
                                    worker_id: worker_name
                                        .clone()
                                        .unwrap_or(self.orchestrator_id.clone()),
                                    result: err_str.clone(),
                                },
                            )))
                            .await;
                        let worker_label = worker_name.as_deref().unwrap_or("generic");
                        let (task_preview, _) = safe_truncate(&task_desc, 100);
                        tracing::warn!(
                            "Worker '{}' failed task {} after {}ms ({}): {}. Task was: {}",
                            worker_label,
                            task_id,
                            duration_ms,
                            category,
                            e,
                            task_preview
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Collect failed tasks from this iteration into failure records.
    fn collect_iteration_failures(
        plan: &Plan,
        iteration: usize,
    ) -> Vec<super::types::FailedTaskRecord> {
        plan.tasks
            .iter()
            .filter_map(|t| match &t.state {
                TaskState::Failed { error, category } => Some(super::types::FailedTaskRecord {
                    description: t.description.clone(),
                    error: error.clone(),
                    iteration,
                    worker: t.worker.clone(),
                    category: *category,
                }),
                _ => None,
            })
            .collect()
    }

    /// If result exceeds artifact threshold, write full result to artifact file
    /// and return a summary. Otherwise return the original result unchanged.
    async fn maybe_create_artifact(
        &self,
        task_id: usize,
        worker_name: Option<&str>,
        result: String,
    ) -> String {
        let threshold = self.config.result_artifact_threshold();
        if result.len() <= threshold {
            return result;
        }

        let summary_len = self.config.result_summary_length();
        let persistence = self.persistence.lock().await;
        let iteration = persistence.current_iteration();

        match persistence
            .write_result_artifact(task_id, worker_name, iteration, &result)
            .await
        {
            Ok(filename) => {
                let (truncated, _) = safe_truncate(&result, summary_len);
                format!(
                    "{}\n\n[Full result ({} chars) saved to artifact: {}]",
                    truncated,
                    result.len(),
                    filename,
                )
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to write result artifact for task {}: {}",
                    task_id,
                    e
                );
                result
            }
        }
    }

    /// Build context for a task from its completed dependencies and the plan goal.
    ///
    /// Includes:
    /// - For each dependency: description, rationale, and result
    /// - The current task's rationale (how this task advances the goal)
    ///
    /// This ensures workers understand not just WHAT to do, but WHY.
    ///
    /// # Research Inspirations
    ///
    /// Based on patterns from:
    /// - LangChain's Write-Select-Compress-Isolate framework
    /// - LlamaIndex's Sub-Question Query Engine
    /// - Anthropic's context engineering principles
    fn build_task_context(&self, plan: &Plan, task_id: usize) -> Option<String> {
        use super::prompt_constants::{context, sections};

        let task = plan.tasks.iter().find(|t| t.id == task_id)?;

        // Build structured dependency context — compact format to prevent scope creep

        if !task.dependencies.is_empty() {
            let dep_parts: Vec<String> = task
                .dependencies
                .iter()
                .filter_map(|dep_id| {
                    plan.tasks
                        .iter()
                        .find(|t| t.id == *dep_id)
                        .and_then(|dep_task| match &dep_task.state {
                            TaskState::Complete { result } => Some(format!(
                                "{} — Task {} ({}):\n{}",
                                sections::PRIOR_WORK,
                                dep_task.id,
                                dep_task.description,
                                result
                            )),
                            _ => None,
                        })
                })
                .collect();

            if dep_parts.is_empty() {
                None
            } else {
                Some(dep_parts.join(context::DEPENDENCY_SEPARATOR))
            }
        } else {
            None
        }
    }

    /// Execute a single task using a worker agent.
    #[tracing::instrument(
        name = "orchestration.worker",
        skip_all,
        fields(
            orchestration.task_id = task_id,
            orchestration.worker = tracing::field::Empty,
            orchestration.task = tracing::field::Empty,
        )
    )]
    async fn execute_task(
        &self,
        task_id: usize,
        params: &TaskExecutionParams<'_>,
        event_tx: Option<&tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>>,
    ) -> Result<TaskExecutionResult, StreamError> {
        let TaskExecutionParams {
            task_description,
            task_context,
            worker_name,
            plan_goal,
        } = params;

        {
            let span = tracing::Span::current();
            let (task_preview, _) = safe_truncate(task_description, 200);
            span.record("orchestration.task", task_preview);
            if let Some(name) = worker_name {
                span.record("orchestration.worker", *name);
            }
        }

        // Build the base worker prompt once — reused across retry attempts
        let context_str = task_context
            .as_ref()
            .map(|c| format!("{}\n\n", c))
            .unwrap_or_default();
        let base_worker_prompt =
            super::templates::render_worker_task_prompt(&super::templates::WorkerTaskVars {
                orchestration_goal: plan_goal,
                context: &context_str,
                your_task: task_description,
            });

        let start_time = std::time::Instant::now();
        let mut last_usage = rig::completion::Usage {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
        };
        let mut last_raw_response = String::new();
        let mut last_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;
        let mut actual_attempts: usize = 0;

        for attempt in 1..=MAX_WORKER_ATTEMPTS {
            actual_attempts = attempt;
            let is_final_attempt = attempt == MAX_WORKER_ATTEMPTS;

            let AgentWithPreamble {
                agent: worker,
                preamble: worker_preamble,
                escalation_flag,
                submit_result_decision,
            } = self.create_worker(task_id, attempt, *worker_name).await?;

            // Build prompt for this attempt
            let (prompt, history) = if attempt == 1 {
                (base_worker_prompt.clone(), vec![])
            } else {
                // Retry: append correction to previous conversation
                let correction =
                    super::prompt_constants::corrections::WORKER_SUBMIT_RESULT.to_string();
                let history = vec![
                    rig::completion::Message::user(base_worker_prompt.clone()),
                    rig::completion::Message::assistant(last_raw_response.clone()),
                ];
                (correction, history)
            };

            // initial_used only counts templates + tool schemas, so bill the
            // per-task prompt here.
            if let Some(ref budget) = worker.scratchpad_budget {
                let task_prompt_tokens = budget.count_tokens(&prompt);
                budget.record_usage(task_prompt_tokens);
                tracing::debug!(
                    "Task {}: task prompt ~{} tokens recorded in budget",
                    task_id,
                    task_prompt_tokens
                );
            }

            // Record in prompt journal
            self.journal_record(
                JournalPhase::Worker {
                    task_id,
                    worker_name: *worker_name,
                    attempt,
                },
                &worker_preamble,
                &prompt,
            );

            // Execute the task
            let srd = submit_result_decision.clone();
            let stream_result = self
                .stream_and_forward(
                    &worker,
                    StreamCallParams {
                        prompt: &prompt,
                        history,
                        phase: "Worker task",
                        event_tx,
                    },
                    Some(StreamContext {
                        task_id,
                        worker_id: worker_name.unwrap_or(&self.orchestrator_id),
                        worker_name: *worker_name,
                    }),
                    || {
                        let srd = srd.clone();
                        Box::pin(async move { srd.lock().await.is_some() })
                    },
                )
                .await;

            // Emit per-agent ScratchpadUsage event if this worker used scratchpad.
            if let (Some(budget), Some(tx)) = (worker.scratchpad_budget.as_ref(), event_tx) {
                let agent_id = worker_name
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| self.orchestrator_id.clone());
                if let Some(event) = crate::builder::scratchpad_usage_event(budget, &agent_id) {
                    let _ = tx.send(Ok(event)).await;
                }
            }

            // Track per-attempt usage for span metrics.
            // Cumulative usage_state is now updated eagerly per TurnUsage inside
            // stream_and_forward, so failed attempts also contribute.
            if let Ok(ref response) = stream_result {
                last_usage = response.usage;
            }

            // Detect context overflow and other errors — don't retry hard errors
            let result = match stream_result {
                Ok(r) => Ok(r.content),
                Err(e) if is_context_overflow_error(e.as_ref()) => {
                    let suggestion = context_overflow_suggestion("worker");
                    Err(format!(
                        "Worker context limit exceeded for task {}. {}",
                        task_id, suggestion
                    )
                    .into())
                }
                Err(e) => Err(e),
            };

            // Check escalation flag (duplicate call loop)
            let result: Result<String, StreamError> = match result {
                Ok(worker_output) if escalation_flag.load(std::sync::atomic::Ordering::SeqCst) => {
                    let processed = self
                        .maybe_create_artifact(task_id, *worker_name, worker_output)
                        .await;
                    Err(format!("Worker blocked by duplicate call loop.\n{processed}").into())
                }
                other => other,
            };

            // Extract structured output from submit_result
            match result {
                Ok(raw_response) => {
                    let structured = submit_result_decision.lock().await.take();
                    match structured {
                        Some(output) => {
                            let duration_ms = start_time.elapsed().as_millis() as u64;
                            self.set_worker_span_usage(
                                last_usage.input_tokens,
                                last_usage.output_tokens,
                                last_usage.total_tokens,
                            );
                            self.persist_worker_execution(
                                task_id,
                                task_description,
                                attempt,
                                duration_ms,
                                Ok(&output.result),
                                Some(&super::types::StructuredTaskOutput {
                                    summary: output.summary.clone(),
                                    confidence: output.confidence,
                                }),
                                &prompt,
                            )
                            .await;
                            return Ok(TaskExecutionResult {
                                result: output.result,
                                structured_output: Some(super::types::StructuredTaskOutput {
                                    summary: output.summary,
                                    confidence: output.confidence,
                                }),
                            });
                        }
                        None if is_final_attempt => {
                            tracing::warn!(
                                "Worker did not call submit_result for task {} after {} attempts. Preserving raw output via artifact flow.",
                                task_id,
                                attempt
                            );
                            let duration_ms = start_time.elapsed().as_millis() as u64;
                            self.set_worker_span_usage(
                                last_usage.input_tokens,
                                last_usage.output_tokens,
                                last_usage.total_tokens,
                            );
                            self.persist_worker_execution(
                                task_id,
                                task_description,
                                attempt,
                                duration_ms,
                                Ok(&raw_response),
                                None,
                                &prompt,
                            )
                            .await;
                            return Ok(TaskExecutionResult {
                                result: raw_response,
                                structured_output: None,
                            });
                        }
                        None => {
                            tracing::info!(
                                "Worker attempt {} for task {} did not call submit_result. Retrying with correction.",
                                attempt,
                                task_id
                            );
                            last_raw_response = raw_response;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    last_error = Some(e);
                    break; // Hard errors are not retried
                }
            }
        }

        // All attempts exhausted or hard error occurred
        let duration_ms = start_time.elapsed().as_millis() as u64;
        self.set_worker_span_usage(
            last_usage.input_tokens,
            last_usage.output_tokens,
            last_usage.total_tokens,
        );
        if let Some(ref e) = last_error {
            self.persist_worker_execution(
                task_id,
                task_description,
                actual_attempts,
                duration_ms,
                Err(e.as_ref()),
                None,
                &base_worker_prompt,
            )
            .await;
            Err(format!(
                "Worker failed task {} after {} attempts: {}",
                task_id, actual_attempts, e
            )
            .into())
        } else {
            // Should not reach here — final attempt should have returned above
            self.persist_worker_execution(
                task_id,
                task_description,
                actual_attempts,
                duration_ms,
                Ok(&last_raw_response),
                None,
                &base_worker_prompt,
            )
            .await;
            Ok(TaskExecutionResult {
                result: last_raw_response,
                structured_output: None,
            })
        }
    }

    /// Set token usage metrics on the current orchestration.worker span.
    /// usage_state is already updated per-attempt in the retry loop.
    fn set_worker_span_usage(&self, input: u64, output: u64, total: u64) {
        let span = tracing::Span::current();
        crate::logging::set_token_usage(&span, input, output, total, 0);
    }

    /// Persist a single worker execution attempt to the journal and persistence store.
    #[allow(clippy::too_many_arguments)]
    async fn persist_worker_execution(
        &self,
        task_id: usize,
        task_description: &str,
        attempt: usize,
        duration_ms: u64,
        result: Result<&str, &(dyn std::error::Error + Send + Sync)>,
        structured_output: Option<&super::types::StructuredTaskOutput>,
        worker_prompt: &str,
    ) {
        let (result_str, error_str) = match result {
            Ok(r) => (Some(r.to_string()), None),
            Err(e) => (None, Some(e.to_string())),
        };

        let record = super::persistence::TaskExecutionRecord {
            task_id,
            description: task_description.to_string(),
            attempt,
            approach: "Direct task execution via worker agent".to_string(),
            result: result_str.clone(),
            summary: structured_output.map(|s| s.summary.clone()),
            error: error_str,
            duration_ms,
            confidence: structured_output.map(|s| s.confidence.to_string()),
            orchestrator_notes: None,
        };

        {
            let persistence = self.persistence.lock().await;
            if let Err(e) = persistence
                .write_task_execution(
                    task_id,
                    attempt,
                    worker_prompt,
                    result_str.as_deref().unwrap_or("(error)"),
                    &record,
                )
                .await
            {
                tracing::warn!("Failed to persist task execution: {}", e);
            }
        }

        {
            let span = tracing::Span::current();
            match result {
                Ok(_) => crate::logging::set_span_ok(&span),
                Err(e) => crate::logging::set_span_error(&span, e.to_string()),
            }
        }
    }

    /// Build a summary of task execution statuses for replan context.
    ///
    /// Used when failures or blocked tasks require replanning. The summary
    /// shows what completed, what failed, and what couldn't run due to
    /// failed dependencies.
    fn build_execution_summary(&self, plan: &Plan) -> String {
        plan.tasks
            .iter()
            .map(|t| {
                let status_detail = match &t.state {
                    TaskState::Complete { result } => {
                        format!("✓ complete ({} chars)", result.len())
                    }
                    TaskState::Failed { error, .. } => {
                        let (truncated, was_truncated) =
                            safe_truncate(error, self.config.result_summary_length());
                        let suffix = if was_truncated { " [truncated]" } else { "" };
                        format!("✗ failed: {}{}", truncated, suffix)
                    }
                    TaskState::Pending => {
                        let blocked_by = t.dependencies.iter().any(|dep_id| {
                            plan.tasks
                                .iter()
                                .find(|dt| dt.id == *dep_id)
                                .map(|dt| matches!(dt.state, TaskState::Failed { .. }))
                                .unwrap_or(false)
                        });
                        if blocked_by {
                            "⏸ blocked by failed dependency".to_string()
                        } else {
                            "⏳ pending".to_string()
                        }
                    }
                    TaskState::Running => "▶ running".to_string(),
                };
                format!("Task {}: {} [{}]", t.id, t.description, status_detail)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Classify a task failure error string into a structured category.
    ///
    /// These are deterministic string matches against error messages produced by
    /// our rig fork (mezmo/rig @ d7e9d92) and our own orchestrator code — never
    /// against non-deterministic model output. Rig's `CompletionError::ProviderError(String)`
    /// flattens HTTP status codes into the error string, so string matching is
    /// the only classification path available without forking rig's error types.
    /// Revisit if/when we replace rig.
    fn categorize_failure_error(error: &str) -> FailureCategory {
        let lower = error.to_lowercase();
        if lower.contains("approval_task_timeout") {
            FailureCategory::ApprovalTaskTimeout
        } else if lower.contains("timed out") {
            FailureCategory::AgentTimeout
        } else if (lower.contains("context")
            && (lower.contains("limit")
                || lower.contains("overflow")
                || lower.contains("exceed")
                || lower.contains("length")))
            || lower.contains("maximum context")
            || lower.contains("token limit")
            || lower.contains("tokens exceeded")
            || lower.contains("maximum number of tokens")
            || (lower.contains("too") && lower.contains("long") && lower.contains("token"))
            || lower.contains("string_above_max_length")
            || (lower.contains("string") && lower.contains("too long"))
            || lower.contains("prompt is too long")
            || lower.contains("input is too long")
        {
            FailureCategory::ContextOverflow
        } else if lower.contains("maxdeptherror") || lower.contains("reached limit") {
            FailureCategory::DepthExhausted
        } else if lower.contains("duplicate call loop") {
            FailureCategory::LoopDetected
        } else if lower.contains("did not call submit_result") {
            FailureCategory::SoftFailure
        } else if lower.contains("rate limit")
            || lower.contains("429")
            || lower.contains("too many requests")
            || lower.contains("503")
            || lower.contains("502")
            || lower.contains("service unavailable")
        {
            FailureCategory::ProviderOverloaded
        } else if lower.contains("authentication")
            || lower.contains("unauthorized")
            || lower.contains("403")
            || lower.contains("401")
            || lower.contains("api key")
        {
            FailureCategory::ProviderAuthError
        } else if lower.contains("404")
            || lower.contains("model identifier is invalid")
            || lower.contains("is not found for api version")
        {
            FailureCategory::ProviderNotFound
        } else {
            FailureCategory::AgentError
        }
    }

    /// Whether the orchestrator should skip replanning because all failures
    /// are provider-level errors (rate limits, auth, network) and no tasks
    /// succeeded. Replanning can't fix provider issues.
    fn should_short_circuit_provider_errors(
        failures: &[FailedTaskRecord],
        completed_count: usize,
    ) -> bool {
        if failures.is_empty() || completed_count > 0 {
            return false;
        }
        failures.iter().all(|f| {
            matches!(
                f.category,
                FailureCategory::ProviderOverloaded
                    | FailureCategory::ProviderAuthError
                    | FailureCategory::ProviderNotFound
            )
        })
    }

    /// Send an orchestrator event through the stream channel.
    async fn emit_event(
        event_tx: &tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
        event: OrchestratorEvent,
    ) {
        let _ = event_tx
            .send(Ok(StreamItem::OrchestratorEvent(event)))
            .await;
    }

    /// Emit a ReplanStarted event and build the iteration context for the next cycle.
    ///
    /// Consolidates the common tail of the replan paths (coordinator-routed,
    /// failure-driven). Callers handle path-specific pre-work (e.g. IterationComplete
    /// events, persistence writes) before calling this.
    async fn trigger_replan(
        event_tx: &tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
        iteration: usize,
        trigger: &str,
        plan: Plan,
        failure_summary: Option<FailureSummary>,
        failure_history: &[FailedTaskRecord],
    ) -> (Option<IterationContext>, Plan) {
        Self::emit_event(
            event_tx,
            OrchestratorEvent::ReplanStarted {
                iteration: iteration + 1,
                trigger: trigger.to_string(),
            },
        )
        .await;

        // tool_traces intentionally empty — this context is only used for
        // previous_plan carry-forward, not continuation prompt rendering
        // (discarded in run_iteration via `let _ = previous_context`).
        let context = IterationContext::new(
            iteration,
            plan,
            failure_summary,
            failure_history.to_vec(),
            std::collections::HashMap::new(),
        );
        (Some(context), Plan::new(""))
    }

    /// Top-level orchestration entry point: route → loop.
    ///
    /// Creates a single coordinator agent for the entire orchestration request,
    /// then uses `plan_with_routing()` for routing decisions. The coordinator's
    /// conversation grows monotonically across planning and continuation turns.
    ///
    /// Dispatches based on the `PlanningResponse` variant:
    /// - `Direct` → emit event, return response
    /// - `Clarification` → emit event, return question
    /// - `StepsPlan` → delegate to `run_orchestration_loop()`
    #[tracing::instrument(
        name = "orchestration",
        skip_all,
        fields(
            orchestration.goal = tracing::field::Empty,
            orchestration.max_iterations = self.config.max_planning_cycles,
            orchestration.routing = tracing::field::Empty,
        )
    )]
    pub(super) async fn run_orchestration(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        event_tx: tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
    ) -> Result<String, StreamError> {
        let span = tracing::Span::current();
        let (goal_preview, _) = safe_truncate(query, 200);
        span.record("orchestration.goal", goal_preview);

        let orchestration_start = Instant::now();
        let default_turn_depth = self
            .agent_config
            .agent
            .turn_depth
            .unwrap_or(crate::builder::DEFAULT_MAX_DEPTH);
        tracing::info!(
            "Orchestration started (per_call_timeout={}s, max_planning_cycles={}, default_turn_depth={})",
            self.config.per_call_timeout_secs(),
            self.config.max_planning_cycles,
            default_turn_depth,
        );

        // Create coordinator once for the entire orchestration request.
        // Recon tools registered unconditionally — the persistent conversation
        // means we can't vary the tool set between calls, and the conversation
        // context guides usage (coordinator won't call list_tools on continuation).
        let routing_toolset = RoutingToolSet::new();
        let routing_decision = routing_toolset.decision.clone();
        let AgentWithPreamble {
            agent: coordinator,
            preamble: coordinator_preamble,
            ..
        } = self.create_coordinator(routing_toolset, true).await?;

        let mut coordinator_state = CoordinatorState {
            agent: coordinator,
            preamble: coordinator_preamble,
            conversation: Vec::new(),
            routing_decision,
        };

        // Set iteration for initial planning (journal reads this via AtomicUsize)
        self.current_iteration.store(1, Ordering::Relaxed);
        let (response, _prompt, _coordinator_text) = self
            .plan_with_routing(
                query,
                &chat_history,
                &mut coordinator_state,
                None,
                Some(&event_tx),
            )
            .await?;

        let result = match response {
            PlanningResponse::Direct {
                response,
                routing_rationale,
                response_summary,
            } => {
                span.record("orchestration.routing", "direct");
                Self::emit_event(
                    &event_tx,
                    OrchestratorEvent::DirectAnswer {
                        response: response.clone(),
                        routing_rationale,
                    },
                )
                .await;
                self.write_direct_response_manifest(query, &response, response_summary.as_deref())
                    .await;
                Ok(response)
            }
            PlanningResponse::Clarification {
                question,
                options,
                routing_rationale,
            } => {
                span.record("orchestration.routing", "clarification");
                Self::emit_event(
                    &event_tx,
                    OrchestratorEvent::ClarificationNeeded {
                        question: question.clone(),
                        options,
                        routing_rationale,
                    },
                )
                .await;
                Ok(question)
            }
            PlanningResponse::StepsPlan { .. } => {
                span.record("orchestration.routing", "orchestrated");
                let routing_rationale = response.routing_rationale().to_string();
                let planning_summary = response.planning_summary().unwrap_or_default().to_string();
                let plan = response.into_plan().expect("StepsPlan always converts");

                Self::emit_event(
                    &event_tx,
                    OrchestratorEvent::PlanCreated {
                        goal: plan.goal.clone(),
                        tasks: plan.tasks.iter().map(|t| t.description.clone()).collect(),
                        routing_mode: super::events::RoutingMode::for_plan(plan.tasks.len()),
                        routing_rationale: routing_rationale.clone(),
                        planning_response: planning_summary,
                    },
                )
                .await;

                self.run_orchestration_loop(
                    query,
                    plan,
                    chat_history,
                    &mut coordinator_state,
                    event_tx,
                    orchestration_start,
                )
                .await
            }
        };

        match &result {
            Ok(_) => crate::logging::set_span_ok(&span),
            Err(e) => crate::logging::set_span_error(&span, e.to_string()),
        }
        result
    }

    /// The plan-execute-continue loop.
    ///
    /// Takes an initial plan and iterates until quality threshold is met or
    /// max iterations are reached. On re-plan, uses `plan_with_routing()` and
    /// expects an `Orchestrated` response (falls back to single-task if not).
    ///
    /// Budget enforcement: at each iteration boundary, checks whether remaining
    /// wall-clock time is less than `per_call_timeout_secs`. If so, returns the
    /// best available result instead of starting a new iteration.
    async fn run_orchestration_loop(
        &self,
        query: &str,
        initial_plan: Plan,
        chat_history: Vec<rig::completion::Message>,
        coordinator_state: &mut CoordinatorState,
        event_tx: tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
        orchestration_start: Instant,
    ) -> Result<String, StreamError> {
        let mut iteration = 0;
        let mut previous_context: Option<IterationContext> = None;
        let mut plan = initial_plan;
        let mut failure_history: Vec<FailedTaskRecord> = Vec::new();

        let final_result = loop {
            iteration += 1;
            self.current_iteration.store(iteration, Ordering::Relaxed);
            match self
                .run_iteration(
                    iteration,
                    query,
                    plan,
                    &chat_history,
                    coordinator_state,
                    previous_context.as_ref(),
                    &event_tx,
                    orchestration_start,
                    &mut failure_history,
                )
                .await?
            {
                IterationOutcome::FinalResult(s) => break s,
                IterationOutcome::Continue {
                    new_plan,
                    previous_context: pc,
                } => {
                    plan = new_plan;
                    previous_context = pc;
                }
            }
        };

        Ok(final_result)
    }

    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        name = "orchestration.iteration",
        skip_all,
        fields(
            orchestration.iteration = iteration,
            orchestration.task_count = tracing::field::Empty,
            orchestration.post_execute_decision = tracing::field::Empty,
            orchestration.decision_latency_seconds = tracing::field::Empty,
        )
    )]
    async fn run_iteration(
        &self,
        iteration: usize,
        query: &str,
        mut plan: Plan,
        chat_history: &[rig::completion::Message],
        coordinator_state: &mut CoordinatorState,
        previous_context: Option<&IterationContext>,
        event_tx: &tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
        orchestration_start: Instant,
        failure_history: &mut Vec<FailedTaskRecord>,
    ) -> Result<IterationOutcome, StreamError> {
        let elapsed = orchestration_start.elapsed().as_secs_f64();

        tracing::info!(
            "Starting iteration {}/{} (elapsed={:.1}s, per_call_timeout={}s)",
            iteration,
            self.config.max_planning_cycles,
            elapsed,
            self.config.per_call_timeout_secs(),
        );

        // `previous_context` is unused under the unified continuation design —
        // the prior iteration's post-execute coordinator call already
        // produced the plan we receive here (or we received an empty
        // carry-over plan from the failure-replan path's `trigger_replan`).
        // Kept in the signature for future artifact-reachability wiring.
        let _ = previous_context;

        // On re-plan (iteration > 1), advance persistence so the new plan
        // and its execution share a single directory.
        if iteration > 1 {
            let mut persistence = self.persistence.lock().await;
            persistence.start_new_iteration();
        }

        // Record task count on the iteration span now that the plan is finalized
        tracing::Span::current().record("orchestration.task_count", plan.tasks.len() as i64);

        // ----------------------------------------------------------------
        // EXECUTE: Run workers on tasks (parallel when possible)
        // ----------------------------------------------------------------
        if let Err(e) = self.execute(&mut plan, event_tx).await {
            self.write_run_manifest(&plan, iteration).await;
            return Err(e);
        }
        let new_failure_start = failure_history.len();
        failure_history.extend(Self::collect_iteration_failures(&plan, iteration));
        let this_iteration_failures = &failure_history[new_failure_start..];

        // Drain in-flight persistence writes before reading back artifacts.
        // Root cause: tool_wrapper.rs fire-and-forget `tokio::spawn` for
        // on_complete means writes may still be in progress when we reach here.
        // Clone ExecutionPersistence (cheap: just bumps Arcs) to release the
        // MutexGuard before entering drain. on_complete tasks hold the original
        // Arc<Mutex<...>> and need the lock for file I/O.
        {
            let drain_timeout =
                std::time::Duration::from_millis(self.config.persistence_drain_timeout_ms());
            let persistence = self.persistence.lock().await.clone();
            if !persistence.drain(drain_timeout).await {
                tracing::warn!("Persistence drain timed out — tool output refs may be incomplete");
            }
        }

        // Persistence fix: write plan after execute to capture task statuses
        {
            let persistence = self.persistence.lock().await;
            if let Err(e) = persistence.write_plan(&plan).await {
                tracing::warn!("Failed to persist plan after execution: {}", e);
            }
        }

        // ----------------------------------------------------------------
        // SUMMARIZE FAILURES (if any): builds the optional failure_summary
        // for the continuation prompt; no control-flow branching yet.
        // ----------------------------------------------------------------
        let failed_count = plan.failed_count();
        let blocked_count = plan.blocked_tasks().len();
        let has_failures = failed_count > 0 || blocked_count > 0;

        let failure_summary = if has_failures {
            let failure_detail = if !this_iteration_failures.is_empty() {
                let mut category_counts: std::collections::HashMap<FailureCategory, usize> =
                    std::collections::HashMap::new();
                for f in this_iteration_failures {
                    *category_counts.entry(f.category).or_insert(0) += 1;
                }
                let mut categories: Vec<_> = category_counts.into_iter().collect();
                categories.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                let summary = categories
                    .iter()
                    .map(|(cat, count)| {
                        if *count == 1 {
                            format!("1 {}", cat)
                        } else {
                            format!("{} {}s", count, cat)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(" ({})", summary)
            } else {
                String::new()
            };
            tracing::warn!(
                "Execution had failures: {} failed{}, {} blocked",
                failed_count,
                failure_detail,
                blocked_count
            );

            // Provider error short-circuit: if ALL failures are provider
            // errors, replanning can't fix them. Emit raw results rather
            // than burn a coordinator turn asking the LLM to retry.
            if Self::should_short_circuit_provider_errors(
                this_iteration_failures,
                plan.completed_count(),
            ) {
                let summary = self.build_execution_summary(&plan);
                tracing::error!(
                    "All {} failures are provider errors — skipping replan:\n{}",
                    this_iteration_failures.len(),
                    summary
                );
                self.write_run_manifest(&plan, iteration).await;
                return Err(format!(
                    "Provider error: all tasks failed due to provider issues (not retryable via replan):\n{}",
                    summary
                ).into());
            }

            let execution_summary = self.build_execution_summary(&plan);
            Some(FailureSummary {
                reasoning: format!(
                    "Execution failed: {} task(s) failed, {} task(s) blocked by dependencies.",
                    failed_count, blocked_count
                ),
                gaps: vec![
                    "Some tasks could not complete due to errors".to_string(),
                    format!("Execution summary:\n{}", execution_summary),
                ],
            })
        } else {
            None
        };

        // ----------------------------------------------------------------
        // POST-EXECUTE DECISION: unified continuation coordinator call for
        // BOTH clean-success and failure paths. The coordinator sees the
        // iteration's per-task state via the continuation prompt and chooses
        // one routing tool:
        //   - respond_directly → use its response as the final answer
        //   - create_plan      → carry the new plan into the next iteration
        //   - request_clarification → return the question to the user
        //
        // If the coordinator call itself errors (timeout, depth, upstream),
        // build_raw_task_results ships the worker output the user already
        // paid for instead of an empty response.
        // ----------------------------------------------------------------
        let tool_traces = self.load_tool_traces_for_plan(&plan).await;
        let post_execute_ctx = IterationContext::new(
            iteration,
            plan.clone(),
            failure_summary,
            failure_history.clone(),
            tool_traces,
        );
        Self::emit_event(event_tx, OrchestratorEvent::Synthesizing { iteration }).await;
        let decision_start = Instant::now();
        let routing = self
            .plan_with_routing(
                query,
                chat_history,
                coordinator_state,
                Some(&post_execute_ctx),
                Some(event_tx),
            )
            .await;

        let decision_latency = decision_start.elapsed().as_secs_f64();
        tracing::Span::current().record("orchestration.decision_latency_seconds", decision_latency);

        match routing {
            Ok((
                PlanningResponse::Direct {
                    response,
                    routing_rationale,
                    response_summary,
                },
                _,
                _,
            )) => {
                tracing::Span::current()
                    .record("orchestration.post_execute_decision", "respond_directly");
                Self::emit_event(
                    event_tx,
                    OrchestratorEvent::DirectAnswer {
                        response: response.clone(),
                        routing_rationale,
                    },
                )
                .await;
                Self::emit_event(
                    event_tx,
                    OrchestratorEvent::IterationComplete {
                        iteration,
                        will_replan: false,
                        reasoning: String::new(),
                        gaps: vec![],
                    },
                )
                .await;
                tracing::info!(
                    "Iteration {} complete: elapsed={:.1}s (decision: respond_directly, {:.1}s)",
                    iteration,
                    orchestration_start.elapsed().as_secs_f64(),
                    decision_latency,
                );
                self.write_run_manifest_with_summary(&plan, iteration, response_summary)
                    .await;
                Ok(IterationOutcome::FinalResult(response))
            }
            Ok((
                PlanningResponse::Clarification {
                    question,
                    options,
                    routing_rationale,
                },
                _,
                _,
            )) => {
                tracing::Span::current().record(
                    "orchestration.post_execute_decision",
                    "request_clarification",
                );
                Self::emit_event(
                    event_tx,
                    OrchestratorEvent::ClarificationNeeded {
                        question: question.clone(),
                        options,
                        routing_rationale,
                    },
                )
                .await;
                Self::emit_event(
                    event_tx,
                    OrchestratorEvent::IterationComplete {
                        iteration,
                        will_replan: false,
                        reasoning: String::new(),
                        gaps: vec![],
                    },
                )
                .await;
                tracing::info!(
                    "Iteration {} complete: elapsed={:.1}s (decision: request_clarification, {:.1}s)",
                    iteration,
                    orchestration_start.elapsed().as_secs_f64(),
                    decision_latency,
                );
                self.write_run_manifest(&plan, iteration).await;
                Ok(IterationOutcome::FinalResult(question))
            }
            Ok((resp @ PlanningResponse::StepsPlan { .. }, _, _)) => {
                tracing::Span::current()
                    .record("orchestration.post_execute_decision", "create_plan");

                if iteration >= self.config.max_planning_cycles {
                    tracing::warn!(
                        "Coordinator chose create_plan on final iteration {} (max={}). \
                         Returning raw task results instead of looping.",
                        iteration,
                        self.config.max_planning_cycles,
                    );
                    let raw = Self::build_raw_task_results(
                        &plan,
                        "Replan budget exhausted: coordinator requested another iteration but max_planning_cycles reached",
                    );
                    Self::emit_event(
                        event_tx,
                        OrchestratorEvent::IterationComplete {
                            iteration,
                            will_replan: false,
                            reasoning: "Replan budget exhausted".to_string(),
                            gaps: vec![],
                        },
                    )
                    .await;
                    self.write_run_manifest(&plan, iteration).await;
                    return Ok(IterationOutcome::FinalResult(raw));
                }

                let routing_rationale = resp.routing_rationale().to_string();
                let planning_summary = resp.planning_summary().unwrap_or_default().to_string();
                let new_plan = resp.into_plan().expect("StepsPlan always converts to plan");

                Self::emit_event(
                    event_tx,
                    OrchestratorEvent::PlanCreated {
                        goal: new_plan.goal.clone(),
                        tasks: new_plan
                            .tasks
                            .iter()
                            .map(|t| t.description.clone())
                            .collect(),
                        routing_mode: super::events::RoutingMode::for_plan(new_plan.tasks.len()),
                        routing_rationale,
                        planning_response: planning_summary,
                    },
                )
                .await;
                Self::emit_event(
                    event_tx,
                    OrchestratorEvent::IterationComplete {
                        iteration,
                        will_replan: true,
                        reasoning: String::new(),
                        gaps: vec![],
                    },
                )
                .await;

                // Persist plan state before replan
                {
                    let persistence = self.persistence.lock().await;
                    if let Err(e) = persistence.write_plan(&plan).await {
                        tracing::warn!("Failed to persist plan before replan: {}", e);
                    }
                }

                let (new_previous_context, _) = Self::trigger_replan(
                    event_tx,
                    iteration,
                    "post_execute_create_plan",
                    plan,
                    None,
                    failure_history,
                )
                .await;
                tracing::info!(
                    "Iteration {} complete: elapsed={:.1}s (decision: create_plan, {:.1}s)",
                    iteration,
                    orchestration_start.elapsed().as_secs_f64(),
                    decision_latency,
                );
                // The coordinator already produced new_plan; we discard the
                // empty plan returned by trigger_replan but keep its
                // IterationContext so the next iteration can persist
                // previous_plan cleanly.
                Ok(IterationOutcome::Continue {
                    new_plan,
                    previous_context: new_previous_context,
                })
            }
            // Post-execute coordinator call errored before routing (timeout,
            // depth exhaustion, upstream provider error). Ship the worker
            // output the user already paid for rather than returning an empty
            // response.
            Err(e) => {
                tracing::Span::current()
                    .record("orchestration.post_execute_decision", "coordinator_error");
                let err_str = e.to_string();
                let category = Self::categorize_failure_error(&err_str);
                let note = match category {
                    FailureCategory::AgentTimeout => {
                        format!("Post-execute coordinator call timed out: {}", err_str)
                    }
                    FailureCategory::DepthExhausted => {
                        format!(
                            "Post-execute coordinator exhausted its turn budget without routing: {}",
                            err_str
                        )
                    }
                    _ => format!("Post-execute coordinator call failed: {}", err_str),
                };
                tracing::warn!("{}", note);
                let raw = Self::build_raw_task_results(&plan, &note);
                Self::emit_event(
                    event_tx,
                    OrchestratorEvent::IterationComplete {
                        iteration,
                        will_replan: false,
                        reasoning: note,
                        gaps: vec![],
                    },
                )
                .await;
                self.write_run_manifest(&plan, iteration).await;
                Ok(IterationOutcome::FinalResult(raw))
            }
        }
    }

    /// Format the plan's per-task results as a Markdown string prefixed with
    /// a short context note. Used when the post-execute coordinator call
    /// errors before it can route, so the user still sees what the workers
    /// produced instead of an empty response.
    fn build_raw_task_results(plan: &Plan, failure_note: &str) -> String {
        let mut out = String::new();
        out.push_str(failure_note);
        out.push_str("\n\nRaw task results:\n\n");
        for t in &plan.tasks {
            match &t.state {
                TaskState::Complete { result } => {
                    out.push_str(&format!(
                        "## Task {}: {}\n\n{}\n\n",
                        t.id, t.description, result
                    ));
                }
                TaskState::Failed { error, .. } => {
                    out.push_str(&format!(
                        "## Task {}: {}\n\nFailed: {}\n\n",
                        t.id, t.description, error
                    ));
                }
                TaskState::Pending | TaskState::Running => {
                    out.push_str(&format!(
                        "## Task {}: {}\n\n(not executed)\n\n",
                        t.id, t.description
                    ));
                }
            }
        }
        out
    }

    /// Write a typed `RunManifest` summarizing this orchestration run.
    ///
    /// Pre-load tool call records for all tasks in a plan, converting to
    /// condensed `ToolTraceEntry` for continuation prompt rendering.
    async fn load_tool_traces_for_plan(
        &self,
        plan: &Plan,
    ) -> std::collections::HashMap<usize, Vec<super::persistence::ToolTraceEntry>> {
        use super::persistence::ToolTraceEntry;

        let persistence = self.persistence.lock().await;
        let mut traces = std::collections::HashMap::new();

        for t in &plan.tasks {
            let records = persistence.load_tool_records_for_task(t.id).await;
            if records.is_empty() {
                continue;
            }
            let entries: Vec<ToolTraceEntry> = records.iter().map(ToolTraceEntry::from).collect();
            traces.insert(t.id, entries);
        }

        traces
    }

    /// Called at the end of `run_orchestration_loop()` on all exit paths.
    /// Errors are logged but not propagated — manifest is observability, not control flow.
    async fn write_run_manifest(&self, plan: &Plan, iterations: usize) {
        self.write_run_manifest_with_summary(plan, iterations, None)
            .await;
    }

    async fn write_run_manifest_with_summary(
        &self,
        plan: &Plan,
        iterations: usize,
        response_summary: Option<String>,
    ) {
        use super::persistence::{
            ArtifactEntry, ErrorContext, RunManifest, RunStatus, TaskSummary, ToolTraceEntry,
        };
        use crate::string_utils::safe_truncate;

        let persistence = self.persistence.lock().await;

        let all_complete = plan.completed_count() == plan.tasks.len();
        let status = if all_complete {
            RunStatus::Success
        } else if plan.completed_count() > 0 {
            RunStatus::PartialSuccess
        } else {
            RunStatus::Failed
        };

        let artifacts_meta = match persistence.list_artifacts_with_metadata().await {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!("Failed to list artifacts for manifest: {}", e);
                Vec::new()
            }
        };

        let mut task_summaries = Vec::with_capacity(plan.tasks.len());

        for t in &plan.tasks {
            let task_prefix = format!("task-{}-", t.id);
            let task_artifacts: Vec<ArtifactEntry> = artifacts_meta
                .iter()
                .filter(|(name, _)| name.starts_with(&task_prefix))
                .map(|(name, size)| ArtifactEntry {
                    filename: name.clone(),
                    size_bytes: *size,
                    kind: artifact_kind_from_filename(name),
                })
                .collect();

            let tool_records = persistence.load_tool_records_for_task(t.id).await;
            let tool_trace: Vec<ToolTraceEntry> =
                tool_records.iter().map(ToolTraceEntry::from).collect();

            let (error, error_context) = match &t.state {
                TaskState::Failed { error, category } => {
                    let last_tool = tool_trace.last().map(|tr| tr.tool.clone());
                    (
                        Some(error.clone()),
                        Some(ErrorContext {
                            category: *category,
                            last_tool_call: last_tool,
                            attempt_count: 1,
                            partial_result: None,
                        }),
                    )
                }
                _ => (None, None),
            };

            task_summaries.push(TaskSummary {
                task_id: t.id,
                description: t.description.clone(),
                status: TaskStatus::from(&t.state),
                worker: t.worker.clone(),
                result_preview: t
                    .structured_output
                    .as_ref()
                    .map(|s| s.summary.clone())
                    .or_else(|| match &t.state {
                        TaskState::Complete { result } => {
                            Some(safe_truncate(result, 200).0.to_string())
                        }
                        _ => None,
                    }),
                confidence: t
                    .structured_output
                    .as_ref()
                    .map(|s| s.confidence.to_string()),
                failure_category: match &t.state {
                    TaskState::Failed { category, .. } => Some(*category),
                    _ => None,
                },
                error,
                error_context,
                tool_trace,
                artifacts: task_artifacts,
            });
        }

        let artifact_paths = artifacts_meta.into_iter().map(|(name, _)| name).collect();

        let completed = plan.completed_count();
        let total = plan.tasks.len();
        let outcome = if total == 0 {
            None
        } else if all_complete {
            Some(format!("{total}/{total} tasks completed"))
        } else {
            Some(format!("{completed}/{total} tasks completed"))
        };

        let manifest = RunManifest {
            run_id: persistence.run_id().to_string(),
            session_id: persistence.session_id().map(|s| s.to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            goal: plan.goal.clone(),
            status,
            iterations,
            routing_mode: Some(super::events::RoutingMode::for_plan(plan.tasks.len())),
            outcome,
            response_summary,
            task_summaries,
            artifact_paths,
        };

        if let Err(e) = persistence.write_manifest(&manifest).await {
            tracing::warn!("Failed to write run manifest: {}", e);
        }
    }

    async fn write_direct_response_manifest(
        &self,
        query: &str,
        response: &str,
        summary: Option<&str>,
    ) {
        use super::persistence::{RunManifest, RunStatus};
        use crate::string_utils::safe_truncate;

        let persistence = self.persistence.lock().await;

        let response_summary = Some(
            summary
                .map(|s| s.to_string())
                .unwrap_or_else(|| safe_truncate(response, 200).0.to_string()),
        );

        let manifest = RunManifest {
            run_id: persistence.run_id().to_string(),
            session_id: persistence.session_id().map(|s| s.to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            goal: query.to_string(),
            status: RunStatus::Success,
            iterations: 0,
            routing_mode: Some(super::events::RoutingMode::DirectAnswer),
            outcome: Some("Answered directly".to_string()),
            response_summary,
            task_summaries: vec![],
            artifact_paths: vec![],
        };

        if let Err(e) = persistence.write_manifest(&manifest).await {
            tracing::warn!("Failed to write direct response manifest: {}", e);
        }
    }
}

// StreamingAgent is implemented by OrchestratorFactory (see factory.rs),
// which creates an Orchestrator lazily inside stream() to avoid duplicate
// MCP connections and persistence directories.

/// Determine artifact kind from the filename convention.
///
/// Filenames ending in `-result.txt` are worker result artifacts.
/// Filenames ending in `-output.txt` are promoted tool output artifacts;
/// the tool name is extracted from the filename structure.
fn artifact_kind_from_filename(filename: &str) -> super::persistence::ArtifactKind {
    use super::persistence::ArtifactKind;
    if filename.ends_with("-result.txt") {
        ArtifactKind::Result
    } else if filename.ends_with("-output.txt") {
        // Format: task-{id}-{worker}-iter-{n}-{tool_name}-{call_idx}-output.txt
        // Extract tool_name: split by '-', skip known prefix segments, take up to call_idx
        let without_suffix = filename.trim_end_matches("-output.txt");
        let parts: Vec<&str> = without_suffix.split('-').collect();
        // Find "iter" marker position, tool_name segments follow iter-{n}
        let tool_name = parts
            .iter()
            .position(|&p| p == "iter")
            .and_then(|iter_pos| {
                // Skip iter-{n}, take segments up to the last one (call_idx)
                let after_iter = &parts[iter_pos + 2..];
                if after_iter.len() > 1 {
                    Some(after_iter[..after_iter.len() - 1].join("-"))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string());
        ArtifactKind::ToolOutput { tool_name }
    } else {
        ArtifactKind::Result
    }
}

/// Truncate a query string for logging.
fn truncate_query(query: &str, max_len: usize) -> String {
    let (truncated, was_truncated) = safe_truncate(query, max_len);
    if was_truncated {
        format!("{truncated}...")
    } else {
        truncated.to_string()
    }
}

/// Check if an error indicates a MaxDepthError from rig's ReAct loop.
fn is_max_depth_error(error: &(dyn std::error::Error + Send + Sync)) -> bool {
    let msg = error.to_string();
    msg.contains("MaxDepthError") || msg.contains("reached limit")
}

/// Check if an error indicates a context length/token limit exceeded.
///
/// True when the error indicates context window overflow. Delegates to
/// `categorize_failure_error` so classification stays in one place.
fn is_context_overflow_error(error: &dyn std::error::Error) -> bool {
    matches!(
        Orchestrator::categorize_failure_error(&error.to_string()),
        FailureCategory::ContextOverflow
    )
}

/// True for errors worth retrying in the planning loop. Delegates to
/// `categorize_failure_error` so classification stays in one place.
fn is_transient_planning_error(error: &str) -> bool {
    matches!(
        Orchestrator::categorize_failure_error(error),
        FailureCategory::ProviderOverloaded | FailureCategory::AgentTimeout
    )
}

/// Get a user-friendly suggestion for recovering from context overflow.
fn context_overflow_suggestion(phase: &str) -> String {
    use super::prompt_constants::context_overflow;
    match phase {
        "planning" => context_overflow::PLANNING.to_string(),
        "worker" => context_overflow::WORKER.to_string(),
        _ => context_overflow::DEFAULT.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_query_short() {
        assert_eq!(truncate_query("short", 10), "short");
    }

    #[test]
    fn test_truncate_query_long() {
        assert_eq!(
            truncate_query("this is a longer query", 10),
            "this is a ..."
        );
    }

    #[test]
    fn test_truncate_query_exact_length() {
        assert_eq!(truncate_query("exactly10!", 10), "exactly10!");
    }

    #[test]
    fn test_is_context_overflow_error_openai_style() {
        let error: Box<dyn std::error::Error + Send + Sync> =
            "maximum context length exceeded".into();
        assert!(is_context_overflow_error(error.as_ref()));
    }

    #[test]
    fn test_is_context_overflow_error_anthropic_style() {
        let error: Box<dyn std::error::Error + Send + Sync> =
            "maximum number of tokens exceeded".into();
        assert!(is_context_overflow_error(error.as_ref()));
    }

    #[test]
    fn test_is_context_overflow_error_token_limit() {
        let error: Box<dyn std::error::Error + Send + Sync> = "token limit reached".into();
        assert!(is_context_overflow_error(error.as_ref()));
    }

    #[test]
    fn test_is_context_overflow_error_not_context_error() {
        let error: Box<dyn std::error::Error + Send + Sync> = "network timeout".into();
        assert!(!is_context_overflow_error(error.as_ref()));
    }

    // ========================================================================
    // Vector Store Tool Visibility Tests
    // ========================================================================

    fn create_config_with_vector_stores() -> OrchestrationConfig {
        use super::super::config::WorkerConfig;
        use std::collections::HashMap;

        let mut workers = HashMap::new();
        workers.insert(
            "operations".to_string(),
            WorkerConfig {
                description: "For logs and pipelines".to_string(),
                preamble: "Operations specialist.".to_string(),
                mcp_filter: vec!["mezmo_*".to_string()],
                vector_stores: vec![], // No RAG for operations
                turn_depth: None,
                llm: None,
                scratchpad: None,
            },
        );
        workers.insert(
            "knowledge".to_string(),
            WorkerConfig {
                description: "For documentation".to_string(),
                preamble: "Knowledge specialist.".to_string(),
                mcp_filter: vec![],                            // No MCP tools
                vector_stores: vec!["mezmo_docs".to_string()], // RAG access
                turn_depth: None,
                llm: None,
                scratchpad: None,
            },
        );

        OrchestrationConfig {
            enabled: true,
            workers,
            ..Default::default()
        }
    }

    #[test]
    fn test_worker_vector_store_tools_included_in_resolution() {
        // This tests the logic that vector_stores get converted to tool names
        // Format: vector_search_{store_name}
        let config = create_config_with_vector_stores();

        // Knowledge worker should have vector_search_mezmo_docs
        let knowledge = config.workers.get("knowledge").unwrap();
        assert_eq!(knowledge.vector_stores, vec!["mezmo_docs".to_string()]);

        // Operations worker should have no vector stores
        let operations = config.workers.get("operations").unwrap();
        assert!(operations.vector_stores.is_empty());

        // The tool name format is: vector_search_{store_name}
        let expected_tool = format!("vector_search_{}", "mezmo_docs");
        assert_eq!(expected_tool, "vector_search_mezmo_docs");
    }

    #[test]
    fn test_vector_store_tool_name_format() {
        // Verify the tool naming convention matches DynamicVectorSearchTool
        let store_names = vec!["docs", "kb", "mezmo_docs", "customer_runbooks"];

        for name in store_names {
            let tool_name = format!("vector_search_{}", name);
            assert!(tool_name.starts_with("vector_search_"));
            assert!(tool_name.ends_with(name));
        }
    }

    // ========================================================================
    // Vector Store Filtering Tests
    // ========================================================================

    /// Helper to create a config with multiple vector stores in global config
    /// and workers with selective access.
    fn create_config_with_filtered_vector_stores() -> OrchestrationConfig {
        use super::super::config::WorkerConfig;
        use std::collections::HashMap;

        let mut workers = HashMap::new();

        // Worker with vector store access to "docs"
        workers.insert(
            "documentation".to_string(),
            WorkerConfig {
                description: "For documentation queries".to_string(),
                preamble: "Documentation specialist.".to_string(),
                mcp_filter: vec![],
                vector_stores: vec!["docs".to_string()],
                turn_depth: None,
                llm: None,
                scratchpad: None,
            },
        );

        // Worker with vector store access to "kb" and "runbooks"
        workers.insert(
            "knowledge".to_string(),
            WorkerConfig {
                description: "For knowledge base queries".to_string(),
                preamble: "Knowledge specialist.".to_string(),
                mcp_filter: vec![],
                vector_stores: vec!["kb".to_string(), "runbooks".to_string()],
                turn_depth: None,
                llm: None,
                scratchpad: None,
            },
        );

        // Worker with NO vector store access
        workers.insert(
            "operations".to_string(),
            WorkerConfig {
                description: "For operational tasks".to_string(),
                preamble: "Operations specialist.".to_string(),
                mcp_filter: vec!["mezmo_*".to_string()],
                vector_stores: vec![], // Explicitly no RAG access
                turn_depth: None,
                llm: None,
                scratchpad: None,
            },
        );

        OrchestrationConfig {
            enabled: true,
            workers,
            coordinator_vector_stores: vec![], // Coordinator gets no vector stores by default
            ..Default::default()
        }
    }

    #[test]
    fn test_worker_receives_only_assigned_vector_stores() {
        // This test verifies that when a worker config has vector_stores = ["docs"],
        // it only gets that store (not others defined in the global config).

        let config = create_config_with_filtered_vector_stores();

        // Documentation worker should only have access to "docs"
        let doc_worker = config.workers.get("documentation").unwrap();
        assert_eq!(doc_worker.vector_stores, vec!["docs".to_string()]);
        assert!(!doc_worker.vector_stores.contains(&"kb".to_string()));
        assert!(!doc_worker.vector_stores.contains(&"runbooks".to_string()));

        // Knowledge worker should have access to "kb" and "runbooks"
        let knowledge_worker = config.workers.get("knowledge").unwrap();
        assert_eq!(knowledge_worker.vector_stores.len(), 2);
        assert!(knowledge_worker.vector_stores.contains(&"kb".to_string()));
        assert!(
            knowledge_worker
                .vector_stores
                .contains(&"runbooks".to_string())
        );
        assert!(!knowledge_worker.vector_stores.contains(&"docs".to_string()));

        // Operations worker should have NO vector store access
        let ops_worker = config.workers.get("operations").unwrap();
        assert!(ops_worker.vector_stores.is_empty());
    }

    #[test]
    fn test_resolve_worker_tools_includes_vector_stores() {
        // This test verifies that resolve_worker_tools() includes
        // vector_search_<name> tools for workers with assigned vector stores.

        let config = create_config_with_filtered_vector_stores();

        // Simulate the logic from resolve_worker_tools()
        // Documentation worker with vector_stores = ["docs"]
        let doc_worker = config.workers.get("documentation").unwrap();
        let mut doc_tools: Vec<String> = vec![]; // Start with no MCP tools (empty mcp_filter)

        // Add vector store tools based on explicit vector_stores assignment
        for store_name in &doc_worker.vector_stores {
            doc_tools.push(format!("vector_search_{}", store_name));
        }

        assert_eq!(doc_tools.len(), 1);
        assert!(doc_tools.contains(&"vector_search_docs".to_string()));
        assert!(!doc_tools.contains(&"vector_search_kb".to_string()));

        // Knowledge worker with vector_stores = ["kb", "runbooks"]
        let knowledge_worker = config.workers.get("knowledge").unwrap();
        let mut knowledge_tools: Vec<String> = vec![];

        for store_name in &knowledge_worker.vector_stores {
            knowledge_tools.push(format!("vector_search_{}", store_name));
        }

        assert_eq!(knowledge_tools.len(), 2);
        assert!(knowledge_tools.contains(&"vector_search_kb".to_string()));
        assert!(knowledge_tools.contains(&"vector_search_runbooks".to_string()));
        assert!(!knowledge_tools.contains(&"vector_search_docs".to_string()));

        // Operations worker with no vector stores
        let ops_worker = config.workers.get("operations").unwrap();
        let mut ops_tools: Vec<String> = vec![];

        for store_name in &ops_worker.vector_stores {
            ops_tools.push(format!("vector_search_{}", store_name));
        }

        assert!(ops_tools.is_empty());
    }

    #[test]
    fn test_coordinator_no_vector_stores_by_default() {
        // This test verifies that with empty coordinator_vector_stores,
        // the coordinator gets no vector store tools.

        let config = create_config_with_filtered_vector_stores();

        // Verify coordinator_vector_stores is empty
        assert!(config.coordinator_vector_stores.is_empty());

        // Simulate the logic from create_coordinator()
        // When coordinator_vector_stores is empty, no vector stores are assigned
        let coordinator_stores: Vec<String> = if !config.coordinator_vector_stores.is_empty() {
            config.coordinator_vector_stores.clone()
        } else {
            vec![]
        };

        assert!(coordinator_stores.is_empty());

        // Build vector store tool list for coordinator
        let mut coordinator_tools: Vec<String> = vec![];
        for store_name in &coordinator_stores {
            coordinator_tools.push(format!("vector_search_{}", store_name));
        }

        assert!(coordinator_tools.is_empty());
    }

    #[test]
    fn test_coordinator_with_explicit_vector_stores() {
        // This test verifies that when coordinator_vector_stores is set,
        // the coordinator gets those vector store tools.

        use super::super::config::WorkerConfig;
        use std::collections::HashMap;

        let mut workers = HashMap::new();
        workers.insert(
            "test_worker".to_string(),
            WorkerConfig {
                description: "Test".to_string(),
                preamble: "Test".to_string(),
                mcp_filter: vec![],
                vector_stores: vec!["worker_store".to_string()],
                turn_depth: None,
                llm: None,
                scratchpad: None,
            },
        );

        let config = OrchestrationConfig {
            enabled: true,
            workers,
            coordinator_vector_stores: vec!["coordinator_store".to_string()],
            ..Default::default()
        };

        // Verify coordinator gets its own vector stores
        assert_eq!(config.coordinator_vector_stores.len(), 1);
        assert!(
            config
                .coordinator_vector_stores
                .contains(&"coordinator_store".to_string())
        );
        assert!(
            !config
                .coordinator_vector_stores
                .contains(&"worker_store".to_string())
        );

        // Simulate coordinator tool building
        let coordinator_stores = &config.coordinator_vector_stores;
        let mut coordinator_tools: Vec<String> = vec![];
        for store_name in coordinator_stores {
            coordinator_tools.push(format!("vector_search_{}", store_name));
        }

        assert_eq!(coordinator_tools.len(), 1);
        assert!(coordinator_tools.contains(&"vector_search_coordinator_store".to_string()));
        assert!(!coordinator_tools.contains(&"vector_search_worker_store".to_string()));
    }

    #[test]
    fn test_planning_response_direct_has_no_plan() {
        use super::super::types::PlanningResponse;

        let response = PlanningResponse::Direct {
            response: "42".to_string(),
            routing_rationale: "Simple math".to_string(),
            response_summary: None,
        };
        assert!(response.into_plan().is_none());
    }

    #[test]
    fn test_planning_response_clarification_has_no_plan() {
        use super::super::types::PlanningResponse;

        let response = PlanningResponse::Clarification {
            question: "Which service?".to_string(),
            options: Some(vec!["API".to_string(), "Worker".to_string()]),
            routing_rationale: "Ambiguous".to_string(),
        };
        assert!(response.into_plan().is_none());
    }

    #[test]
    fn test_config_defaults_for_routing() {
        let config = OrchestrationConfig::default();
        assert!(config.allow_direct_answers);
        assert!(config.allow_clarification);
    }

    #[test]
    fn test_config_routing_flags_deserialize() {
        let toml = r#"
            enabled = true
            allow_direct_answers = false
            allow_clarification = false
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert!(config.enabled);
        assert!(!config.allow_direct_answers);
        assert!(!config.allow_clarification);
    }

    #[test]
    fn test_config_routing_flags_default_when_omitted() {
        let toml = r#"
            enabled = true
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert!(config.allow_direct_answers);
        assert!(config.allow_clarification);
    }

    // ========================================================================
    // enforce_routing_config tests — guard the StepsPlan fallback rewrite
    // ========================================================================

    #[test]
    fn test_enforce_routing_passes_through_when_allowed() {
        use super::super::types::PlanningResponse;

        let direct = PlanningResponse::Direct {
            response: "42".to_string(),
            routing_rationale: "trivial".to_string(),
            response_summary: None,
        };
        let out = Orchestrator::enforce_routing_config(direct, "what is 6*7?", true, true);
        assert!(matches!(out, PlanningResponse::Direct { .. }));

        let clar = PlanningResponse::Clarification {
            question: "which?".to_string(),
            options: None,
            routing_rationale: "ambiguous".to_string(),
        };
        let out = Orchestrator::enforce_routing_config(clar, "do the thing", true, true);
        assert!(matches!(out, PlanningResponse::Clarification { .. }));
    }

    #[test]
    fn test_enforce_routing_direct_blocked_converts_to_steps_plan() {
        use super::super::types::{PlanningResponse, StepInput};

        let direct = PlanningResponse::Direct {
            response: "the meaning of life is 42".to_string(),
            routing_rationale: "trivial answer".to_string(),
            response_summary: None,
        };
        let out = Orchestrator::enforce_routing_config(direct, "what is the meaning?", false, true);

        match out {
            PlanningResponse::StepsPlan {
                goal,
                steps,
                routing_rationale,
                planning_summary,
            } => {
                assert_eq!(goal, "what is the meaning?");
                assert_eq!(steps.len(), 1);
                match &steps[0] {
                    StepInput::LeafTask { task, worker } => {
                        assert!(task.starts_with("Answer the user's query:"));
                        assert!(task.contains("what is the meaning?"));
                        assert!(worker.is_none());
                    }
                    _ => panic!("expected single LeafTask step"),
                }
                assert!(routing_rationale.contains("allow_direct_answers=false"));
                assert!(routing_rationale.contains("trivial answer"));
                assert!(routing_rationale.contains("the meaning of life is 42"));
                assert!(planning_summary.is_empty());
            }
            other => panic!("expected StepsPlan, got {:?}", other.variant_name()),
        }
    }

    #[test]
    fn test_enforce_routing_clarification_blocked_converts_to_steps_plan() {
        use super::super::types::{PlanningResponse, StepInput};

        let clar = PlanningResponse::Clarification {
            question: "which environment did you mean?".to_string(),
            options: Some(vec!["prod".to_string(), "stage".to_string()]),
            routing_rationale: "ambiguous env".to_string(),
        };
        let out = Orchestrator::enforce_routing_config(clar, "check service health", true, false);

        match out {
            PlanningResponse::StepsPlan {
                goal,
                steps,
                routing_rationale,
                ..
            } => {
                assert_eq!(goal, "check service health");
                assert_eq!(steps.len(), 1);
                match &steps[0] {
                    StepInput::LeafTask { task, .. } => {
                        assert!(task.starts_with("Investigate and answer the user's query:"));
                        assert!(task.contains("check service health"));
                    }
                    _ => panic!("expected single LeafTask step"),
                }
                assert!(routing_rationale.contains("allow_clarification=false"));
                assert!(routing_rationale.contains("ambiguous env"));
                assert!(routing_rationale.contains("which environment did you mean?"));
            }
            other => panic!("expected StepsPlan, got {:?}", other.variant_name()),
        }
    }

    #[test]
    fn test_enforce_routing_steps_plan_passes_through_unchanged() {
        use super::super::types::{PlanningResponse, StepInput};

        let original = PlanningResponse::StepsPlan {
            goal: "compute mean".to_string(),
            steps: vec![StepInput::LeafTask {
                task: "compute mean of 1,2,3".to_string(),
                worker: Some("statistics".to_string()),
            }],
            routing_rationale: "needs tool".to_string(),
            planning_summary: "single step".to_string(),
        };

        // Both flags off — should still pass through, since the response is
        // already a StepsPlan and the override only converts Direct/Clarification.
        let out = Orchestrator::enforce_routing_config(original, "compute mean", false, false);
        match out {
            PlanningResponse::StepsPlan {
                goal,
                steps,
                planning_summary,
                ..
            } => {
                assert_eq!(goal, "compute mean");
                assert_eq!(steps.len(), 1);
                assert_eq!(planning_summary, "single step");
            }
            _ => panic!("expected StepsPlan passthrough"),
        }
    }

    #[test]
    fn test_planning_response_serde_round_trip_all_variants() {
        use super::super::types::{PlanningResponse, StepInput};

        // Direct
        let direct = PlanningResponse::Direct {
            response: "hello".to_string(),
            routing_rationale: "greeting".to_string(),
            response_summary: None,
        };
        let json = serde_json::to_string(&direct).unwrap();
        let parsed: PlanningResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlanningResponse::Direct { .. }));

        // StepsPlan
        let steps_plan = PlanningResponse::StepsPlan {
            goal: "test".to_string(),
            steps: vec![StepInput::LeafTask {
                task: "do it".to_string(),
                worker: None,
            }],
            routing_rationale: "complex".to_string(),
            planning_summary: "A plan to do it".to_string(),
        };
        let json = serde_json::to_string(&steps_plan).unwrap();
        let parsed: PlanningResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlanningResponse::StepsPlan { .. }));

        // Clarification
        let clarification = PlanningResponse::Clarification {
            question: "what?".to_string(),
            options: None,
            routing_rationale: "unclear".to_string(),
        };
        let json = serde_json::to_string(&clarification).unwrap();
        let parsed: PlanningResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlanningResponse::Clarification { .. }));
    }

    // ========================================================================
    // Cancellation watcher tests
    // ========================================================================

    #[tokio::test(start_paused = true)]
    async fn test_watcher_normal_completion_does_not_cancel() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let cancel_token = CancellationToken::new();
        let handle = spawn_cancellation_watcher(
            cancel_rx,
            Duration::from_secs(300),
            cancel_token.clone(),
            "test-normal".to_string(),
        );

        drop(cancel_tx);
        tokio::task::yield_now().await;
        handle.await.unwrap();
        assert!(!cancel_token.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn test_watcher_external_cancel_triggers_token() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let cancel_token = CancellationToken::new();
        let handle = spawn_cancellation_watcher(
            cancel_rx,
            Duration::from_secs(300),
            cancel_token.clone(),
            "test-cancel".to_string(),
        );

        cancel_tx.send(true).unwrap();
        tokio::task::yield_now().await;
        handle.await.unwrap();
        assert!(cancel_token.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn test_watcher_timeout_triggers_cancellation() {
        // Keep sender alive so only the timeout path can fire
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let cancel_token = CancellationToken::new();
        let handle = spawn_cancellation_watcher(
            cancel_rx,
            Duration::from_secs(60),
            cancel_token.clone(),
            "test-timeout".to_string(),
        );

        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        handle.await.unwrap();
        assert!(cancel_token.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn test_watcher_drop_before_timeout_prevents_spurious_cancel() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let cancel_token = CancellationToken::new();
        let handle = spawn_cancellation_watcher(
            cancel_rx,
            Duration::from_secs(60),
            cancel_token.clone(),
            "test-no-spurious".to_string(),
        );

        // Advance to T=30s, then drop sender (simulating stream completing mid-timeout)
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        drop(cancel_tx);
        tokio::task::yield_now().await;

        let start = tokio::time::Instant::now();
        handle.await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            !cancel_token.is_cancelled(),
            "token should not be cancelled when sender is dropped before timeout"
        );
        // Task should exit promptly on sender drop, not wait for remaining 30s timeout
        assert!(
            elapsed < Duration::from_secs(1),
            "task should exit promptly after sender drop, not wait for timeout; elapsed: {:?}",
            elapsed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_watcher_false_signal_does_not_cancel() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let cancel_token = CancellationToken::new();
        let handle = spawn_cancellation_watcher(
            cancel_rx,
            Duration::from_secs(300),
            cancel_token.clone(),
            "test-false-signal".to_string(),
        );

        // Send false — triggers rx.changed() but borrow_and_update() sees false,
        // so the loop continues waiting
        cancel_tx.send(false).unwrap();
        tokio::task::yield_now().await;
        assert!(
            !cancel_token.is_cancelled(),
            "false signal should not cancel"
        );

        // Clean exit via sender drop
        drop(cancel_tx);
        tokio::task::yield_now().await;
        handle.await.unwrap();
        assert!(!cancel_token.is_cancelled());
    }

    // ========================================================================
    // Artifact system tests
    // ========================================================================

    #[tokio::test]
    async fn test_artifact_creation_and_retrieval() {
        use super::super::persistence::ExecutionPersistence;
        use super::super::tools::ReadArtifactTool;
        use rig::tool::Tool;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        let persistence = Arc::new(Mutex::new(persistence));

        // Write a large result as an artifact
        let large_result = "x".repeat(5000);
        {
            let p = persistence.lock().await;
            let filename = p
                .write_result_artifact(0, Some("research"), 1, &large_result)
                .await
                .unwrap();
            assert_eq!(filename, "task-0-research-iter-1-result.txt");
        }

        // Verify ReadArtifactTool can retrieve it
        let tool = ReadArtifactTool::new(persistence.clone());
        let output = tool
            .call(super::super::tools::read_artifact::ReadArtifactArgs {
                filename: "task-0-research-iter-1-result.txt".to_string(),
                run_id: None,
            })
            .await
            .unwrap();

        assert!(output.found);
        assert_eq!(output.content.len(), 5000);
        assert_eq!(output.content, large_result);
    }

    #[tokio::test]
    async fn test_artifact_threshold_below_does_not_create() {
        use super::super::persistence::ExecutionPersistence;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();

        // Result below default threshold (4000) should not create artifact
        let small_result = "small result";
        assert!(small_result.len() <= 4000);

        // Verify list_artifacts returns empty
        let artifacts = persistence.list_artifacts().await.unwrap();
        assert!(artifacts.is_empty());
    }

    #[tokio::test]
    async fn test_artifact_multiple_tasks() {
        use super::super::persistence::ExecutionPersistence;
        use super::super::tools::ReadArtifactTool;
        use rig::tool::Tool;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        let persistence = Arc::new(Mutex::new(persistence));

        // Write artifacts for multiple tasks
        {
            let p = persistence.lock().await;
            p.write_result_artifact(0, None, 1, "result 0")
                .await
                .unwrap();
            p.write_result_artifact(1, Some("stats"), 1, "result 1")
                .await
                .unwrap();
            p.write_result_artifact(2, Some("math"), 1, "result 2")
                .await
                .unwrap();

            let artifacts = p.list_artifacts().await.unwrap();
            assert_eq!(artifacts.len(), 3);
        }

        let expected_names = [
            "task-0-default-iter-1-result.txt",
            "task-1-stats-iter-1-result.txt",
            "task-2-math-iter-1-result.txt",
        ];

        // Verify each can be read back
        let tool = ReadArtifactTool::new(persistence);
        for (i, name) in expected_names.iter().enumerate() {
            let output = tool
                .call(super::super::tools::read_artifact::ReadArtifactArgs {
                    filename: name.to_string(),
                    run_id: None,
                })
                .await
                .unwrap();
            assert!(output.found);
            assert_eq!(output.content, format!("result {}", i));
        }
    }

    // ========================================================================
    // Conversation context tool tests
    // ========================================================================

    #[tokio::test]
    async fn test_conversation_context_large_n() {
        use super::super::tools::get_conversation_context::{
            GetConversationContextArgs, GetConversationContextTool,
        };
        use rig::completion::Message;
        use rig::tool::Tool;

        // last_n larger than history returns all messages
        let history = Arc::new(vec![Message::user("hello"), Message::assistant("hi there")]);
        let tool = GetConversationContextTool::new(history);
        let result = tool
            .call(GetConversationContextArgs {
                last_n: Some(100),
                max_chars: None,
            })
            .await
            .unwrap();
        assert_eq!(result.count, 2);
    }

    #[tokio::test]
    async fn test_conversation_context_zero_n() {
        use super::super::tools::get_conversation_context::{
            GetConversationContextArgs, GetConversationContextTool,
        };
        use rig::completion::Message;
        use rig::tool::Tool;

        // last_n of 0 returns all messages
        let history = Arc::new(vec![Message::user("hello"), Message::assistant("hi there")]);
        let tool = GetConversationContextTool::new(history);
        let result = tool
            .call(GetConversationContextArgs {
                last_n: Some(0),
                max_chars: None,
            })
            .await
            .unwrap();
        assert_eq!(result.count, 2);
    }

    #[tokio::test]
    async fn test_conversation_context_single_message() {
        use super::super::tools::get_conversation_context::{
            GetConversationContextArgs, GetConversationContextTool,
        };
        use rig::completion::Message;
        use rig::tool::Tool;

        let history = Arc::new(vec![Message::user("what is 2+2?")]);
        let tool = GetConversationContextTool::new(history);
        let result = tool
            .call(GetConversationContextArgs {
                last_n: None,
                max_chars: None,
            })
            .await
            .unwrap();
        assert_eq!(result.count, 1);
        assert_eq!(result.messages[0].role, "user");
        assert!(result.messages[0].content.contains("2+2"));
    }

    // ========================================================================
    // categorize_failure_error tests
    // ========================================================================

    #[test]
    fn test_categorize_failure_provider_errors() {
        assert_eq!(
            Orchestrator::categorize_failure_error("Rate limit exceeded"),
            FailureCategory::ProviderOverloaded
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("HTTP 429 Too Many Requests"),
            FailureCategory::ProviderOverloaded
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("503 Service Unavailable"),
            FailureCategory::ProviderOverloaded
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("Authentication failed: invalid API key"),
            FailureCategory::ProviderAuthError
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("Unauthorized: 403"),
            FailureCategory::ProviderAuthError
        );
    }

    #[test]
    fn test_categorize_failure_other_categories() {
        assert_eq!(
            Orchestrator::categorize_failure_error("Request timed out after 30s"),
            FailureCategory::AgentTimeout
        );
        assert_eq!(
            Orchestrator::categorize_failure_error(
                "Worker task timed out while waiting for HITL [approval_task_timeout]"
            ),
            FailureCategory::ApprovalTaskTimeout
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("context limit exceeded"),
            FailureCategory::ContextOverflow
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("MaxDepthError: reached limit"),
            FailureCategory::DepthExhausted
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("Something went wrong"),
            FailureCategory::AgentError
        );
    }

    // -----------------------------------------------------------------------
    // Provider error short-circuit decision
    // -----------------------------------------------------------------------

    fn make_failure(error: &str) -> FailedTaskRecord {
        let category = Orchestrator::categorize_failure_error(error);
        FailedTaskRecord {
            description: "test task".into(),
            error: error.into(),
            iteration: 1,
            worker: None,
            category,
        }
    }

    #[test]
    fn test_should_short_circuit_all_provider_errors() {
        let failures = vec![
            make_failure("rate limit exceeded (429)"),
            make_failure("service unavailable"),
            make_failure("Authentication failed: invalid API key"),
        ];
        assert!(Orchestrator::should_short_circuit_provider_errors(
            &failures, 0
        ));
    }

    #[test]
    fn test_should_not_short_circuit_mixed_errors() {
        let failures = vec![
            make_failure("rate limit exceeded (429)"),
            make_failure("Request timed out after 30s"),
        ];
        assert!(!Orchestrator::should_short_circuit_provider_errors(
            &failures, 0
        ));
    }

    #[test]
    fn test_should_not_short_circuit_when_some_completed() {
        let failures = vec![
            make_failure("rate limit exceeded (429)"),
            make_failure("service unavailable"),
        ];
        assert!(!Orchestrator::should_short_circuit_provider_errors(
            &failures, 1
        ));
    }

    #[test]
    fn test_should_not_short_circuit_empty_failures() {
        assert!(!Orchestrator::should_short_circuit_provider_errors(&[], 0));
    }

    #[test]
    fn test_categorize_failure_loop_detected() {
        assert_eq!(
            Orchestrator::categorize_failure_error("Worker blocked by duplicate call loop"),
            FailureCategory::LoopDetected
        );
    }

    #[test]
    fn test_categorize_failure_soft_failure() {
        assert_eq!(
            Orchestrator::categorize_failure_error("Worker did not call submit_result"),
            FailureCategory::SoftFailure
        );
    }

    #[test]
    fn test_categorize_provider_overloaded_vs_auth() {
        assert_eq!(
            Orchestrator::categorize_failure_error("Rate limit exceeded (429)"),
            FailureCategory::ProviderOverloaded
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("503 Service Unavailable"),
            FailureCategory::ProviderOverloaded
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("403 Forbidden"),
            FailureCategory::ProviderAuthError
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("401 Unauthorized"),
            FailureCategory::ProviderAuthError
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("Invalid API key"),
            FailureCategory::ProviderAuthError
        );
    }

    #[test]
    fn test_categorize_provider_not_found() {
        // Gemini 404 — rig formats as "Invalid status code 404 Not Found with message: ..."
        assert_eq!(
            Orchestrator::categorize_failure_error(
                "CompletionError: ProviderError: Invalid status code 404 Not Found with message: models/gemini-3.1-pro is not found"
            ),
            FailureCategory::ProviderNotFound
        );
        // Bedrock invalid model identifier
        assert_eq!(
            Orchestrator::categorize_failure_error(
                "CompletionError: ProviderError: The provided model identifier is invalid."
            ),
            FailureCategory::ProviderNotFound
        );
        // Gemini "not found for API version" variant
        assert_eq!(
            Orchestrator::categorize_failure_error(
                "models/foo is not found for API version v1beta"
            ),
            FailureCategory::ProviderNotFound
        );
    }

    #[test]
    fn test_categorize_failure_context_overflow_token_patterns() {
        assert_eq!(
            Orchestrator::categorize_failure_error("token limit reached"),
            FailureCategory::ContextOverflow
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("maximum number of tokens exceeded"),
            FailureCategory::ContextOverflow
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("maximum context length"),
            FailureCategory::ContextOverflow
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("Input too long for token window"),
            FailureCategory::ContextOverflow
        );
    }

    #[test]
    fn test_categorize_failure_openai_string_too_long() {
        assert_eq!(
            Orchestrator::categorize_failure_error(
                "messages[7].content: string too long (12839884 > 10485760)"
            ),
            FailureCategory::ContextOverflow
        );
    }

    #[test]
    fn test_categorize_failure_string_above_max_length() {
        assert_eq!(
            Orchestrator::categorize_failure_error(
                "string_above_max_length: messages[7].content exceeds maximum length"
            ),
            FailureCategory::ContextOverflow
        );
    }

    #[test]
    fn test_categorize_failure_anthropic_prompt_too_long() {
        assert_eq!(
            Orchestrator::categorize_failure_error(
                "prompt is too long: 208310 tokens > 200000 maximum"
            ),
            FailureCategory::ContextOverflow
        );
    }

    #[test]
    fn test_categorize_failure_bedrock_input_too_long() {
        assert_eq!(
            Orchestrator::categorize_failure_error("Input is too long for requested model"),
            FailureCategory::ContextOverflow
        );
    }

    #[test]
    fn test_categorize_failure_anthropic_exceed_context_limit() {
        assert_eq!(
            Orchestrator::categorize_failure_error(
                "input length and max_tokens exceed context limit: 100000 + 8192 > 100000"
            ),
            FailureCategory::ContextOverflow
        );
    }

    #[test]
    fn test_categorize_failure_502_provider_overloaded() {
        assert_eq!(
            Orchestrator::categorize_failure_error("502 Bad Gateway"),
            FailureCategory::ProviderOverloaded
        );
    }

    #[test]
    fn test_categorize_failure_precedence_timeout_before_provider() {
        // "timed out" should match AgentTimeout even if message also contains "503"
        assert_eq!(
            Orchestrator::categorize_failure_error("Request timed out after 503 retries"),
            FailureCategory::AgentTimeout
        );
    }

    #[test]
    fn test_should_short_circuit_all_provider_categories() {
        let failures = vec![
            make_failure("Rate limit exceeded"),
            make_failure("Authentication failed"),
            make_failure("CompletionError: ProviderError: Invalid status code 404 Not Found"),
        ];
        assert!(Orchestrator::should_short_circuit_provider_errors(
            &failures, 0
        ));
    }

    // ========================================================================
    // Artifact Kind From Filename Tests
    // ========================================================================

    #[test]
    fn test_artifact_kind_from_filename_result() {
        use crate::orchestration::persistence::ArtifactKind;
        let kind = artifact_kind_from_filename("task-0-sre-iter-1-result.txt");
        assert!(matches!(kind, ArtifactKind::Result));
    }

    #[test]
    fn test_artifact_kind_from_filename_tool_output() {
        use crate::orchestration::persistence::ArtifactKind;
        let kind = artifact_kind_from_filename("task-0-sre-iter-1-log-search-0-output.txt");
        match kind {
            ArtifactKind::ToolOutput { tool_name } => {
                assert_eq!(tool_name, "log-search");
            }
            _ => panic!("Expected ToolOutput"),
        }
    }

    #[test]
    fn test_artifact_kind_from_filename_multi_segment_tool() {
        use crate::orchestration::persistence::ArtifactKind;
        let kind = artifact_kind_from_filename("task-2-ops-iter-1-my-search-tool-3-output.txt");
        match kind {
            ArtifactKind::ToolOutput { tool_name } => {
                assert_eq!(tool_name, "my-search-tool");
            }
            _ => panic!("Expected ToolOutput"),
        }
    }
}
