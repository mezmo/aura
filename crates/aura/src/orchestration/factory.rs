//! Lightweight factory for orchestration streaming.
//!
//! `OrchestratorFactory` implements `StreamingAgent` without constructing a full
//! `Orchestrator` up front. The real orchestrator is created lazily inside `stream()`
//! when a request arrives, ensuring MCP progress notifications route correctly
//! and avoiding duplicate resource allocation.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{self, BoxStream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use std::sync::Arc;

use crate::config::AgentRuntimeConfig;
use crate::provider_agent::{StreamError, StreamItem};
use crate::session_store::RunStore;
use crate::streaming::StreamingAgent;

use super::RunId;
use super::orchestrator::{
    Orchestrator, STREAM_CHUNK_SIZE, spawn_cancellation_watcher, spawn_tool_event_forwarder,
};
use super::park::{
    AgentInstanceId, FencingGeneration, LeaseTtl, RunEvent, RunState, Session, SessionId,
    SessionRecord,
};

/// Zero-state wrapper that implements `StreamingAgent` for orchestration mode.
///
/// Defers `Orchestrator` construction to `stream()` to avoid duplicate resource
/// allocation and ensure MCP progress notifications route correctly.
pub struct OrchestratorFactory {
    agent_config: AgentRuntimeConfig,
    /// The durable-parking capability from the deployment's session store,
    /// when it provides one. Handed to each lazily built `Orchestrator` the
    /// same way `usage_state` is; `None` (the default) means quiescent
    /// blocking cannot park and refuses fail-closed.
    run_store: Option<Arc<dyn RunStore>>,
}

impl OrchestratorFactory {
    pub fn new(agent_config: AgentRuntimeConfig) -> Self {
        Self {
            agent_config,
            run_store: None,
        }
    }

    /// Arm durable parking with the session store's run-store capability.
    #[must_use]
    pub fn with_run_store(mut self, run_store: Arc<dyn RunStore>) -> Self {
        self.run_store = Some(run_store);
        self
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
        let run_store = self.run_store.clone();
        let park_window = agent_config
            .hitl
            .as_ref()
            .and_then(crate::hitl::HitlRuntime::parkable_timeout);

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
                // Share the caller's usage handle so accumulate_usage() writes
                // are visible to the streaming handler (UsageState is Arc-backed).
                orchestrator.usage_state = usage_state;
                orchestrator.outer_budget = outer_budget;
                orchestrator.run_store = run_store;
                orchestrator.request_id = request_id.clone();

                // Wire the durable session id and fencing generation so
                // commit_quiescent_park can perform the park CAS. Failures here
                // are non-fatal: park refuses fail-closed at call time rather
                // than aborting the entire run.
                if let (Some(store), Some(park_window)) =
                    (orchestrator.run_store.as_ref().map(Arc::clone), park_window)
                {
                    let run_id = orchestrator.persistence.lock().await.run_id().to_string();
                    match begin_run(store.as_ref(), &run_id, park_window).await {
                        Ok((session_id, generation)) => {
                            orchestrator.session_id = Some(session_id);
                            orchestrator.fencing_generation = Some(generation);
                        }
                        Err(e) => {
                            tracing::warn!("durable parking unarmed: run start failed: {e}");
                        }
                    }
                }

                // Set MCP request ID for progress notification routing, and
                // surface per-server connection status so degraded/unavailable
                // MCP servers are visible in orchestration mode too (workers
                // share this one manager).
                if let Some(ref mcp_manager) = orchestrator.mcp_manager {
                    mcp_manager.set_current_request(&request_id).await;
                    let snapshot = mcp_manager.server_status_snapshot();
                    if !snapshot.is_empty() {
                        let _ = event_tx.send(Ok(StreamItem::McpStatus(snapshot))).await;
                    }
                }

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
                                for chunk in final_result.chars().collect::<Vec<_>>().chunks(STREAM_CHUNK_SIZE) {
                                    let text: String = chunk.iter().collect();
                                    let _ = event_tx.send(Ok(StreamItem::StreamAssistantItem(
                                        crate::provider_agent::StreamedAssistantContent::Text(text)
                                    ))).await;
                                }

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

/// Mint the run's durable session, claim its lease, and move the record
/// `Created -> Running` before the first wave.
///
/// The park CAS presents the generation this returns, not the lease's: the
/// `Start` write advances the record, so the lease's own generation is
/// stale the moment it succeeds. The lease is held for the park window, so
/// a run that parks keeps its claim for as long as the approval is
/// claimable.
pub(super) async fn begin_run(
    store: &dyn RunStore,
    run_id: &str,
    park_window: Duration,
) -> Result<(SessionId, FencingGeneration), Box<dyn std::error::Error + Send + Sync>> {
    let run_id: RunId = run_id.parse()?;
    let session_id = SessionId::generate();
    let ttl = LeaseTtl::new(park_window)?;
    store
        .create(SessionRecord {
            session: Session {
                id: session_id,
                chat_session_id: None,
                created_at: Utc::now(),
            },
            run_id: None,
            state: RunState::Created,
            lease: None,
            generation: FencingGeneration::INITIAL,
        })
        .await?;
    // A lease this claim cannot take strands the record just minted above.
    // `RunStore` has no delete, so there is nothing to undo it with here;
    // the record stays inert `Created` debris that no run points at, and
    // P8's reaper collects it.
    let lease = store
        .acquire_lease(session_id, AgentInstanceId::generate(), ttl)
        .await?;
    let started = match store
        .apply(session_id, lease.generation, RunEvent::Start { run_id })
        .await
    {
        Ok(record) => record,
        Err(e) => {
            // Hand the lease back so a later claim is not fenced out by a
            // holder that never ran. The `Created` record it leaves behind is
            // inert - no run points at it - and P8's reaper collects it.
            if let Err(release) = store.release_lease(session_id, lease.generation).await {
                tracing::warn!("run start failed and its lease could not be released: {release}");
            }
            return Err(e.into());
        }
    };
    Ok((session_id, started.generation))
}

#[async_trait]
impl StreamingAgent for OrchestratorFactory {
    fn get_provider_info(&self) -> (&str, &str) {
        self.agent_config.llm.model_info()
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
