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
    spawn_cancellation_watcher, spawn_tool_event_forwarder, Orchestrator, STREAM_CHUNK_SIZE,
};

/// Zero-state wrapper that implements `StreamingAgent` for orchestration mode.
///
/// Instead of constructing a full `Orchestrator` (with MCP connections, persistence)
/// at build time, this factory defers construction to `stream()`. The real
/// `Orchestrator` is created inside a `tokio::spawn`, and `request_id` flows
/// directly as a parameter — no shared mutable state needed.
pub struct OrchestratorFactory {
    agent_config: AgentConfig,
}

impl OrchestratorFactory {
    pub fn new(agent_config: AgentConfig) -> Self {
        Self { agent_config }
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
        let query = query.to_string();
        let request_id = request_id.to_string();
        let mut agent_config = self.agent_config.clone();

        // Create channel for orchestrator events
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(100);

        // Inject conversation history for worker access via get_conversation_context tool
        agent_config.orchestration_chat_history = Some(Arc::new(chat_history.clone()));

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

                // Set MCP request ID on the inner orchestrator so progress
                // notifications route to the correct SSE subscriber.
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

        let stream = match self.stream(query, chat_history, cancel_token, request_id).await {
            Ok(s) => s,
            Err(e) => Box::pin(stream::once(async move { Err(e) })),
        };

        // Orchestration has its own usage tracking; return a fresh state for the handler.
        (stream, cancel_tx, crate::UsageState::new())
    }

    async fn cancel_and_close_mcp(&self, _request_id: &str, _reason: &str) -> usize {
        // No-op: cancellation is handled inside the spawned task via cancel_token.
        0
    }
}
