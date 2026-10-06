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
    run_tools: crate::builder::RunToolFactory,
}

/// A run's cancellation, and the signal that its task ended.
struct RunTokens {
    cancel: CancellationToken,
    finished: Option<CancellationToken>,
}

/// The response an orchestrated run finishes with.
///
/// Orchestration accumulates usage across every worker and coordinator turn, so
/// the totals come from the run rather than from one response. The cache counts
/// come with them, because a reader that finds usage on the response takes the
/// split from there too, and a split from a different turn population would not
/// be a subset of the prompt tokens it sits beside.
fn final_response(
    content: String,
    usage_state: &crate::UsageState,
) -> crate::provider_agent::FinalResponseInfo {
    let (input_tokens, output_tokens, total_tokens) = usage_state.get_final_usage();

    crate::provider_agent::FinalResponseInfo {
        content,
        usage: rig::completion::Usage {
            input_tokens,
            output_tokens,
            total_tokens,
        },
        cache_usage: usage_state.get_cache_usage().map(
            |(cache_read_input_tokens, cache_creation_input_tokens)| rig::completion::CacheUsage {
                cache_read_input_tokens,
                cache_creation_input_tokens,
            },
        ),
    }
}

impl OrchestratorFactory {
    pub fn new(agent_config: AgentRuntimeConfig) -> Self {
        Self {
            agent_config,
            run_tools: crate::builder::no_run_tools(),
        }
    }

    /// Give every worker of every run the tools `run_tools` builds, one
    /// instance per worker.
    pub fn with_run_tools(mut self, run_tools: crate::builder::RunToolFactory) -> Self {
        self.run_tools = run_tools;
        self
    }

    /// Spawn the background orchestration task and return its event stream.
    ///
    /// The `usage_state` handle is assigned to the inner `Orchestrator` so
    /// planning, worker, synthesis, and evaluation turns can accumulate into it.
    /// [`stream`](Self::stream) keeps a clone on the run it returns, so the
    /// streaming handler can read the totals for the final `aura.usage` event.
    fn spawn_orchestration_stream(
        &self,
        query: String,
        chat_history: Vec<rig::completion::Message>,
        tokens: RunTokens,
        run: std::sync::Arc<crate::run_context::RunContext>,
        usage_state: crate::UsageState,
        outer_budget: Option<Duration>,
    ) -> BoxStream<'static, Result<StreamItem, StreamError>> {
        let agent_config = self.agent_config.clone();
        let run_tools = std::sync::Arc::clone(&self.run_tools);

        // Create channel for orchestrator events
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<Result<StreamItem, StreamError>>(100);

        let run_id = run.id().clone();
        let RunTokens { cancel, finished } = tokens;
        let cancel_token_clone = cancel.clone();
        // Marks the run finished on every exit path, which is what lets the
        // timeout watcher stop rather than sleeping out its full duration. It is
        // not the run's cancel token: a run that finished was not cancelled.
        let done_guard = finished.map(CancellationToken::drop_guard);
        // Capture parent span so child spans nest correctly in tracing.
        let parent_span = tracing::Span::current();
        tokio::spawn(tracing::Instrument::instrument(
            crate::run_context::with_run(std::sync::Arc::clone(&run), async move {
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
                orchestrator.usage_state = usage_state.clone();
                orchestrator.outer_budget = outer_budget;
                orchestrator.run_tools = run_tools;

                // Surface per-server connection status so degraded/unavailable
                // MCP servers are visible in orchestration mode too (workers
                // share this one manager).
                if let Some(ref mcp_manager) = orchestrator.mcp_manager {
                    // Workers share this manager, and their tool calls run on
                    // rig's server task where the run's scope does not reach.
                    mcp_manager
                        .bind_call(
                            std::sync::Arc::clone(&run),
                            aura_events::AgentContext::coordinator(),
                        )
                        .await;
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
                                    final_response(final_result, &usage_state)
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
                                .cancel_and_close_all(&run_id, "Client disconnected or timeout")
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

    /// The coordinator's window: it holds the persistent conversation, so it
    /// is the context a client measures the session against.
    fn context_window(&self) -> Option<u64> {
        self.agent_config.llm.context_window()
    }

    fn skills(&self) -> &[aura_config::SkillConfig] {
        &self.agent_config.agent.skills
    }

    async fn stream(
        &self,
        query: &str,
        chat_history: Vec<rig::completion::Message>,
        options: crate::streaming::RunOptions,
        request_id: &crate::domain::RequestId,
    ) -> crate::streaming::AgentRun {
        let (timeout, cancel) = options.into_parts();
        let cancel_token = cancel.unwrap_or_default();

        // Only a watcher observes this, so an unbounded run needs none.
        let finished = timeout.map(|timeout| {
            let finished = CancellationToken::new();
            // Fire-and-forget: self-terminates when the run ends or the timeout fires.
            let _watcher_handle = spawn_timeout_watcher(
                timeout,
                cancel_token.clone(),
                finished.clone(),
                request_id.clone(),
            );
            finished
        });

        // Share one UsageState between the inner orchestrator (writer) and the
        // streaming handler (reader) so aura.usage reflects the aggregate of
        // all orchestration LLM turns.
        let usage_state = crate::UsageState::new();
        let (run, run_events) =
            crate::run_context::RunContext::channel_on(request_id.clone(), cancel_token.clone());
        let stream = self.spawn_orchestration_stream(
            query.to_string(),
            chat_history,
            RunTokens {
                cancel: cancel_token.clone(),
                finished,
            },
            std::sync::Arc::clone(&run),
            usage_state.clone(),
            timeout,
        );

        // The answer an orchestrated run produces is the coordinator's, so it
        // is attributed there rather than to any worker that fed it.
        let stream = Box::pin(crate::streaming::tee_content(
            run,
            aura_events::AgentContext::coordinator(),
            stream,
        ));

        let run = crate::streaming::AgentRun::new(stream, cancel_token, usage_state)
            .observed_by(run_events);
        match &self.agent_config.hitl {
            Some(hitl) => run.sweeping_approvals(&hitl.route, request_id),
            None => run,
        }
    }

    async fn cancel_and_close_mcp(
        &self,
        _request_id: &crate::domain::RequestId,
        _reason: &str,
    ) -> usize {
        // No-op: cancellation is handled inside the spawned task via cancel_token.
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that finds usage on the response takes the cache split from
    /// there too, so the response carries the run's split with its totals.
    #[test]
    fn an_orchestrated_response_reports_what_the_run_billed() {
        let usage = crate::UsageState::new();
        usage.accumulate_usage(5_000, 200);
        usage.accumulate_usage(8_000, 400);
        usage.store_cache_usage(4_000, 1_000);

        let response = final_response("the answer".to_string(), &usage);

        assert_eq!(response.content, "the answer");
        assert_eq!(response.usage.input_tokens, 13_000);
        assert_eq!(response.usage.output_tokens, 600);
        assert_eq!(response.usage.total_tokens, 13_600);

        let cache = response
            .cache_usage
            .expect("a run that used the cache says so");
        assert_eq!(cache.cache_read_input_tokens, 4_000);
        assert_eq!(cache.cache_creation_input_tokens, 1_000);
    }

    /// A run that never touched the cache reports none, rather than zeros that
    /// would read as a cache miss.
    #[test]
    fn a_response_from_a_run_without_cache_reports_none() {
        let usage = crate::UsageState::new();
        usage.accumulate_usage(10, 5);

        assert!(
            final_response("hi".to_string(), &usage)
                .cache_usage
                .is_none()
        );
    }
}
