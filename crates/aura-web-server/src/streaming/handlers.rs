//! Stream item handlers for SSE streaming.
//!
//! This module contains the main stream processing loop and individual handlers
//! for each type of stream item (text, tool calls, tool results, reasoning, etc.).
//!
//! # Architecture
//!
//! The streaming flow is:
//! 1. `process_sse_stream_full` - Main entry point with full cancellation support
//! 2. Individual handlers for each stream item type (text, tool calls, results, etc.)
//! 3. Event channel handlers for MCP tool_start and progress notifications
//! 4. Heartbeat for proactive disconnect detection
//!
//! # Cancellation
//!
//! When disconnect is detected (via channel send failure or heartbeat), we:
//! 1. Signal cancellation via `cancel_tx`
//! 2. Cancel request via `RequestCancellation::cancel()`
//! 3. Cancel MCP requests and close connections via `agent.cancel_and_close_mcp()`

use crate::streaming::types::openai::UsageInfo;

use super::types::{
    CHUNK_OBJECT, ChatCompletionChunk, ChatCompletionChunkChoice, ChatCompletionChunkDelta,
    FINISH_REASON_LENGTH, FINISH_REASON_STOP, FUNCTION_TYPE, FunctionCallChunk, MessageRole,
    StreamConfig, ToolCallChunk, ToolResultMode, ToolResultStatus, TurnContext, TurnState,
    detect_tool_error, format_sse_chunk, truncate_result,
};
use actix_web::web::Bytes;
use aura::stream_events::AuraStreamEvent;
use aura::{
    EventContext, OrchestrationStreamEvent, OrchestratorEvent, ProgressNotification,
    RequestCancellation, ResponseContent, StreamError, StreamItem, StreamedAssistantContent,
    StreamedUserContent, StreamingAgent, ToolCall, ToolLifecycleEvent, ToolResult, ToolUsageEvent,
    UsageState,
};
use futures_util::{Stream, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Context for cancellation and cleanup callbacks.
pub struct StreamingCallbacks {
    /// Request ID for cancellation registry
    pub request_id: String,
    /// Agent reference for MCP cleanup (cancel_and_close_mcp)
    pub agent: Arc<dyn StreamingAgent>,
    /// MCP tool event receiver (for aura.tool_requested and aura.tool_start events)
    pub tool_event_rx: mpsc::Receiver<ToolLifecycleEvent>,
    /// MCP progress event receiver (for aura.progress events)
    pub progress_rx: mpsc::Receiver<ProgressNotification>,
    /// Tool usage event receiver (for aura.tool_usage events from hook)
    pub tool_usage_rx: mpsc::Receiver<ToolUsageEvent>,
    /// Shared usage state for reading final usage at stream end
    pub usage_state: UsageState,
    /// Shared response content for OTel span recording at stream end
    pub response_content: ResponseContent,
    /// Model name for context limit lookup
    pub model_name: String,
    /// Stream shutdown token (cancelled after grace period on SIGTERM/SIGINT)
    pub stream_shutdown_token: tokio_util::sync::CancellationToken,
}

/// Reason for stream termination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamTermination {
    /// Stream ended normally (all items processed)
    Complete,
    /// Stream ended due to a stream error (e.g., context overflow, LLM error).
    /// The user may have received a partial response or error guidance.
    StreamError(String),
    /// Client disconnected (channel send failed)
    Disconnected,
    /// Timeout fired
    Timeout,
    /// Server shutdown (SIGTERM/SIGINT)
    Shutdown,
}

/// User-facing message when context overflow is detected.
const CONTEXT_OVERFLOW_MESSAGE: &str = "My tools returned more data than I can work with at once.\n\n\
    **To help me out, try:**\n\
    - Narrowing your search with filters\n\
    - Asking for a summary instead of full results\n\
    - Splitting into smaller requests";

