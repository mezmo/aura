//! Lightweight factory for orchestration streaming.
//!
//! `OrchestratorFactory` implements `StreamingAgent` without constructing a full
//! `Orchestrator` up front. The real orchestrator is created lazily inside `stream()`
//! when a request arrives, ensuring MCP progress notifications route correctly
//! and avoiding duplicate resource allocation.

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::AgentRuntimeConfig;
use crate::provider_agent::{StreamError, StreamItem};
use crate::streaming::StreamingAgent;

use super::orchestrator::{
    Orchestrator, STREAM_CHUNK_SIZE, spawn_cancellation_watcher, spawn_tool_event_forwarder,
};
use super::park::resume::{ResumeClaimTable, ResumeGrant, ResumeRunId};
use super::{ReservationFault, RunExecutionScope};
use std::sync::Arc;

/// Why the initial producer's supervisor refused to start: the
/// persistence-bound run could not be reserved for this producer — a live
/// execution already holds it, or (unreachably, by construction) the run id
/// failed validation. Streamed as the terminal `Err` so a caller sees a typed
/// refusal instead of an orchestration started under an occupied run.
#[derive(Debug)]
struct ProducerReservationRefused {
    run_id: String,
    reason: String,
}

impl std::fmt::Display for ProducerReservationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "run {} could not be reserved for its initial producer: {}",
            self.run_id, self.reason
        )
    }
}

impl std::error::Error for ProducerReservationRefused {}

/// Zero-state wrapper that implements `StreamingAgent` for orchestration mode.
///
/// Defers `Orchestrator` construction to `stream()` to avoid duplicate resource
/// allocation and ensure MCP progress notifications route correctly.
pub struct OrchestratorFactory {
    agent_config: AgentRuntimeConfig,
    /// The shared run-reservation table a park-enabled deployment injects:
    /// the initial producer reserves its persistence-bound run id through
    /// this table immediately after orchestrator construction, and its
    /// supervisor owns that reservation until shutdown. `None` on a factory
    /// that never parks.
    reservations: Option<Arc<ResumeClaimTable>>,
}

impl OrchestratorFactory {
    pub fn new(agent_config: AgentRuntimeConfig) -> Self {
        Self {
            agent_config,
            reservations: None,
        }
    }

    /// Inject the shared reservation table (park-enabled deployments).
    #[must_use]
    pub fn with_reservation_table(mut self, table: Arc<ResumeClaimTable>) -> Self {
        self.reservations = Some(table);
        self
    }

    /// The shared reservation table, when this deployment's factories park.
    #[must_use]
    pub fn reservation_table(&self) -> Option<&Arc<ResumeClaimTable>> {
        self.reservations.as_ref()
    }

