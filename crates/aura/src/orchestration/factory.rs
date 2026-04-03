//! Lightweight factory for orchestration streaming.
//!
//! `OrchestratorFactory` implements `StreamingAgent` without constructing a full
//! `Orchestrator` up front. The real orchestrator is created lazily inside `stream()`
//! when a request arrives. This avoids duplicate MCP connections, persistence
//! directories, and ensures MCP progress notifications route correctly.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;

use crate::config::AgentConfig;
use crate::mcp::McpManager;
use crate::provider_agent::{StreamError, StreamItem};
use crate::streaming::StreamingAgent;

use super::orchestrator::{spawn_cancellation_watcher, spawn_tool_event_forwarder, Orchestrator, STREAM_CHUNK_SIZE};

/// Lightweight wrapper that implements `StreamingAgent` for orchestration mode.
///
/// Instead of constructing a full `Orchestrator` (with MCP connections, persistence)
/// at build time, this factory defers construction to `stream()`.
/// This eliminates the duplicate-orchestrator problem where the "factory" instance's
/// resources were never used.
pub struct OrchestratorFactory {
    agent_config: AgentConfig,
    /// HTTP request ID stored by `set_mcp_request_id`, passed to inner orchestrator.
    request_id: Arc<RwLock<Option<String>>>,
    /// Handle to the inner orchestrator's MCP manager, set after spawn creates it.
    /// Used by `cancel_and_close_mcp` for client disconnect propagation.
    inner_mcp: Arc<RwLock<Option<Arc<McpManager>>>>,
}

impl OrchestratorFactory {
    pub fn new(agent_config: AgentConfig) -> Self {
        Self {
            agent_config,
            request_id: Arc::new(RwLock::new(None)),
            inner_mcp: Arc::new(RwLock::new(None)),
        }
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
    ) -> Result<BoxStream<'static, Result<StreamItem, StreamError>>, StreamError> {
        let query = query.to_string();
        let chat_history = chat_history.clone();
        let mut agent_config = self.agent_config.clone();

        // Create channel for orchestrator events
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(100);

        // Inject conversation history for worker access via get_conversation_context tool
        agent_config.orchestration_chat_history = Some(Arc::new(chat_history.clone()));

        // Capture request ID and inner_mcp handle for the spawned task
        let request_id = self.request_id.clone();
        let inner_mcp = self.inner_mcp.clone();

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

                // Bridge: set MCP request ID on the inner orchestrator so progress
                // notifications route to the correct SSE subscriber.
                if let Some(ref mcp_manager) = orchestrator.mcp_manager {
                    if let Some(ref req_id) = *request_id.read().await {
                        mcp_manager.set_current_request(req_id).await;
                    }
                    // Publish inner MCP manager so cancel_and_close_mcp can reach it.
                    *inner_mcp.write().await = Some(Arc::clone(mcp_manager));
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
                                // Emit final response as text chunks
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
                        if let Some(ref mcp_manager) = orchestrator.mcp_manager {
                            let cancelled = mcp_manager
                                .cancel_and_close_all("orchestration", "Client disconnected or timeout")
                                .await;
                            if cancelled > 0 {
                                tracing::info!("Cancelled {} MCP request(s) during orchestration shutdown", cancelled);
                            }
                        }
                    }
                }

                // Clear inner MCP handle on exit so stale references don't linger.
                *inner_mcp.write().await = None;
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

        let stream = match self.stream(query, chat_history, cancel_token).await {
            Ok(s) => s,
            Err(e) => Box::pin(stream::once(async move { Err(e) })),
        };

        // Orchestration has its own usage tracking; return a fresh state for the handler.
        (stream, cancel_tx, crate::UsageState::new())
    }

    async fn cancel_and_close_mcp(&self, request_id: &str, reason: &str) -> usize {
        if let Some(ref mcp_manager) = *self.inner_mcp.read().await {
            mcp_manager.cancel_and_close_all(request_id, reason).await
        } else {
            0
        }
    }

    async fn set_mcp_request_id(&self, request_id: &str) {
        *self.request_id.write().await = Some(request_id.to_string());
        // Also forward to inner MCP if already constructed (unlikely but safe)
        if let Some(ref mcp_manager) = *self.inner_mcp.read().await {
            mcp_manager.set_current_request(request_id).await;
        }
    }

    async fn clear_mcp_request_id(&self) {
        *self.request_id.write().await = None;
        if let Some(ref mcp_manager) = *self.inner_mcp.read().await {
            mcp_manager.clear_current_request().await;
        }
    }
}