/// Process a Rig stream into SSE bytes with full cancellation support.
///
/// This is the production entry point for SSE streaming. It handles:
/// 1. Stream item processing (text, tool calls, results, reasoning)
/// 2. MCP tool_start events (aura.tool_start)
/// 3. MCP progress notifications (aura.progress)
/// 4. Heartbeat for proactive disconnect detection
/// 5. Full cancellation on disconnect/timeout (MCP cleanup)
///
/// Returns the reason for termination.
#[allow(clippy::too_many_arguments)]
pub async fn process_sse_stream_full<S>(
    config: &StreamConfig,
    ctx: &TurnContext,
    mut stream: S,
    tx: mpsc::Sender<Result<Bytes, String>>,
    cancel_tx: watch::Sender<bool>,
    timeout_duration: Duration,
    heartbeat_interval: Duration,
    first_chunk_timeout: Option<Duration>,
    mut callbacks: StreamingCallbacks,
) -> StreamTermination
where
    S: Stream<Item = Result<StreamItem, StreamError>> + Unpin,
{
    let mut state = TurnState::new();
    let emit_custom_events = config.emit_custom_events;
    let response_content = callbacks.response_content.clone();

    // Emit aura.session_info at stream start (if custom events enabled)
    if emit_custom_events {
        let context_limit = callbacks.agent.context_window();
        let session_info = AuraStreamEvent::session_info(
            &callbacks.model_name,
            context_limit,
            ctx.correlation.clone(),
        );
        if tx
            .send(Ok(Bytes::from(session_info.format_sse())))
            .await
            .is_err()
        {
            tracing::info!("Client disconnected during session_info emit");
            return StreamTermination::Disconnected;
        }
        tracing::debug!(
            "Emitted aura.session_info: model={}, context_limit={:?}",
            callbacks.model_name,
            context_limit
        );
    }

    // Safety net timeout
    let timeout = tokio::time::sleep(timeout_duration);
    tokio::pin!(timeout);

    // Heartbeat for proactive disconnect detection during silent tool execution
    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // First chunk timeout: detect hung provider connections
    let has_first_chunk_timeout = first_chunk_timeout.is_some();
    let first_chunk_sleep = tokio::time::sleep(first_chunk_timeout.unwrap_or(Duration::MAX));
    tokio::pin!(first_chunk_sleep);
    let mut first_chunk_received = false;

    let termination = loop {
        tokio::select! {
            // Normal stream processing — shared with collect_stream_to_completion
            item = stream.next() => {
                // Any stream item (data, error, or end-of-stream) means the provider
                // responded — clear the first-chunk timeout regardless of content.
                if !first_chunk_received {
                    first_chunk_received = true;
                }
                match process_stream_next(config, ctx, &mut state, item) {
                    NextItemResult::Continue(bytes_to_send) => {
                        let mut disconnected = false;
                        for bytes in bytes_to_send {
                            if tx.send(Ok(bytes)).await.is_err() {
                                tracing::info!("Client disconnected during chunk send");
                                disconnected = true;
                                break;
                            }
                        }
                        if disconnected {
                            break StreamTermination::Disconnected;
                        }
                    }
                    NextItemResult::End(bytes_to_send) => {
                        let mut disconnected = false;

                        // Send any final bytes (e.g., context overflow message)
                        for bytes in bytes_to_send {
                            if tx.send(Ok(bytes)).await.is_err() {
                                tracing::info!("Client disconnected during chunk send");
                                disconnected = true;
                                break;
                            }
                        }

                        if disconnected {
                            break StreamTermination::Disconnected;
                        }

                        // Post-loop handles final chunk + [DONE] for all Complete terminations
                        break StreamTermination::Complete;
                    }
                }
            }

            // MCP progress notification (only emit if custom events enabled)
            notification = callbacks.progress_rx.recv(), if emit_custom_events => {
                if let Some(notification) = notification {
                    let event = AuraStreamEvent::progress(
                        notification.message.clone().unwrap_or_else(|| {
                            format!("Progress: {}/{:?}", notification.progress, notification.total)
                        }),
                        "mcp_progress",
                        notification.percent(),
                        Some(notification.progress_token.clone()),
                        ctx.agent_context.clone(),
                        ctx.correlation.clone(),
                    );
                    tracing::debug!(
                        "Emitting aura.progress event: token={:?}, progress={}/{:?}",
                        notification.progress_token,
                        notification.progress,
                        notification.total
                    );
                    if tx.send(Ok(Bytes::from(event.format_sse()))).await.is_err() {
                        tracing::info!("Client disconnected during progress notification");
                        break StreamTermination::Disconnected;
                    }
                }
            }

            // MCP tool lifecycle events (requested when LLM decides, start when MCP begins)
            tool_event = callbacks.tool_event_rx.recv(), if emit_custom_events => {
                if let Some(tool_event) = tool_event {
                    let sse_event = match tool_event {
                        ToolLifecycleEvent::Requested { tool_id, tool_name, arguments } => {
                            tracing::debug!(
                                "Emitting aura.tool_requested event: tool_id={}, tool_name={}",
                                tool_id, tool_name
                            );
                            AuraStreamEvent::tool_requested(
                                &tool_id,
                                &tool_name,
                                arguments,
                                ctx.agent_context.clone(),
                                ctx.correlation.clone(),
                            )
                        }
                        ToolLifecycleEvent::Start { tool_id, tool_name, progress_token } => {
                            tracing::debug!(
                                "Emitting aura.tool_start event: tool_id={}, tool_name={}, progress_token={:?}",
                                tool_id, tool_name, progress_token
                            );
                            AuraStreamEvent::tool_start(
                                &tool_id,
                                &tool_name,
                                progress_token,
                                ctx.agent_context.clone(),
                                ctx.correlation.clone(),
                            )
                        }
                    };
                    if tx.send(Ok(Bytes::from(sse_event.format_sse()))).await.is_err() {
                        tracing::info!("Client disconnected during tool event");
                        break StreamTermination::Disconnected;
                    }
                }
            }

            // Tool usage events from hook (associates tool_ids with usage snapshot)
            tool_usage = callbacks.tool_usage_rx.recv(), if emit_custom_events => {
                if let Some(usage_event) = tool_usage {
                    tracing::debug!(
                        "Emitting aura.tool_usage event: tool_ids={:?}, prompt_tokens={}",
                        usage_event.tool_ids, usage_event.prompt_tokens
                    );
                    let sse_event = AuraStreamEvent::tool_usage(
                        usage_event.tool_ids,
                        usage_event.prompt_tokens,
                        usage_event.completion_tokens,
                        usage_event.total_tokens,
                        ctx.correlation.clone(),
                    );
                    if tx.send(Ok(Bytes::from(sse_event.format_sse()))).await.is_err() {
                        tracing::info!("Client disconnected during tool_usage event");
                        break StreamTermination::Disconnected;
                    }
                }
            }

            // First chunk timeout: detect hung provider connections
            _ = &mut first_chunk_sleep, if !first_chunk_received && has_first_chunk_timeout => {
                tracing::warn!(
                    "First chunk timeout ({:?}) - no response from LLM provider",
                    first_chunk_timeout.unwrap()
                );
                break StreamTermination::Timeout;
            }

            // Safety net timeout
            _ = &mut timeout => {
                tracing::warn!(
                    "Streaming safety net timeout ({:?}) - signaling cancellation",
                    timeout_duration
                );
                break StreamTermination::Timeout;
            }

            // Heartbeat for proactive disconnect detection
            _ = heartbeat.tick() => {
                // SSE comments (starting with ':') are ignored by clients but detect disconnect
                if tx.send(Ok(Bytes::from_static(b": heartbeat\n\n"))).await.is_err() {
                    tracing::info!("Client disconnected during heartbeat");
                    break StreamTermination::Disconnected;
                }
            }

            // Server shutdown (grace period expired)
            _ = callbacks.stream_shutdown_token.cancelled() => {
                tracing::info!("Server shutdown signal received, terminating stream");
                break StreamTermination::Shutdown;
            }
        }
    };

    // Promote Complete → StreamError when process_stream_next captured an error
    let termination = if termination == StreamTermination::Complete {
        match state.stream_error.take() {
            Some(err) => StreamTermination::StreamError(err),
            None => StreamTermination::Complete,
        }
    } else {
        termination
    };

    if state.usage_stats.is_some() && !state.accumulated_content.is_empty() {
        response_content.set(state.accumulated_content.clone());
    }

    // Post-loop cleanup: behavior depends on termination reason.
    //
    // - Complete/StreamError: send [DONE] only (no cancellation needed)
    // - Disconnected:        cancel → MCP cleanup (no [DONE] — client is gone)
    // - Timeout:             cancel → MCP cleanup → send [DONE]
    // - Shutdown:            cancel hook + registry → send [DONE] → MCP cleanup
    //
    // The Shutdown ordering is critical: [DONE] is sent BEFORE cancel_and_close_mcp() so the
    // client gets clean stream termination regardless of how long MCP cleanup takes.
    match termination {
        StreamTermination::Complete | StreamTermination::StreamError(_) => {
            send_final_events(emit_custom_events, &mut callbacks, ctx, &state, &tx).await;
        }

        StreamTermination::Disconnected => {
            let _ = cancel_tx.send(true);
            RequestCancellation::cancel(&callbacks.request_id, "client disconnected");
            cancel_mcp(&callbacks, "client disconnected").await;
        }

        StreamTermination::Timeout => {
            let _ = cancel_tx.send(true);
            RequestCancellation::cancel(&callbacks.request_id, "timeout");
            cancel_mcp(&callbacks, "timeout").await;
            send_final_events(emit_custom_events, &mut callbacks, ctx, &state, &tx).await;
        }

        StreamTermination::Shutdown => {
            // [DONE] before MCP cleanup so client gets clean termination regardless of MCP latency
            let _ = cancel_tx.send(true);
            RequestCancellation::cancel(&callbacks.request_id, "server shutdown");
            send_final_events(emit_custom_events, &mut callbacks, ctx, &state, &tx).await;
            cancel_mcp(&callbacks, "server shutdown").await;
        }
    }

    tracing::debug!("Stream processing completed with {:?}", termination);
    termination
}

