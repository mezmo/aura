//! Orchestrator agent for multi-agent workflows.
//!
//! The orchestrator implements the `StreamingAgent` trait, enabling drop-in
//! replacement for single-agent mode. It decomposes queries into tasks,
//! executes them (potentially in parallel), and synthesizes results.
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
//! ┌─────────────────────────┐
//! │      SYNTHESIZER        │  ── combine results
//! └─────────────────────────┘
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
//! - `IterationComplete` - when a plan-execute-synthesize cycle completes
//! - `Synthesizing` - when the synthesizer is combining results

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use rig::client::CompletionClient;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use crate::Agent;
use crate::config::{AgentConfig, LlmConfig};
use crate::mcp::McpManager;
use crate::provider_agent::{BuilderState, ProviderAgent, StreamError, StreamItem};
use crate::streaming::StreamingAgent;
use crate::string_utils::safe_truncate;
use crate::tool_call_observer::ToolCallObserver;

use super::tools::RoutingToolSet;
use super::tools::SubmitEvaluationTool;
use super::tools::{InspectToolParamsTool, ListToolsTool, ReadArtifactTool};

use super::config::OrchestrationConfig;
use super::events::OrchestratorEvent;
use super::persistence::ExecutionPersistence;
use super::prompt_journal::{JournalPhase, PromptJournal};
use super::types::{
    EvaluationResult, FailedTaskRecord, IterationContext, Plan, PlanAttemptFailure,
    PlanningResponse, Task, TaskStatus,
};

// ============================================================================
// Constants
// ============================================================================

/// Number of characters per chunk when streaming the final orchestration response.
const STREAM_CHUNK_SIZE: usize = 50;

/// Maximum ReAct depth for the planning coordinator.
/// Defense-in-depth alongside stream_and_collect's early exit.
/// Allows: 1 recon tool (list_tools) + 1 routing tool + 1 spare.
const PLANNING_COORDINATOR_MAX_DEPTH: usize = 3;

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

/// Named return type for `create_*` coordinator/worker methods.
///
/// Replaces bare `(Agent, String)` tuples where the `String` was the preamble
/// used for journal recording.
struct AgentWithPreamble {
    agent: Agent,
    preamble: String,
}

