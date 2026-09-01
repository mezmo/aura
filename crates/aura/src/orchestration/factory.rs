//! Lightweight factory for orchestration streaming.
//!
//! `OrchestratorFactory` implements `StreamingAgent` without constructing a full
//! `Orchestrator` up front. The real orchestrator is created lazily inside `stream()`
//! when a request arrives, ensuring MCP progress notifications route correctly
//! and avoiding duplicate resource allocation.

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio_util::sync::CancellationToken;

use crate::config::AgentRuntimeConfig;
use crate::provider_agent::{StreamError, StreamItem};
use crate::streaming::StreamingAgent;

use super::orchestrator::{
    Orchestrator, STREAM_CHUNK_SIZE, spawn_timeout_watcher, spawn_tool_event_forwarder,
};

/// Zero-state wrapper that implements `StreamingAgent` for orchestration mode.
///
/// Defers `Orchestrator` construction to `stream()` to avoid duplicate resource
/// allocation and ensure MCP progress notifications route correctly.
pub struct OrchestratorFactory {
    agent_config: AgentRuntimeConfig,
}

impl OrchestratorFactory {
    pub fn new(agent_config: AgentRuntimeConfig) -> Self {
        Self { agent_config }
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

        // Create channel for orchestrator events
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(100);

        let cancel_token_clone = cancel_token.clone();
        // Cancels on every exit path, which is what lets the timeout watcher
        // stop rather than sleeping out its full duration after a run ends.
        let done_guard = cancel_token.clone().drop_guard();
        // Capture parent span so child spans nest correctly in tracing.
        let parent_span = tracing::Span::current();
        tokio::spawn(tracing::Instrument::instrument(
            crate::run_context::with_run_id(request_id.clone(), async move {
                let _done_guard = done_guard;
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

                // Surface per-server connection status so degraded/unavailable
                // MCP servers are visible in orchestration mode too (workers
                // share this one manager).
                if let Some(ref mcp_manager) = orchestrator.mcp_manager {
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
            }),
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

    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        timeout: Option<Duration>,
        request_id: &str,
    ) -> crate::streaming::AgentRun {
        let cancel_token = CancellationToken::new();

        if let Some(timeout) = timeout {
            // Fire-and-forget: self-terminates when the run ends or the timeout fires.
            let _watcher_handle =
                spawn_timeout_watcher(timeout, cancel_token.clone(), request_id.to_string());
        }

        // Share one UsageState between the inner orchestrator (writer) and the
        // streaming handler (reader) so aura.usage reflects the aggregate of
        // all orchestration LLM turns.
        let usage_state = crate::UsageState::new();
        let stream = self.spawn_orchestration_stream(
            query.to_string(),
            chat_history,
            cancel_token.clone(),
            request_id.to_string(),
            usage_state.clone(),
            timeout,
        );

        crate::streaming::AgentRun::new(stream, cancel_token, usage_state)
    }

    async fn cancel_and_close_mcp(&self, _request_id: &str, _reason: &str) -> usize {
        // No-op: cancellation is handled inside the spawned task via cancel_token.
        0
    }
}