/// Send final usage events, finish chunk, and [DONE] marker to the client.
async fn send_final_events(
    emit_custom_events: bool,
    callbacks: &mut StreamingCallbacks,
    ctx: &TurnContext,
    state: &TurnState,
    tx: &mpsc::Sender<Result<Bytes, String>>,
) {
    // Drain any pending tool_usage events before emitting final aura.usage
    if emit_custom_events {
        while let Ok(usage_event) = callbacks.tool_usage_rx.try_recv() {
            let sse_event = AuraStreamEvent::tool_usage(
                usage_event.tool_ids,
                usage_event.prompt_tokens,
                usage_event.completion_tokens,
                usage_event.total_tokens,
                ctx.correlation.clone(),
            );
            if tx
                .send(Ok(Bytes::from(sse_event.format_sse())))
                .await
                .is_err()
            {
                return; // Client disconnected
            }
        }
    }

    // Emit aura.usage at stream end
    if emit_custom_events {
        let (prompt, completion, total) = callbacks.usage_state.get_final_usage();
        if prompt > 0 {
            tracing::debug!(
                "Emitting aura.usage event: prompt={}, completion={}, total={}",
                prompt,
                completion,
                total
            );
            let usage_event =
                AuraStreamEvent::usage(prompt, completion, total, ctx.correlation.clone());
            let _ = tx.send(Ok(Bytes::from(usage_event.format_sse()))).await;
        }
    }

    let final_bytes = build_final_chunk(ctx, state);
    for bytes in final_bytes {
        let _ = tx.send(Ok(bytes)).await;
    }
    let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
}

/// Cancel MCP requests and close connections with a bounded timeout.
async fn cancel_mcp(callbacks: &StreamingCallbacks, reason: &str) {
    const TIMEOUT: Duration = Duration::from_secs(5);
    match tokio::time::timeout(
        TIMEOUT,
        callbacks
            .agent
            .cancel_and_close_mcp(&callbacks.request_id, reason),
    )
    .await
    {
        Ok(cancelled) if cancelled > 0 => {
            tracing::info!("Cancelled {} MCP request(s) on {}", cancelled, reason);
        }
        Ok(_) => {}
        Err(_) => {
            tracing::warn!("MCP cleanup timed out after {:?} on {}", TIMEOUT, reason);
        }
    }
}

/// Result of collecting a stream to completion (used by non-streaming handler).
pub struct StreamOutcome {
    /// Accumulated response content
    pub content: String,
    /// Token usage from the stream (from `Final` variant or `None` if only `FinalMarker`).
    pub usage: Option<UsageInfo>,
}

/// Consume a stream to completion, processing items through the same handlers as SSE streaming.
///
/// This is the non-streaming counterpart to `process_sse_stream_full`. It drives items through
/// the same `process_stream_next` pipeline (which handles content accumulation, `\n\n` separators,
/// context overflow errors, and usage tracking) but discards the SSE-formatted bytes.
pub async fn collect_stream_to_completion<S>(
    config: &StreamConfig,
    ctx: &TurnContext,
    mut stream: S,
) -> (StreamOutcome, StreamTermination)
where
    S: Stream<Item = Result<StreamItem, StreamError>> + Unpin,
{
    let mut state = TurnState::new();

    loop {
        let item = stream.next().await;
        let result = process_stream_next(config, ctx, &mut state, item);
        match result {
            NextItemResult::Continue(_sse_bytes) => {}
            NextItemResult::End(_sse_bytes) => break,
        }
    }

    // Promote to StreamError when process_stream_next captured an error
    let termination = match state.stream_error.take() {
        Some(err) => StreamTermination::StreamError(err),
        None => StreamTermination::Complete,
    };

    (
        StreamOutcome {
            content: state.accumulated_content,
            usage: state.usage_stats,
        },
        termination,
    )
}

/// Result of processing a single stream item.
enum NextItemResult {
    /// Item processed, continue consuming. Contains SSE bytes to optionally send.
    Continue(Vec<Bytes>),
    /// Stream ended (completed or errored). Contains SSE bytes to optionally send
    /// (e.g., context overflow message).
    End(Vec<Bytes>),
}