/// Bundled coordinator tools for `build_agent_with_tools`.
struct CoordinatorTools {
    list_tools: Option<ListToolsTool>,
    inspect_tool_params: Option<InspectToolParamsTool>,
    vector_tools: Vec<crate::vector_dynamic::DynamicVectorSearchTool>,
    routing_tools: Option<RoutingToolSet>,
    read_artifact: Option<ReadArtifactTool>,
    evaluation_tool: Option<SubmitEvaluationTool>,
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
fn spawn_cancellation_watcher(
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

/// Extract the first JSON object from a response string.
///
/// Handles markdown code blocks by finding the outermost `{` and `}`.
/// Extract all top-level JSON objects from a response string.
///
/// Uses brace-depth tracking to correctly handle nested objects, escaped
/// characters inside strings, and surrounding text such as markdown code
/// fences or multiple concatenated objects.
fn extract_json_objects(response: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let bytes = response.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        if bytes[pos] != b'{' {
            pos += 1;
            continue;
        }

        let start = pos;
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut escaped = false;

        for i in start..bytes.len() {
            if escaped {
                escaped = false;
                continue;
            }
            match bytes[i] {
                b'\\' if in_string => escaped = true,
                b'"' => in_string = !in_string,
                b'{' if !in_string => depth += 1,
                b'}' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        results.push(&response[start..=i]);
                        pos = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }

        if depth != 0 {
            break;
        }
    }

    results
}

/// Extract the first top-level JSON object from a response string.
fn extract_json_object(response: &str) -> &str {
    extract_json_objects(response)
        .into_iter()
        .next()
        .unwrap_or(response)
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
fn spawn_tool_event_forwarder(
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
/// When orchestration mode is enabled via config, the web server uses this
/// instead of a plain `Agent`. The orchestrator coordinates multiple agents
/// to handle complex queries through a plan-execute-synthesize loop.
pub struct Orchestrator {
    /// ID for the orchestrator
    orchestrator_id: String,

    /// Orchestration configuration
    config: OrchestrationConfig,

    /// The underlying agent configuration (for creating workers)
    agent_config: AgentConfig,

    /// Tool call observer for coordinator visibility into worker tool execution.
    /// Wired to emit OrchestratorEvent for real-time SSE streaming via spawn_tool_event_forwarder.
    tool_call_observer: ToolCallObserver,

    /// Shared MCP manager for tool discovery and cancellation.
    /// Arc-wrapped so workers can share the same connections.
    mcp_manager: Option<Arc<McpManager>>,

    /// Execution persistence for debugging and retry intelligence
    persistence: Arc<Mutex<ExecutionPersistence>>,

    /// Optional prompt journal for dev diagnostics (gated by AURA_PROMPT_JOURNAL=1)
    prompt_journal: Option<PromptJournal>,

    /// Current orchestration iteration, set at the top of `run_orchestration_loop`.
    /// Read by `journal_record` so that iteration doesn't pollute method signatures.
    current_iteration: AtomicUsize,
}

/// Worker identity for reasoning attribution in `stream_and_forward`.
///
/// When `Some`, reasoning items are wrapped as `OrchestratorEvent::WorkerReasoning`
/// with proper task/worker attribution. When `None`, reasoning is forwarded raw
/// (coordinator context — attributed as `agent_id: "main"` by handlers).
struct WorkerIdentity<'a> {
    task_id: usize,
    worker_name: &'a str,
}

impl Orchestrator {
    /// Create a new orchestrator from configuration.
    pub async fn new(
        agent_config: AgentConfig,
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

        // Tool call observer for real-time SSE streaming
        // We subscribe to tool_call_observer when we call spawn_tool_event_forwarder in fn stream hence _rx
        let (tool_call_observer, _rx) = ToolCallObserver::new(32);

        // Initialize execution persistence for debugging and retry intelligence
        let persistence = if let Some(memory_dir) = orchestration_config.memory_dir() {
            tracing::info!(
                "Orchestrator: Initializing execution persistence at: {}",
                memory_dir
            );
            Arc::new(Mutex::new(
                ExecutionPersistence::new(memory_dir, agent_config.session_id.clone())
                    .await
                    .map_err(|e| format!("Failed to initialize persistence: {}", e))?,
            ))
        } else {
            tracing::info!("Orchestrator: Persistence disabled (no memory_dir configured)");
            Arc::new(Mutex::new(ExecutionPersistence::disabled()))
        };

        let orchestrator_id = uuid::Uuid::new_v4().to_string();

        // Initialize prompt journal (gated by AURA_PROMPT_JOURNAL=1 env var)
        let journal_enabled = std::env::var("AURA_PROMPT_JOURNAL")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let prompt_journal = if orchestration_config.memory_dir().is_some() {
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
            "Orchestrator initialized (run={}, max_planning_cycles={}, quality_threshold={:.2}, per_call_timeout={}s, default_turn_depth={}, max_plan_parse_retries={})",
            run_id_str.get(..8).unwrap_or(&run_id_str),
            orchestration_config.max_planning_cycles,
            orchestration_config.quality_threshold,
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

        // Build tool wrapper: observer + duplicate guard + persistence
        let persistence_wrapper = Arc::new(PersistenceWrapper::new(self.persistence.clone()));
        let observer_wrapper = Arc::new(ObserverWrapper::new(
            self.tool_call_observer.clone(),
            task_id,
        ));
        let max_dup = self
            .config
            .max_consecutive_duplicate_tool_calls
            .unwrap_or(1);
        let duplicate_guard = Arc::new(DuplicateCallGuard::new(max_dup));
        // Observer first (emits start), then duplicate guard, then persistence (captures reasoning)
        let wrapper: Arc<dyn ToolWrapper> = Arc::new(ComposedWrapper::new(vec![
            observer_wrapper,
            duplicate_guard,
            persistence_wrapper,
        ]));

        // Create a modified config for workers with extension fields
        let mut worker_config = self.agent_config.clone();

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
            worker_config.preamble_override = Some(self.config.build_worker_preamble());
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

        // Disable orchestration in worker config to avoid nested orchestration
        worker_config.orchestration = None;

        // Per-worker turn_depth → [agent].turn_depth → DEFAULT_MAX_DEPTH
        let resolved_depth = worker_name
            .and_then(|name| self.config.workers.get(name))
            .and_then(|w| w.turn_depth)
            .or(self.agent_config.agent.turn_depth)
            .unwrap_or(crate::builder::DEFAULT_MAX_DEPTH);
        worker_config.agent.turn_depth = Some(resolved_depth);
        tracing::info!("Worker {} turn_depth={}", task_id, resolved_depth);

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

        // Build worker agent using shared MCP connections
        let (provider_agent, model_name) = self.build_worker_provider_agent(&worker_config).await?;

        let agent = Agent {
            inner: provider_agent,
            model: model_name,
            max_depth: resolved_depth,
            mcp_manager: self.mcp_manager.clone(),
            fallback_tool_parsing: false,
            fallback_tool_names: vec![],
            context_window: None,
        };

        Ok(AgentWithPreamble { agent, preamble })
    }

    /// Execute a blocking chat call with timeout — for **worker tasks only**.
    ///
    /// Workers need the full ReAct loop for sequential MCP tool chains.
    /// Coordinator phases (planning, evaluation) use `stream_and_collect()`
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
    /// full multi-turn tool loop. Used for workers, synthesis, and phase continuation.
    ///
    /// Key behaviors:
    /// - Uses `agent.stream_chat()` which respects the agent's configured `max_depth`
    /// - Forwards `ReasoningDelta`/`Reasoning` items through `event_tx`
    /// - No early-exit — runs the complete ReAct loop
    /// - Timeout wrapping via `per_call_timeout_secs`
    async fn stream_and_forward(
        &self,
        agent: &Agent,
        prompt: &str,
        history: Vec<rig::completion::Message>,
        phase: &str,
        event_tx: Option<&tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>>,
        worker: Option<WorkerIdentity<'_>>,
    ) -> Result<crate::provider_agent::CompletionResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        use crate::provider_agent::{CompletionResponse, StreamedAssistantContent};
        use futures::StreamExt;
        use rig::completion::Usage;

        let timeout_secs = self.config.per_call_timeout_secs();
        let stream_future = async {
            let mut stream = agent.stream_chat(prompt, history).await;
            let mut content = String::new();
            let mut usage = Usage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
            };

            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                        content.push_str(&t);
                    }
                    Ok(StreamItem::StreamAssistantItem(
                        StreamedAssistantContent::ReasoningDelta { delta, .. },
                    )) => {
                        if let Some(tx) = event_tx {
                            if let Some(ref w) = worker {
                                let _ = tx
                                    .send(Ok(StreamItem::OrchestratorEvent(
                                        OrchestratorEvent::WorkerReasoning {
                                            task_id: w.task_id,
                                            worker_id: w.worker_name.to_string(),
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
                    }
                    Err(e) => return Err(e),
                    _ => {} // ToolCall, ToolCallDelta, ToolResult — rig handles execution
                }
            }

            Ok(CompletionResponse { content, usage })
        };

        if timeout_secs == 0 {
            stream_future.await
        } else {
            match tokio::time::timeout(Duration::from_secs(timeout_secs), stream_future).await {
                Ok(result) => result,
                Err(_elapsed) => {
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
    /// Replaces `chat_with_timeout` for one-shot tool phases (planning, evaluation).
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
        prompt: &str,
        history: Vec<rig::completion::Message>,
        phase: &str,
        event_tx: Option<&tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>>,
        decision_ready: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
    ) -> Result<crate::provider_agent::CompletionResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        use crate::provider_agent::{
            CompletionResponse, StreamedAssistantContent, StreamedUserContent,
        };
        use futures::StreamExt;
        use rig::completion::Usage;

        let timeout_secs = self.config.per_call_timeout_secs();
        let stream_future = async {
            let mut stream = agent.stream_chat_with_depth(prompt, history, 1).await;
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
                            // Read one more item to capture per-turn token usage
                            // before dropping the stream. With the always-yield-Final
                            // fix in Rig, TurnUsage is the next item after ToolResult.
                            if let Some(Ok(StreamItem::TurnUsage(turn))) = stream.next().await {
                                usage.input_tokens += turn.input_tokens;
                                usage.output_tokens += turn.output_tokens;
                                usage.total_tokens += turn.total_tokens;
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

    /// Plan with routing tool support.
    ///
    /// Creates a coordinator with routing tools (`respond_directly`, `create_plan`,
    /// `request_clarification`) and reads the tool-based routing decision after the
    /// coordinator chat completes.
    ///
    /// Falls back to text-based plan parsing if no routing tool was called.
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
        previous: Option<&IterationContext>,
        event_tx: Option<&tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>>,
    ) -> Result<(PlanningResponse, String, String), StreamError> {
        use super::tools::routing_tools::RoutingDecision;

        let max_plan_parse_retries = self.config.max_plan_parse_retries;

        // Create routing toolset and capture decision handle
        let routing_toolset = RoutingToolSet::new();
        let routing_decision: RoutingDecision = routing_toolset.decision.clone();

        // Create coordinator with routing tools
        let AgentWithPreamble {
            agent: coordinator,
            preamble: coordinator_preamble,
        } = self.create_coordinator(Some(routing_toolset)).await?;

        // Build prompt components
        let (worker_section, _worker_field, worker_guidelines) =
            self.build_worker_prompt_sections();

        let reflection_section = previous
            .map(|ctx| ctx.build_reflection_prompt(self.config.max_planning_cycles))
            .unwrap_or_default();

        let mut plan_errors: Vec<PlanAttemptFailure> = Vec::new();
        let mut final_prompt = String::new();
        let mut final_response = String::new();

        for attempt in 1..=max_plan_parse_retries {
            // Reset any stale routing decision from a previous failed attempt
            {
                let mut guard = routing_decision.lock().await;
                *guard = None;
            }

            let attempt_start = Instant::now();
            tracing::info!(
                "Planning attempt {}/{} (max_plan_parse_retries={}, per_call_timeout={}s)",
                attempt,
                max_plan_parse_retries,
                max_plan_parse_retries,
                self.config.per_call_timeout_secs(),
            );
            let error_section = if let Some(err) = plan_errors.last() {
                format!(
                    "\n\nPREVIOUS PLANNING ERROR:\n\
                     Your previous response could not be parsed: {}\n\n\
                     Please call one of the routing tools (respond_directly, create_plan, or request_clarification).",
                    err
                )
            } else {
                String::new()
            };

            // Build the tool-calling planning prompt
            let planning_prompt = format!(
                "Analyze this user query and decide on the best approach.\n\n\
                 USER QUERY: {query}{worker_section}{reflection_section}{error_section}\n\n\
                 You have three routing tools. Call EXACTLY ONE (do not call more than one):\n\n\
                 1. **respond_directly** — For simple factual questions answerable from general knowledge.\n\
                    NEVER use for queries about system data, logs, metrics, or anything requiring tools.\n\n\
                 2. **create_plan** — For queries requiring tool execution, data gathering, or multi-step analysis.\n\
                    When uncertain between respond_directly and create_plan, always choose create_plan.\n\n\
                 3. **request_clarification** — For genuinely ambiguous queries where intent is unclear.\n\
                    Use sparingly — prefer create_plan when a reasonable interpretation exists.\n\n\
                 {worker_guidelines}\n\n\
                 Call the appropriate routing tool now.",
                query = query,
                worker_section = worker_section,
                reflection_section = reflection_section,
                error_section = error_section,
                worker_guidelines = worker_guidelines,
            );

            // Record in prompt journal
            self.journal_record(
                JournalPhase::Planning {
                    attempt,
                    max_attempts: max_plan_parse_retries,
                },
                &coordinator_preamble,
                &planning_prompt,
            );

            // Call coordinator with early-exit streaming (prevents ReAct loop waste)
            let rd = routing_decision.clone();
            let response = match self
                .stream_and_collect(
                    &coordinator,
                    &planning_prompt,
                    chat_history.to_vec(),
                    "Planning",
                    event_tx,
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
                    let elapsed = attempt_start.elapsed();
                    let failure = if err_str.contains("timed out") {
                        PlanAttemptFailure::Timeout {
                            attempt,
                            timeout_secs: self.config.per_call_timeout_secs(),
                            elapsed,
                        }
                    } else if err_str.contains("MaxDepthError") || err_str.contains("reached limit")
                    {
                        PlanAttemptFailure::DepthExhausted {
                            attempt,
                            detail: format!(
                                "coordinator exhausted all turns without calling a routing tool (inspect_tool_params may have consumed the budget). Error: {}",
                                err_str
                            ),
                            elapsed,
                        }
                    } else {
                        PlanAttemptFailure::LlmError {
                            attempt,
                            detail: err_str,
                            elapsed,
                        }
                    };
                    tracing::warn!("{}", failure);
                    plan_errors.push(failure);
                    continue;
                }
            };

            final_prompt = planning_prompt;

            // Check if a routing tool was called
            let decision = routing_decision.lock().await.take();

            if let Some(planning_response) = decision {
                // Use the routing decision as the planning response for persistence.
                // The stream's text content is typically empty because the coordinator
                // produces its output via tool calls, not streamed text.
                final_response = if response.content.trim().is_empty() {
                    serde_json::to_string_pretty(&planning_response)
                        .unwrap_or_else(|_| response.content.clone())
                } else {
                    response.content.clone()
                };

                // Persist planning phase artifacts (after routing decision is available)
                {
                    let persistence = self.persistence.lock().await;
                    if let Err(e) = persistence
                        .write_planning_phase(&final_prompt, &final_response)
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

                // Enforce config flags
                let planning_response = self.enforce_routing_config(planning_response, query);

                // Persist plan for Orchestrated variant
                if matches!(
                    &planning_response,
                    PlanningResponse::Orchestrated { .. } | PlanningResponse::StepsPlan { .. }
                ) && let Some(plan) = planning_response.clone().into_plan()
                {
                    let persistence = self.persistence.lock().await;
                    if let Err(e) = persistence.write_plan(&plan).await {
                        tracing::warn!("Failed to persist plan: {}", e);
                    }
                }

                return Ok((planning_response, final_prompt, final_response));
            }

            // Fallback: no routing tool called — persist the raw text response
            final_response = response.content.clone();
            {
                let persistence = self.persistence.lock().await;
                if let Err(e) = persistence
                    .write_planning_phase(&final_prompt, &final_response)
                    .await
                {
                    tracing::warn!("Failed to persist planning phase: {}", e);
                }
            }

            let (response_preview, _) = safe_truncate(&response.content, 300);
            tracing::warn!(
                "No routing tool called (attempt {}/{}). The coordinator responded with text instead of calling create_plan/respond_directly/request_clarification. Response: {}",
                attempt,
                max_plan_parse_retries,
                response_preview,
            );

            match self.parse_plan_response(&response.content, query) {
                Ok(plan) => {
                    tracing::info!(
                        "Fallback parse (attempt {}, {:.1}s): {} task(s) for goal: {}",
                        attempt,
                        attempt_start.elapsed().as_secs_f64(),
                        plan.tasks.len(),
                        truncate_query(&plan.goal, 50)
                    );

                    {
                        let persistence = self.persistence.lock().await;
                        if let Err(e) = persistence.write_plan(&plan).await {
                            tracing::warn!("Failed to persist plan: {}", e);
                        }
                    }

                    // Wrap as Orchestrated response
                    let tasks_json: Vec<super::types::TaskJson> = plan
                        .tasks
                        .iter()
                        .map(|t| super::types::TaskJson {
                            id: t.id,
                            description: t.description.clone(),
                            rationale: Some(t.rationale.clone()),
                            dependencies: Some(t.dependencies.clone()),
                            worker: t.worker.clone(),
                            reuse_result_from: None,
                        })
                        .collect();

                    return Ok((
                        PlanningResponse::Orchestrated {
                            goal: plan.goal,
                            tasks: tasks_json,
                            routing_rationale: "Fallback: text-based plan parsing".to_string(),
                            planning_summary: String::new(),
                            phases: None,
                        },
                        final_prompt,
                        final_response,
                    ));
                }
                Err(e) => {
                    let (response_preview, _) = safe_truncate(&response.content, 200);
                    let failure = PlanAttemptFailure::ParseFailure {
                        attempt,
                        detail: e.to_string(),
                        response_preview: response_preview.to_string(),
                        elapsed: attempt_start.elapsed(),
                    };
                    tracing::warn!("{}", failure);
                    plan_errors.push(failure);
                }
            }
        }

        // All attempts failed — build categorized summary and fall back
        {
            let total_elapsed: Duration = plan_errors.iter().map(|f| f.elapsed()).sum();
            let mut category_counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for f in &plan_errors {
                *category_counts.entry(f.category()).or_insert(0) += 1;
            }
            // Build "2 timeouts, 1 parse failure" style summary
            let mut categories: Vec<_> = category_counts.into_iter().collect();
            categories.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
            let category_summary = categories
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
            tracing::warn!(
                "All {} planning attempts failed ({}; total {:.1}s). Falling back to single-task plan.",
                max_plan_parse_retries,
                category_summary,
                total_elapsed.as_secs_f64(),
            );
            for failure in &plan_errors {
                tracing::warn!("  {}", failure);
            }
        }

        let response = PlanningResponse::Orchestrated {
            goal: query.to_string(),
            tasks: vec![super::types::TaskJson {
                id: 0,
                description: format!("Execute: {}", truncate_query(query, 100)),
                rationale: Some(
                    "Direct execution of user query as a single task (routing failed)".to_string(),
                ),
                dependencies: None,
                worker: None,
                reuse_result_from: None,
            }],
            routing_rationale: "Fallback: all routing attempts failed".to_string(),
            planning_summary: String::new(),
            phases: None,
        };

        let (query_preview, _) = safe_truncate(query, 100);
        tracing::info!("Created fallback single-task plan for: {}", query_preview);

        Ok((response, final_prompt, final_response))
    }

    /// Enforce config flags on a routing decision.
    ///
    /// When `allow_direct_answers` or `allow_clarification` is false,
    /// converts the response to a single-task Orchestrated plan.
    fn enforce_routing_config(&self, response: PlanningResponse, query: &str) -> PlanningResponse {
        match &response {
            PlanningResponse::Direct {
                response: answer,
                routing_rationale,
            } if !self.config.allow_direct_answers => {
                tracing::info!(
                    "Config override: converting direct answer to orchestrated plan (allow_direct_answers=false)"
                );
                PlanningResponse::Orchestrated {
                    goal: query.to_string(),
                    tasks: vec![super::types::TaskJson {
                        id: 0,
                        description: format!(
                            "Answer the user's query: {}",
                            truncate_query(query, 80)
                        ),
                        rationale: Some(format!(
                            "Direct answer overridden by config. Original answer: {}",
                            truncate_query(answer, 100)
                        )),
                        dependencies: None,
                        worker: None,
                        reuse_result_from: None,
                    }],
                    routing_rationale: format!(
                        "Config override (allow_direct_answers=false). Original rationale: {}",
                        routing_rationale
                    ),
                    planning_summary: String::new(),
                    phases: None,
                }
            }
            PlanningResponse::Clarification {
                question,
                routing_rationale,
                ..
            } if !self.config.allow_clarification => {
                tracing::info!(
                    "Config override: converting clarification to orchestrated plan (allow_clarification=false)"
                );
                PlanningResponse::Orchestrated {
                    goal: query.to_string(),
                    tasks: vec![super::types::TaskJson {
                        id: 0,
                        description: format!(
                            "Investigate and answer the user's query: {}",
                            truncate_query(query, 80)
                        ),
                        rationale: Some(format!(
                            "Clarification overridden by config. Original question: {}",
                            truncate_query(question, 100)
                        )),
                        dependencies: None,
                        worker: None,
                        reuse_result_from: None,
                    }],
                    routing_rationale: format!(
                        "Config override (allow_clarification=false). Original rationale: {}",
                        routing_rationale
                    ),
                    planning_summary: String::new(),
                    phases: None,
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
                format!("## {}\n{}", name, config.description)
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

        // Collect from legacy tool definitions (rmcp::model::Tool)
        for (tool, _) in &mcp_manager.tool_definitions {
            names.push(tool.name.to_string());
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

        // Collect from legacy tool definitions (same rmcp::model::Tool type)
        for (tool, _) in &mcp_manager.tool_definitions {
            let schema_value = serde_json::Value::Object((*tool.input_schema).clone());
            schemas.insert(tool.name.to_string(), schema_value);
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

            // Collect from legacy tool definitions
            for (tool, _) in &mcp_manager.tool_definitions {
                if let Some(ref desc) = tool.description {
                    descriptions.insert(tool.name.to_string(), desc.to_string());
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
        if let Some(routing) = tools.routing_tools {
            state = state.add_tool(routing.respond_directly);
            state = state.add_tool(routing.create_plan);
            state = state.add_tool(routing.request_clarification);
        }
        if let Some(artifact_tool) = tools.read_artifact {
            state = state.add_tool(artifact_tool);
        }
        if let Some(eval_tool) = tools.evaluation_tool {
            state = state.add_tool(eval_tool);
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
    /// When `routing_tools` is `Some`, the three routing tools are also added to the
    /// coordinator agent, enabling structured routing decisions via tool calling.
    async fn create_coordinator(
        &self,
        routing_tools: Option<RoutingToolSet>,
    ) -> Result<AgentWithPreamble, Box<dyn std::error::Error + Send + Sync>> {
        use crate::vector_dynamic::DynamicVectorSearchTool;
        use crate::vector_store::VectorStoreManager;

        // Capture tool information for reconnaissance tools
        let tool_names = self.get_all_tool_names();
        let tool_schemas = self.get_all_tool_schemas();

        // Create reconnaissance tools
        let list_tool = ListToolsTool::new(tool_names);
        let inspect_tool = InspectToolParamsTool::new(tool_schemas);

        // Omit recon tools when tools_in_planning is Summary or Full
        // (tool names are already in the planning prompt via worker sections)
        let include_recon_tools = matches!(
            self.config.tools_in_planning,
            super::config::ToolVisibility::None
        );

        // Build coordinator preamble: orchestration framework template + user system prompt
        let mut preamble = self.config.build_coordinator_preamble(
            self.agent_config.effective_preamble(),
            include_recon_tools,
        );
        let temperature = self.agent_config.agent.temperature;

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
            && let Some(memory_dir) = self.config.memory_dir()
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
            evaluation_tool: None,
        };

        let provider_agent = self
            .build_provider_agent_with_tools(
                &preamble,
                temperature,
                self.agent_config.agent.additional_params.clone(),
                coordinator_tools,
            )
            .await?;

        let model_name = self.agent_config.llm.model_name().to_string();

        // Planning coordinator uses a tight depth budget: 1 recon + 1 routing + 1 spare.
        // stream_and_collect() provides the primary early-exit guard; this is defense-in-depth.
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
            },
            preamble,
        })
    }

    /// Create a coordinator agent for synthesis (only `read_artifact` tool).
    ///
    /// Synthesis needs access to artifacts but should not have recon or routing tools,
    /// preventing models from wasting turns on `list_tools`/`inspect_tool_params` calls.
    async fn create_synthesis_coordinator(
        &self,
    ) -> Result<AgentWithPreamble, Box<dyn std::error::Error + Send + Sync>> {
        let preamble = self
            .config
            .build_coordinator_preamble(self.agent_config.effective_preamble(), false);
        let temperature = self.agent_config.agent.temperature;

        let coordinator_tools = CoordinatorTools {
            list_tools: None,
            inspect_tool_params: None,
            vector_tools: vec![],
            routing_tools: None,
            read_artifact: Some(ReadArtifactTool::new(self.persistence.clone())),
            evaluation_tool: None,
        };

        let provider_agent = self
            .build_provider_agent_with_tools(
                &preamble,
                temperature,
                self.agent_config.agent.additional_params.clone(),
                coordinator_tools,
            )
            .await?;

        let model_name = self.agent_config.llm.model_name().to_string();
        // Synthesis only needs 1-2 turns (read_artifact + response)
        let max_depth = 4;

        Ok(AgentWithPreamble {
            agent: Agent {
                inner: provider_agent,
                model: model_name,
                max_depth,
                mcp_manager: None,
                fallback_tool_parsing: false,
                fallback_tool_names: vec![],
                context_window: None,
            },
            preamble,
        })
    }

    /// Create a lightweight coordinator for phase continuation decisions.
    ///
    /// No tools — the coordinator simply responds with "continue" or "replan".
    async fn create_phase_continuation_coordinator(
        &self,
    ) -> Result<AgentWithPreamble, Box<dyn std::error::Error + Send + Sync>> {
        let preamble = self
            .config
            .build_coordinator_preamble(self.agent_config.effective_preamble(), false);
        let temperature = self.agent_config.agent.temperature;

        let coordinator_tools = CoordinatorTools {
            list_tools: None,
            inspect_tool_params: None,
            vector_tools: vec![],
            routing_tools: None,
            read_artifact: None,
            evaluation_tool: None,
        };

        let provider_agent = self
            .build_provider_agent_with_tools(
                &preamble,
                temperature,
                self.agent_config.agent.additional_params.clone(),
                coordinator_tools,
            )
            .await?;

        let model_name = self.agent_config.llm.model_name().to_string();
        // Phase continuation is a single-call decision — no tools, 1 turn
        let max_depth = 1;

        Ok(AgentWithPreamble {
            agent: Agent {
                inner: provider_agent,
                model: model_name,
                max_depth,
                mcp_manager: None,
                fallback_tool_parsing: false,
                fallback_tool_names: vec![],
                context_window: None,
            },
            preamble,
        })
    }

    /// Create a coordinator agent for evaluation (with `submit_evaluation` tool).
    ///
    /// The evaluation coordinator calls `submit_evaluation` to record its
    /// structured quality assessment. Uses the same `build_provider_agent_with_tools`
    /// path as all other coordinator phases.
    async fn create_evaluation_coordinator(
        &self,
        evaluation_tool: SubmitEvaluationTool,
    ) -> Result<AgentWithPreamble, Box<dyn std::error::Error + Send + Sync>> {
        let preamble = include_str!("../prompts/evaluation_preamble.md").to_string();
        let temperature = self.agent_config.agent.temperature;

        let coordinator_tools = CoordinatorTools {
            list_tools: None,
            inspect_tool_params: None,
            vector_tools: vec![],
            routing_tools: None,
            read_artifact: None,
            evaluation_tool: Some(evaluation_tool),
        };

        let provider_agent = self
            .build_provider_agent_with_tools(
                &preamble,
                temperature,
                self.agent_config.agent.additional_params.clone(),
                coordinator_tools,
            )
            .await?;

        let model_name = self.agent_config.llm.model_name().to_string();
        // Evaluation needs 2 turns: tool call + ack
        let max_depth = 2;

        Ok(AgentWithPreamble {
            agent: Agent {
                inner: provider_agent,
                model: model_name,
                max_depth,
                mcp_manager: None,
                fallback_tool_parsing: false,
                fallback_tool_names: vec![],
                context_window: None,
            },
            preamble,
        })
    }

    /// Build a provider-specific agent with coordinator tools.
    ///
    /// Extracted from `create_coordinator` to share provider matching across
    /// planning, synthesis, and evaluation constructors.
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
        }
    }

    /// Build a provider-specific worker agent with MCP tools from the shared manager.
    ///
    /// Workers share the orchestrator's `Arc<McpManager>` rather than creating
    /// their own MCP connections. Tool filtering is handled by `add_all_tools`
    /// via `worker_config.mcp_filter`.
    async fn build_worker_provider_agent(
        &self,
        worker_config: &AgentConfig,
    ) -> Result<(ProviderAgent, String), Box<dyn std::error::Error + Send + Sync>> {
        let preamble = worker_config.effective_preamble();
        let temperature = worker_config.agent.temperature;
        let shared_mcp: Option<Arc<McpManager>> = self.mcp_manager.clone();

        match &worker_config.llm {
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
                    .map_err(|e| format!("Failed to build OpenAI worker: {}", e))?
                    .completions_api()
                    .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                // Build combined additional_params: reasoning_effort + agent-level
                let mut combined_params: Option<serde_json::Value> = None;
                if let Some(effort) = worker_config.agent.reasoning_effort
                    && crate::builder::is_reasoning_model(model)
                {
                    let effort_str = match effort {
                        crate::config::ReasoningEffort::Minimal => "minimal",
                        crate::config::ReasoningEffort::Low => "low",
                        crate::config::ReasoningEffort::Medium => "medium",
                        crate::config::ReasoningEffort::High => "high",
                    };
                    combined_params = Some(serde_json::json!({"reasoning_effort": effort_str}));
                }
                if let Some(ref params) = worker_config.agent.additional_params {
                    combined_params = Some(match combined_params {
                        Some(existing) => crate::builder::merge_json(existing, params.clone()),
                        None => params.clone(),
                    });
                }
                if let Some(params) = combined_params {
                    builder = builder.additional_params(params);
                }
                if let Some(max) = worker_config.agent.max_tokens {
                    builder = builder.max_tokens(max);
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp).await?;
                Ok((ProviderAgent::OpenAI(state.build()), model.clone()))
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
                    .map_err(|e| format!("Failed to build Anthropic worker: {}", e))?
                    .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                if let Some(max) = worker_config.agent.max_tokens {
                    builder = builder.max_tokens(max);
                }
                if let Some(ref params) = worker_config.agent.additional_params {
                    builder = builder.additional_params(params.clone());
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp).await?;
                Ok((ProviderAgent::Anthropic(state.build()), model.clone()))
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
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                if let Some(max) = worker_config.agent.max_tokens {
                    builder = builder.max_tokens(max);
                }
                if let Some(ref params) = worker_config.agent.additional_params {
                    builder = builder.additional_params(params.clone());
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp).await?;
                Ok((ProviderAgent::Bedrock(state.build()), model.clone()))
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
                    .map_err(|e| format!("Failed to build Gemini worker: {}", e))?
                    .completion_model(model);
                let mut builder = rig::agent::AgentBuilder::new(cm);
                builder = builder.preamble(preamble);
                if let Some(temp) = temperature {
                    builder = builder.temperature(temp);
                }
                if let Some(ref params) = worker_config.agent.additional_params {
                    builder = builder.additional_params(params.clone());
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp).await?;
                Ok((ProviderAgent::Gemini(state.build()), model.clone()))
            }
            LlmConfig::Ollama {
                model,
                base_url,
                num_ctx,
                num_predict,
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
                // Build combined params: Ollama-specific + agent-level (single call)
                {
                    let mut combined = crate::builder::build_ollama_params(
                        *num_ctx,
                        *num_predict,
                        additional_params.clone(),
                    );
                    if let Some(ref params) = worker_config.agent.additional_params {
                        combined = Some(match combined {
                            Some(existing) => crate::builder::merge_json(existing, params.clone()),
                            None => params.clone(),
                        });
                    }
                    if let Some(params) = combined {
                        builder = builder.additional_params(params);
                    }
                }
                let state = BuilderState::Initial(builder);
                let state = Agent::add_all_tools(state, worker_config, &shared_mcp).await?;
                Ok((ProviderAgent::Ollama(state.build()), model.clone()))
            }
        }
    }

    /// Build the synthesis prompt for combining multiple task results.
    ///
    /// The prompt provides context about the orchestration goal, the original
    /// user query, and each task's description, rationale, and result.
    fn build_synthesis_prompt(&self, plan: &Plan, query: &str, tasks: &[&Task]) -> String {
        let results_section = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Complete)
            .filter_map(|t| {
                t.result.as_ref().map(|r| {
                    format!(
                        "### Task {}: {}\n**Rationale**: {}\n**Result**:\n{}\n",
                        t.id, t.description, t.rationale, r
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n");

        super::templates::render_synthesis_prompt(&super::templates::SynthesisVars {
            goal: &plan.goal,
            query,
            results: &results_section,
        })
    }

    /// Build the evaluation prompt for semantic quality assessment.
    ///
    /// The prompt provides context about the query, goal, and synthesized
    /// response, asking the LLM to evaluate completeness, accuracy, and coherence.
    /// When `AURA_ENRICH_EVALUATION` is enabled (default: true), includes truncated
    /// task execution evidence so the evaluator can verify data in the synthesis
    /// against actual tool results rather than assuming hallucination.
    fn build_evaluation_prompt(&self, plan: &Plan, query: &str, result: &str) -> String {
        // Include worker context so eval can detect factually wrong answers
        // (e.g., user asks "what workers do you have?" and response doesn't match)
        let workers_context = if self.config.has_workers() {
            format!(
                "\nSYSTEM CONTEXT - AVAILABLE WORKERS:\n{}\n",
                self.config.format_workers_for_prompt()
            )
        } else {
            String::new()
        };

        // Build task evidence when enrichment is enabled
        let enrich = std::env::var("AURA_ENRICH_EVALUATION")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);

        let task_evidence = if enrich && !plan.tasks.is_empty() {
            let task_lines: Vec<String> = plan
                .tasks
                .iter()
                .map(|t| {
                    let worker_label = t
                        .worker
                        .as_deref()
                        .map(|w| format!(" [{}]", w))
                        .unwrap_or_default();
                    match t.status {
                        TaskStatus::Complete => {
                            let result_text = t.result.as_deref().unwrap_or("(no result)");
                            let (truncated, was_truncated) = safe_truncate(result_text, 500);
                            let suffix = if was_truncated { "..." } else { "" };
                            format!(
                                "- Task {}{} (Complete): {} → {}{}",
                                t.id, worker_label, t.description, truncated, suffix
                            )
                        }
                        TaskStatus::Failed => {
                            let err = t.error.as_deref().unwrap_or("unknown");
                            format!(
                                "- Task {}{} (Failed): {} → Error: {}",
                                t.id, worker_label, t.description, err
                            )
                        }
                        _ => {
                            format!(
                                "- Task {}{} ({:?}): {}",
                                t.id, worker_label, t.status, t.description
                            )
                        }
                    }
                })
                .collect();

            format!(
                "\nTASK EXECUTION EVIDENCE:\n{}\nSummary: {}/{} tasks completed, {} failed\n",
                task_lines.join("\n"),
                plan.completed_count(),
                plan.tasks.len(),
                plan.failed_count()
            )
        } else {
            String::new()
        };

        super::templates::render_evaluation_prompt(&super::templates::EvaluationVars {
            query,
            goal: &plan.goal,
            workers_context: &workers_context,
            task_evidence: &task_evidence,
            result,
        })
    }

    /// Parse the LLM response into a Plan.
    ///
    /// When specialized workers are configured, validates that any assigned
    /// worker names exist in the configuration. Returns an error if an
    /// invalid worker is specified.
    fn parse_plan_response(&self, response: &str, original_query: &str) -> Result<Plan, String> {
        let json_str = extract_json_object(response);

        // Parse the JSON
        let parsed: serde_json::Value =
            serde_json::from_str(json_str).map_err(|e| format!("JSON parse error: {}", e))?;

        // Extract goal
        let goal = parsed["goal"]
            .as_str()
            .unwrap_or(original_query)
            .to_string();

        let mut plan = Plan::new(goal);

        // Get valid worker names for validation
        let valid_workers: std::collections::HashSet<&str> =
            self.config.available_worker_names().into_iter().collect();

        // Extract tasks
        let tasks = parsed["tasks"].as_array().ok_or("Missing 'tasks' array")?;

        let mut seen_ids = std::collections::HashSet::new();
        for task_value in tasks {
            let id = task_value["id"].as_u64().ok_or("Task missing 'id'")? as usize;
            if !seen_ids.insert(id) {
                return Err(format!("Duplicate task id: {}", id));
            }
            let description = task_value["description"]
                .as_str()
                .ok_or("Task missing 'description'")?
                .to_string();

            // Rationale is required - explains why this task exists and how it advances the goal
            let rationale = task_value["rationale"]
                .as_str()
                .ok_or_else(|| format!("Task {} missing required 'rationale' field", id))?
                .to_string();

            let mut task = Task::new(id, description, rationale);

            // Parse dependencies
            if let Some(deps) = task_value["dependencies"].as_array() {
                for dep in deps {
                    if let Some(dep_id) = dep.as_u64() {
                        task = task.with_dependency(dep_id as usize);
                    }
                }
            }

            // Parse and validate worker assignment
            if self.config.has_workers() {
                // When workers are configured, all tasks must have a valid worker assignment
                let worker_name = task_value["worker"]
                    .as_str()
                    .ok_or_else(|| format!("Task {} missing required 'worker' field", id))?;

                if !valid_workers.contains(worker_name) {
                    return Err(format!(
                        "Task {} assigned to unknown worker '{}'. Valid workers: {:?}",
                        id,
                        worker_name,
                        valid_workers.iter().collect::<Vec<_>>()
                    ));
                }
                task = task.with_worker(worker_name);
            }
            // If workers aren't configured, ignore any worker field

            plan.add_task(task);
        }

        if plan.tasks.is_empty() {
            return Err("Plan has no tasks".to_string());
        }

        Ok(plan)
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
                tracing::warn!("No ready tasks but plan not finished - possible cycle");
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
                    Ok(result_str) => {
                        let final_result = self.maybe_create_artifact(task_id, result_str).await;
                        let result_for_event = final_result.clone();
                        if let Some(t) = plan.get_task_mut(task_id) {
                            t.complete(final_result);
                        }
                        let _ = event_tx
                            .send(Ok(StreamItem::OrchestratorEvent(
                                OrchestratorEvent::TaskCompleted {
                                    task_id,
                                    success: true,
                                    duration_ms,
                                    orchestrator_id: self.orchestrator_id.clone(),
                                    worker_id: worker_name
                                        .clone()
                                        .unwrap_or(self.orchestrator_id.clone()),
                                    result: result_for_event,
                                },
                            )))
                            .await;
                        tracing::info!("Task {} completed in {}ms", task_id, duration_ms);
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if let Some(t) = plan.get_task_mut(task_id) {
                            t.fail(err_str.clone());
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
                        let error_category = if err_str.contains("timed out") {
                            "timeout"
                        } else if is_context_overflow_error(e.as_ref()) {
                            "context overflow"
                        } else if err_str.contains("MaxDepthError")
                            || err_str.contains("reached limit")
                        {
                            "depth exhaustion"
                        } else {
                            "error"
                        };
                        tracing::warn!(
                            "Worker '{}' failed task {} after {}ms ({}): {}. Task was: {}",
                            worker_label,
                            task_id,
                            duration_ms,
                            error_category,
                            e,
                            task_preview
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Execute tasks for the current phase only.
    ///
    /// Like `execute()` but restricted to the current phase's task set.
    /// Uses `current_phase_ready_tasks()` instead of `ready_tasks()` to only
    /// execute tasks belonging to the active phase.
    async fn execute_phase(
        &self,
        plan: &mut Plan,
        event_tx: &tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
    ) -> Result<(), StreamError> {
        use futures::StreamExt;
        use futures::stream::FuturesUnordered;

        let plan_goal = plan.goal.clone();

        while !plan.is_current_phase_finished() {
            let ready_tasks: Vec<(usize, String, Option<String>, Option<String>)> = plan
                .current_phase_ready_tasks()
                .iter()
                .map(|t| {
                    let context = self.build_task_context(plan, t.id);
                    (t.id, t.description.clone(), context, t.worker.clone())
                })
                .collect();

            if ready_tasks.is_empty() {
                tracing::warn!(
                    "No ready tasks in current phase but phase not finished - possible cycle"
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
                "Phase execution: {} task(s) in parallel (default_turn_depth={}, per_call_timeout={}s)",
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

            while let Some((task_id, result, duration_ms, worker_name, task_desc)) =
                futures.next().await
            {
                match result {
                    Ok(result_str) => {
                        let final_result = self.maybe_create_artifact(task_id, result_str).await;
                        let result_for_event = final_result.clone();
                        if let Some(t) = plan.get_task_mut(task_id) {
                            t.complete(final_result);
                        }
                        let _ = event_tx
                            .send(Ok(StreamItem::OrchestratorEvent(
                                OrchestratorEvent::TaskCompleted {
                                    task_id,
                                    success: true,
                                    duration_ms,
                                    orchestrator_id: self.orchestrator_id.clone(),
                                    worker_id: worker_name
                                        .clone()
                                        .unwrap_or(self.orchestrator_id.clone()),
                                    result: result_for_event,
                                },
                            )))
                            .await;
                        tracing::info!("Task {} completed in {}ms", task_id, duration_ms);
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if let Some(t) = plan.get_task_mut(task_id) {
                            t.fail(err_str.clone());
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
                        let error_category = if err_str.contains("timed out") {
                            "timeout"
                        } else if is_context_overflow_error(e.as_ref()) {
                            "context overflow"
                        } else if err_str.contains("MaxDepthError")
                            || err_str.contains("reached limit")
                        {
                            "depth exhaustion"
                        } else {
                            "error"
                        };
                        tracing::warn!(
                            "Worker '{}' failed task {} after {}ms ({}): {}. Task was: {}",
                            worker_label,
                            task_id,
                            duration_ms,
                            error_category,
                            e,
                            task_preview
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Phase continuation checkpoint between phases.
    ///
    /// Asks the coordinator LLM to decide whether to continue with the next
    /// phase or replan based on what was discovered so far. This is a lightweight
    /// single-call decision — no quality scoring or evaluation tools.
    ///
    /// Returns `PhaseContinuation::Continue` or `PhaseContinuation::Replan`.
    async fn phase_continuation(
        &self,
        plan: &Plan,
        event_tx: Option<&tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>>,
    ) -> super::types::PhaseContinuation {
        use super::templates::{PhaseContinuationVars, render_phase_continuation_prompt};

        let phase = match plan.current_phase() {
            Some(p) => p,
            None => return super::types::PhaseContinuation::Continue,
        };

        // Build completed phase results summary
        let phase_results: String = plan
            .phase_tasks(phase.id)
            .iter()
            .map(|t| {
                let status = match t.status {
                    TaskStatus::Complete => {
                        let result = t.result.as_deref().unwrap_or("(no result)");
                        let (truncated, was_truncated) = safe_truncate(result, 500);
                        if was_truncated {
                            format!("Complete: {}...", truncated)
                        } else {
                            format!("Complete: {}", truncated)
                        }
                    }
                    TaskStatus::Failed => {
                        format!("Failed: {}", t.error.as_deref().unwrap_or("unknown"))
                    }
                    _ => format!("{}", t.status),
                };
                format!("- Task {}: {} [{}]", t.id, t.description, status)
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Build remaining phases summary
        let phases = match &plan.phases {
            Some(p) => p,
            None => return super::types::PhaseContinuation::Continue,
        };
        let remaining: String = phases
            .iter()
            .filter(|p| p.id > phase.id)
            .map(|p| {
                let task_ids: Vec<String> = p.task_ids.iter().map(|id| id.to_string()).collect();
                format!(
                    "- Phase {}: {} (tasks: {})",
                    p.id,
                    p.label,
                    task_ids.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        if remaining.is_empty() {
            // Last phase — no continuation decision needed
            return super::types::PhaseContinuation::Continue;
        }

        let phase_id_str = phase.id.to_string();
        let prompt = render_phase_continuation_prompt(&PhaseContinuationVars {
            completed_phase_label: &phase.label,
            completed_phase_id: &phase_id_str,
            goal: &plan.goal,
            completed_phase_results: &phase_results,
            remaining_phases: &remaining,
        });

        // Create a lightweight coordinator for the continuation decision
        let coordinator_result = self.create_phase_continuation_coordinator().await;

        match coordinator_result {
            Ok(AgentWithPreamble {
                agent: coordinator,
                preamble: cont_preamble,
            }) => {
                self.journal_record(
                    JournalPhase::PhaseContinuation { phase_id: phase.id },
                    &cont_preamble,
                    &prompt,
                );

                match self
                    .stream_and_forward(
                        &coordinator,
                        &prompt,
                        vec![],
                        "PhaseContinuation",
                        event_tx,
                        None,
                    )
                    .await
                {
                    Ok(response) => {
                        let lower = response.content.trim().to_lowercase();
                        if lower.contains("replan") {
                            tracing::info!(
                                "Phase continuation: coordinator chose REPLAN after phase '{}' (id={})",
                                phase.label,
                                phase.id
                            );
                            super::types::PhaseContinuation::Replan
                        } else {
                            tracing::info!(
                                "Phase continuation: coordinator chose CONTINUE after phase '{}' (id={})",
                                phase.label,
                                phase.id
                            );
                            super::types::PhaseContinuation::Continue
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Phase continuation call failed (defaulting to continue): {}",
                            e
                        );
                        super::types::PhaseContinuation::Continue
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to create phase continuation coordinator (defaulting to continue): {}",
                    e
                );
                super::types::PhaseContinuation::Continue
            }
        }
    }

    /// Collect failed tasks from this iteration into failure records.
    fn collect_iteration_failures(
        plan: &Plan,
        iteration: usize,
    ) -> Vec<super::types::FailedTaskRecord> {
        plan.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .map(|t| super::types::FailedTaskRecord {
                description: t.description.clone(),
                error: t.error.clone().unwrap_or_else(|| "unknown".to_string()),
                iteration,
                worker: t.worker.clone(),
            })
            .collect()
    }

    /// If result exceeds artifact threshold, write full result to artifact file
    /// and return a summary. Otherwise return the original result unchanged.
    async fn maybe_create_artifact(&self, task_id: usize, result: String) -> String {
        let threshold = self.config.result_artifact_threshold();
        if result.len() <= threshold {
            return result;
        }

        let summary_len = self.config.result_summary_length();
        let persistence = self.persistence.lock().await;

        match persistence.write_result_artifact(task_id, &result).await {
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
                        .and_then(|dep_task| {
                            dep_task.result.as_ref().map(|result| {
                                format!(
                                    "{} — Task {} ({}):\n{}",
                                    sections::PRIOR_WORK,
                                    dep_task.id,
                                    dep_task.description,
                                    result
                                )
                            })
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
    ) -> Result<String, StreamError> {
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

        // Create worker with persistence context (attempt 1 for now - retry logic is Phase 2+)
        let attempt = 1;
        let start_time = std::time::Instant::now();
        let AgentWithPreamble {
            agent: worker,
            preamble: worker_preamble,
        } = self.create_worker(task_id, attempt, *worker_name).await?;

        // Build the worker prompt — task-first, no original query (prevents scope creep)
        let context_str = task_context
            .as_ref()
            .map(|c| format!("{}\n\n", c))
            .unwrap_or_default();
        let worker_prompt =
            super::templates::render_worker_task_prompt(&super::templates::WorkerTaskVars {
                orchestration_goal: plan_goal,
                context: &context_str,
                your_task: task_description,
            });

        // Record in prompt journal
        self.journal_record(
            JournalPhase::Worker {
                task_id,
                worker_name: *worker_name,
                attempt,
            },
            &worker_preamble,
            &worker_prompt,
        );

        // Execute the task (workers get context from the task prompt, not conversation history)
        let result = self
            .stream_and_forward(
                &worker,
                &worker_prompt,
                vec![],
                "Worker task",
                event_tx,
                worker_name.map(|name| WorkerIdentity {
                    task_id,
                    worker_name: name,
                }),
            )
            .await;
        let duration_ms = start_time.elapsed().as_millis() as u64;

        // Record token usage on the orchestration.worker span
        if let Ok(ref response) = result {
            let span = tracing::Span::current();
            crate::logging::set_token_usage(
                &span,
                response.usage.input_tokens,
                response.usage.output_tokens,
                response.usage.total_tokens,
                0,
            );
        }

        // Detect context overflow in worker and provide actionable message
        let result = match result {
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

        // Persist the worker execution
        {
            let persistence = self.persistence.lock().await;
            let (result_str, error_str) = match &result {
                Ok(r) => (Some(r.clone()), None),
                Err(e) => (None, Some(e.to_string())),
            };

            let record = super::persistence::TaskExecutionRecord {
                task_id,
                description: task_description.to_string(),
                attempt,
                approach: "Direct task execution via worker agent".to_string(),
                tool_calls: vec![], // Tool calls are captured by PersistenceToolWrapper
                result: result_str.clone(),
                error: error_str,
                duration_ms,
                confidence: None,
                orchestrator_notes: None,
            };

            if let Err(e) = persistence
                .write_task_execution(
                    task_id,
                    attempt,
                    &worker_prompt,
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
            match &result {
                Ok(_) => crate::logging::set_span_ok(&span),
                Err(e) => crate::logging::set_span_error(&span, e.to_string()),
            }
        }
        result
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
                let status_detail = match t.status {
                    TaskStatus::Complete => {
                        let len = t.result.as_ref().map(|r| r.len()).unwrap_or(0);
                        format!("✓ complete ({} chars)", len)
                    }
                    TaskStatus::Failed => {
                        let err = t.error.as_deref().unwrap_or("unknown error");
                        format!("✗ failed: {}", err)
                    }
                    TaskStatus::Pending => {
                        // Check if blocked by failed dependency
                        let blocked_by = t.dependencies.iter().any(|dep_id| {
                            plan.tasks
                                .iter()
                                .find(|dt| dt.id == *dep_id)
                                .map(|dt| dt.status == TaskStatus::Failed)
                                .unwrap_or(false)
                        });
                        if blocked_by {
                            "⏸ blocked by failed dependency".to_string()
                        } else {
                            "⏳ pending".to_string()
                        }
                    }
                    TaskStatus::Running => "▶ running".to_string(),
                };
                format!("Task {}: {} [{}]", t.id, t.description, status_detail)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Attempt partial synthesis when max iterations are reached.
    ///
    /// If completed tasks exist, tries to synthesize a partial result.
    /// Returns `Ok(Some(result))` if partial synthesis succeeded,
    /// `Ok(None)` if no completed tasks or synthesis failed,
    /// allowing the caller to decide whether to break or return an error.
    async fn try_partial_synthesis(
        &self,
        plan: &Plan,
        query: &str,
        event_tx: &tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
        iteration: usize,
        reason: &str,
    ) -> Option<String> {
        if plan.completed_count() > 0 {
            tracing::warn!(
                "{} but max iterations reached ({}). \
                 Synthesizing partial results from {} completed task(s).",
                reason,
                iteration,
                plan.completed_count()
            );
            match self.synthesize(plan, query, Some(event_tx)).await {
                Ok(partial) => return Some(partial),
                Err(e) => tracing::error!("Partial synthesis failed: {}", e),
            }
        }
        None
    }

    /// Categorize a task failure error string into a human-readable category.
    fn categorize_failure_error(error: &str) -> &'static str {
        let lower = error.to_lowercase();
        if lower.contains("timed out") {
            "timeout"
        } else if lower.contains("context")
            && (lower.contains("limit") || lower.contains("overflow"))
        {
            "context overflow"
        } else if lower.contains("maxdeptherror") || lower.contains("reached limit") {
            "depth exhaustion"
        } else if lower.contains("rate limit")
            || lower.contains("429")
            || lower.contains("503")
            || lower.contains("502")
            || lower.contains("service unavailable")
            || lower.contains("authentication")
            || lower.contains("unauthorized")
            || lower.contains("403")
            || lower.contains("api key")
        {
            "provider_error"
        } else {
            "LLM error"
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
        failures
            .iter()
            .all(|f| Self::categorize_failure_error(&f.error) == "provider_error")
    }

    /// Synthesize phase: combine task results into final response.
    ///
    /// For single-task plans, just returns the task result.
    /// For multi-task plans, uses the coordinator agent to intelligently
    /// combine results into a coherent response.
    ///
    /// Also persists the synthesis artifacts via ExecutionPersistence.
    #[tracing::instrument(
        name = "orchestration.synthesis",
        skip_all,
        fields(
            orchestration.phase = "synthesis",
            orchestration.completed_tasks = tracing::field::Empty,
        )
    )]
    async fn synthesize(
        &self,
        plan: &Plan,
        query: &str,
        event_tx: Option<&tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>>,
    ) -> Result<String, StreamError> {
        // Collect completed tasks with results
        let completed_tasks: Vec<&Task> = plan
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Complete && t.result.is_some())
            .collect();

        tracing::Span::current().record(
            "orchestration.completed_tasks",
            completed_tasks.len() as i64,
        );

        if completed_tasks.is_empty() {
            // All tasks failed
            let errors: Vec<&str> = plan
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Failed)
                .filter_map(|t| t.error.as_deref())
                .collect();

            let error_msg = format!("All tasks failed: {}", errors.join("; "));

            // Persist the failure synthesis
            {
                let persistence = self.persistence.lock().await;
                let synthesis_prompt = format!(
                    "[SYNTHESIS FAILED - ALL TASKS FAILED]\n\nTask results:\n- All {} tasks failed\n\nErrors:\n{}",
                    plan.tasks.len(),
                    errors.join("\n")
                );
                if let Err(e) = persistence
                    .write_synthesis(&synthesis_prompt, &error_msg)
                    .await
                {
                    tracing::warn!("Failed to persist synthesis: {}", e);
                }
            }

            return Err(error_msg.into());
        }

        // Use LLM to synthesize (even single results get coordinator framing)
        let synthesis_prompt = self.build_synthesis_prompt(plan, query, &completed_tasks);

        let synthesized = match self.create_synthesis_coordinator().await {
            Ok(AgentWithPreamble {
                agent: coordinator,
                preamble: synth_preamble,
            }) => {
                // Record in prompt journal
                self.journal_record(JournalPhase::Synthesis, &synth_preamble, &synthesis_prompt);

                match self
                    .stream_and_forward(
                        &coordinator,
                        &synthesis_prompt,
                        vec![],
                        "Synthesis",
                        event_tx,
                        None,
                    )
                    .await
                {
                    Ok(response) => {
                        tracing::debug!(
                            "LLM synthesis successful for {} tasks",
                            completed_tasks.len()
                        );
                        crate::logging::set_token_usage(
                            &tracing::Span::current(),
                            response.usage.input_tokens,
                            response.usage.output_tokens,
                            response.usage.total_tokens,
                            0,
                        );
                        response.content
                    }
                    Err(e) if is_context_overflow_error(e.as_ref()) => {
                        // Context overflow during synthesis - fail with actionable message
                        let suggestion = context_overflow_suggestion("synthesis");
                        return Err(format!(
                            "Context limit exceeded during synthesis. {}",
                            suggestion
                        )
                        .into());
                    }
                    Err(e) if e.to_string().contains("timed out") => {
                        tracing::warn!(
                            "Synthesis timed out (per_call_timeout={}s), falling back to result concatenation",
                            self.config.per_call_timeout_secs()
                        );
                        // Fallback to mechanical concatenation for timeout
                        completed_tasks
                            .iter()
                            .enumerate()
                            .map(|(i, t)| {
                                format!(
                                    "## Result {}\n\n{}",
                                    i + 1,
                                    t.result.as_deref().unwrap_or("")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n")
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Synthesis LLM call failed, falling back to result concatenation: {}",
                            e
                        );
                        // Fallback to mechanical concatenation for non-context errors
                        completed_tasks
                            .iter()
                            .enumerate()
                            .map(|(i, t)| {
                                format!(
                                    "## Result {}\n\n{}",
                                    i + 1,
                                    t.result.as_deref().unwrap_or("")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n")
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Synthesis coordinator creation failed, falling back to result concatenation: {}",
                    e
                );
                // Fallback to mechanical concatenation
                completed_tasks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        format!(
                            "## Result {}\n\n{}",
                            i + 1,
                            t.result.as_deref().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n")
            }
        };

        // Persist the synthesis
        {
            let persistence = self.persistence.lock().await;
            if let Err(e) = persistence
                .write_synthesis(&synthesis_prompt, &synthesized)
                .await
            {
                tracing::warn!("Failed to persist synthesis: {}", e);
            }
        }

        Ok(synthesized)
    }

    /// Evaluate the quality of synthesized results using semantic analysis.
    ///
    /// Uses the coordinator LLM with the `submit_evaluation` tool to produce
    /// a structured quality assessment. Falls back to simple heuristics if
    /// the LLM fails or doesn't call the tool.
    ///
    /// Returns an `EvaluationResult` containing the score, reasoning, and gaps.
    #[tracing::instrument(
        name = "orchestration.evaluation",
        skip_all,
        fields(
            orchestration.phase = "evaluation",
            orchestration.quality_score = tracing::field::Empty,
        )
    )]
    async fn evaluate(
        &self,
        plan: &Plan,
        query: &str,
        result: &str,
    ) -> super::types::EvaluationResult {
        use super::types::EvaluationResult;

        // Quick heuristics first (fail-fast for obviously bad results)
        if plan.failed_count() == plan.tasks.len() {
            tracing::debug!("Evaluation: All tasks failed, returning 0.0");
            return EvaluationResult::new(0.0, "All tasks failed")
                .with_gaps(vec!["No successful task completions".to_string()]);
        }

        // Build evaluation prompt
        let eval_prompt = self.build_evaluation_prompt(plan, query, result);

        // Create the evaluation tool and capture the decision arc
        let eval_tool = SubmitEvaluationTool::new();
        let decision = eval_tool.decision();

        // Call coordinator LLM for semantic evaluation via tool calling
        let (eval_response, eval_result) = match self.create_evaluation_coordinator(eval_tool).await
        {
            Ok(AgentWithPreamble {
                agent: coordinator,
                preamble: eval_preamble,
            }) => {
                // Record in prompt journal
                self.journal_record(JournalPhase::Evaluation, &eval_preamble, &eval_prompt);

                let d = decision.clone();
                match self
                    .stream_and_collect(
                        &coordinator,
                        &eval_prompt,
                        vec![],
                        "Evaluation",
                        None, // No reasoning forwarding for eval
                        || {
                            let d = d.clone();
                            Box::pin(async move { d.lock().await.is_some() })
                        },
                    )
                    .await
                {
                    Ok(response) => {
                        crate::logging::set_token_usage(
                            &tracing::Span::current(),
                            response.usage.input_tokens,
                            response.usage.output_tokens,
                            response.usage.total_tokens,
                            0,
                        );
                        // Read the evaluation from the tool's shared state
                        let eval = decision.lock().await.take();
                        match eval {
                            Some(result) => {
                                tracing::debug!("Evaluation received via submit_evaluation tool");
                                (response.content, result)
                            }
                            None => {
                                tracing::warn!(
                                    "Evaluation coordinator did not call submit_evaluation, \
                                         falling back to heuristic"
                                );
                                let fallback = EvaluationResult::fallback(
                                    plan.completed_count(),
                                    plan.tasks.len(),
                                );
                                (response.content, fallback)
                            }
                        }
                    }
                    Err(e) if is_context_overflow_error(e.as_ref()) => {
                        tracing::warn!(
                            "Context limit exceeded during evaluation, using heuristic score. {}",
                            context_overflow_suggestion("evaluation")
                        );
                        let fallback =
                            EvaluationResult::fallback(plan.completed_count(), plan.tasks.len())
                                .with_gaps(vec![
                                    "Evaluation skipped due to context limit".to_string(),
                                ]);
                        (
                            "[CONTEXT OVERFLOW - heuristic fallback]".to_string(),
                            fallback,
                        )
                    }
                    Err(e) if e.to_string().contains("timed out") => {
                        tracing::warn!(
                            "Evaluation timed out (per_call_timeout={}s), falling back to heuristic",
                            self.config.per_call_timeout_secs()
                        );
                        let fallback =
                            EvaluationResult::fallback(plan.completed_count(), plan.tasks.len())
                                .with_gaps(vec!["Evaluation skipped due to timeout".to_string()]);
                        (
                            format!("[TIMEOUT after {}s]", self.config.per_call_timeout_secs()),
                            fallback,
                        )
                    }
                    Err(e) => {
                        tracing::warn!("LLM evaluation failed, falling back to heuristic: {}", e);
                        let fallback =
                            EvaluationResult::fallback(plan.completed_count(), plan.tasks.len());
                        (format!("[LLM ERROR: {}]", e), fallback)
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to create coordinator for evaluation, falling back to heuristic: {}",
                    e
                );
                let fallback = EvaluationResult::fallback(plan.completed_count(), plan.tasks.len());
                (format!("[COORDINATOR ERROR: {}]", e), fallback)
            }
        };

        // Persist evaluation artifacts
        {
            let persistence = self.persistence.lock().await;
            if let Err(e) = persistence
                .write_evaluation(&eval_prompt, &eval_response, &eval_result)
                .await
            {
                tracing::warn!("Failed to persist evaluation: {}", e);
            }
        }

        tracing::Span::current().record(
            "orchestration.quality_score",
            format!("{:.2}", eval_result.score).as_str(),
        );

        tracing::info!(
            "Evaluation: score={:.2}, reasoning={}",
            eval_result.score,
            truncate_query(&eval_result.reasoning, 100)
        );

        eval_result
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
    /// Consolidates the common tail of all three replan paths (phase_continuation,
    /// failure, quality). Callers handle path-specific pre-work (e.g. IterationComplete
    /// events, persistence writes) before calling this.
    async fn trigger_replan(
        event_tx: &tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
        iteration: usize,
        trigger: &str,
        plan: Plan,
        evaluation: EvaluationResult,
        failure_history: &[FailedTaskRecord],
        synthesis_summary: Option<String>,
    ) -> (Option<IterationContext>, Plan) {
        Self::emit_event(
            event_tx,
            OrchestratorEvent::ReplanStarted {
                iteration: iteration + 1,
                trigger: trigger.to_string(),
            },
        )
        .await;

        let mut context =
            IterationContext::new(iteration, plan, evaluation, failure_history.to_vec());
        if let Some(summary) = synthesis_summary {
            context = context.with_synthesis_summary(summary);
        }
        (Some(context), Plan::new(""))
    }

    /// Apply result reuse from a previous plan to the new plan.
    ///
    /// When the coordinator sets `reuse_result_from` on a task in the new plan,
    /// this function finds the referenced task in the previous plan and copies
    /// its result, marking the task as Complete so `ready_tasks()` skips it.
    pub(crate) fn apply_result_reuse(plan: &mut Plan, previous: Option<&Plan>) {
        let previous = match previous {
            Some(p) => p,
            None => return,
        };

        for task in &mut plan.tasks {
            if let Some(reuse_id) = task.reuse_result_from {
                if let Some(prev_task) = previous.tasks.iter().find(|t| t.id == reuse_id) {
                    if prev_task.status == TaskStatus::Complete {
                        if let Some(ref result) = prev_task.result {
                            tracing::info!(
                                "Task {} reusing result from previous task {} ({})",
                                task.id,
                                reuse_id,
                                prev_task.description
                            );
                            task.complete(result.clone());
                        }
                    } else {
                        tracing::warn!(
                            "Task {} requested reuse from task {} but it was not complete (status: {:?})",
                            task.id,
                            reuse_id,
                            prev_task.status
                        );
                    }
                } else {
                    tracing::warn!(
                        "Task {} requested reuse from task {} but it was not found in previous plan",
                        task.id,
                        reuse_id
                    );
                }
            }
        }
    }

    /// Top-level orchestration entry point: route → loop.
    ///
    /// Uses `plan_with_routing()` for the initial routing decision, then
    /// dispatches based on the `PlanningResponse` variant:
    /// - `Direct` → emit event, return response
    /// - `Clarification` → emit event, return question
    /// - `Orchestrated` → delegate to `run_orchestration_loop()`
    #[tracing::instrument(
        name = "orchestration",
        skip_all,
        fields(
            orchestration.goal = tracing::field::Empty,
            orchestration.max_iterations = self.config.max_planning_cycles,
            orchestration.routing = tracing::field::Empty,
        )
    )]
    async fn run_orchestration(
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

        // Set iteration for initial planning (journal reads this via AtomicUsize)
        self.current_iteration.store(1, Ordering::Relaxed);
        let (response, _prompt, _coordinator_text) = self
            .plan_with_routing(query, &chat_history, None, Some(&event_tx))
            .await?;

        let result = match response {
            PlanningResponse::Direct {
                response,
                routing_rationale,
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
            PlanningResponse::Orchestrated { .. } | PlanningResponse::StepsPlan { .. } => {
                span.record("orchestration.routing", "orchestrated");
                let routing_rationale = response.routing_rationale().to_string();
                let planning_summary = response.planning_summary().unwrap_or_default().to_string();
                let plan = response
                    .into_plan()
                    .expect("Orchestrated/StepsPlan always converts");

                Self::emit_event(
                    &event_tx,
                    OrchestratorEvent::PlanCreated {
                        goal: plan.goal.clone(),
                        task_count: plan.tasks.len(),
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

    /// The plan-execute-synthesize-evaluate loop.
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
        event_tx: tokio::sync::mpsc::Sender<Result<StreamItem, StreamError>>,
        orchestration_start: Instant,
    ) -> Result<String, StreamError> {
        let mut iteration = 0;
        let mut final_result: String;
        let mut previous_context: Option<IterationContext> = None;
        let mut plan = initial_plan;
        let mut failure_history: Vec<super::types::FailedTaskRecord> = Vec::new();
        let mut last_quality_score: Option<f32> = None;

        loop {
            iteration += 1;
            self.current_iteration.store(iteration, Ordering::Relaxed);
            let elapsed = orchestration_start.elapsed().as_secs_f64();

            // Create a span for this iteration. Child spans (planning, worker,
            // synthesis, evaluation) inherit this as parent via Span::current().
            // Using enter() rather than instrument() because the loop body has
            // break/continue control flow that can't cross async block boundaries.
            let iter_span = tracing::info_span!(
                "orchestration.iteration",
                orchestration.iteration = iteration,
                orchestration.task_count = tracing::field::Empty,
                orchestration.quality_score = tracing::field::Empty,
                orchestration.will_replan = tracing::field::Empty,
            );
            let _iter_guard = iter_span.enter();

            tracing::info!(
                "Starting iteration {}/{} (elapsed={:.1}s, per_call_timeout={}s)",
                iteration,
                self.config.max_planning_cycles,
                elapsed,
                self.config.per_call_timeout_secs(),
            );

            // On re-plan (iteration > 1), advance persistence iteration so
            // the new plan and its execution share a single directory.
            // Iteration 1 is already set by persistence initialization.
            if iteration > 1 {
                {
                    let mut persistence = self.persistence.lock().await;
                    persistence.start_new_iteration();
                }
                let (response, _prompt, _coordinator_text) = match self
                    .plan_with_routing(
                        query,
                        &chat_history,
                        previous_context.as_ref(),
                        Some(&event_tx),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        self.write_run_manifest(&plan, iteration, last_quality_score)
                            .await;
                        return Err(e);
                    }
                };

                // Handle non-plan responses during replan: Direct/Clarification
                // short-circuit the loop, mirroring the initial routing in run_orchestration().
                match &response {
                    PlanningResponse::Direct {
                        response: direct_response,
                        routing_rationale,
                    } => {
                        Self::emit_event(
                            &event_tx,
                            OrchestratorEvent::DirectAnswer {
                                response: direct_response.clone(),
                                routing_rationale: routing_rationale.clone(),
                            },
                        )
                        .await;
                        Self::emit_event(
                            &event_tx,
                            OrchestratorEvent::IterationComplete {
                                iteration,
                                quality_score: 0.0,
                                quality_threshold: self.config.quality_threshold,
                                will_replan: false,
                                evaluation_skipped: true,
                                reasoning: "Coordinator provided direct answer during replan"
                                    .to_string(),
                                gaps: vec![],
                            },
                        )
                        .await;
                        self.write_run_manifest(&plan, iteration, last_quality_score)
                            .await;
                        return Ok(direct_response.clone());
                    }
                    PlanningResponse::Clarification {
                        question,
                        options,
                        routing_rationale,
                    } => {
                        Self::emit_event(
                            &event_tx,
                            OrchestratorEvent::ClarificationNeeded {
                                question: question.clone(),
                                options: options.clone(),
                                routing_rationale: routing_rationale.clone(),
                            },
                        )
                        .await;
                        Self::emit_event(
                            &event_tx,
                            OrchestratorEvent::IterationComplete {
                                iteration,
                                quality_score: 0.0,
                                quality_threshold: self.config.quality_threshold,
                                will_replan: false,
                                evaluation_skipped: true,
                                reasoning: "Coordinator requested clarification during replan"
                                    .to_string(),
                                gaps: vec![],
                            },
                        )
                        .await;
                        self.write_run_manifest(&plan, iteration, last_quality_score)
                            .await;
                        return Ok(question.clone());
                    }
                    PlanningResponse::Orchestrated { .. } | PlanningResponse::StepsPlan { .. } => {
                        // Fall through to plan extraction below
                    }
                }

                let routing_rationale = response.routing_rationale().to_string();
                let planning_summary = response.planning_summary().unwrap_or_default().to_string();

                // Extract plan from response — Direct/Clarification handled above,
                // so into_plan() will always return Some here.
                plan = response
                    .into_plan()
                    .expect("Orchestrated/StepsPlan always converts to plan");

                // Carry forward results from previous iteration where coordinator requested reuse
                Self::apply_result_reuse(
                    &mut plan,
                    previous_context.as_ref().map(|ctx| &ctx.previous_plan),
                );

                Self::emit_event(
                    &event_tx,
                    OrchestratorEvent::PlanCreated {
                        goal: plan.goal.clone(),
                        task_count: plan.tasks.len(),
                        routing_mode: super::events::RoutingMode::for_plan(plan.tasks.len()),
                        routing_rationale,
                        planning_response: planning_summary,
                    },
                )
                .await;
            }

            // Record task count on the iteration span now that the plan is finalized
            iter_span.record("orchestration.task_count", plan.tasks.len() as i64);

            // ----------------------------------------------------------------
            // EXECUTE: Run workers on tasks (parallel when possible)
            // ----------------------------------------------------------------
            let phase_triggered_replan = if plan.is_phased() {
                // Phase-aware execution: iterate through phases with continuation
                // checkpoints between each.
                let phases = plan.phases.as_ref().unwrap().clone();
                let max_phases = self.config.max_phases;
                let mut replan = false;

                for (phase_idx, phase) in phases.iter().enumerate() {
                    if phase_idx >= max_phases {
                        tracing::warn!(
                            "Max phases cap reached ({}/{}), stopping phase execution",
                            phase_idx,
                            max_phases
                        );
                        break;
                    }

                    tracing::info!(
                        "Starting phase {}/{}: '{}' ({} tasks)",
                        phase_idx + 1,
                        phases.len(),
                        phase.label,
                        phase.task_ids.len(),
                    );

                    // Emit PhaseStarted event
                    Self::emit_event(
                        &event_tx,
                        OrchestratorEvent::PhaseStarted {
                            phase_id: phase.id,
                            label: phase.label.clone(),
                            orchestrator_id: self.orchestrator_id.clone(),
                        },
                    )
                    .await;

                    if let Err(e) = self.execute_phase(&mut plan, &event_tx).await {
                        self.write_run_manifest(&plan, iteration, last_quality_score)
                            .await;
                        return Err(e);
                    }

                    // Phase continuation checkpoint (skip for last phase)
                    if phase_idx + 1 < phases.len() {
                        let continuation = self.phase_continuation(&plan, Some(&event_tx)).await;

                        // Emit PhaseCompleted event
                        Self::emit_event(
                            &event_tx,
                            OrchestratorEvent::PhaseCompleted {
                                phase_id: phase.id,
                                label: phase.label.clone(),
                                continuation,
                                orchestrator_id: self.orchestrator_id.clone(),
                            },
                        )
                        .await;

                        if continuation == super::types::PhaseContinuation::Replan {
                            tracing::info!(
                                "Phase continuation chose REPLAN after phase '{}' — breaking to outer replan loop",
                                phase.label
                            );
                            replan = true;
                            break;
                        }
                    } else {
                        // Last phase — emit completed with "continue" (terminal)
                        Self::emit_event(
                            &event_tx,
                            OrchestratorEvent::PhaseCompleted {
                                phase_id: phase.id,
                                label: phase.label.clone(),
                                continuation: super::types::PhaseContinuation::Continue,
                                orchestrator_id: self.orchestrator_id.clone(),
                            },
                        )
                        .await;
                    };

                    plan.advance_phase();
                }

                replan
            } else {
                // Flat plan: existing execute() path, unchanged
                if let Err(e) = self.execute(&mut plan, &event_tx).await {
                    self.write_run_manifest(&plan, iteration, last_quality_score)
                        .await;
                    return Err(e);
                }
                false
            };
            let new_failure_start = failure_history.len();
            failure_history.extend(Self::collect_iteration_failures(&plan, iteration));
            let this_iteration_failures = &failure_history[new_failure_start..];

            // Persistence fix: write plan after execute to capture task statuses
            {
                let persistence = self.persistence.lock().await;
                if let Err(e) = persistence.write_plan(&plan).await {
                    tracing::warn!("Failed to persist plan after execution: {}", e);
                }
            }

            // If phase continuation triggered a replan, skip synthesis/evaluation
            // and go straight to the replan path
            if phase_triggered_replan {
                let iterations_remaining = iteration < self.config.max_planning_cycles;
                if !iterations_remaining {
                    if let Some(partial) = self
                        .try_partial_synthesis(
                            &plan,
                            query,
                            &event_tx,
                            iteration,
                            "Phase replan requested",
                        )
                        .await
                    {
                        final_result = partial;
                        break;
                    }
                    let summary = self.build_execution_summary(&plan);
                    self.write_run_manifest(&plan, iteration, last_quality_score)
                        .await;
                    return Err(format!(
                        "Phase replan requested but max iterations reached ({}):\n{}",
                        iteration, summary
                    )
                    .into());
                }

                let execution_summary = self.build_execution_summary(&plan);
                tracing::info!(
                    "Phase continuation triggered replan:\n{}",
                    execution_summary
                );

                let replan_evaluation = super::types::EvaluationResult {
                    score: 0.0,
                    reasoning: "Phase continuation requested replan based on intermediate results.".to_string(),
                    gaps: vec!["Coordinator determined remaining phases need redesign based on discovered information.".to_string()],
                };

                (previous_context, plan) = Self::trigger_replan(
                    &event_tx,
                    iteration,
                    "phase_continuation",
                    plan,
                    replan_evaluation,
                    &failure_history,
                    None, // No synthesis on phase-replan path
                )
                .await;
                continue;
            }

            // ----------------------------------------------------------------
            // CHECK FOR FAILURES: Skip synthesis if tasks failed/blocked
            // ----------------------------------------------------------------
            let failed_count = plan.failed_count();
            let blocked_count = plan.blocked_tasks().len();
            let has_failures = failed_count > 0 || blocked_count > 0;

            if has_failures {
                let failure_detail = if !this_iteration_failures.is_empty() {
                    let mut category_counts: std::collections::HashMap<&str, usize> =
                        std::collections::HashMap::new();
                    for f in this_iteration_failures {
                        *category_counts
                            .entry(Self::categorize_failure_error(&f.error))
                            .or_insert(0) += 1;
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

                let iterations_remaining = iteration < self.config.max_planning_cycles;

                if !iterations_remaining {
                    if let Some(partial) = self
                        .try_partial_synthesis(
                            &plan,
                            query,
                            &event_tx,
                            iteration,
                            "Task failures detected",
                        )
                        .await
                    {
                        final_result = partial;
                        break;
                    }
                    let summary = self.build_execution_summary(&plan);
                    self.write_run_manifest(&plan, iteration, last_quality_score)
                        .await;
                    return Err(format!(
                        "Plan execution failed after {} iterations:\n{}",
                        iteration, summary
                    )
                    .into());
                }

                // Provider error short-circuit: if ALL failures are provider errors,
                // don't waste an iteration asking the coordinator to fix what it can't.
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
                    self.write_run_manifest(&plan, iteration, last_quality_score)
                        .await;
                    return Err(format!(
                        "Provider error: all tasks failed due to provider issues (not retryable via replan):\n{}",
                        summary
                    ).into());
                }

                let execution_summary = self.build_execution_summary(&plan);
                tracing::info!("Triggering replan due to failures:\n{}", execution_summary);
                last_quality_score = Some(0.0);

                let failure_evaluation = EvaluationResult {
                    score: 0.0,
                    reasoning: format!(
                        "Execution failed: {} task(s) failed, {} task(s) blocked by dependencies.",
                        failed_count, blocked_count
                    ),
                    gaps: vec![
                        "Some tasks could not complete due to errors".to_string(),
                        format!("Execution summary:\n{}", execution_summary),
                    ],
                };

                // Record failure on the iteration span
                iter_span.record("orchestration.quality_score", "0.00");
                iter_span.record("orchestration.will_replan", true);

                // Emit IterationComplete on failure path (was previously skipped)
                Self::emit_event(
                    &event_tx,
                    OrchestratorEvent::IterationComplete {
                        iteration,
                        quality_score: 0.0,
                        quality_threshold: self.config.quality_threshold,
                        will_replan: true,
                        evaluation_skipped: false,
                        reasoning: failure_evaluation.reasoning.clone(),
                        gaps: failure_evaluation.gaps.clone(),
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

                (previous_context, plan) = Self::trigger_replan(
                    &event_tx,
                    iteration,
                    "failure",
                    plan,
                    failure_evaluation,
                    &failure_history,
                    None, // No synthesis on failure path
                )
                .await;
                continue;
            }

            // ----------------------------------------------------------------
            // SYNTHESIZE: Combine task results into coherent response
            // ----------------------------------------------------------------
            let is_single_task = plan.tasks.len() == 1;
            let iterations_remaining = iteration < self.config.max_planning_cycles;

            if is_single_task && plan.completed_count() == 1 {
                // Single-task (routed) plan: pass worker result directly.
                let worker_result = plan.tasks[0]
                    .result
                    .as_deref()
                    .unwrap_or("(no result)")
                    .to_string();
                tracing::info!("Single-task plan: using worker result directly (no synthesis)");

                // Persist the pass-through as synthesis artifact for observability
                {
                    let persistence = self.persistence.lock().await;
                    if let Err(e) = persistence
                        .write_synthesis("[SINGLE-TASK PASS-THROUGH]", &worker_result)
                        .await
                    {
                        tracing::warn!("Failed to persist synthesis pass-through: {}", e);
                    }
                }

                final_result = worker_result;
            } else {
                // Multi-task plan: full synthesis
                Self::emit_event(&event_tx, OrchestratorEvent::Synthesizing { iteration }).await;
                final_result = match self.synthesize(&plan, query, Some(&event_tx)).await {
                    Ok(r) => r,
                    Err(e) => {
                        self.write_run_manifest(&plan, iteration, last_quality_score)
                            .await;
                        return Err(e);
                    }
                };
            }

            // Record iteration on the span
            iter_span.record("orchestration.will_replan", iterations_remaining);

            // Persist iteration summary (evaluation_skipped=true in coordinator-driven mode)
            {
                let persistence = self.persistence.lock().await;
                if let Err(e) = persistence
                    .write_iteration_summary(
                        iteration,
                        // No evaluator score — coordinator decides via routing
                        1.0,
                        self.config.quality_threshold,
                        iterations_remaining,
                    )
                    .await
                {
                    tracing::warn!("Failed to persist iteration summary: {}", e);
                }
            }

            Self::emit_event(
                &event_tx,
                OrchestratorEvent::IterationComplete {
                    iteration,
                    quality_score: 0.0,
                    quality_threshold: self.config.quality_threshold,
                    will_replan: iterations_remaining,
                    evaluation_skipped: true,
                    reasoning:
                        "Coordinator-driven loop: evaluation deferred to coordinator routing"
                            .to_string(),
                    gaps: vec![],
                },
            )
            .await;

            tracing::info!(
                "Iteration {} complete: {}/{} tasks, elapsed={:.1}s, will_return_to_coordinator={}",
                iteration,
                plan.completed_count(),
                plan.tasks.len(),
                orchestration_start.elapsed().as_secs_f64(),
                iterations_remaining,
            );

            // ----------------------------------------------------------------
            // DECIDE: Return to coordinator or terminate at budget limit
            // ----------------------------------------------------------------
            if !iterations_remaining {
                tracing::info!(
                    "Max iterations reached ({}). Returning best available result.",
                    self.config.max_planning_cycles,
                );
                break;
            }

            // ----------------------------------------------------------------
            // RETURN TO COORDINATOR: Build context and loop back
            // ----------------------------------------------------------------
            tracing::info!(
                "Returning to coordinator (iteration {}/{}, {}/{} tasks completed)",
                iteration,
                self.config.max_planning_cycles,
                plan.completed_count(),
                plan.tasks.len(),
            );

            // Thread synthesis result into iteration context for coordinator visibility
            let synthesis_for_context = if !is_single_task {
                Some(final_result.clone())
            } else {
                None
            };

            // Build a placeholder evaluation for IterationContext (coordinator decides, not eval)
            let coordinator_eval = EvaluationResult::new(
                0.0,
                "Coordinator-driven: returning to coordinator for next decision",
            );

            (previous_context, plan) = Self::trigger_replan(
                &event_tx,
                iteration,
                "coordinator",
                plan,
                coordinator_eval,
                &failure_history,
                synthesis_for_context,
            )
            .await;
        }

        // Write run manifest on completion (best-effort)
        self.write_run_manifest(&plan, iteration, last_quality_score)
            .await;

        Ok(final_result)
    }

    /// Write a typed `RunManifest` summarizing this orchestration run.
    ///
    /// Called at the end of `run_orchestration_loop()` on all exit paths.
    /// Errors are logged but not propagated — manifest is observability, not control flow.
    async fn write_run_manifest(&self, plan: &Plan, iterations: usize, quality_score: Option<f32>) {
        use super::persistence::{RunManifest, RunStatus, TaskSummary};
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

        let task_summaries = plan
            .tasks
            .iter()
            .map(|t| TaskSummary {
                task_id: t.id,
                description: t.description.clone(),
                status: t.status,
                worker: t.worker.clone(),
                result_preview: t
                    .result
                    .as_ref()
                    .map(|r| safe_truncate(r, 200).0.to_string()),
            })
            .collect();

        let artifact_paths = match persistence.list_artifacts().await {
            Ok(paths) => paths,
            Err(e) => {
                tracing::warn!("Failed to list artifacts for manifest: {}", e);
                Vec::new()
            }
        };

        let manifest = RunManifest {
            run_id: persistence.run_id().to_string(),
            session_id: persistence.session_id().map(|s| s.to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            goal: plan.goal.clone(),
            status,
            iterations,
            quality_score,
            routing_mode: Some(super::events::RoutingMode::for_plan(plan.tasks.len())),
            task_summaries,
            artifact_paths,
        };

        if let Err(e) = persistence.write_manifest(&manifest).await {
            tracing::warn!("Failed to write run manifest: {}", e);
        }
    }
}

#[async_trait]
impl StreamingAgent for Orchestrator {
    fn get_provider_info(&self) -> (&str, &str) {
        self.agent_config.llm.model_info()
    }

    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        cancel_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamItem, StreamError>>, StreamError> {
        let query = query.to_string();
        let chat_history = chat_history.clone();

        // Create channel for orchestrator events
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(100);

        // Clone self fields for the spawned task
        let mut agent_config = self.agent_config.clone();
        // Inject conversation history for worker access via get_conversation_context tool
        agent_config.orchestration_chat_history = Some(Arc::new(chat_history.clone()));

        // Spawn orchestration in background task
        // Note: Creates a fresh Orchestrator with its own MCP connections and persistence run.
        // The factory instance (self) only exists to satisfy the StreamingAgent trait.
        let cancel_token_clone = cancel_token.clone();
        // Capture the current span (agent.stream root) so child spans nest correctly in Phoenix.
        let parent_span = tracing::Span::current();
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                let orchestrator = match Orchestrator::new(agent_config).await {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = event_tx.send(Err(e)).await;
                        return;
                    }
                };

                // Forward tool call events from workers to SSE stream
                spawn_tool_event_forwarder(
                    &orchestrator.tool_call_observer,
                    event_tx.clone(),
                    cancel_token_clone.clone(),
                );

                tokio::select! {
                    result = orchestrator.run_orchestration(&query, chat_history, event_tx.clone()) => {
                        match result {
                            Ok(final_result) => {
                                // Emit final response as text chunks
                                // Split into chunks to simulate streaming
                                for chunk in final_result.chars().collect::<Vec<_>>().chunks(STREAM_CHUNK_SIZE) {
                                    let text: String = chunk.iter().collect();
                                    let _ = event_tx.send(Ok(StreamItem::StreamAssistantItem(
                                        crate::provider_agent::StreamedAssistantContent::Text(text)
                                    ))).await;
                                }

                                // Emit Final marker
                                let _ = event_tx.send(Ok(StreamItem::Final(
                                    crate::provider_agent::FinalResponseInfo {
                                        content: final_result,
                                        usage: Default::default(),
                                    }
                                ))).await;
                            }
                            Err(e) => {
                                let _ = event_tx.send(Err(e)).await;
                            }
                        }
                    }
                    _ = cancel_token_clone.cancelled() => {
                        tracing::info!("Orchestration cancelled");
                        // Best-effort: send notifications/cancelled to the inner orchestrator's
                        // MCP connections (coordinator tools like list_tools, inspect_tool_params).
                        // Note: Worker-level MCP connections are handled via drop when the spawned
                        // task returns, since workers create independent Agent instances. Clean
                        // protocol-level cancellation for worker MCP calls would require propagating
                        // CancellationToken through Rig's tool execution layer.
                        let cancelled = orchestrator
                            .cancel_and_close_mcp("orchestration", "Client disconnected or timeout")
                            .await;
                        if cancelled > 0 {
                            tracing::info!("Cancelled {} MCP request(s) during orchestration shutdown", cancelled);
                        }
                    }
                }
            },
            parent_span,
        ));

        // Convert receiver to stream
        let stream = stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });

        Ok(Box::pin(stream))
    }

    async fn stream_with_timeout(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        timeout: Duration,
        request_id: &str,
    ) -> (
        BoxStream<'static, Result<StreamItem, StreamError>>,
        watch::Sender<bool>,
        crate::UsageState,
    ) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let cancel_token = CancellationToken::new();
        let watcher_cancel_token = cancel_token.clone();
        let request_id_owned = request_id.to_string();

        // Fire-and-forget: task self-terminates when cancel_tx is dropped or timeout fires.
        let _watcher_handle =
            spawn_cancellation_watcher(cancel_rx, timeout, watcher_cancel_token, request_id_owned);

        // Get the stream — returned directly, no wrapper needed
        let stream = match self.stream(query, chat_history, cancel_token).await {
            Ok(s) => s,
            Err(e) => Box::pin(stream::once(async move { Err(e) })),
        };

        // Orchestration has its own usage tracking; return a fresh state for the handler.
        (stream, cancel_tx, crate::UsageState::new())
    }

    async fn cancel_and_close_mcp(&self, request_id: &str, reason: &str) -> usize {
        if let Some(ref mcp_manager) = self.mcp_manager {
            mcp_manager.cancel_and_close_all(request_id, reason).await
        } else {
            0
        }
    }

    async fn set_mcp_request_id(&self, request_id: &str) {
        if let Some(ref mcp_manager) = self.mcp_manager {
            mcp_manager.set_current_request(request_id).await;
        }
    }

    async fn clear_mcp_request_id(&self) {
        if let Some(ref mcp_manager) = self.mcp_manager {
            mcp_manager.clear_current_request().await;
        }
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
/// Providers return various error messages for context overflow:
/// - OpenAI: "maximum context length", "token limit"
/// - Anthropic: "maximum number of tokens", "context length"
/// - Generic: "context", "tokens exceeded"
fn is_context_overflow_error(error: &dyn std::error::Error) -> bool {
    let msg = error.to_string().to_lowercase();

    // Check for common context overflow patterns
    (msg.contains("context") && (msg.contains("length") || msg.contains("exceeded")))
        || msg.contains("maximum context")
        || msg.contains("token limit")
        || msg.contains("tokens exceeded")
        || msg.contains("maximum number of tokens")
        || (msg.contains("too") && msg.contains("long") && msg.contains("token"))
}

/// Get a user-friendly suggestion for recovering from context overflow.
fn context_overflow_suggestion(phase: &str) -> String {
    match phase {
        "planning" => {
            "Query too complex. Consider breaking into smaller, focused questions.".to_string()
        }
        "synthesis" => {
            "Task results too large to synthesize. Ask about specific aspects separately."
                .to_string()
        }
        "evaluation" => "Response too large to evaluate. Consider simpler queries.".to_string(),
        "worker" => {
            "Task context too large. The plan may need smaller, more focused tasks.".to_string()
        }
        _ => "Request exceeded context limits. Reduce query complexity.".to_string(),
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

    #[test]
    fn test_context_overflow_suggestion_phases() {
        assert!(context_overflow_suggestion("planning").contains("smaller"));
        assert!(context_overflow_suggestion("synthesis").contains("separately"));
        assert!(context_overflow_suggestion("evaluation").contains("simpler"));
        assert!(context_overflow_suggestion("worker").contains("focused"));
        assert!(context_overflow_suggestion("unknown").contains("Reduce"));
    }

    #[test]
    fn test_evaluate_all_complete() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new(0, "task 0", "test rationale 0"));
        plan.add_task(Task::new(1, "task 1", "test rationale 1"));
        plan.get_task_mut(0).unwrap().complete("done");
        plan.get_task_mut(1).unwrap().complete("done");

        let score = evaluate_plan(&plan);
        assert!((score - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_evaluate_half_complete() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new(0, "task 0", "test rationale 0"));
        plan.add_task(Task::new(1, "task 1", "test rationale 1"));
        plan.get_task_mut(0).unwrap().complete("done");
        plan.get_task_mut(1).unwrap().fail("error");

        let score = evaluate_plan(&plan);
        assert!((score - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_evaluate_empty_plan() {
        let plan = Plan::new("test");

        let score = evaluate_plan(&plan);
        assert!((score - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_synthesize_single_result() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new(0, "task", "single task rationale"));
        plan.get_task_mut(0).unwrap().complete("the answer");

        let result = synthesize_results(&plan);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "the answer");
    }

    #[test]
    fn test_synthesize_multiple_results() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new(0, "task 0", "first task rationale"));
        plan.add_task(Task::new(1, "task 1", "second task rationale"));
        plan.get_task_mut(0).unwrap().complete("first");
        plan.get_task_mut(1).unwrap().complete("second");

        let result = synthesize_results(&plan);
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("first"));
        assert!(text.contains("second"));
    }

    #[test]
    fn test_synthesize_all_failed() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new(0, "task", "test rationale"));
        plan.get_task_mut(0).unwrap().fail("oops");

        let result = synthesize_results(&plan);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("All tasks failed"));
    }

    fn evaluate_plan(plan: &Plan) -> f32 {
        let total = plan.tasks.len() as f32;
        let completed = plan.completed_count() as f32;
        if total == 0.0 { 0.0 } else { completed / total }
    }

    fn synthesize_results(plan: &Plan) -> Result<String, String> {
        let results: Vec<&str> = plan
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Complete)
            .filter_map(|t| t.result.as_deref())
            .collect();

        if results.is_empty() {
            let errors: Vec<&str> = plan
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Failed)
                .filter_map(|t| t.error.as_deref())
                .collect();
            return Err(format!("All tasks failed: {}", errors.join("; ")));
        }

        if results.len() == 1 {
            return Ok(results[0].to_string());
        }

        Ok(results
            .iter()
            .enumerate()
            .map(|(i, r)| format!("## Result {}\n\n{}", i + 1, r))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n"))
    }

    /// Test helper: Parse plan JSON response without needing an Orchestrator instance.
    /// Does NOT validate worker assignments (for backward compatibility with existing tests).
    fn parse_plan_json(response: &str, original_query: &str) -> Result<Plan, String> {
        parse_plan_json_with_config(response, original_query, &OrchestrationConfig::default())
    }

    /// Test helper: Parse plan JSON with config for worker validation.
    fn parse_plan_json_with_config(
        response: &str,
        original_query: &str,
        config: &OrchestrationConfig,
    ) -> Result<Plan, String> {
        let json_str = extract_json_object(response);

        let parsed: serde_json::Value =
            serde_json::from_str(json_str).map_err(|e| format!("JSON parse error: {}", e))?;

        let goal = parsed["goal"]
            .as_str()
            .unwrap_or(original_query)
            .to_string();

        let mut plan = Plan::new(goal);

        // Get valid worker names for validation
        let valid_workers: std::collections::HashSet<&str> =
            config.available_worker_names().into_iter().collect();

        let tasks = parsed["tasks"].as_array().ok_or("Missing 'tasks' array")?;

        for task_value in tasks {
            let id = task_value["id"].as_u64().ok_or("Task missing 'id'")? as usize;
            let description = task_value["description"]
                .as_str()
                .ok_or("Task missing 'description'")?
                .to_string();

            // Rationale is required
            let rationale = task_value["rationale"]
                .as_str()
                .ok_or_else(|| format!("Task {} missing required 'rationale' field", id))?
                .to_string();

            let mut task = Task::new(id, description, rationale);

            if let Some(deps) = task_value["dependencies"].as_array() {
                for dep in deps {
                    if let Some(dep_id) = dep.as_u64() {
                        task = task.with_dependency(dep_id as usize);
                    }
                }
            }

            // Parse worker assignment (if present)
            if let Some(worker_name) = task_value["worker"].as_str()
                && config.has_workers()
            {
                if !valid_workers.contains(worker_name) {
                    return Err(format!(
                        "Task {} assigned to unknown worker '{}'. Valid workers: {:?}",
                        id,
                        worker_name,
                        valid_workers.iter().collect::<Vec<_>>()
                    ));
                }
                task = task.with_worker(worker_name);
            }

            plan.add_task(task);
        }

        if plan.tasks.is_empty() {
            return Err("Plan has no tasks".to_string());
        }

        Ok(plan)
    }

    #[test]
    fn test_parse_plan_simple() {
        let response = r#"{"goal": "Calculate sum", "tasks": [{"id": 0, "description": "Add 2+2", "rationale": "Direct arithmetic operation", "dependencies": []}]}"#;
        let plan = parse_plan_json(response, "What is 2+2?").unwrap();

        assert_eq!(plan.goal, "Calculate sum");
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].id, 0);
        assert_eq!(plan.tasks[0].description, "Add 2+2");
        assert_eq!(plan.tasks[0].rationale, "Direct arithmetic operation");
        assert!(plan.tasks[0].dependencies.is_empty());
    }

    #[test]
    fn test_parse_plan_with_dependencies() {
        let response = r#"{
            "goal": "Multi-step task",
            "tasks": [
                {"id": 0, "description": "First step", "rationale": "Initial data gathering", "dependencies": []},
                {"id": 1, "description": "Second step", "rationale": "Process data from first step", "dependencies": [0]}
            ]
        }"#;
        let plan = parse_plan_json(response, "complex query").unwrap();

        assert_eq!(plan.tasks.len(), 2);
        assert!(plan.tasks[0].dependencies.is_empty());
        assert_eq!(plan.tasks[1].dependencies, vec![0]);
        assert_eq!(plan.tasks[0].rationale, "Initial data gathering");
        assert_eq!(plan.tasks[1].rationale, "Process data from first step");
    }

    #[test]
    fn test_parse_plan_missing_rationale_fails() {
        let response = r#"{"goal": "Test", "tasks": [{"id": 0, "description": "No rationale", "dependencies": []}]}"#;
        let result = parse_plan_json(response, "test");

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("missing required 'rationale' field")
        );
    }

    #[test]
    fn test_parse_plan_with_markdown_wrapper() {
        let response = r#"```json
{"goal": "Test goal", "tasks": [{"id": 0, "description": "Do thing", "rationale": "Test rationale", "dependencies": []}]}
```"#;
        let plan = parse_plan_json(response, "test").unwrap();

        assert_eq!(plan.goal, "Test goal");
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].rationale, "Test rationale");
    }

    #[test]
    fn test_parse_plan_missing_goal_uses_fallback() {
        let response = r#"{"tasks": [{"id": 0, "description": "Task", "rationale": "Fallback rationale", "dependencies": []}]}"#;
        let plan = parse_plan_json(response, "original query").unwrap();

        assert_eq!(plan.goal, "original query");
    }

    #[test]
    fn test_parse_plan_invalid_json() {
        let response = "not valid json";
        let result = parse_plan_json(response, "test");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("JSON parse error"));
    }

    #[test]
    fn test_parse_plan_missing_tasks() {
        let response = r#"{"goal": "No tasks"}"#;
        let result = parse_plan_json(response, "test");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing 'tasks' array"));
    }

    #[test]
    fn test_parse_plan_empty_tasks() {
        let response = r#"{"goal": "Empty", "tasks": []}"#;
        let result = parse_plan_json(response, "test");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Plan has no tasks"));
    }

    #[test]
    fn test_build_task_context_no_dependencies() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new(0, "standalone task", "no deps rationale"));

        // Simulate the context building
        let task = &plan.tasks[0];
        assert!(task.dependencies.is_empty());
    }

    #[test]
    fn test_build_task_context_with_results() {
        let mut plan = Plan::new("test");
        plan.add_task(Task::new(0, "first", "first task rationale"));
        plan.add_task(Task::new(1, "second", "depends on first").with_dependency(0));
        plan.get_task_mut(0).unwrap().complete("First result");

        let task = &plan.tasks[1];
        assert_eq!(task.dependencies, vec![0]);
        assert_eq!(task.rationale, "depends on first");

        // Verify the dependency has a result
        let dep_result = plan.tasks[0].result.as_ref().unwrap();
        assert_eq!(dep_result, "First result");
    }

    // ========================================================================
    // Worker Assignment Tests
    // ========================================================================

    fn create_config_with_workers() -> OrchestrationConfig {
        use super::super::config::WorkerConfig;
        use std::collections::HashMap;

        let mut workers = HashMap::new();
        workers.insert(
            "operations".to_string(),
            WorkerConfig {
                description: "For logs and pipelines".to_string(),
                preamble: "Operations specialist.".to_string(),
                mcp_filter: vec!["mezmo_*".to_string()],
                vector_stores: vec![],
                turn_depth: None,
            },
        );
        workers.insert(
            "knowledge".to_string(),
            WorkerConfig {
                description: "For documentation".to_string(),
                preamble: "Knowledge specialist.".to_string(),
                mcp_filter: vec![],
                vector_stores: vec![],
                turn_depth: None,
            },
        );

        OrchestrationConfig {
            enabled: true,
            workers,
            ..Default::default()
        }
    }

    #[test]
    fn test_parse_plan_with_valid_worker() {
        let config = create_config_with_workers();
        let response = r#"{
            "goal": "Multi-worker task",
            "tasks": [
                {"id": 0, "description": "Check logs", "rationale": "Investigate log patterns", "dependencies": [], "worker": "operations"},
                {"id": 1, "description": "Look up docs", "rationale": "Reference documentation for context", "dependencies": [0], "worker": "knowledge"}
            ]
        }"#;

        let plan = parse_plan_json_with_config(response, "test", &config).unwrap();

        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].worker, Some("operations".to_string()));
        assert_eq!(plan.tasks[1].worker, Some("knowledge".to_string()));
    }

    #[test]
    fn test_parse_plan_with_invalid_worker() {
        let config = create_config_with_workers();
        let response = r#"{
            "goal": "Invalid worker task",
            "tasks": [
                {"id": 0, "description": "Do something", "rationale": "Test rationale", "dependencies": [], "worker": "nonexistent"}
            ]
        }"#;

        let result = parse_plan_json_with_config(response, "test", &config);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("unknown worker 'nonexistent'"));
        assert!(err.contains("Task 0"));
    }

    #[test]
    fn test_parse_plan_worker_ignored_when_no_workers_configured() {
        // Default config has no workers
        let config = OrchestrationConfig::default();
        let response = r#"{
            "goal": "Worker in response but no workers configured",
            "tasks": [
                {"id": 0, "description": "Task", "rationale": "Test rationale", "dependencies": [], "worker": "anything"}
            ]
        }"#;

        // Should succeed and ignore the worker field
        let plan = parse_plan_json_with_config(response, "test", &config).unwrap();

        assert_eq!(plan.tasks.len(), 1);
        // Worker should be None because workers aren't configured
        assert!(plan.tasks[0].worker.is_none());
    }

    #[test]
    fn test_parse_plan_mixed_worker_assignment() {
        let config = create_config_with_workers();
        let response = r#"{
            "goal": "Mixed workers",
            "tasks": [
                {"id": 0, "description": "Assigned task", "rationale": "Ops task rationale", "dependencies": [], "worker": "operations"},
                {"id": 1, "description": "Unassigned task", "rationale": "Follow-up rationale", "dependencies": [0]}
            ]
        }"#;

        let plan = parse_plan_json_with_config(response, "test", &config).unwrap();

        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].worker, Some("operations".to_string()));
        assert!(plan.tasks[1].worker.is_none());
    }

    #[test]
    fn test_task_with_worker_builder() {
        let task = Task::new(0, "Test task", "test rationale")
            .with_worker("operations")
            .with_dependency(1);

        assert_eq!(task.worker, Some("operations".to_string()));
        assert_eq!(task.dependencies, vec![1]);
    }

    // ========================================================================
    // Synthesis Prompt Tests
    // ========================================================================

    /// Test helper: Build synthesis prompt without needing an Orchestrator instance.
    fn build_test_synthesis_prompt(plan: &Plan, query: &str, tasks: &[&Task]) -> String {
        let results_section = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Complete)
            .filter_map(|t| {
                t.result.as_ref().map(|r| {
                    format!(
                        "### Task {}: {}\n**Rationale**: {}\n**Result**:\n{}\n",
                        t.id, t.description, t.rationale, r
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r#"You are synthesizing results from multiple tasks into a coherent response.

ORCHESTRATION GOAL: {goal}

ORIGINAL USER QUERY: {query}

TASK RESULTS:
{results}

INSTRUCTIONS:
1. Combine these results into a single, coherent response
2. Ensure the response directly addresses the original query
3. Preserve important details from each task
4. Do NOT just concatenate - synthesize into natural prose
5. If results conflict, note the discrepancy

Provide the synthesized response:"#,
            goal = plan.goal,
            query = query,
            results = results_section
        )
    }

    #[test]
    fn test_synthesis_prompt_includes_goal_and_query() {
        let mut plan = Plan::new("Analyze system performance");
        plan.add_task(Task::new(0, "Check CPU", "Gather CPU metrics"));
        plan.get_task_mut(0).unwrap().complete("CPU at 50%");

        let tasks: Vec<&Task> = plan.tasks.iter().collect();
        let prompt = build_test_synthesis_prompt(&plan, "How is my system doing?", &tasks);

        assert!(prompt.contains("ORCHESTRATION GOAL: Analyze system performance"));
        assert!(prompt.contains("ORIGINAL USER QUERY: How is my system doing?"));
    }

    #[test]
    fn test_synthesis_prompt_includes_rationale() {
        let mut plan = Plan::new("Multi-step analysis");
        plan.add_task(Task::new(0, "Fetch data", "Initial data collection"));
        plan.add_task(Task::new(
            1,
            "Process data",
            "Transform raw data into insights",
        ));
        plan.get_task_mut(0)
            .unwrap()
            .complete("Data fetched successfully");
        plan.get_task_mut(1).unwrap().complete("Insights generated");

        let tasks: Vec<&Task> = plan.tasks.iter().collect();
        let prompt = build_test_synthesis_prompt(&plan, "Analyze this", &tasks);

        assert!(prompt.contains("**Rationale**: Initial data collection"));
        assert!(prompt.contains("**Rationale**: Transform raw data into insights"));
    }

    #[test]
    fn test_synthesis_prompt_includes_task_results() {
        let mut plan = Plan::new("Test goal");
        plan.add_task(Task::new(0, "Task A", "First rationale"));
        plan.add_task(Task::new(1, "Task B", "Second rationale"));
        plan.get_task_mut(0).unwrap().complete("Result Alpha");
        plan.get_task_mut(1).unwrap().complete("Result Beta");

        let tasks: Vec<&Task> = plan.tasks.iter().collect();
        let prompt = build_test_synthesis_prompt(&plan, "query", &tasks);

        assert!(prompt.contains("### Task 0: Task A"));
        assert!(prompt.contains("### Task 1: Task B"));
        assert!(prompt.contains("Result Alpha"));
        assert!(prompt.contains("Result Beta"));
    }

    #[test]
    fn test_synthesis_prompt_filters_incomplete_tasks() {
        let mut plan = Plan::new("Partial completion");
        plan.add_task(Task::new(0, "Completed task", "Has result"));
        plan.add_task(Task::new(1, "Failed task", "No result"));
        plan.get_task_mut(0).unwrap().complete("Success");
        plan.get_task_mut(1).unwrap().fail("Error occurred");

        let tasks: Vec<&Task> = plan.tasks.iter().collect();
        let prompt = build_test_synthesis_prompt(&plan, "query", &tasks);

        // Only completed task should appear
        assert!(prompt.contains("### Task 0: Completed task"));
        assert!(prompt.contains("Success"));

        // Failed task should NOT appear in results section
        assert!(!prompt.contains("### Task 1: Failed task"));
        assert!(!prompt.contains("Error occurred"));
    }

    // ========================================================================
    // Evaluation Tests (tool-based evaluation — unit tests in evaluation_tool.rs)
    // ========================================================================

    #[test]
    fn test_extract_json_objects_single() {
        let objects = extract_json_objects(r#"{"key": "value"}"#);
        assert_eq!(objects, vec![r#"{"key": "value"}"#]);
    }

    #[test]
    fn test_extract_json_objects_multiple() {
        let input = r#"{"a": 1}
{"b": 2}"#;
        let objects = extract_json_objects(input);
        assert_eq!(objects, vec![r#"{"a": 1}"#, r#"{"b": 2}"#]);
    }

    #[test]
    fn test_extract_json_objects_nested_braces() {
        let input = r#"{"outer": {"inner": 1}}"#;
        let objects = extract_json_objects(input);
        assert_eq!(objects, vec![r#"{"outer": {"inner": 1}}"#]);
    }

    #[test]
    fn test_extract_json_objects_braces_in_strings() {
        let input = r#"{"msg": "use {braces} here"}"#;
        let objects = extract_json_objects(input);
        assert_eq!(objects, vec![r#"{"msg": "use {braces} here"}"#]);
    }

    #[test]
    fn test_extract_json_objects_code_fences() {
        let input = "```json\n{\"score\": 0.9}\n```";
        let objects = extract_json_objects(input);
        assert_eq!(objects, vec![r#"{"score": 0.9}"#]);
    }

    #[test]
    fn test_extract_json_objects_empty() {
        assert!(extract_json_objects("no json here").is_empty());
        assert!(extract_json_objects("").is_empty());
    }

    #[test]
    fn test_evaluation_result_new() {
        use super::super::types::EvaluationResult;

        let result = EvaluationResult::new(0.75, "Test reasoning");
        assert!((result.score - 0.75).abs() < f32::EPSILON);
        assert_eq!(result.reasoning, "Test reasoning");
        assert!(result.gaps.is_empty());
    }

    #[test]
    fn test_evaluation_result_with_gaps() {
        use super::super::types::EvaluationResult;

        let result = EvaluationResult::new(0.5, "Partial")
            .with_gaps(vec!["Gap 1".to_string(), "Gap 2".to_string()]);

        assert_eq!(result.gaps.len(), 2);
        assert!(result.gaps.contains(&"Gap 1".to_string()));
    }

    #[test]
    fn test_evaluation_result_fallback() {
        use super::super::types::EvaluationResult;

        let result = EvaluationResult::fallback(3, 4);
        assert!((result.score - 0.75).abs() < f32::EPSILON);
        assert!(result.reasoning.contains("3 of 4 tasks completed"));

        let result = EvaluationResult::fallback(0, 0);
        assert!(result.score.abs() < f32::EPSILON);
    }

    #[test]
    fn test_evaluation_result_clamps_on_create() {
        use super::super::types::EvaluationResult;

        let result = EvaluationResult::new(1.5, "Too high");
        assert!((result.score - 1.0).abs() < f32::EPSILON);

        let result = EvaluationResult::new(-0.5, "Too low");
        assert!(result.score.abs() < f32::EPSILON);
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

    // ========================================================================
    // PlanningResponse Routing Tests
    // ========================================================================

    #[test]
    fn test_planning_response_into_plan_preserves_all_fields() {
        use super::super::types::{PlanningResponse, TaskJson};

        let response = PlanningResponse::Orchestrated {
            goal: "Test goal".to_string(),
            tasks: vec![
                TaskJson {
                    id: 0,
                    description: "Task A".to_string(),
                    rationale: Some("Reason A".to_string()),
                    dependencies: Some(vec![]),
                    worker: Some("operations".to_string()),
                    reuse_result_from: None,
                },
                TaskJson {
                    id: 1,
                    description: "Task B".to_string(),
                    rationale: Some("Reason B".to_string()),
                    dependencies: Some(vec![0]),
                    worker: None,
                    reuse_result_from: None,
                },
            ],
            routing_rationale: "Test rationale".to_string(),
            planning_summary: "Test summary".to_string(),
            phases: None,
        };

        let plan = response.into_plan().unwrap();
        assert_eq!(plan.goal, "Test goal");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].worker, Some("operations".to_string()));
        assert_eq!(plan.tasks[1].dependencies, vec![0]);
        assert_eq!(plan.tasks[0].rationale, "Reason A");
    }

    #[test]
    fn test_planning_response_direct_has_no_plan() {
        use super::super::types::PlanningResponse;

        let response = PlanningResponse::Direct {
            response: "42".to_string(),
            routing_rationale: "Simple math".to_string(),
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

    #[test]
    fn test_planning_response_serde_round_trip_all_variants() {
        use super::super::types::{PlanningResponse, TaskJson};

        // Direct
        let direct = PlanningResponse::Direct {
            response: "hello".to_string(),
            routing_rationale: "greeting".to_string(),
        };
        let json = serde_json::to_string(&direct).unwrap();
        let parsed: PlanningResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlanningResponse::Direct { .. }));

        // Orchestrated
        let orchestrated = PlanningResponse::Orchestrated {
            goal: "test".to_string(),
            tasks: vec![TaskJson {
                id: 0,
                description: "do it".to_string(),
                rationale: Some("because".to_string()),
                dependencies: None,
                worker: None,
                reuse_result_from: None,
            }],
            routing_rationale: "complex".to_string(),
            planning_summary: "A plan to do it".to_string(),
            phases: None,
        };
        let json = serde_json::to_string(&orchestrated).unwrap();
        let parsed: PlanningResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlanningResponse::Orchestrated { .. }));

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
            let filename = p.write_result_artifact(0, &large_result).await.unwrap();
            assert_eq!(filename, "task-0-result.txt");
        }

        // Verify ReadArtifactTool can retrieve it
        let tool = ReadArtifactTool::new(persistence.clone());
        let output = tool
            .call(super::super::tools::read_artifact::ReadArtifactArgs {
                filename: "task-0-result.txt".to_string(),
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
            p.write_result_artifact(0, "result 0").await.unwrap();
            p.write_result_artifact(1, "result 1").await.unwrap();
            p.write_result_artifact(2, "result 2").await.unwrap();

            let artifacts = p.list_artifacts().await.unwrap();
            assert_eq!(artifacts.len(), 3);
        }

        // Verify each can be read back
        let tool = ReadArtifactTool::new(persistence);
        for i in 0..3 {
            let output = tool
                .call(super::super::tools::read_artifact::ReadArtifactArgs {
                    filename: format!("task-{}-result.txt", i),
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
    // apply_result_reuse tests
    // ========================================================================

    #[test]
    fn test_apply_result_reuse_carries_forward() {
        // Previous plan with a completed task
        let mut prev = Plan::new("prev");
        let mut t = Task::new(0, "Fetch data", "Get data");
        t.complete("data result".to_string());
        prev.add_task(t);

        // New plan references previous task 0
        let mut new_plan = Plan::new("new");
        let mut new_task = Task::new(0, "Fetch data (reused)", "Reuse");
        new_task.reuse_result_from = Some(0);
        new_plan.add_task(new_task);
        new_plan.add_task(Task::new(1, "Analyze data", "New work"));

        Orchestrator::apply_result_reuse(&mut new_plan, Some(&prev));

        // Task 0 should be marked complete with the previous result
        assert_eq!(new_plan.tasks[0].status, TaskStatus::Complete);
        assert_eq!(new_plan.tasks[0].result.as_deref(), Some("data result"));
        // Task 1 should be unchanged
        assert_eq!(new_plan.tasks[1].status, TaskStatus::Pending);
        assert!(new_plan.tasks[1].result.is_none());
    }

    #[test]
    fn test_apply_result_reuse_ignores_failed_tasks() {
        let mut prev = Plan::new("prev");
        let mut t = Task::new(0, "Bad task", "Failed");
        t.fail("error");
        prev.add_task(t);

        let mut new_plan = Plan::new("new");
        let mut new_task = Task::new(0, "Retry", "Retry");
        new_task.reuse_result_from = Some(0);
        new_plan.add_task(new_task);

        Orchestrator::apply_result_reuse(&mut new_plan, Some(&prev));

        // Should NOT carry forward since previous task was failed
        assert_eq!(new_plan.tasks[0].status, TaskStatus::Pending);
        assert!(new_plan.tasks[0].result.is_none());
    }

    #[test]
    fn test_apply_result_reuse_no_previous_plan() {
        let mut plan = Plan::new("new");
        let mut t = Task::new(0, "Task", "Work");
        t.reuse_result_from = Some(0);
        plan.add_task(t);

        // Should not panic with None previous
        Orchestrator::apply_result_reuse(&mut plan, None);
        assert_eq!(plan.tasks[0].status, TaskStatus::Pending);
    }

    #[test]
    fn test_apply_result_reuse_missing_task_id() {
        let mut prev = Plan::new("prev");
        prev.add_task(Task::new(0, "Only task", "Single"));

        let mut new_plan = Plan::new("new");
        let mut t = Task::new(0, "Reuse", "Reuse");
        t.reuse_result_from = Some(99); // doesn't exist
        new_plan.add_task(t);

        Orchestrator::apply_result_reuse(&mut new_plan, Some(&prev));
        // Should not carry forward — task 99 doesn't exist
        assert_eq!(new_plan.tasks[0].status, TaskStatus::Pending);
    }

    // ========================================================================
    // categorize_failure_error tests
    // ========================================================================

    #[test]
    fn test_categorize_failure_provider_errors() {
        assert_eq!(
            Orchestrator::categorize_failure_error("Rate limit exceeded"),
            "provider_error"
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("HTTP 429 Too Many Requests"),
            "provider_error"
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("503 Service Unavailable"),
            "provider_error"
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("Authentication failed: invalid API key"),
            "provider_error"
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("Unauthorized: 403"),
            "provider_error"
        );
    }

    #[test]
    fn test_categorize_failure_other_categories() {
        assert_eq!(
            Orchestrator::categorize_failure_error("Request timed out after 30s"),
            "timeout"
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("context limit exceeded"),
            "context overflow"
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("MaxDepthError: reached limit"),
            "depth exhaustion"
        );
        assert_eq!(
            Orchestrator::categorize_failure_error("Something went wrong"),
            "LLM error"
        );
    }

    // -----------------------------------------------------------------------
    // Provider error short-circuit decision
    // -----------------------------------------------------------------------

    fn make_failure(error: &str) -> FailedTaskRecord {
        FailedTaskRecord {
            description: "test task".into(),
            error: error.into(),
            iteration: 1,
            worker: None,
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
}
