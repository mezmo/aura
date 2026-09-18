//! Lightweight factory for orchestration streaming.
//!
//! `OrchestratorFactory` implements `StreamingAgent` without constructing a full
//! `Orchestrator` up front. The real orchestrator is created lazily inside `stream()`
//! when a request arrives, ensuring MCP progress notifications route correctly
//! and avoiding duplicate resource allocation.

use std::collections::HashMap;
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
use super::park::resume::{
    ResumeClaimTable, ResumeGrant, ResumeRunId, ResumeStreamEnd, SegmentError, run_segment_borrowed,
};
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
    pub async fn resume_stream_with_timeout(
        &self,
        grant: ResumeGrant,
        timeout: Duration,
        _request_id: &str,
    ) -> (
        BoxStream<'static, Result<StreamItem, StreamError>>,
        watch::Sender<bool>,
        crate::UsageState,
    ) {
        // The same channel bound the chat path's supervisor uses: the
        // driver's forwarded segment events and this supervisor's exit-arm
        // sends share one capacity, so the resumed path back-pressures
        // exactly like the fresh one.
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(100);

        // The caller's cancellation surface, and the request-scoped watcher
        // that bridges it to the grant's ONE execution scope — the resumed
        // segment's sole cancellation path. Cancellation-prioritized
        // (biased, the chat path's precedent): an already-latched cancel
        // wins over a simultaneous start, so a cancelling consumer never
        // races a late bridge. The watcher holds only the token clone,
        // never the scope `Arc`, so a parked watcher owns no lease and the
        // reservation fence releases with the supervisor's drive. Dropping
        // the watch sender with no explicit signal is an unexplained abort
        // and fails safe by cancelling (#305) — a no-op on an ended run.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let scope = grant.execution_scope();
        let watcher_token = scope.cancellation().clone();
        let _watcher_handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = watcher_token.cancelled() => {
                    // The run's execution already ended: nothing left to
                    // bridge.
                }
                _ = async {
                    let mut cancel_rx = cancel_rx;
                    loop {
                        if cancel_rx.changed().await.is_err() {
                            return; // sender dropped: fail safe (#305)
                        }
                        if !*cancel_rx.borrow_and_update() {
                            continue;
                        }
                        return; // external cancellation requested
                    }
                } => {}
            }
            watcher_token.cancel();
        });

        // The factory projection: the zero-timeout convention. A zero
        // timeout means no outer budget, not an instantly exhausted one.
        let outer_budget = (!timeout.is_zero()).then_some(timeout);

        // The supervisor: spawn the resumed execution and return its
        // stream. The grant stays OWNED here across cancellation — the
        // segment drive future borrows it and is never cancelled, so the
        // grant is never moved into a cancellable select arm.
        let agent_config = self.agent_config.clone();
        let usage_state = crate::UsageState::new();
        let supervisor_state = usage_state.clone();
        let _supervisor_handle = tokio::spawn(async move {
            let segment_headers = HashMap::new();
            let drive = run_segment_borrowed(
                &grant,
                &agent_config,
                // The EMPTY headers map is deliberate: S1's
                // `prepare_agent_config` already resolved
                // `headers_from_request` forwarding once against the resume
                // caller into this config; re-resolution is a no-op.
                &segment_headers,
                event_tx.clone(),
                supervisor_state,
                outer_budget,
            );
            tokio::pin!(drive);
            // Drive the segment to its end, watching for the consumer's
            // disconnect: the returned stream is the only reader of
            // `event_tx`, so its drop closes the channel. A disconnect
            // cancels the grant's ONE scope — the segment's sole
            // cancellation path — and the drive then runs to its cancelled
            // end: the drive future is never dropped here, so the grant
            // keeps its owner. The disconnect arm disables itself after it
            // fires (a closed channel reports closed immediately), leaving
            // the drive arm as the loop's only exit.
            let mut disconnected = false;
            let probe = event_tx.clone();
            let end = loop {
                tokio::select! {
                    end = &mut drive => break end,
                    _ = probe.closed(), if !disconnected => {
                        scope.cancel();
                        disconnected = true;
                    }
                }
            };

            match end {
                // The run completed within the segment: the normal factory
                // finalization — chunked answer text and ONE `Final`, the
                // same shape the chat path emits, racing cancellation so a
                // cancelled run truncates its final instead of stranding
                // the supervisor. The driver emitted no terminal of its
                // own; this is the only one.
                Ok(ResumeStreamEnd::Completed { final_answer }) => {
                    let mut cancelled = false;
                    for chunk in final_answer
                        .chars()
                        .collect::<Vec<_>>()
                        .chunks(STREAM_CHUNK_SIZE)
                    {
                        let text: String = chunk.iter().collect();
                        tokio::select! {
                            biased;
                            _ = scope.cancellation().cancelled() => {
                                cancelled = true;
                                break;
                            }
                            _ = event_tx.send(Ok(StreamItem::StreamAssistantItem(
                                crate::provider_agent::StreamedAssistantContent::Text(text),
                            ))) => {}
                        }
                    }
                    if !cancelled {
                        tokio::select! {
                            biased;
                            _ = scope.cancellation().cancelled() => {}
                            _ = event_tx.send(Ok(StreamItem::Final(
                                crate::provider_agent::FinalResponseInfo {
                                    content: final_answer,
                                    usage: Default::default(),
                                    cache_usage: None,
                                },
                            ))) => {}
                        }
                    }
                }
                // A fresh park: the publication owner already emitted the
                // ONE `RunParked` while the segment was live. The normal
                // terminal stream policy adds nothing.
                Ok(ResumeStreamEnd::Reparked) => {}
                // The segment faulted: the fault rides the error arm as the
                // stream's terminal `Err` (never unwrapped). The segment's
                // `Diagnostic` prose is the stream error's content.
                Err(segment_fault) => {
                    let fault: StreamError = match segment_fault {
                        SegmentError::Continuation(diagnostic) => diagnostic.to_string().into(),
                    };
                    tokio::select! {
                        biased;
                        _ = scope.cancellation().cancelled() => {}
                        _ = event_tx.send(Err(fault)) => {}
                    }
                }
            }

            // Every exit arm (completed, reparked, fault, cancelled)
            // reaches the same drain: signal the grant's scope first so
            // cancellation-aware tracked tails exit early, then join every
            // tracked child before the task ends. The scope local — and
            // with it the grant's reservation lease — drops last, so the
            // fence releases only after the drain ends, never at a stream
            // drop or a terminal send. NO MCP close runs here: the
            // supervisor owns no MCP handle (the manager lives inside the
            // segment's orchestrator); MCP cancel-and-close is the driver's
            // exit-arm duty.
            scope.cancel();
            scope.drain().await;
        });

        // Convert receiver to stream — the chat path's same unfold.
        let stream = stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });

        (Box::pin(stream), cancel_tx, usage_state)
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

                // Cancellation-prioritized: an already-latched cancel wins
                // over a simultaneous result, so a cancelled run never
                // publishes chunks or a Final (checkpoint round-2 finding).
                tokio::select! {
                    biased;
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
                                        biased;
                                        _ = cancel_token_clone.cancelled() => {
                                            cancelled = true;
                                            break;
                                        }
                                        _ = event_tx.send(Ok(StreamItem::StreamAssistantItem(
                                            crate::provider_agent::StreamedAssistantContent::Text(text)
                                        ))) => {}
                                    }
                                }

                                if !cancelled {
                                    tokio::select! {
                                        biased;
                                        _ = cancel_token_clone.cancelled() => {}
                                        _ = event_tx.send(Ok(StreamItem::Final(
                                            crate::provider_agent::FinalResponseInfo {
                                                content: final_result,
                                                usage: Default::default(),
                                                cache_usage: None,
                                            }
                                        ))) => {}
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.send(Err(e)).await;
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
    use crate::hitl::{
        AgentScope, ApprovalDecision, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId,
        DecisionRoute, HitlRuntime, PROTOCOL_VERSION, ParkedApproval, PendingApprovals,
    };
    use crate::orchestration::park::resume::{ResumeEvaluation, ResumeGrant, evaluate_resume};
    use crate::orchestration::run_owner_id;
    use crate::orchestration::test_rig::{
        CoordinatorOverride, ECHO_TOOL_NAME, RecordingTool, ScriptedCompletionModel,
        ScriptedToolCall, ScriptedTurn, StallHook, WORKER_OVERRIDE_SERIAL, WorkerOverride,
        install_coordinator_overrides, install_worker_overrides, take_coordinator_override,
        take_worker_override,
    };
    use crate::orchestration::{
        OrchestrationConfig, OrchestratorEvent, ParkSnapshot, PendingCall, Plan, ResumeRunId, Task,
        TaskState, WorkerConfig,
    };
    use crate::provider_agent::{StreamError, StreamItem};
    use crate::session_store::{FileApprovalStore, InMemoryApprovalStore, InMemoryEventBus};
    use crate::streaming::StreamingAgent;
    use tokio::io::AsyncWriteExt;

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

    // =================================================================
    // S3 frames (the SSE wave's RED, aura/P45 joint hole #21): the
    // factory's resume supervisor. Each frame stages one whole granted
    // run — the same webhook-poll world the resume goldens build — and
    // drives it through `resume_stream_with_timeout`, RED today at the
    // hole's named todo.
    // =================================================================

    /// The granted-run frames' session path segment — distinct from
    /// `l4a-sess` (the initial-producer frames' fixture) so the two
    /// fixture families never collide on disk.
    const RESUME_SESSION: &str = "s3-resume-sess";
    /// The granted run's persistence-bound run id, fixed for determinism.
    const RESUME_RUN: &str = "0199c0de-1313-7000-8000-000000003131";
    /// The pending call's decision id.
    const RESUME_DECISION: &str = "0199c0de-1313-7000-8000-000000004141";
    /// The pending call's tool — inside the gate's `kubectl_*` pattern.
    const RESUME_TOOL: &str = "kubectl_apply";
    /// The pending call's id.
    const RESUME_CALL_ID: &str = "call_apply_1";
    /// The checkpoint's pending call arguments.
    fn resume_args() -> serde_json::Value {
        serde_json::json!({ "namespace": "prod" })
    }
    /// A long deadline the frames that mutate budgets post to.
    const RESUME_BOUND: Duration = Duration::from_secs(5);

    fn resume_decision() -> DecisionId {
        DecisionId::parse(RESUME_DECISION).expect("resume decision id parses")
    }

    /// The granted-run world a resume frame drives: the fingerprint-matching
    /// config, the file approval store behind the registry, the 207 poll
    /// receiver the fresh-gated paths deliver through, and the run's claim
    /// table the frames observe the fence through.
    struct GrantedRunWorld {
        // Held so the checkpoint tempdirs outlive every frame's drive.
        _dir: tempfile::TempDir,
        memory_dir: String,
        registry: PendingApprovals,
        config: AgentRuntimeConfig,
        claims: ResumeClaimTable,
        _receiver: tokio::task::JoinHandle<()>,
    }

    fn granted_world(mutate: impl FnOnce(&mut OrchestrationConfig)) -> GrantedRunWorld {
        let dir = tempfile::tempdir().expect("temp memory root");
        std::fs::create_dir_all(dir.path().join("approvals")).expect("approval dir");
        let store = Arc::new(
            FileApprovalStore::open(dir.path().join("approvals")).expect("file approval store"),
        );
        let registry = PendingApprovals::with_backend(
            std::sync::Arc::clone(&store) as Arc<dyn crate::session_store::ApprovalStore>,
            Arc::new(InMemoryEventBus::new()) as Arc<dyn crate::session_store::EventBus>,
        );
        let (url, receiver) = resume_receiver();
        let memory_dir = dir.path().join("memory").to_string_lossy().into_owned();
        std::fs::create_dir_all(&memory_dir).expect("memory dir");
        let mut workers = HashMap::new();
        workers.insert(
            "operations".to_string(),
            WorkerConfig {
                description: "Runs the gated apply".to_string(),
                preamble: "You apply changes with the gated tools.".to_string(),
                mcp_filter: Some(vec![]),
                vector_stores: vec![],
                turn_depth: None,
                llm: None,
                scratchpad: None,
                skills: None,
            },
        );
        let mut orchestration = OrchestrationConfig {
            enabled: true,
            workers,
            ..Default::default()
        };
        mutate(&mut orchestration);
        let config = AgentRuntimeConfig {
            hitl: Some(resume_route_hitl(&url, &registry)),
            memory_dir: Some(memory_dir.clone()),
            session_id: Some(RESUME_SESSION.to_string()),
            request_id: Some(format!("req_{}", uuid::Uuid::new_v4().simple())),
            orchestration: Some(orchestration),
            ..AgentRuntimeConfig::default()
        };
        GrantedRunWorld {
            _dir: dir,
            memory_dir,
            registry,
            config,
            claims: ResumeClaimTable::new(),
            _receiver: receiver,
        }
    }

    /// The persistent scripted 207 receiver the granted-world parks against:
    /// one ephemeral listener answering up to 64 POST connections with an
    /// empty `207 Multi-Status`, exactly like the resume goldens' world.
    fn resume_receiver() -> (String, tokio::task::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("receiver listener binds");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("receiver address")
        );
        listener
            .set_nonblocking(true)
            .expect("receiver listener goes non-blocking for tokio");
        let handle = tokio::spawn(async move {
            let listener =
                tokio::net::TcpListener::from_std(listener).expect("async receiver listener");
            for _ in 0..64 {
                let (mut socket, _) = match listener.accept().await {
                    Ok(accept) => accept,
                    Err(_) => return,
                };
                let _ = crate::hitl::read_full_request(&mut socket).await;
                let response = "HTTP/1.1 207 Multi-Status\r\ncontent-type: application/json\r\n\
                                content-length: 0\r\nconnection: close\r\n\r\n";
                socket.write_all(response.as_bytes()).await.ok();
                socket.shutdown().await.ok();
            }
        });
        (url, handle)
    }

    /// The webhook-poll HITL runtime the granted-world config arms (the same
    /// route shape `evaluate_resume` resumes against in production).
    fn resume_route_hitl(url: &str, registry: &PendingApprovals) -> HitlRuntime {
        let config = aura_config::HitlConfig {
            require_approval: vec![aura_config::GlobPattern::new("kubectl_*").unwrap()],
            park: aura_config::ParkConfig {
                enabled: true,
                bind_identity: false,
                park_ttl: aura_config::ParkTtl::default(),
            },
            route: aura_config::DecisionRouteConfig::Webhook {
                url: aura_config::WebhookUrl::new(url).unwrap(),
                timeout_secs: 3600,
                headers: HashMap::new(),
                headers_from_request: HashMap::new(),
                tool_headers_from_response: aura_config::ToolHeaderMappings::default(),
                delivery: aura_config::WebhookDelivery::Poll,
                poll_url: None,
                poll_interval_secs: 10,
                poll_request_timeout_secs: 30,
                receiver_wait_timeout_secs: 900,
            },
        };
        crate::hitl::HitlRuntime::from_config(&config, registry, None, None)
    }

    /// The worker-scoped approval for the checkpoint's pending call, riding
    /// the granted run's owner id (the same shape the resume goldens stamp).
    fn resume_approval() -> ParkedApproval {
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "factory-frame".to_string(),
                decision_id: resume_decision(),
                request_id: run_owner_id(RESUME_RUN),
                scope: AgentScope::Worker {
                    run_id: RESUME_RUN.parse().expect("the granted run id parses"),
                    task: crate::orchestration::TaskIdentity::new(3, None),
                    session_id: None,
                },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "kubectl_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: RESUME_TOOL.to_string(),
                    arguments: resume_args(),
                    tool_call_intent: None,
                }],
            },
            registered_at: chrono::Utc::now(),
            expires_at: chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
                .expect("the ticket stamp parses")
                .with_timezone(&chrono::Utc),
            authority: crate::hitl::ApprovalAuthority::WebhookPoll,
            egress_headers: None,
            acknowledgment: crate::hitl::AcknowledgmentState::RequiresNotification,
        }
    }

    /// The sentinel fixture the decided resume drives: one awaiting node
    /// whose pending call was recorded approved.
    async fn register_decided_run(world: &GrantedRunWorld) {
        world
            .registry
            .register_durable(resume_approval())
            .await
            .expect("register the granted approval");
        world
            .registry
            .resolve(
                &resume_decision(),
                crate::hitl::ApprovalAuthority::WebhookPoll,
                ApprovalDecision::Approved.into(),
            )
            .await
            .expect("record the approval");
    }

    /// The granted run's checkpoint plan: one AwaitingApproval node (task 3)
    /// whose pending call was registered in the store and recorded approved,
    /// plus — for the re-parking leg — one never-started Pending sibling
    /// (task 5). The park commit stamps the config fingerprint itself.
    fn granted_plan(with_sibling: bool) -> Plan {
        let mut plan = Plan::new("Deploy the service");
        plan.add_task(Task {
            id: 3,
            description: "Gated apply".to_string(),
            dependencies: vec![],
            state: TaskState::AwaitingApproval {
                pending: vec![PendingCall {
                    decision_id: resume_decision(),
                    tool_name: RESUME_TOOL.to_string(),
                    arguments: resume_args(),
                    call_id: RESUME_CALL_ID.to_string(),
                }],
            },
            worker: Some("operations".to_string()),
            rationale: "the manifest needs the gated apply".to_string(),
            structured_output: None,
        });
        if with_sibling {
            plan.add_task(Task {
                id: 5,
                description: "Run the post-apply deployment checks".to_string(),
                dependencies: vec![],
                state: TaskState::Pending,
                worker: Some("operations".to_string()),
                rationale: String::new(),
                structured_output: None,
            });
        }
        plan
    }

    /// The sentinel tool-result prompt a live park leaves as the awaiting
    /// node's current prompt: the placeholder keyed by the pending call's id.
    fn resume_sentinel_prompt() -> rig::completion::Message {
        rig::completion::Message::User {
            content: rig::OneOrMany::one(rig::message::UserContent::ToolResult(
                rig::message::ToolResult {
                    id: RESUME_CALL_ID.to_string(),
                    call_id: None,
                    content: rig::OneOrMany::one(rig::message::ToolResultContent::text(
                        serde_json::to_string(
                            "This tool call is parked pending human approval. \
                             It has not run. Do not retry.",
                        )
                        .expect("a plain string serializes"),
                    )),
                },
            )),
        }
    }

    /// Publish the granted run's checkpoint through the production park
    /// commit: the commit stamps the config fingerprint itself, so the
    /// document always matches the frame's config.
    async fn publish_checkpoint(world: &GrantedRunWorld, with_sibling: bool) {
        let plan = granted_plan(with_sibling);
        let mut records: HashMap<usize, crate::orchestration::ParkedTaskRecord> = HashMap::new();
        records.insert(
            3,
            crate::orchestration::ParkedTaskRecord {
                attempt: 1,
                snapshot: ParkSnapshot {
                    history: vec![rig::completion::Message::user("apply it")],
                    current_prompt: resume_sentinel_prompt(),
                },
            },
        );
        let chat_history = vec![rig::completion::Message::user("Deploy the service")];
        let inputs = crate::orchestration::ParkCommitInputs {
            state: crate::orchestration::RunStateForPark {
                run_id: RESUME_RUN,
                session_id: Some(RESUME_SESSION),
                query: "Deploy the service",
                chat_history: &chat_history,
                coordinator_conversation: &[],
                routing_decision: None,
                iteration: 1,
                planning_ms: 0,
                failure_history: &[],
            },
            plan: &plan,
            records: &records,
            registry: &world.registry,
            memory_dir: &world.memory_dir,
            config: &world.config,
            park_ttl: world
                .config
                .hitl
                .as_ref()
                .expect("the granted world's hitl")
                .park_ttl,
            identity_hash: None,
        };
        crate::orchestration::commit_from_run_state(&inputs, None)
            .await
            .expect("the granted checkpoint publishes");
    }

    /// Stage the checkpoint and grant the run: the frames' shared granted-run
    /// staging (register_decided + publish + evaluate).
    async fn granted_run(world: &GrantedRunWorld, with_sibling: bool) -> ResumeGrant {
        register_decided_run(world).await;
        publish_checkpoint(world, with_sibling).await;
        evaluate_resume(ResumeEvaluation {
            path: crate::orchestration::ValidatedResumePath::parse(RESUME_SESSION, RESUME_RUN)
                .expect("the granted path validates"),
            memory_dir: &world.memory_dir,
            config: &world.config,
            store: &world.registry,
            claims: &world.claims,
            bind_identity: false,
            presented_identity: None,
            request_id: format!("req_{}", uuid::Uuid::new_v4().simple()),
            now: chrono::Utc::now(),
        })
        .await
        .expect("the all-decided run grants")
    }

    /// The factory over the granted world's config, with its own (never
    /// consulted) reservation table: the resume surface the frames drive.
    fn granted_factory(world: &GrantedRunWorld) -> OrchestratorFactory {
        OrchestratorFactory::new(world.config.clone())
            .with_reservation_table(Arc::new(ResumeClaimTable::new()))
    }

    /// A factory frame's own namespaced scripted tool:
    /// the stand-in worker's invocation log the substitution's decided
    /// call rides.
    fn resume_invocations()
    -> Arc<std::sync::Mutex<Vec<crate::orchestration::test_rig::ToolInvocation>>> {
        Arc::new(std::sync::Mutex::new(Vec::new()))
    }

    /// One creation-plan coordinator decision turn with a marker as the
    /// fresh plan's text: the repeated-cycle shape the fresh-cycle pinning
    /// frames script several of.
    fn resume_plan_turn(marker: &str) -> ScriptedTurn {
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_plan",
            "create_plan",
            serde_json::json!({
                "goal": marker,
                "steps": [
                    {"type": "task", "task": marker, "worker": "operations"}
                ],
                "routing_rationale": "the restored run has tool work to finish",
                "planning_summary": marker,
            }),
        )])
        .with_text(marker)
    }

    /// The coordinator's scripted deterministic direct answer — the
    /// deterministic natural finish of a completed segment.
    const RESUME_COORD_ANSWER: &str = "the resumed run is done";

    fn resume_direct_turn() -> ScriptedTurn {
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_route",
            "respond_directly",
            serde_json::json!({
                "response": RESUME_COORD_ANSWER,
                "routing_rationale": "the resumed run's tasks are done",
            }),
        )])
        .with_text(RESUME_COORD_ANSWER)
    }

    /// A frame's own panic must not leak its undriven overrides into the next
    /// consumer's builds: the queues are take-once and process-global. Drains
    /// on drop; instantiate right after installing (the goldens' convention).
    struct ResumeOverrideDrain;

    impl Drop for ResumeOverrideDrain {
        fn drop(&mut self) {
            while take_worker_override().is_some() {
                // drained
            }
            while take_coordinator_override().is_some() {
                // drained
            }
        }
    }

    /// Drive a resumed factory stream to its terminal item, returning the
    /// last `Final` item's content (empty when none rode the stream).
    async fn drive_resume_final_text(
        stream: &mut BoxStream<'static, Result<StreamItem, StreamError>>,
    ) -> String {
        let mut final_text = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamItem::Final(info)) => final_text = info.content,
                Ok(_) => {}
                Err(e) => panic!("the resumed stream must not error: {e}"),
            }
        }
        final_text
    }

    /// Poll the granted run's claim table until the run's reservation is
    /// released, bounded — the supervisor's exit (its tracked tails joined)
    /// is the only release.
    async fn await_resume_release(world: &GrantedRunWorld, run: &ResumeRunId) {
        let deadline = Instant::now() + RESUME_BOUND;
        while world.claims.is_live(run) {
            assert!(
                Instant::now() < deadline,
                "the supervisor releases the run reservation within the bound"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// One `submit_result` turn the awaiting node's continuation streams:
    /// the loop's success semantics, the worker's completion report.
    fn resume_submit_turn(marker: &str) -> ScriptedTurn {
        ScriptedTurn::tool_calls(vec![ScriptedToolCall::new(
            "call_sub",
            "submit_result",
            serde_json::json!({
                "summary": "apply done",
                "result": marker,
                "confidence": "high",
            }),
        )])
        .with_text(marker)
    }

    /// The awaiting-node worker override the resumed drive loop builds
    /// first: its scripted model streams once (the substitution precedes
    /// every model turn), carrying the shared invocation log.
    fn resume_worker_override(
        turns: Vec<ScriptedTurn>,
        invocations: Arc<std::sync::Mutex<Vec<crate::orchestration::test_rig::ToolInvocation>>>,
    ) -> WorkerOverride {
        WorkerOverride {
            model: ScriptedCompletionModel::new(turns),
            extra_tools: vec![Box::new(
                RecordingTool::new(invocations).with_name(RESUME_TOOL),
            )],
        }
    }

    /// The fresh-gated call the never-started sibling's continuation
    /// issues: inside the gate's `kubectl_*` pattern, distinct from the
    /// checkpoint's decided call, so a re-parking segment registers a NEW
    /// pending call.
    const RESUME_NEW_TOOL: &str = "kubectl_delete";
    const RESUME_NEW_CALL_ID: &str = "call_id_0";
    const RESUME_FRESH_CALL_ID: &str = "call_0";

    /// The tracked-tail stand-in the decided call's tool carries: on
    /// invocation it spawns a TRACKED tail through the run's ONE execution
    /// scope — the same registration path every production fire-and-forget
    /// tail takes — and holds until the frame releases it. The spawn
    /// happens DURING the segment (the substitution invokes this tool), so
    /// the tail is still in flight when the segment body finishes. The
    /// `finished` counter increments in the tail's own completion moment,
    /// so a frame can pin "the tail had not completed" ahead of its
    /// release and "the tail completed" after it. The `spawned` counter
    /// increments at the SUBSTITUTION's spawn point itself — the frame's
    /// synchronisation on the held tail.
    struct ResumeGatedTailTool {
        scope: Arc<RunExecutionScope>,
        gate: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        spawned: Arc<std::sync::atomic::AtomicUsize>,
        finished: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ResumeGatedTailTool {
        /// Arm the stand-in over the run's ONE scope with the frame's two
        /// observables: the substitution's spawn moment and the tail's own
        /// completion moment.
        fn new(
            scope: Arc<RunExecutionScope>,
            gate: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
            spawned: Arc<std::sync::atomic::AtomicUsize>,
            finished: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self {
                scope,
                gate,
                spawned,
                finished,
            }
        }
    }

    impl rig::tool::Tool for ResumeGatedTailTool {
        const NAME: &'static str = "resume_gated_tail";

        type Error = std::convert::Infallible;
        type Args = crate::orchestration::test_rig::FreeformArgs;
        type Output = String;

        fn name(&self) -> String {
            RESUME_TOOL.to_string()
        }

        async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
            rig::completion::ToolDefinition {
                name: self.name(),
                description: "Test stand-in: spawns a tracked tail on the run's scope.".to_string(),
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
            }
        }

        async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
            let gate = self
                .gate
                .lock()
                .expect("the gated tail's gate lock")
                .take()
                .expect("the gated tail spawns once");
            let finished = Arc::clone(&self.finished);
            self.spawned
                .fetch_add(1, std::sync::atomic::Ordering::Release);
            self.scope.spawn_tracked(async move {
                let _ = gate.await;
                finished.fetch_add(1, std::sync::atomic::Ordering::Release);
            });
            Ok(ECHO_TOOL_NAME.to_string())
        }
    }

    /// Hold the gated tail's spawn point open: poll the frame's `spawned`
    /// handle until the substitution has registered the tracked tail
    /// (in flight, holding) — bounded.
    async fn await_tail_spawned(spawned: &Arc<std::sync::atomic::AtomicUsize>) {
        let deadline = Instant::now() + RESUME_BOUND;
        while spawned.load(std::sync::atomic::Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "the decided call's substitution spawns the tracked tail within the bound"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// The pre-release blocked-window probe: assert the supervisor is
    /// BLOCKED on its drain for a bounded window CONCURRENTLY with the
    /// held tail — the window runs to its end and EXITS there; it never
    /// waits for the hold to end. On every iteration of the window: (i)
    /// the fence is still live (`is_live`) and (ii) the stream has not
    /// terminated (`futures::poll!` stays pending). Under an honest
    /// implementation the window passes (drain blocked, fence live), and
    /// the caller then releases the tail; under a skip-drain shortcut the
    /// supervisor exits DURING the window and the probe fails that
    /// iteration. The callers never reach this probe under today's todo
    /// state (the factory call panics at the S3 hole).
    async fn await_drain_blocked_window(
        world: &GrantedRunWorld,
        run: &ResumeRunId,
        stream: &mut BoxStream<'static, Result<StreamItem, StreamError>>,
    ) {
        for _ in 0..20 {
            assert!(
                world.claims.is_live(run),
                "the supervisor's fence released before its drain completed: \
                 the exit arm's drain must hold the in-flight tail"
            );
            assert!(
                !futures::poll!(stream.next()).is_ready(),
                "the supervisor's stream terminated while its drain still \
                 waited on the in-flight tail: the exit arm drained late or \
                 skipped its tracked-child join"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// Frame 1 (S3): `outer_budget` projects from the timeout by the zero-
    /// timeout convention. NONZERO timeout → the resumed coordinator's
    /// `outer_budget` is `Some(timeout)` — observable at the strongest seam
    /// the module's harness can reach: the projected budget's behavioral
    /// consequence at the segment seam. The fixture's per-call slice is
    /// bigger than the whole budget, so with the projection engaged the
    /// FIRST post-execute `create_plan` hits the 'Time budget exhausted'
    /// arm and the loop stops (one scripted decision turn consumed); with
    /// the ZERO timeout projecting `None`, the loop replans normally
    /// (three decision turns consumed; the fourth scripted reference
    /// never consulted).
    #[tokio::test]
    async fn s3_outer_budget_projects_from_the_factory_timeout_by_the_zero_timeout_convention() {
        let _serial = WORKER_OVERRIDE_SERIAL.lock().await;

        // Arm A: a NONZERO timeout whose whole budget cannot fit even one
        // per-call slice — the projected Some(timeout) engages the
        // coordinator's budget check.
        let arm_a_world = granted_world(|orchestration| {
            orchestration.timeouts.per_call_timeout_secs = 5;
        });
        let arm_a_grant = granted_run(&arm_a_world, false).await;
        let arm_a_factory = granted_factory(&arm_a_world);
        let arm_a_coordinator = ScriptedCompletionModel::new(vec![resume_plan_turn("cycle one")]);
        let arm_a_requests = arm_a_coordinator.requests();
        install_coordinator_overrides(vec![CoordinatorOverride {
            model: arm_a_coordinator,
        }]);
        install_worker_overrides(vec![resume_worker_override(
            vec![resume_submit_turn("applied and settled")],
            resume_invocations(),
        )]);

        let (mut stream, _cancel_tx, _usage) = arm_a_factory
            .resume_stream_with_timeout(arm_a_grant, Duration::from_secs(1), "req_budget_some")
            .await;
        let arm_a_text = drive_resume_final_text(&mut stream).await;
        assert_eq!(
            arm_a_requests
                .lock()
                .expect("coordinator request log")
                .len(),
            1,
            "the projected Some(timeout) outer budget stops the coordinator loop at \
             its FIRST create_plan decision ('Time budget exhausted') — the projected \
             value reached the segment seam"
        );
        assert!(
            arm_a_text.contains("Time budget exhausted"),
            "the projected budget's exhausted arm rides the resumed run's final \
             answer: {arm_a_text:?}"
        );

        // Arm B: a ZERO timeout projects `None` — the budget does not
        // engage, so the coordinator replans normally (three decision
        // turns; the fourth is never requested, max three fresh cycles).
        let arm_b_world = granted_world(|orchestration| {
            orchestration.timeouts.per_call_timeout_secs = 5;
        });
        let arm_b_grant = granted_run(&arm_b_world, false).await;
        let arm_b_factory = granted_factory(&arm_b_world);
        let arm_b_coordinator = ScriptedCompletionModel::new(vec![
            resume_plan_turn("cycle one"),
            resume_plan_turn("cycle two"),
            resume_plan_turn("cycle three"),
            resume_direct_turn(),
        ]);
        let arm_b_requests = arm_b_coordinator.requests();
        install_coordinator_overrides(vec![CoordinatorOverride {
            model: arm_b_coordinator,
        }]);
        // Every fresh `operations` plan consumes its OWN worker build: the
        // restored node's build first, then one override per permitted
        // fresh cycle, so all three fresh cycles are reachable by an
        // honest implementation (an exhausted queue is never a passage).
        install_worker_overrides(vec![
            resume_worker_override(
                vec![resume_submit_turn("applied and settled")],
                resume_invocations(),
            ),
            resume_worker_override(
                vec![resume_submit_turn("fresh one settled")],
                resume_invocations(),
            ),
            resume_worker_override(
                vec![resume_submit_turn("fresh two settled")],
                resume_invocations(),
            ),
            resume_worker_override(
                vec![resume_submit_turn("fresh three settled")],
                resume_invocations(),
            ),
        ]);

        let (mut stream, _cancel, _usage) = arm_b_factory
            .resume_stream_with_timeout(arm_b_grant, Duration::ZERO, "req_budget_none")
            .await;
        let arm_b_text = drive_resume_final_text(&mut stream).await;
        assert_eq!(
            arm_b_requests
                .lock()
                .expect("coordinator request log")
                .len(),
            3,
            "the projected None budget never disturbs the coordinator loop — all \
             three decision turns consumed, the fourth never requested"
        );
        assert!(
            arm_b_text.contains("Replan budget exhausted"),
            "the zero-timeout arm's own stopping reason is the cycle budget, not \
             the projected outer one: {arm_b_text:?}"
        );
    }

    /// Frame 2 (S3): firing the returned watch sender cancels the grant's
    /// execution scope — the supervisor's own cancellation path is that
    /// bridge. The scope's cancelled state is the observed effect: absent
    /// before the watch fires, present after it.
    ///
    /// The cancellation surface holding the run has no second token to
    /// name: the frozen `run_segment_borrowed` signature carries no token
    /// parameter (evaluate.rs:973), so "the watch bridge is the ONE
    /// cancellation path" is a structural property of the frozen surface,
    /// not a runtime-observable — this frame observes the bridge's effect
    /// alone.
    #[tokio::test]
    async fn s3_request_scoped_watcher_bridges_cancel_to_the_grants_one_scope() {
        let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
        let _drain = ResumeOverrideDrain;
        let world = granted_world(|_| {});
        let grant = granted_run(&world, false).await;
        // The scope handle is captured BEFORE the factory consumes the grant:
        // the grant's ONE execution scope is the bridge's observable.
        let scope = grant.execution_scope();
        let factory = granted_factory(&world);
        install_coordinator_overrides(vec![CoordinatorOverride {
            model: ScriptedCompletionModel::new(vec![resume_direct_turn()]),
        }]);
        install_worker_overrides(vec![resume_worker_override(
            vec![resume_submit_turn("applied and settled")],
            resume_invocations(),
        )]);

        let (stream, cancel_tx, _usage) = factory
            .resume_stream_with_timeout(grant, Duration::from_secs(600), "req_watcher_bridge")
            .await;

        // Before the watch fires, no cancellation runs.
        assert!(
            !scope.cancellation().is_cancelled(),
            "the grant's execution scope stays live until the watch fires"
        );

        cancel_tx.send(true).expect("the cancellation watch sends");
        let deadline = Instant::now() + RESUME_BOUND;
        while !scope.cancellation().is_cancelled() {
            assert!(
                Instant::now() < deadline,
                "firing the returned watch sender cancels the grant's ONE execution \
                 scope within the bound"
            );
            sleep(Duration::from_millis(10)).await;
        }
        drop(stream);
    }

    /// Frame 3 (S3): disconnect before the first SSE byte cancels execution —
    /// the returned stream dropped without consuming any item cancels the
    /// grant's execution scope, and the reservation fence releases only
    /// after the supervisor's drain, not at stream drop. The drain-wait is
    /// made OBSERVABLE: a tracked tail spawned during the segment holds
    /// past the segment body, so the frame can pin (i) the fence stays
    /// live after the stream's drop (release-at-drop falsified), and
    /// (ii) the tail's completion happens only at the drain's release —
    /// the tail's own completion moment rendezvouses with the fence's.
    #[tokio::test]
    async fn s3_disconnect_before_first_sse_byte_cancels_execution() {
        let _serial = WORKER_OVERRIDE_SERIAL.lock().await;
        let _drain = ResumeOverrideDrain;
        let world = granted_world(|_| {});
        let grant = granted_run(&world, false).await;
        let run = grant.run_id().clone();
        let scope = grant.execution_scope();
        let spawned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let finished = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory = granted_factory(&world);
        install_coordinator_overrides(vec![CoordinatorOverride {
            model: ScriptedCompletionModel::new(vec![resume_direct_turn()]),
        }]);
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let gate = Arc::new(std::sync::Mutex::new(Some(gate)));
        install_worker_overrides(vec![WorkerOverride {
            model: ScriptedCompletionModel::new(vec![resume_submit_turn("applied and settled")]),
            extra_tools: vec![Box::new(ResumeGatedTailTool::new(
                Arc::clone(&scope),
                Arc::clone(&gate),
                Arc::clone(&spawned),
                Arc::clone(&finished),
            ))],
        }]);

        let (stream, _cancel_tx, _usage) = factory
            .resume_stream_with_timeout(grant, Duration::from_secs(600), "req_disconnect_early")
            .await;
        // The substitution runs even on a stream nobody consumes: the
        // decided call's tool spawns the tracked tail and holds it.
        await_tail_spawned(&spawned).await;

        // The disconnect: the stream is dropped WITHOUT ever consuming an
        // item — no first SSE byte ever rode it.
        drop(stream);

        // The supervisor's disconnect detection cancels the grant's scope.
        let deadline = Instant::now() + RESUME_BOUND;
        while !scope.cancellation().is_cancelled() {
            assert!(
                Instant::now() < deadline,
                "dropping the returned stream before any SSE byte cancels the \
                 grant's execution scope within the bound"
            );
            sleep(Duration::from_millis(10)).await;
        }

        // The fence stays live past the drop: the held tail keeps the
        // supervisor's drain pending (release-at-drop falsified).
        assert!(
            world.claims.is_live(&run),
            "the fence is live after the stream's drop — the reservation releases \
             only after the supervisor's drain completes, never at drop"
        );
        // The tail had not completed while the drain held.
        assert!(
            finished.load(std::sync::atomic::Ordering::Acquire) == 0,
            "the tracked tail had not completed while the supervisor's drain \
             waited on it"
        );

        // End the frame's own lease binding: from here the only remaining
        // owner of the run's reservation is the supervisor's drive (the
        // grant's lease through its drain) — without this drop the next
        // assert could never observe a release at all.
        drop(scope);

        // Releasing the tail completes the drain — the tail's completion
        // moment and the fence's release are the same transition.
        release.send(()).expect("the gated tail is still held");
        await_resume_release(&world, &run).await;
        assert!(
            finished.load(std::sync::atomic::Ordering::Acquire) == 1,
            "the tracked tail completed exactly once, at the drain's end"
        );
    }

    // =================================================================
    // FRAME 4 WITHDRAWN (owner ruling, round 2 — same disposition as the
    // frames 5/6 withdrawal this round accepted): `s3_cancellation_aware_
    // send_never_strands_the_supervisor` cannot test the BLOCKED-send
    // class honestly from this module. Falsifying a genuinely blocked
    // send requires controlling the supervisor's internal channel
    // occupancy — the fill-owned capacity and per-segment event count,
    // neither constructible from the test module: a scripted segment's
    // few forwarded events against the factory's capacity-100 channel
    // never block, so no honest staging of the blocked-send class exists.
    // Cancellation-aware sends are review-covered (Gate A plus the phase
    // checkpoint verify the biased select pattern); the no-strand
    // property itself is covered by frame 3's and frame 7's drain
    // assertions (a stranded supervisor never reaches its drain, so their
    // bounded release waits fail).
    // =================================================================

    /// Frame 7 (S3): tracked tails spawned during the segment are joined in
    /// EVERY exit arm before the fence releases, asserted via the run's
    /// reservation-table state transitions. Four legs: COMPLETED (terminals
    /// volitional), RE-PARKED (the publication arm), FAULT (the fault
    /// arrives after a tracked tail spawned), and CANCELLED (the run's
    /// consumer cancels after a tracked tail spawned). In each, the run's
    /// ONE execution scope carried the tracked tail across the segment
    /// body's end, the fence stayed live while the drain waited it out,
    /// and the release happened only after the drain completed.
    #[tokio::test]
    async fn s3_supervisor_drains_tracked_work_in_every_exit_arm() {
        let _serial = WORKER_OVERRIDE_SERIAL.lock().await;

        // -------- Leg A: the COMPLETED exit arm ------------------------
        {
            let _drain = ResumeOverrideDrain;
            let world = granted_world(|_| {});
            let granted = granted_run(&world, false).await;
            let run = granted.run_id().clone();
            // The tail registers through the grant's ONE scope (captured
            // before the factory consumes the grant).
            let scope = granted.execution_scope();
            let spawned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let finished = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (release, gate) = tokio::sync::oneshot::channel::<()>();
            let gate = Arc::new(std::sync::Mutex::new(Some(gate)));
            let tool = ResumeGatedTailTool::new(
                scope,
                Arc::clone(&gate),
                Arc::clone(&spawned),
                Arc::clone(&finished),
            );
            install_worker_overrides(vec![WorkerOverride {
                model: ScriptedCompletionModel::new(vec![resume_submit_turn(
                    "applied and settled",
                )]),
                extra_tools: vec![Box::new(tool)],
            }]);
            install_coordinator_overrides(vec![CoordinatorOverride {
                model: ScriptedCompletionModel::new(vec![resume_direct_turn()]),
            }]);
            let factory = granted_factory(&world);
            let (mut stream, _cancel, _usage) = factory
                .resume_stream_with_timeout(
                    granted,
                    Duration::from_secs(600),
                    "req_drain_completed",
                )
                .await;

            // The completed exit arm: with the tracked tail still in flight,
            // the supervisor cannot reach its terminal finalization — no
            // Final may arrive while the fence holds the drain.
            let blocked =
                tokio::time::timeout(RESUME_BOUND, drive_resume_final_text(&mut stream)).await;
            assert!(
                blocked.is_err(),
                "the resumed stream finalized while a tracked tail was still in \
                 flight: the supervisor's drain must wait out every tracked child \
                 before the fence releases"
            );
            assert!(
                world.claims.is_live(&run),
                "the run stays reserved while the supervisor's drain waits out the \
                 in-flight tail"
            );

            release.send(()).expect("the gated tail is still held");
            let final_text =
                tokio::time::timeout(RESUME_BOUND, drive_resume_final_text(&mut stream))
                    .await
                    .expect("the stream finalizes once its tracked tail ends");
            assert_eq!(
                final_text, RESUME_COORD_ANSWER,
                "the completed exit arm finalizes through the scripted natural \
                 finish after the drain releases: {final_text:?}"
            );
            await_resume_release(&world, &run).await;
        }

        // -------- Leg B: the RE-PARKED (publication) exit arm -----------
        {
            let _drain = ResumeOverrideDrain;
            let world = granted_world(|_| {});
            let granted = granted_run(&world, true).await;
            let run = granted.run_id().clone();
            let scope = granted.execution_scope();
            let spawned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let finished = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (release, gate) = tokio::sync::oneshot::channel::<()>();
            let gate = Arc::new(std::sync::Mutex::new(Some(gate)));
            install_worker_overrides(vec![
                // Build order: the drive loop's awaiting node first (its
                // substitution spawns the tracked tail), then the sibling's
                // (a build only the resumed coordinator loop can make; its
                // continuation issues the fresh gated call that re-parks).
                WorkerOverride {
                    model: ScriptedCompletionModel::new(vec![resume_submit_turn(
                        "applied and settled",
                    )]),
                    extra_tools: vec![Box::new(ResumeGatedTailTool::new(
                        scope,
                        Arc::clone(&gate),
                        Arc::clone(&spawned),
                        Arc::clone(&finished),
                    ))],
                },
                WorkerOverride {
                    model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                        ScriptedToolCall::new(
                            RESUME_FRESH_CALL_ID,
                            RESUME_NEW_TOOL,
                            serde_json::json!({ "namespace": "stage" }),
                        )
                        .with_call_id(RESUME_NEW_CALL_ID),
                    ])]),
                    extra_tools: vec![],
                },
            ]);
            // The continuation builds its coordinator before the loop; the
            // park path skips the coordinator call, so the script is never
            // consumed by a request.
            install_coordinator_overrides(vec![CoordinatorOverride {
                model: ScriptedCompletionModel::new(vec![resume_direct_turn()]),
            }]);
            let factory = granted_factory(&world);
            let (mut stream, _cancel, _usage) = factory
                .resume_stream_with_timeout(granted, Duration::from_secs(600), "req_drain_reparked")
                .await;

            // The re-parked exit arm: the RunParked event (the publication
            // owner's ONE terminal) rides the stream BEFORE the segment
            // body ends, with the tracked tail still in flight.
            let parked = tokio::time::timeout(RESUME_BOUND, stream.next()).await;
            let Ok(Some(item)) = parked else {
                panic!(
                    "a re-parking run publishes its checkpoint and emits \
                        RunParked within the bound: settled {parked:?}"
                )
            };
            assert!(
                matches!(
                    item,
                    Ok(StreamItem::OrchestratorEvent(
                        OrchestratorEvent::RunParked { .. }
                    ))
                ),
                "the publication owner's RunParked rides the stream: got {item:?}"
            );

            // The supervisor is BLOCKED on the drain BEFORE the release
            // (completed-leg shape): the bounded blocked-window runs
            // CONCURRENTLY with the hold — the fence stays held and the
            // stream stays open past the publication owner's RunParked for
            // the window's whole run; the window then exits and the frame
            // releases the tail.
            await_drain_blocked_window(&world, &run, &mut stream).await;

            release.send(()).expect("the gated tail is still held");
            // The re-parked arm carries no Final: the publication owner's
            // RunParked is the terminal, and the supervisor's exit drains.
            let rest = drive_resume_final_text(&mut stream).await;
            assert!(
                rest.is_empty(),
                "a re-parked segment carries no Final — its own terminal is the \
                 publication owner's RunParked: {rest:?}"
            );
            await_resume_release(&world, &run).await;
        }

        // -------- Leg C: the FAULT exit arm ------------------------------
        {
            let _drain = ResumeOverrideDrain;
            let world = granted_world(|_| {});
            let granted = granted_run(&world, false).await;
            let run = granted.run_id().clone();
            let scope = granted.execution_scope();
            let spawned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let finished = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (release, gate) = tokio::sync::oneshot::channel::<()>();
            let gate = Arc::new(std::sync::Mutex::new(Some(gate)));
            install_worker_overrides(vec![WorkerOverride {
                // The substitution's decided call spawns the tracked tail;
                // the node's own continuation then fails its provider
                // stream deterministically (the loop's established
                // mid-turn error break), faulting the segment AFTER the
                // tail was spawned.
                model: ScriptedCompletionModel::new(vec![
                    ScriptedTurn::tool_calls_then_stream_failure(vec![]),
                ]),
                extra_tools: vec![Box::new(ResumeGatedTailTool::new(
                    scope,
                    Arc::clone(&gate),
                    Arc::clone(&spawned),
                    Arc::clone(&finished),
                ))],
            }]);
            // The fault arm exits the drive loop before an answer, so no
            // coordinator decision is ever consulted; the queue's script
            // stays unconsumed (the drain clears what no build consumed).
            install_coordinator_overrides(vec![CoordinatorOverride {
                model: ScriptedCompletionModel::new(vec![resume_direct_turn()]),
            }]);
            let factory = granted_factory(&world);
            let (mut stream, _cancel, _usage) = factory
                .resume_stream_with_timeout(granted, Duration::from_secs(600), "req_drain_fault")
                .await;

            // The tail spawns during the substitution (still in flight);
            // then the node's continuation faults.
            await_tail_spawned(&spawned).await;
            let faulted = tokio::time::timeout(RESUME_BOUND, stream.next()).await;
            let Ok(Some(Err(_))) = faulted else {
                panic!(
                    "a faulting resumed segment surfaces its error on the \
                     factory stream within the bound: settled {faulted:?}"
                )
            };

            // The supervisor is BLOCKED on the drain BEFORE the release
            // (completed-leg shape): the bounded blocked-window runs
            // CONCURRENTLY with the hold — the fence stays held and the
            // stream stays open past the fault's error item for the
            // window's whole run; the window then exits and the frame
            // releases the tail.
            await_drain_blocked_window(&world, &run, &mut stream).await;

            release.send(()).expect("the gated tail is still held");
            let _rest = drive_resume_final_text(&mut stream).await;
            await_resume_release(&world, &run).await;
            assert!(
                finished.load(std::sync::atomic::Ordering::Acquire) == 1,
                "the tracked tail joined only at the fault arm's drain end"
            );
        }

        // -------- Leg D: the CANCELLED exit arm -------------------------
        {
            let _drain = ResumeOverrideDrain;
            let world = granted_world(|_| {});
            let granted = granted_run(&world, false).await;
            let run = granted.run_id().clone();
            let scope = granted.execution_scope();
            let spawned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let finished = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (release, gate) = tokio::sync::oneshot::channel::<()>();
            let gate = Arc::new(std::sync::Mutex::new(Some(gate)));
            install_worker_overrides(vec![WorkerOverride {
                // The continuation holds: the cancellation fires while the
                // node's stream is still live, with the tracked tail held
                // at its spawn point — the supervisor's cancel arm runs.
                model: ScriptedCompletionModel::new(vec![ScriptedTurn::tool_calls(vec![
                    ScriptedToolCall::new(
                        "call_sub_a",
                        "submit_result",
                        serde_json::json!({
                            "summary": "apply done",
                            "result": "applied cleanly",
                            "confidence": "high",
                        }),
                    ),
                ])]),
                extra_tools: vec![Box::new(ResumeGatedTailTool::new(
                    scope,
                    Arc::clone(&gate),
                    Arc::clone(&spawned),
                    Arc::clone(&finished),
                ))],
            }]);
            install_coordinator_overrides(vec![CoordinatorOverride {
                model: ScriptedCompletionModel::new(vec![resume_direct_turn()]),
            }]);
            let factory = granted_factory(&world);
            let (mut stream, cancel_tx, _usage) = factory
                .resume_stream_with_timeout(
                    granted,
                    Duration::from_secs(600),
                    "req_drain_cancelled",
                )
                .await;

            // The tail spawns during the substitution; the run's own
            // cancellation fires WHILE it is still in flight.
            await_tail_spawned(&spawned).await;
            cancel_tx.send(true).expect("the cancellation watch sends");

            // The cancelled run still reaches its stop within the bound.
            tokio::time::timeout(RESUME_BOUND, stream.next())
                .await
                .expect("a cancelling run surfaces its stop within the bound");

            // The supervisor is BLOCKED on the drain BEFORE the release
            // (completed-leg shape): the bounded blocked-window runs
            // CONCURRENTLY with the hold — the fence stays held and the
            // stream stays open past the cancelled arm's stop for the
            // window's whole run; the window then exits and the frame
            // releases the tail.
            await_drain_blocked_window(&world, &run, &mut stream).await;

            release.send(()).expect("the gated tail is still held");
            await_resume_release(&world, &run).await;
            assert!(
                finished.load(std::sync::atomic::Ordering::Acquire) == 1,
                "the tracked tail completed exactly once, joined at the \
                 cancelled arm's drain end"
            );
        }
    }
}