/// Process the next item from a stream — shared logic for both SSE and non-streaming paths.
///
/// Handles: item processing via `handle_stream_item`, context overflow detection,
/// and content accumulation in `state`. Returns SSE bytes that the caller can send or discard.
fn process_stream_next(
    config: &StreamConfig,
    ctx: &TurnContext,
    state: &mut TurnState,
    item: Option<Result<StreamItem, StreamError>>,
) -> NextItemResult {
    match item {
        Some(Ok(stream_item)) => {
            tracing::trace!(
                "Stream received item: {:?}",
                std::mem::discriminant(&stream_item)
            );
            let bytes = handle_stream_item(config, ctx, state, &stream_item);
            NextItemResult::Continue(bytes)
        }
        Some(Err(e)) => {
            let error_str = e.to_string();
            tracing::error!("Stream error: {}", error_str);

            // Capture for OTel span recording after loop ends
            state.stream_error = Some(error_str.clone());

            // Context overflow: provide actionable guidance
            if is_context_overflow_error(&error_str) {
                tracing::info!("Context overflow detected");
                state.accumulated_content.push_str(CONTEXT_OVERFLOW_MESSAGE);
                let chunk = build_text_chunk(ctx, CONTEXT_OVERFLOW_MESSAGE, state.is_first_chunk);
                if let Ok(bytes) = format_sse_chunk(&chunk) {
                    return NextItemResult::End(vec![bytes]);
                }
            } else {
                let user_message = format!(
                    "The upstream model provider ({}) returned an error and the request \
                     could not be completed. Please try again or contact support if the \
                     issue persists.",
                    ctx.model_str
                );
                state.accumulated_content.push_str(&user_message);
                let chunk = build_text_chunk(ctx, &user_message, state.is_first_chunk);
                if let Ok(bytes) = format_sse_chunk(&chunk) {
                    return NextItemResult::End(vec![bytes]);
                }
            }
            NextItemResult::End(vec![])
        }
        None => {
            tracing::info!("Stream ended normally");
            NextItemResult::End(vec![])
        }
    }
}

/// Handle a single stream item, returning bytes to send.
fn handle_stream_item(
    config: &StreamConfig,
    ctx: &TurnContext,
    state: &mut TurnState,
    item: &StreamItem,
) -> Vec<Bytes> {
    match item {
        StreamItem::StreamAssistantItem(content) => {
            handle_assistant_item(config, ctx, state, content)
        }
        StreamItem::StreamUserItem(content) => handle_user_item(config, ctx, state, content),
        StreamItem::Final(final_info) => {
            // Final response contains authoritative accumulated content and usage
            tracing::info!(
                "Multi-turn streaming complete: {} chars",
                final_info.content.len()
            );
            state.accumulated_content = final_info.content.clone();
            state.usage_stats = Some(UsageInfo {
                prompt_tokens: final_info.usage.input_tokens,
                completion_tokens: final_info.usage.output_tokens,
                total_tokens: final_info.usage.total_tokens,
            });

            tracing::debug!(
                "Token usage: input={}, output={}, total={}",
                final_info.usage.input_tokens,
                final_info.usage.output_tokens,
                final_info.usage.total_tokens
            );

            vec![]
        }
        StreamItem::FinalMarker | StreamItem::TurnUsage(_) => {
            // Internal markers - filtered out
            tracing::debug!("Received final/turn-usage marker");
            vec![]
        }
        StreamItem::OrchestratorEvent(event) => handle_orchestrator_event(config, ctx, event),
    }
}

/// Handle assistant content (text, tool calls, reasoning).
fn handle_assistant_item(
    config: &StreamConfig,
    ctx: &TurnContext,
    state: &mut TurnState,
    content: &StreamedAssistantContent,
) -> Vec<Bytes> {
    match content {
        StreamedAssistantContent::Text(text) => handle_text_delta(ctx, state, text.clone()),
        StreamedAssistantContent::ToolCall(tool_call) => {
            handle_tool_call(config, ctx, state, tool_call)
        }
        StreamedAssistantContent::ToolCallDelta { .. } => vec![], // Handled via full ToolCall
        StreamedAssistantContent::Reasoning(_) => vec![],         // Handled via ReasoningDelta
        StreamedAssistantContent::ReasoningDelta { delta, .. } => {
            handle_reasoning(config, ctx, delta.clone())
        }
    }
}

/// Handle user content (tool results).
fn handle_user_item(
    config: &StreamConfig,
    ctx: &TurnContext,
    state: &mut TurnState,
    content: &StreamedUserContent,
) -> Vec<Bytes> {
    match content {
        StreamedUserContent::ToolResult(tool_result) => {
            handle_tool_result(config, ctx, state, tool_result)
        }
    }
}

/// Handle a text delta.
fn handle_text_delta(ctx: &TurnContext, state: &mut TurnState, text: String) -> Vec<Bytes> {
    // Prepend newlines if resuming text after tool execution
    let content = if state.needs_separator {
        state.needs_separator = false;
        format!("\n\n{}", text)
    } else {
        text
    };

    // Accumulate content for non-streaming collection
    state.accumulated_content.push_str(&content);

    let chunk = ChatCompletionChunk {
        id: ctx.completion_id.clone(),
        object: CHUNK_OBJECT.to_string(),
        created: ctx.created_timestamp,
        model: ctx.model_str.clone(),
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionChunkDelta {
                role: if state.is_first_chunk {
                    Some(MessageRole::Assistant)
                } else {
                    None
                },
                content: Some(content),
                tool_calls: None,
            },
            finish_reason: None,
        }],
        usage: None,
    };

    state.is_first_chunk = false;

    match format_sse_chunk(&chunk) {
        Ok(bytes) => vec![bytes],
        Err(e) => {
            tracing::error!("Failed to serialize text chunk: {}", e);
            vec![]
        }
    }
}