    /// Resume one granted run into the normal orchestration stream:
    /// consumes the grant (single-use, owned — this factory stays reusable
    /// and never stores it) and returns the stream, cancellation sender, and
    /// usage-state tuple the completion pipeline already consumes for chat.
    ///
    /// The spawned supervisor owns the grant's reservation lease for the
    /// whole resumed execution: cancellation, MCP shutdown, forwarder drain,
    /// and tracked-child joins complete before the fence releases.
    #[expect(
        unused_variables,
        reason = "todo!() body; filled by P45 wave fill units"
    )]
    pub async fn resume_stream_with_timeout(
        &self,
        grant: ResumeGrant,
        timeout: Duration,
        request_id: &str,
    ) -> (
        BoxStream<'static, Result<StreamItem, StreamError>>,
        watch::Sender<bool>,
        crate::UsageState,
    ) {
        todo!(
            "P45 wave fill unit S3: the factory supervisor that drives the granted run through the owned resume segment"
        )
    }

    /// Spawn the background orchestration task and return its event stream.
    ///
    /// Shared by [`stream`](Self::stream) and
    /// [`stream_with_timeout`](Self::stream_with_timeout). The `usage_state`
    /// handle is assigned to the inner `Orchestrator` so planning, worker,
    /// synthesis, and evaluation turns can accumulate into it; the caller
    /// (`stream_with_timeout`) retains a clone and hands it to the streaming
    /// handler for the final `aura.usage` event. `stream()` passes a detached
    /// state since its trait-visible callers don't observe usage.
    fn spawn_orchestration_stream(
        &self,
        query: String,
        chat_history: Vec<rig::completion::Message>,
        cancel_token: CancellationToken,
        request_id: String,
        usage_state: crate::UsageState,
        outer_budget: Option<Duration>,
    ) -> BoxStream<'static, Result<StreamItem, StreamError>> {
        let agent_config = self.agent_config.clone();
        let reservations = self.reservations.clone();

        // Create channel for orchestrator events
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(100);

        let cancel_token_clone = cancel_token.clone();
        // Capture parent span so child spans nest correctly in tracing.
        let parent_span = tracing::Span::current();
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                let mut orchestrator = match Orchestrator::new(agent_config).await {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = event_tx.send(Err(e)).await;
                        return;
                    }
                };

                // The initial park-enabled producer reserves its
                // persistence-bound run id IMMEDIATELY after construction and
                // before worker registration: an occupied run refuses a second
                // producer outright, and the reservation is the fence the
                // supervisor owns until its drain ends. Unscoped (no table, or
                // park off) stays byte-equal to today.
                let mut execution_scope = None;
                if let Some(table) = reservations.as_ref()
                    && orchestrator.park_enabled()
                {
                    let run_id_str = orchestrator.persistence.lock().await.run_id().to_string();
                    let run = match ResumeRunId::parse(&run_id_str) {
                        Ok(run) => run,
                        Err(e) => {
                            let _ = event_tx
                                .send(Err(Box::new(ProducerReservationRefused {
                                    run_id: run_id_str,
                                    reason: format!(
                                        "the persistence-bound run id failed validation: {e}"
                                    ),
                                })))
                                .await;
                            return;
                        }
                    };
                    match table.reserve(&run) {
                        Ok(lease) => {
                            let scope = RunExecutionScope::new(lease);
                            orchestrator.arm_execution_scope(Arc::clone(&scope));
                            execution_scope = Some(scope);
                        }
                        Err(ReservationFault::Live) => {
                            let _ = event_tx
                                .send(Err(Box::new(ProducerReservationRefused {
                                    run_id: run_id_str,
                                    reason: "a live execution already holds the run".to_string(),
                                })))
                                .await;
                            return;
                        }
                    }
                }

                // Share the caller's usage handle so accumulate_usage() writes
                // are visible to the streaming handler (UsageState is Arc-backed).
                orchestrator.usage_state = usage_state;
                orchestrator.outer_budget = outer_budget;

                // Set MCP request ID for progress notification routing, and
                // surface per-server connection status so degraded/unavailable
                // MCP servers are visible in orchestration mode too (workers
                // share this one manager).
                if let Some(ref mcp_manager) = orchestrator.mcp_manager {
                    mcp_manager.set_current_request(&request_id).await;
                    let snapshot = mcp_manager.server_status_snapshot();
                    if !snapshot.is_empty() {
                        // Race the send against cancellation: a full channel
                        // with no consumer must not strand the supervisor
                        // ahead of its drain.
                        tokio::select! {
                            _ = event_tx.send(Ok(StreamItem::McpStatus(snapshot))) => {}
                            _ = cancel_token_clone.cancelled() => {}
                        }
                    }
                }

                // Forward tool call events from workers to SSE stream, tracked
                // through the run's scope when armed so the drain below waits
                // it out.
                spawn_tool_event_forwarder(
                    &orchestrator.tool_call_observer,
                    event_tx.clone(),
                    cancel_token_clone.clone(),
                    execution_scope.clone(),
                );

                tokio::select! {
                    result = orchestrator.run_orchestration(&query, chat_history, event_tx.clone()) => {
                        match result {
                            Ok(final_result) => {
                                // Each send races cancellation so a full
                                // channel cannot strand the supervisor before
                                // its drain. A cancelled run truncates the
                                // final; the run is cancelled, so that is
                                // correct.
                                let mut cancelled = false;
                                for chunk in final_result.chars().collect::<Vec<_>>().chunks(STREAM_CHUNK_SIZE) {
                                    let text: String = chunk.iter().collect();
                                    tokio::select! {
                                        _ = event_tx.send(Ok(StreamItem::StreamAssistantItem(
                                            crate::provider_agent::StreamedAssistantContent::Text(text)
                                        ))) => {}
                                        _ = cancel_token_clone.cancelled() => {
                                            cancelled = true;
                                            break;
                                        }
                                    }
                                }

                                if !cancelled {
                                    tokio::select! {
                                        _ = event_tx.send(Ok(StreamItem::Final(
                                            crate::provider_agent::FinalResponseInfo {
                                                content: final_result,
                                                usage: Default::default(),
                                                cache_usage: None,
                                            }
                                        ))) => {}
                                        _ = cancel_token_clone.cancelled() => {}
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.send(Err(e)).await;
                            }
                        }
                    }
                    _ = cancel_token_clone.cancelled() => {
                        tracing::info!("Orchestration cancelled");
                        if let Some(ref mcp_manager) = orchestrator.mcp_manager {
                            let cancelled = mcp_manager
                                .cancel_and_close_all(&request_id, "Client disconnected or timeout")
                                .await;
                            if cancelled > 0 {
                                tracing::info!("Cancelled {} MCP request(s) during orchestration shutdown", cancelled);
                            }
                        }
                    }
                }

                // Every exit arm (normal, error, cancellation) reaches here:
                // the supervisor owns the reservation until its tracked tails
                // end. Dropping the orchestrator releases its scope clone and
                // closes the observer (ending the forwarder on the non-cancel
                // arms); `drain` then joins every tracked child, and the scope
                // local drops last, so the fence releases only after the
                // forwarder and every other tracked tail has ended. MCP
                // cancel-and-close stays in its arm, before this drain.
                if let Some(scope) = execution_scope {
                    drop(orchestrator);
                    // Signal cancellation before draining so cancellation-aware
                    // tracked tails exit early; the drain must not wait on work
                    // that has been told to stop.
                    scope.cancel();
                    scope.drain().await;
                }
            },
            parent_span,
        ));

        // Convert receiver to stream
        let stream = stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });

        Box::pin(stream)
    }
}

