//! Lightweight factory for orchestration streaming.
//!
//! `OrchestratorFactory` implements `StreamingAgent` without constructing a full
//! `Orchestrator` up front. The real orchestrator is created lazily inside `stream()`
//! when a request arrives, ensuring MCP progress notifications route correctly
//! and avoiding duplicate resource allocation.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::AgentConfig;
use crate::provider_agent::{StreamError, StreamItem};
use crate::streaming::StreamingAgent;

use super::orchestrator::{
    Orchestrator, STREAM_CHUNK_SIZE, spawn_cancellation_watcher, spawn_tool_event_forwarder,
};

/// Zero-state wrapper that implements `StreamingAgent` for orchestration mode.
///
/// Defers `Orchestrator` construction to `stream()` to avoid duplicate resource
/// allocation and ensure MCP progress notifications route correctly.
pub struct OrchestratorFactory {
    agent_config: AgentConfig,
}

impl OrchestratorFactory {
    pub fn new(agent_config: AgentConfig) -> Self {
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
    ) -> BoxStream<'static, Result<StreamItem, StreamError>> {
        let mut agent_config = self.agent_config.clone();

        // Create channel for orchestrator events
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(100);

        // Inject conversation history for worker access
        agent_config.orchestration_chat_history = Some(Arc::new(chat_history.clone()));

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

                // Set MCP request ID for progress notification routing
                if let Some(ref mcp_manager) = orchestrator.mcp_manager {
                    mcp_manager.set_current_request(&request_id).await;
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
        );

        (stream, cancel_tx, usage_state)
    }

    async fn cancel_and_close_mcp(&self, _request_id: &str, _reason: &str) -> usize {
        // No-op: cancellation is handled inside the spawned task via cancel_token.
        0
    }
}