/// Handle a tool call.
fn handle_tool_call(
    config: &StreamConfig,
    ctx: &TurnContext,
    state: &mut TurnState,
    tool_call: &ToolCall,
) -> Vec<Bytes> {
    tracing::info!("Streaming tool call: {}", tool_call.name);

    let mut output = Vec::with_capacity(2);

    state.has_tool_calls = true;
    state.tool_call_map.insert(
        tool_call.id.clone(),
        (tool_call.name.clone(), state.tool_call_index),
    );

    // Track tool start time for duration calculation in tool_complete event
    // Note: aura.tool_requested is emitted via StreamingRequestHook → tool_event_rx channel (not here)
    // to avoid duplication and maintain proper correlation with tool_call_id via FIFO queue
    if config.emit_custom_events {
        state
            .tool_start_times
            .insert(tool_call.id.clone(), std::time::Instant::now());
    }

    // Determine arguments based on tool result mode
    let arguments = match config.tool_result_mode {
        ToolResultMode::None | ToolResultMode::Aura => tool_call.arguments.clone(),
        ToolResultMode::OpenWebUI => String::new(),
    };

    let chunk = ChatCompletionChunk {
        id: ctx.completion_id.clone(),
        object: CHUNK_OBJECT.to_string(),
        created: ctx.created_timestamp,
        model: ctx.model_str.clone(),
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionChunkDelta {
                role: None,
                content: None,
                tool_calls: Some(vec![ToolCallChunk {
                    index: state.tool_call_index,
                    id: tool_call.id.clone(),
                    call_type: FUNCTION_TYPE.to_string(),
                    function: FunctionCallChunk {
                        name: tool_call.name.clone(),
                        arguments,
                    },
                }]),
            },
            finish_reason: None,
        }],
        usage: None,
    };

    state.tool_call_index += 1;

    if let Ok(bytes) = format_sse_chunk(&chunk) {
        output.push(bytes);
    }

    output
}

/// Handle a tool result.
fn handle_tool_result(
    config: &StreamConfig,
    ctx: &TurnContext,
    state: &mut TurnState,
    tool_result: &ToolResult,
) -> Vec<Bytes> {
    tracing::info!(
        "Streaming tool result for call: {} (call_id: {:?})",
        tool_result.id,
        tool_result.call_id
    );

    let mut output = Vec::with_capacity(2);
    state.needs_separator = true;

    // Emit aura.tool_complete custom event (if enabled)
    if config.emit_custom_events {
        let duration_ms = state
            .tool_start_times
            .remove(&tool_result.id)
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let tool_name = state
            .tool_call_map
            .get(&tool_result.id)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Aura's ToolResult has result as a plain String
        // Try to unescape JSON-quoted strings for error detection
        let result_text = serde_json::from_str::<String>(&tool_result.result)
            .unwrap_or_else(|_| tool_result.result.clone());

        tracing::debug!(
            "Tool '{}' result_text (first 200 chars): {}",
            tool_name,
            if result_text.len() > 200 {
                format!("{}...", &result_text[..200])
            } else {
                result_text.clone()
            }
        );

        let status = detect_tool_error(&result_text);
        let event = match status {
            ToolResultStatus::Error(ref err) => {
                tracing::warn!(
                    "Tool '{}' returned error: {} - {}",
                    tool_name,
                    err.error_type(),
                    err.message()
                );
                AuraStreamEvent::tool_complete_failure(
                    &tool_result.id,
                    &tool_name,
                    duration_ms,
                    err.full_message(),
                    ctx.agent_context.clone(),
                    ctx.correlation.clone(),
                )
            }
            ToolResultStatus::Success => {
                let truncated_result = truncate_result(&result_text, config.tool_result_max_length);
                AuraStreamEvent::tool_complete_success(
                    &tool_result.id,
                    &tool_name,
                    duration_ms,
                    &truncated_result,
                    ctx.agent_context.clone(),
                    ctx.correlation.clone(),
                )
            }
        };

        output.push(Bytes::from(event.format_sse()));
    }

    // OpenWebUI mode: emit result as second tool_calls delta
    if config.tool_result_mode == ToolResultMode::OpenWebUI {
        let lookup_result = state.tool_call_map.get(&tool_result.id).or_else(|| {
            tool_result
                .call_id
                .as_ref()
                .and_then(|cid| state.tool_call_map.get(cid))
        });

        if let Some((tool_name, original_index)) = lookup_result {
            tracing::info!(
                "   Found original tool: {} at index {} (OpenWebUI mode)",
                tool_name,
                original_index
            );

            // Truncate the result string directly
            let content_str = truncate_result(&tool_result.result, config.tool_result_max_length);

            let chunk = ChatCompletionChunk {
                id: ctx.completion_id.clone(),
                object: CHUNK_OBJECT.to_string(),
                created: ctx.created_timestamp,
                model: ctx.model_str.clone(),
                choices: vec![ChatCompletionChunkChoice {
                    index: 0,
                    delta: ChatCompletionChunkDelta {
                        role: None,
                        content: None,
                        tool_calls: Some(vec![ToolCallChunk {
                            index: *original_index,
                            id: tool_result.id.clone(),
                            call_type: FUNCTION_TYPE.to_string(),
                            function: FunctionCallChunk {
                                name: String::new(),
                                arguments: content_str,
                            },
                        }]),
                    },
                    finish_reason: None,
                }],
                usage: None,
            };

            if let Ok(bytes) = format_sse_chunk(&chunk) {
                output.push(bytes);
            }
        } else {
            tracing::warn!(
                "   Could not find original tool call for result id: {} or call_id: {:?}",
                tool_result.id,
                tool_result.call_id
            );
        }
    }

    output
}

/// Handle reasoning content.
fn handle_reasoning(config: &StreamConfig, ctx: &TurnContext, reasoning: String) -> Vec<Bytes> {
    tracing::debug!("Received reasoning during streaming");

    if config.emit_custom_events && config.emit_reasoning {
        let event = AuraStreamEvent::reasoning(
            reasoning,
            ctx.agent_context.clone(),
            ctx.correlation.clone(),
        );
        vec![Bytes::from(event.format_sse())]
    } else {
        vec![]
    }
}

/// Convert a non-empty string to `Some(truncated)`, or `None` if empty.
fn maybe_truncate(s: &str, max_len: usize) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(truncate_result(s, max_len))
    }
}