#[async_trait]
impl StreamingAgent for OrchestratorFactory {
    fn get_provider_info(&self) -> (&str, &str) {
        self.agent_config.llm.model_info()
    }

    /// The coordinator's window: it holds the persistent conversation, so it
    /// is the context a client measures the session against.
    fn context_window(&self) -> Option<u64> {
        self.agent_config.llm.context_window()
    }

    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        cancel_token: CancellationToken,
        request_id: &str,
    ) -> Result<BoxStream<'static, Result<StreamItem, StreamError>>, StreamError> {
        // Raw-stream callers don't observe usage; hand the spawn a detached
        // UsageState so the field is populated but nobody reads it.
        Ok(self.spawn_orchestration_stream(
            query.to_string(),
            chat_history,
            cancel_token,
            request_id.to_string(),
            crate::UsageState::new(),
            None,
        ))
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

        // Share one UsageState between the inner orchestrator (writer) and the
        // streaming handler (reader) so aura.usage reflects the aggregate of
        // all orchestration LLM turns.
        let usage_state = crate::UsageState::new();
        let stream = self.spawn_orchestration_stream(
            query.to_string(),
            chat_history,
            cancel_token,
            request_id.to_string(),
            usage_state.clone(),
            (!timeout.is_zero()).then_some(timeout),
        );

        (stream, cancel_tx, usage_state)
    }

    async fn cancel_and_close_mcp(&self, _request_id: &str, _reason: &str) -> usize {
        // No-op: cancellation is handled inside the spawned task via cancel_token.
        0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::StreamExt;
    use futures::stream::BoxStream;
    use tokio::time::{Instant, sleep};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::config::AgentRuntimeConfig;
    use crate::hitl::{DecisionRoute, HitlRuntime, PendingApprovals};
    use crate::orchestration::test_rig::{
        CoordinatorOverride, ECHO_TOOL_NAME, RecordingTool, ScriptedCompletionModel,
        ScriptedToolCall, ScriptedTurn, StallHook, WORKER_OVERRIDE_SERIAL, WorkerOverride,
        install_coordinator_overrides, install_worker_overrides,
    };
    use crate::orchestration::{OrchestrationConfig, ResumeRunId, WorkerConfig};
    use crate::provider_agent::{StreamError, StreamItem};
    use crate::session_store::{InMemoryApprovalStore, InMemoryEventBus};
    use crate::streaming::StreamingAgent;

    /// The scripted coordinator's final answer — the deterministic natural
    /// finish of a run that never needs a worker.
    const FINAL_ANSWER: &str = "the initial producer is complete";
    /// The session segment both frames persist under.
    const SESSION: &str = "l4a-sess";

    /// Bound every wait: a held stream fails fast instead of hanging the
    /// suite.
    const BOUND: Duration = Duration::from_secs(10);

    /// The one `respond_directly` turn a direct-answer frame's coordinator
    /// script carries.
    fn direct_turn() -> ScriptedTurn {
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_route",
            "respond_directly",
            serde_json::json!({
                "response": FINAL_ANSWER,
                "routing_rationale": "the request needs no tool work",
            }),
        )])
        .with_text(FINAL_ANSWER)
    }

    /// The coordinator's one-task plan turn: the worker frame's opening
    /// routing decision, naming the configured `operations` worker.
    fn plan_turn() -> ScriptedTurn {
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_plan",
            "create_plan",
            serde_json::json!({
                "goal": "apply the manifest",
                "steps": [
                    {"type": "task", "task": "apply the manifest", "worker": "operations"}
                ],
                "routing_rationale": "the request needs tool work",
                "planning_summary": "one operations worker applies the manifest",
            }),
        )])
        .with_text("planning the apply")
    }

    /// The one worker definition the worker frame plans against.
    fn operations_worker() -> (String, WorkerConfig) {
        (
            "operations".to_string(),
            WorkerConfig {
                description: "Runs the scripted tool".to_string(),
                preamble: "You apply changes with the echo tool.".to_string(),
                mcp_filter: Some(vec![]),
                vector_stores: vec![],
                turn_depth: None,
                llm: None,
                scratchpad: None,
                skills: None,
            },
        )
    }

    /// A park-enabled config over an in-memory approval store: the flag is on
    /// and the conversational route holds a registry, so `park_enabled()` is
    /// true. `workers` is the plan surface a worker frame needs; a
    /// direct-answer frame plans against none.
    fn park_config(dir: &Path, workers: HashMap<String, WorkerConfig>) -> AgentRuntimeConfig {
        let registry = PendingApprovals::with_backend(
            Arc::new(InMemoryApprovalStore::new()),
            Arc::new(InMemoryEventBus::new()),
        );
        AgentRuntimeConfig {
            hitl: Some(HitlRuntime {
                patterns: Arc::from([aura_config::GlobPattern::new("kubectl_*").unwrap()]),
                route: Arc::new(DecisionRoute::Conversational {
                    registry,
                    timeout: Duration::from_secs(3600),
                }),
                park_enabled: true,
                park_ttl: aura_config::ParkTtl::default(),
            }),
            memory_dir: Some(dir.join("memory").to_string_lossy().into_owned()),
            session_id: Some(SESSION.to_string()),
            request_id: Some(format!("req_{}", uuid::Uuid::new_v4().simple())),
            orchestration: Some(OrchestrationConfig {
                enabled: true,
                workers,
                ..Default::default()
            }),
            ..AgentRuntimeConfig::default()
        }
    }

    /// The persistence-bound run id of the one run under the session: the run
    /// directory `ExecutionPersistence::new` minted and named. Bounded: the
    /// directory exists by the time the producer has reserved the run.
    async fn discover_run_id(memory_dir: &Path) -> ResumeRunId {
        let dir = memory_dir.join(SESSION);
        let deadline = Instant::now() + BOUND;
        loop {
            if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    if let Some(name) = entry.file_name().to_str()
                        && let Ok(run) = ResumeRunId::parse(name)
                    {
                        return run;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "the persistence-bound run directory appears within the bound"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// Drive a factory stream to its terminal item, returning whether the
    /// scripted final answer rode it.
    async fn drive_to_end(
        stream: &mut BoxStream<'static, Result<StreamItem, StreamError>>,
    ) -> bool {
        let mut saw_final = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamItem::Final(info)) => saw_final = info.content == FINAL_ANSWER,
                Ok(_) => {}
                Err(e) => panic!("the factory stream must not error: {e}"),
            }
        }
        saw_final
    }

    /// Poll the shared table until the run's reservation is released, bounded.
    async fn await_release(table: &ResumeClaimTable, run: &ResumeRunId) {
        let deadline = Instant::now() + BOUND;
        while table.is_live(run) {
            assert!(
                Instant::now() < deadline,
                "the supervisor releases the run reservation within the bound"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// The initial park-enabled producer holds its persistence-bound run
    /// reservation from before worker registration until its supervisor's
    /// drain ends. The worker's held tool call pins the stream open across
    /// the liveness observation, so the assertion cannot race a fast finish.
    #[tokio::test]
    async fn park_enabled_stream_reserves_run_until_supervisor_ends() {
        let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
        let dir = tempfile::tempdir().expect("temp memory root");
        std::fs::create_dir_all(dir.path().join("memory")).expect("memory dir");
        let table = Arc::new(ResumeClaimTable::new());
        let factory = OrchestratorFactory::new(park_config(
            dir.path(),
            HashMap::from([operations_worker()]),
        ))
        .with_reservation_table(Arc::clone(&table));
        install_coordinator_overrides(vec![CoordinatorOverride {
            model: ScriptedCompletionModel::new(vec![plan_turn(), direct_turn()]),
        }]);
        let stall = StallHook::new();
        install_worker_overrides(vec![WorkerOverride {
            model: ScriptedCompletionModel::new(vec![
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_hold",
                    ECHO_TOOL_NAME,
                    serde_json::json!({"namespace": "prod"}),
                )]),
                ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
                    "call_submit",
                    "submit_result",
                    serde_json::json!({
                        "summary": "applied",
                        "result": "the manifest is applied",
                        "confidence": "high",
                    }),
                )]),
            ]),
            extra_tools: vec![Box::new(
                RecordingTool::new(Arc::new(std::sync::Mutex::new(Vec::new())))
                    .with_stall(stall.clone()),
            )],
        }]);

        let cancel = CancellationToken::new();
        let mut stream = factory
            .stream("apply the manifest", Vec::new(), cancel, "req_reserve")
            .await
            .expect("the factory stream starts");

        // The held tool call is proof the supervisor reserved the run and
        // registered the worker path; nothing can complete while it holds.
        tokio::time::timeout(BOUND, stall.wait_entered())
            .await
            .expect("the worker reaches its held tool call");
        let run = discover_run_id(&dir.path().join("memory")).await;
        assert!(
            table.is_live(&run),
            "the initial park-enabled producer holds its persistence-bound run \
             reservation while the stream is live"
        );

        stall.release();
        assert!(
            drive_to_end(&mut stream).await,
            "the reserved stream still completes with the scripted final answer"
        );
        await_release(&table, &run).await;
    }

    /// CHARACTERIZATION: a factory with no reservation table behaves byte-for-
    /// byte as before — the run completes with no reservation involved.
    #[tokio::test]
    async fn unscoped_stream_keeps_current_behavior() {
        let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
        let dir = tempfile::tempdir().expect("temp memory root");
        std::fs::create_dir_all(dir.path().join("memory")).expect("memory dir");
        let factory = OrchestratorFactory::new(park_config(dir.path(), HashMap::new()));
        install_coordinator_overrides(vec![CoordinatorOverride {
            model: ScriptedCompletionModel::new(vec![direct_turn()]),
        }]);

        let cancel = CancellationToken::new();
        let mut stream = factory
            .stream("say hello", Vec::new(), cancel, "req_unscoped")
            .await
            .expect("the plain factory stream starts");

        assert!(
            drive_to_end(&mut stream).await,
            "a factory with no reservation table completes exactly as before"
        );
    }
}