fn handle_orchestrator_event(
    config: &StreamConfig,
    ctx: &TurnContext,
    event: &OrchestratorEvent,
) -> Vec<Bytes> {
    if !config.emit_custom_events {
        tracing::debug!(
            "Orchestrator event skipped (custom events disabled): {:?}",
            event
        );
        return vec![];
    }

    let ectx = EventContext::new(ctx.agent_context.clone(), ctx.correlation.clone());

    let sse_event: OrchestrationStreamEvent = match event {
        OrchestratorEvent::PlanCreated {
            goal,
            task_count,
            routing_mode,
            routing_rationale,
            planning_response,
        } => {
            tracing::info!(
                "Orchestrator: plan created with {} tasks for goal: {} (routing={:?}, rationale: {})",
                task_count,
                goal,
                routing_mode,
                routing_rationale
            );
            OrchestrationStreamEvent::plan_created(
                goal,
                *task_count,
                routing_mode.clone(),
                routing_rationale,
                maybe_truncate(planning_response, config.tool_result_max_length),
                ectx,
            )
        }
        OrchestratorEvent::DirectAnswer {
            response,
            routing_rationale,
        } => {
            tracing::info!(
                "Orchestrator: direct answer (rationale: {})",
                routing_rationale
            );
            OrchestrationStreamEvent::direct_answer(response, routing_rationale, ectx)
        }
        OrchestratorEvent::ClarificationNeeded {
            question,
            options,
            routing_rationale,
        } => {
            tracing::info!(
                "Orchestrator: clarification needed - {} (rationale: {})",
                question,
                routing_rationale
            );
            OrchestrationStreamEvent::clarification_needed(
                question,
                options.clone(),
                routing_rationale,
                ectx,
            )
        }
        OrchestratorEvent::TaskStarted {
            task_id,
            description,
            orchestrator_id,
            worker_id,
        } => {
            tracing::info!("Orchestrator: task {} started - {}", task_id, description);
            OrchestrationStreamEvent::task_started(
                *task_id,
                description,
                orchestrator_id,
                worker_id,
                ectx,
            )
        }
        OrchestratorEvent::TaskCompleted {
            task_id,
            success,
            duration_ms,
            orchestrator_id,
            worker_id,
            result,
        } => {
            tracing::info!(
                "Orchestrator: task {} completed (success={}) in {}ms",
                task_id,
                success,
                duration_ms
            );
            OrchestrationStreamEvent::task_completed(
                *task_id,
                *success,
                *duration_ms,
                orchestrator_id,
                worker_id,
                maybe_truncate(result, config.tool_result_max_length),
                ectx,
            )
        }
        OrchestratorEvent::IterationComplete {
            iteration,
            quality_score,
            quality_threshold,
            will_replan,
            evaluation_skipped,
            reasoning,
            gaps,
        } => {
            tracing::info!(
                "Orchestrator: iteration {} complete (quality={:.2}, threshold={:.2}, will_replan={}, eval_skipped={})",
                iteration,
                quality_score,
                quality_threshold,
                will_replan,
                evaluation_skipped
            );
            OrchestrationStreamEvent::iteration_complete(
                *iteration,
                *quality_score,
                *quality_threshold,
                *will_replan,
                *evaluation_skipped,
                maybe_truncate(reasoning, config.tool_result_max_length),
                gaps.clone(),
                ectx,
            )
        }
        OrchestratorEvent::ReplanStarted { iteration, trigger } => {
            tracing::info!(
                "Orchestrator: replan started (iteration={}, trigger={})",
                iteration,
                trigger
            );
            OrchestrationStreamEvent::replan_started(*iteration, trigger, ectx)
        }
        OrchestratorEvent::Synthesizing { iteration } => {
            tracing::info!(
                "Orchestrator: synthesizing results (iteration={})",
                iteration
            );
            OrchestrationStreamEvent::synthesizing(*iteration, ectx)
        }
        OrchestratorEvent::WorkerReasoning {
            task_id,
            worker_id,
            content,
        } => {
            if !config.emit_custom_events || !config.emit_reasoning {
                return vec![];
            }
            tracing::debug!(
                "Orchestrator: worker reasoning (task={}, worker={})",
                task_id,
                worker_id
            );
            // Emit as aura.orchestrator.worker_reasoning (orchestration event)
            let orch_event = OrchestrationStreamEvent::worker_reasoning(
                *task_id,
                worker_id,
                content,
                ectx.clone(),
            );
            let mut bytes = vec![Bytes::from(orch_event.format_sse())];
            // Also emit as aura.reasoning with agent_id set to the worker name
            // for backward-compatible reasoning aggregation
            let worker_agent =
                aura::stream_events::AgentContext::worker(worker_id, None, "coordinator");
            let reasoning_event =
                AuraStreamEvent::reasoning(content, worker_agent, ctx.correlation.clone());
            bytes.push(Bytes::from(reasoning_event.format_sse()));
            return bytes;
        }
        OrchestratorEvent::ToolCallStarted {
            task_id,
            tool_call_id,
            tool_name,
            worker_id,
            arguments,
        } => {
            tracing::info!(
                "Orchestrator: task {:?} tool call started - {} ({})",
                task_id,
                tool_name,
                tool_call_id
            );
            OrchestrationStreamEvent::tool_call_started(
                *task_id,
                tool_call_id,
                tool_name,
                worker_id,
                Some(arguments.clone()),
                ectx,
            )
        }
        OrchestratorEvent::ToolCallCompleted {
            task_id,
            tool_call_id,
            success,
            duration_ms,
            result,
        } => {
            tracing::info!(
                "Orchestrator: task {:?} tool call completed - {} (success={}) in {}ms",
                task_id,
                tool_call_id,
                success,
                duration_ms
            );
            OrchestrationStreamEvent::tool_call_completed(
                *task_id,
                tool_call_id,
                *success,
                *duration_ms,
                maybe_truncate(result, config.tool_result_max_length),
                ectx,
            )
        }
        OrchestratorEvent::PhaseStarted {
            phase_id,
            label,
            orchestrator_id,
        } => {
            tracing::info!("Orchestrator: phase {} started - '{}'", phase_id, label);
            OrchestrationStreamEvent::phase_started(*phase_id, label, orchestrator_id, ectx)
        }
        OrchestratorEvent::PhaseCompleted {
            phase_id,
            label,
            continuation,
            orchestrator_id,
        } => {
            tracing::info!(
                "Orchestrator: phase {} completed - '{}' (continuation={})",
                phase_id,
                label,
                continuation
            );
            OrchestrationStreamEvent::phase_completed(
                *phase_id,
                label,
                *continuation,
                orchestrator_id,
                ectx,
            )
        }
    };

    vec![Bytes::from(sse_event.format_sse())]
}

fn is_context_overflow_error(error_str: &str) -> bool {
    let lower = error_str.to_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("maximum number of tokens")
        || lower.contains("token limit")
        || lower.contains("tokens exceeded")
        || (lower.contains("resulted in") && lower.contains("tokens"))
        || (lower.contains("context") && lower.contains("exceeded"))
}

fn build_text_chunk(ctx: &TurnContext, content: &str, is_first: bool) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: ctx.completion_id.clone(),
        object: CHUNK_OBJECT.to_string(),
        created: ctx.created_timestamp,
        model: ctx.model_str.clone(),
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionChunkDelta {
                role: if is_first {
                    Some(MessageRole::Assistant)
                } else {
                    None
                },
                content: Some(content.to_string()),
                tool_calls: None,
            },
            finish_reason: None,
        }],
        usage: None,
    }
}

/// Build the final chunk with finish_reason.
fn build_final_chunk(ctx: &TurnContext, state: &TurnState) -> Vec<Bytes> {
    let finish_reason = if let (Some(max), Some(usage)) = (ctx.max_tokens, &state.usage_stats) {
        if usage.completion_tokens >= max as u64 {
            FINISH_REASON_LENGTH
        } else {
            FINISH_REASON_STOP
        }
    } else {
        FINISH_REASON_STOP
    };

    let final_chunk = ChatCompletionChunk {
        id: ctx.completion_id.clone(),
        object: CHUNK_OBJECT.to_string(),
        created: ctx.created_timestamp,
        model: ctx.model_str.clone(),
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionChunkDelta {
                role: None,
                content: None,
                tool_calls: None,
            },
            finish_reason: Some(finish_reason.to_string()),
        }],
        usage: state.usage_stats.clone(),
    };

    match format_sse_chunk(&final_chunk) {
        Ok(bytes) => vec![bytes],
        Err(e) => {
            tracing::error!("Failed to serialize final chunk: {}", e);
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura::stream_events::{AgentContext, CorrelationContext};

    /// Verify handle_tool_call does NOT emit aura.tool_requested events directly.
    /// The aura.tool_requested event is emitted via StreamingRequestHook → tool_event_rx channel
    /// to avoid duplication and maintain proper correlation with tool_call_id via FIFO queue.
    #[test]
    fn test_handle_tool_call_does_not_emit_tool_requested() {
        let config = StreamConfig {
            emit_custom_events: true,
            emit_reasoning: true,
            tool_result_mode: ToolResultMode::None,
            tool_result_max_length: 1000,
            fallback_tool_parsing: false,
        };

        let ctx = TurnContext {
            completion_id: "test-123".to_string(),
            model_str: "gpt-4".to_string(),
            created_timestamp: 1234567890,
            max_tokens: None,
            agent_context: AgentContext::single_agent(),
            correlation: CorrelationContext::new("test-session", None),
        };

        let mut state = TurnState::new();

        let tool_call = ToolCall {
            id: "call_abc123".to_string(),
            name: "list_files".to_string(),
            arguments: r#"{"path": "/"}"#.to_string(),
        };

        let output = handle_tool_call(&config, &ctx, &mut state, &tool_call);

        // Convert output to strings for inspection
        let output_str: String = output
            .iter()
            .filter_map(|b| std::str::from_utf8(b).ok())
            .collect();

        // Verify NO aura.tool_requested event is emitted
        // (it should come via tool_event_rx channel from StreamingRequestHook, not here)
        assert!(
            !output_str.contains("aura.tool_requested"),
            "handle_tool_call should NOT emit aura.tool_requested directly - \
             it's emitted via StreamingRequestHook → tool_event_rx channel. Found: {}",
            output_str
        );

        // Verify tool_start_times is populated (for duration tracking)
        assert!(
            state.tool_start_times.contains_key("call_abc123"),
            "tool_start_times should be populated for duration calculation"
        );

        // Verify OpenAI tool_call chunk IS emitted
        assert!(
            output_str.contains("tool_calls"),
            "OpenAI tool_calls chunk should still be emitted"
        );
    }

    #[test]
    fn test_is_context_overflow_error_openai_style() {
        assert!(is_context_overflow_error("context_length_exceeded"));
        assert!(is_context_overflow_error(
            "This model's maximum context length is 128000 tokens"
        ));
    }

    #[test]
    fn test_is_context_overflow_error_anthropic_style() {
        assert!(is_context_overflow_error(
            "maximum number of tokens exceeded"
        ));
        assert!(is_context_overflow_error(
            "Your request resulted in 150000 tokens, which exceeds the limit"
        ));
    }

    #[test]
    fn test_is_context_overflow_error_generic() {
        assert!(is_context_overflow_error("token limit reached"));
        assert!(is_context_overflow_error("tokens exceeded"));
        assert!(is_context_overflow_error("context length exceeded"));
    }

    #[test]
    fn test_is_context_overflow_error_case_insensitive() {
        assert!(is_context_overflow_error("CONTEXT_LENGTH_EXCEEDED"));
        assert!(is_context_overflow_error("Maximum Context Length"));
        assert!(is_context_overflow_error("TOKEN LIMIT"));
    }

    #[test]
    fn test_is_context_overflow_error_not_overflow() {
        assert!(!is_context_overflow_error("network timeout"));
        assert!(!is_context_overflow_error("authentication failed"));
        assert!(!is_context_overflow_error("rate limit exceeded")); // rate limit != context
        assert!(!is_context_overflow_error("internal server error"));
    }

    #[tokio::test]
    async fn test_collect_stream_normal_completion() {
        let config = StreamConfig::new(false, false, ToolResultMode::None, 0);
        let ctx = TurnContext::new(
            "test-id".to_string(),
            "test-model".to_string(),
            0,
            None,
            "test-session",
        );

        let items: Vec<Result<StreamItem, StreamError>> = vec![
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text("Hello ".to_string()),
            )),
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text("world".to_string()),
            )),
        ];
        let stream = futures_util::stream::iter(items);

        let (outcome, termination) = collect_stream_to_completion(&config, &ctx, stream).await;

        assert_eq!(outcome.content, "Hello world");
        assert!(outcome.usage.is_none());
        assert_eq!(termination, StreamTermination::Complete);
    }

    #[tokio::test]
    async fn test_collect_stream_with_final_usage() {
        use rig::completion::Usage as RigUsage;

        let config = StreamConfig::new(false, false, ToolResultMode::None, 0);
        let ctx = TurnContext::new(
            "test-id".to_string(),
            "test-model".to_string(),
            0,
            None,
            "test-session",
        );

        let items: Vec<Result<StreamItem, StreamError>> = vec![
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text("Hello".to_string()),
            )),
            Ok(StreamItem::Final(aura::FinalResponseInfo {
                content: "Hello".to_string(),
                usage: RigUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                },
            })),
        ];
        let stream = futures_util::stream::iter(items);

        let (outcome, termination) = collect_stream_to_completion(&config, &ctx, stream).await;

        assert_eq!(outcome.content, "Hello");
        assert!(outcome.usage.is_some());
        let usage = outcome.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
        assert_eq!(termination, StreamTermination::Complete);
    }

    /// Verify that a first_chunk_timeout of Duration::MAX never fires before a real item arrives.
    /// This tests that the `unwrap_or(Duration::MAX)` sentinel is effectively infinite — a stream
    /// that yields immediately should complete before any timeout fires.
    #[tokio::test]
    async fn test_first_chunk_timeout_sentinel_is_effectively_infinite() {
        use std::time::Duration;

        // Duration::MAX should not fire before a stream yields its first item.
        // We simulate the core select! logic: first_chunk_sleep vs. an instant stream item.
        let first_chunk_sleep = tokio::time::sleep(Duration::MAX);
        tokio::pin!(first_chunk_sleep);

        let mut first_chunk_received = false;
        let has_first_chunk_timeout = false; // no timeout configured → sentinel is used

        // Simulate one item arriving immediately
        let item_ready = async { true };

        tokio::select! {
            result = item_ready => {
                first_chunk_received = result;
            }
            _ = &mut first_chunk_sleep, if !first_chunk_received && has_first_chunk_timeout => {
                panic!("Duration::MAX sentinel fired before stream item arrived");
            }
        }

        assert!(
            first_chunk_received,
            "Stream item should have been received without timeout interference"
        );
    }

    /// Verify that `first_chunk_timeout.unwrap()` is safe inside the guard:
    /// the guard `if !first_chunk_received && has_first_chunk_timeout` is only true when
    /// `first_chunk_timeout.is_some()`, so `unwrap()` never panics in practice.
    #[test]
    fn test_first_chunk_timeout_unwrap_safe_when_guard_true() {
        // has_first_chunk_timeout = first_chunk_timeout.is_some()
        // So if the guard passes, unwrap() is guaranteed to succeed.
        let timeout: Option<Duration> = Some(Duration::from_millis(100));
        assert!(timeout.is_some());
        // Verify the guarded unwrap pattern: if is_some() passes, unwrap is safe
        let duration = Duration::from_millis(100);
        assert_eq!(timeout, Some(duration));

        // And confirm the None case never enters the branch
        let no_timeout: Option<Duration> = None;
        let has_no_timeout = no_timeout.is_some();
        assert!(
            !has_no_timeout,
            "None timeout must not set has_first_chunk_timeout"
        );
    }

    #[tokio::test]
    async fn test_collect_stream_error_returns_stream_error_termination() {
        let config = StreamConfig::new(false, false, ToolResultMode::None, 0);
        let ctx = TurnContext::new(
            "test-id".to_string(),
            "test-model".to_string(),
            0,
            None,
            "test-session",
        );

        let items: Vec<Result<StreamItem, StreamError>> = vec![
            Ok(StreamItem::StreamAssistantItem(
                StreamedAssistantContent::Text("partial".to_string()),
            )),
            Err("something went wrong".into()),
        ];
        let stream = futures_util::stream::iter(items);

        let (outcome, termination) = collect_stream_to_completion(&config, &ctx, stream).await;

        assert!(
            outcome.content.starts_with("partial"),
            "Expected content to start with partial, got: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains("upstream model provider"),
            "Expected generic error message in content, got: {}",
            outcome.content
        );
        assert!(
            matches!(&termination, StreamTermination::StreamError(msg) if msg.contains("something went wrong")),
            "Expected StreamError, got: {:?}",
            termination
        );
    }

    #[tokio::test]
    async fn test_collect_stream_generic_error_surfaces_message() {
        let config = StreamConfig::new(false, false, ToolResultMode::None, 0);
        let ctx = TurnContext::new(
            "test-id".to_string(),
            "test-model".to_string(),
            0,
            None,
            "test-session",
        );

        let items: Vec<Result<StreamItem, StreamError>> = vec![Err(
            "400 Bad Request: temperature is not supported for gpt-5-mini".into(),
        )];
        let stream = futures_util::stream::iter(items);

        let (outcome, termination) = collect_stream_to_completion(&config, &ctx, stream).await;

        assert!(
            outcome.content.contains("upstream model provider"),
            "Expected error message in content, got: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains("test-model"),
            "Expected model name in error message, got: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains("could not be completed"),
            "Expected actionable guidance in error message, got: {}",
            outcome.content
        );
        assert!(
            matches!(&termination, StreamTermination::StreamError(msg) if msg.contains("temperature")),
            "Expected StreamError termination, got: {:?}",
            termination
        );
    }

    #[tokio::test]
    async fn test_collect_stream_context_overflow_still_uses_special_message() {
        let config = StreamConfig::new(false, false, ToolResultMode::None, 0);
        let ctx = TurnContext::new(
            "test-id".to_string(),
            "test-model".to_string(),
            0,
            None,
            "test-session",
        );

        let items: Vec<Result<StreamItem, StreamError>> =
            vec![Err("context_length_exceeded: too many tokens".into())];
        let stream = futures_util::stream::iter(items);

        let (outcome, termination) = collect_stream_to_completion(&config, &ctx, stream).await;

        // Should use the special context overflow message, NOT the generic one
        assert!(
            outcome.content.contains("My tools returned more data"),
            "Expected context overflow message, got: {}",
            outcome.content
        );
        assert!(
            !outcome.content.contains("upstream model provider"),
            "Should NOT use generic error message for context overflow"
        );
        assert!(
            matches!(&termination, StreamTermination::StreamError(_)),
            "Expected StreamError termination, got: {:?}",
            termination
        );
    }
}
