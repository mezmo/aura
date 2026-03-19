use actix_web::{HttpResponse, web};
use aura::{
    RequestCancellation, ResponseContent, StreamingAgent, UsageState, request_progress_subscribe,
    tool_event_subscribe, tool_usage_subscribe,
};
use aura_config::RigBuilder;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{Instrument, error};
use uuid::Uuid;

use crate::streaming::{
    StreamConfig, StreamOtelContext, StreamOutcome, StreamTermination, StreamingCallbacks,
    TurnContext, collect_stream_to_completion, process_sse_stream_full,
};
use crate::types::*;

/// RAII guard that increments the active request counter on creation and
/// decrements it on drop, notifying the shutdown task when the count hits zero.
struct ActiveRequestGuard {
    tracker: Arc<ActiveRequestTracker>,
}

impl ActiveRequestGuard {
    fn new(tracker: Arc<ActiveRequestTracker>) -> Self {
        tracker.increment();
        Self { tracker }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.tracker.decrement();
    }
}

/// RAII guard for all request-scoped resources. Ensures cleanup even on panic.
/// Manages: cancellation, subscriptions (progress, tool events, tool usage), MCP state.
struct RequestResourceGuard {
    request_id: String,
    agent: Arc<dyn StreamingAgent>,
}

impl RequestResourceGuard {
    fn new(request_id: String, agent: Arc<dyn StreamingAgent>) -> Self {
        Self { request_id, agent }
    }
}

impl Drop for RequestResourceGuard {
    fn drop(&mut self) {
        use aura::{request_progress_unsubscribe, tool_event_unsubscribe, tool_usage_unsubscribe};

        // Use try_current to avoid panic during runtime shutdown
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let id = self.request_id.clone();
            let agent = self.agent.clone();
            handle.spawn(async move {
                // Cleanup all request-scoped resources
                agent.clear_mcp_request_id().await;
                RequestCancellation::unregister(&id);
                request_progress_unsubscribe(&id).await;
                tool_event_unsubscribe(&id).await;
                tool_usage_unsubscribe(&id).await;
            });
        }
        // If no runtime, cleanup is best-effort (server is shutting down anyway)
    }
}

/// Used by completion logic to determine how output should be delivered to the client.
/// Both streaming and non-streaming handlers delegate to the same core logic with different
/// delivery modes but the same observability instrumentation and stream processing.
struct CompletionConfig {
    request_id: String,
    timeout_duration: std::time::Duration,
    first_chunk_timeout: Option<std::time::Duration>,
    stream_config: StreamConfig,
    turn_context: TurnContext,
    stream_shutdown_token: tokio_util::sync::CancellationToken,
    active_requests: Arc<ActiveRequestTracker>,
    // OTel values
    provider: String,
    model: String,
    query_for_otel: String,
    message_count: usize,
    response_content: ResponseContent,
}

/// Determines how stream output reaches the client.
enum DeliveryMode {
    /// Non-streaming: collect everything, send back via oneshot.
    Collect {
        result_tx: oneshot::Sender<CollectedResult>,
    },
    /// Streaming SSE: send chunks via mpsc.
    Sse {
        chunk_tx: mpsc::Sender<Result<actix_web::web::Bytes, String>>,
        heartbeat_interval: std::time::Duration,
    },
}

/// Result sent back via oneshot for the non-streaming path.
struct CollectedResult {
    outcome: StreamOutcome,
    usage_state: UsageState,
}

/// Values needed to build the final JSON response (cloned before spawn).
struct ResponseContext {
    completion_id: String,
    model_str: String,
    created_timestamp: u64,
    chat_session_id: String,
}

/// Build a fresh agent for a request, applying headers_from_request mappings.
async fn build_agent_for_request(
    config: &aura_config::Config,
    req_headers: &HashMap<String, String>,
) -> Result<Arc<aura::Agent>, HttpResponse> {
    let builder = RigBuilder::new(config.clone());
    let agent = builder
        .build_agent_with_headers(Some(req_headers))
        .await
        .map_err(|e| {
            error!("Failed to build agent: {}", e);
            HttpResponse::InternalServerError().json(ErrorResponse {
                error: ErrorDetail {
                    message: format!("Failed to build agent: {e}"),
                    error_type: "internal_error".to_string(),
                },
            })
        })?;
    Ok(Arc::new(agent))
}

/// Shared request setup extracted from the incoming ChatCompletionRequest.
/// Used by both streaming and non-streaming handlers.
struct RequestSetup {
    query: String,
    chat_history: Vec<aura::Message>,
    streaming_agent: Arc<dyn StreamingAgent>,
    completion_id: String,
    model_str: String,
    created_timestamp: u64,
    chat_session_id: String,
}

/// Extract query, chat history, and build agent -- shared across both code paths.
async fn prepare_request(
    data: &web::Data<AppState>,
    req: &mut ChatCompletionRequest,
    chat_session_id: &str,
    req_headers_map: &HashMap<String, String>,
) -> Result<RequestSetup, HttpResponse> {
    // Pop the last message as the query — the remainder becomes chat history
    let query = match req.messages.pop() {
        Some(msg) if msg.role == Role::User => msg.content,
        Some(msg) => {
            return Err(HttpResponse::BadRequest().json(ErrorResponse {
                error: ErrorDetail {
                    message: format!("Last message must be from user, got: {}", msg.role),
                    error_type: "invalid_request_error".to_string(),
                },
            }));
        }
        None => {
            return Err(HttpResponse::BadRequest().json(ErrorResponse {
                error: ErrorDetail {
                    message: "messages array is empty".to_string(),
                    error_type: "invalid_request_error".to_string(),
                },
            }));
        }
    };

    // Convert remaining OpenAI messages to Rig messages,
    // dropping system role messages and filtering empty content.
    let chat_history = convert_chat_messages(&req.messages);

    // Build the appropriate agent type based on orchestration config
    let streaming_agent: Arc<dyn StreamingAgent> = if data.config.orchestration_enabled() {
        // Orchestration path: build via streaming agent builder (returns Orchestrator)
        let builder = RigBuilder::new((*data.config).clone());
        builder
            .build_streaming_agent_with_headers(Some(req_headers_map))
            .await
            .map_err(|e| {
                error!("Failed to build streaming agent: {}", e);
                HttpResponse::InternalServerError().json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Failed to build streaming agent: {e}"),
                        error_type: "internal_error".to_string(),
                    },
                })
            })?
    } else {
        // Standard path: build Agent, coerce to Arc<dyn StreamingAgent>
        build_agent_for_request(&data.config, req_headers_map).await? as Arc<dyn StreamingAgent>
    };

    let (provider, model) = streaming_agent.get_provider_info();
    let model_str = format!("{provider}/{model}");
    let completion_id = format!("chatcmpl-{}", Uuid::new_v4());
    let created_timestamp = Utc::now().timestamp() as u64;

    Ok(RequestSetup {
        query,
        chat_history,
        streaming_agent,
        completion_id,
        model_str,
        created_timestamp,
        chat_session_id: chat_session_id.to_string(),
    })
}

/// Handle chat completions endpoint
#[tracing::instrument(name = "chat_completions", skip(data, req, http_req), fields(otel.kind = "server"))]
pub async fn chat_completions(
    data: web::Data<AppState>,
    req: web::Json<ChatCompletionRequest>,
    http_req: actix_web::HttpRequest,
) -> HttpResponse {
    // Validate we have messages
    if req.messages.is_empty() {
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: ErrorDetail {
                message: "No messages provided".to_string(),
                error_type: "invalid_request_error".to_string(),
            },
        });
    }

    // Extract or generate chat_session_id
    // Priority: metadata > X-Chat-Session-Id header > x-openwebui-chat-id header > generate new
    let chat_session_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.get("chat_session_id"))
        .cloned()
        .or_else(|| {
            http_req
                .headers()
                .get("X-Chat-Session-Id")
                .or_else(|| http_req.headers().get("x-openwebui-chat-id"))
                .and_then(|h| h.to_str().ok())
                .map(String::from)
        })
        .unwrap_or_else(generate_chat_session_id);

    // Convert actix HeaderMap to HashMap for framework-agnostic passing
    let req_headers_map: HashMap<String, String> = http_req
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|val| (k.to_string(), val.to_string())))
        .collect();

    let mut req = req.into_inner();
    let setup = match prepare_request(&data, &mut req, &chat_session_id, &req_headers_map).await {
        Ok(s) => s,
        Err(response) => return response,
    };

    if req.stream == Some(true) {
        handle_streaming_completion(data, setup, req.max_tokens).await
    } else {
        handle_non_streaming_completion(&data, setup, req.max_tokens).await
    }
}

/// Build configuration for the spawned completion task from AppState and RequestSetup.
fn build_completion_config(
    data: &web::Data<AppState>,
    setup: &RequestSetup,
    max_tokens: Option<u32>,
    emit_custom_events: bool,
    emit_reasoning: bool,
) -> CompletionConfig {
    let timeout_duration = std::time::Duration::from_secs(data.streaming_timeout_secs);
    let first_chunk_timeout = if data.first_chunk_timeout_secs > 0 {
        Some(std::time::Duration::from_secs(
            data.first_chunk_timeout_secs,
        ))
    } else {
        None
    };
    let request_id = format!("req_{}", Uuid::new_v4().simple());
    let fallback_tool_parsing = data.config.is_fallback_tool_parsing_enabled();

    let stream_config = StreamConfig::new(
        emit_custom_events,
        emit_reasoning,
        data.tool_result_mode,
        data.tool_result_max_length,
    )
    .with_fallback_tool_parsing(fallback_tool_parsing);

    let turn_context = {
        let ctx = TurnContext::new(
            setup.completion_id.clone(),
            setup.model_str.clone(),
            setup.created_timestamp,
            max_tokens,
            &setup.chat_session_id,
        );
        if data.config.orchestration_enabled() {
            ctx.with_orchestration()
        } else {
            ctx
        }
    };

    let (p, m) = setup.streaming_agent.get_provider_info();
    let (provider, model) = (p.to_string(), m.to_string());
    let response_content = ResponseContent::new();
    let message_count = setup.chat_history.len() + 1; // +1 for the current query

    CompletionConfig {
        request_id,
        timeout_duration,
        first_chunk_timeout,
        stream_config,
        turn_context,
        stream_shutdown_token: data.stream_shutdown_token.clone(),
        active_requests: data.active_requests.clone(),
        provider,
        model,
        query_for_otel: setup.query.clone(),
        message_count,
        response_content,
    }
}

/// Core request completion logic.
///
/// Both streaming and non-streaming handlers delegate here. The `delivery` parameter
/// determines whether output is collected into a oneshot (non-streaming) or streamed
/// via mpsc (SSE).
async fn execute_completion(setup: RequestSetup, config: CompletionConfig, delivery: DeliveryMode) {
    let _active_guard = ActiveRequestGuard::new(config.active_requests.clone());
    let _cancellation = RequestCancellation::register(config.request_id.clone());
    setup
        .streaming_agent
        .set_mcp_request_id(&config.request_id)
        .await;

    // RAII guard ensures cleanup of all request resources even on panic
    let _resource_guard =
        RequestResourceGuard::new(config.request_id.clone(), setup.streaming_agent.clone());

    // Destructure to move chat_history instead of cloning
    let RequestSetup {
        query,
        chat_history,
        streaming_agent,
        completion_id: _,
        model_str,
        created_timestamp: _,
        chat_session_id,
    } = setup;

    // Create stream with timeout — single path for both Agent and Orchestrator
    let (stream, cancel_tx, usage_state) = streaming_agent
        .stream_with_timeout(
            &query,
            chat_history,
            config.timeout_duration,
            &config.request_id,
        )
        .await;

    let response_content = config.response_content.clone();
    let otel_ctx = StreamOtelContext {
        provider: config.provider,
        model: config.model,
        request_id: config.request_id.clone(),
        session_id: chat_session_id.clone(),
        query: config.query_for_otel,
        identity_id: String::new(),
        message_count: config.message_count,
        usage_state: usage_state.clone(),
        response_content: config.response_content,
    };
    otel_ctx.record_input();

    let termination = match delivery {
        DeliveryMode::Collect { result_tx } => {
            let (outcome, termination) =
                collect_stream_to_completion(&config.stream_config, &config.turn_context, stream)
                    .await;

            if outcome.usage.is_some() && !outcome.content.is_empty() {
                response_content.set(outcome.content.clone());
            }

            let _ = result_tx.send(CollectedResult {
                outcome,
                usage_state: usage_state.clone(),
            });
            termination
        }
        DeliveryMode::Sse {
            chunk_tx,
            heartbeat_interval,
        } => {
            // Request-scoped subscriptions (isolated per request to prevent cross-tenant leakage)
            let progress_rx = request_progress_subscribe(&config.request_id).await;
            let tool_event_rx = tool_event_subscribe(&config.request_id).await;
            let tool_usage_rx = tool_usage_subscribe(&config.request_id).await;

            let callbacks = StreamingCallbacks {
                request_id: config.request_id.clone(),
                agent: streaming_agent.clone(),
                tool_event_rx,
                progress_rx,
                tool_usage_rx,
                usage_state: usage_state.clone(),
                response_content,
                model_name: model_str,
                stream_shutdown_token: config.stream_shutdown_token.clone(),
            };

            process_sse_stream_full(
                &config.stream_config,
                &config.turn_context,
                stream,
                chunk_tx,
                cancel_tx,
                config.timeout_duration,
                heartbeat_interval,
                config.first_chunk_timeout,
                callbacks,
            )
            .await
        }
    };

    otel_ctx.record_output(&termination);

    match &termination {
        StreamTermination::Complete => {
            tracing::debug!("Stream producer completed normally");
        }
        StreamTermination::StreamError(err) => {
            tracing::warn!("Stream producer ended with error: {}", err);
        }
        StreamTermination::Disconnected => {
            tracing::info!("Stream producer ended: client disconnected");
        }
        StreamTermination::Timeout => {
            tracing::warn!("Stream producer ended: timeout");
        }
        StreamTermination::Shutdown => {
            tracing::info!("Stream producer ended: server shutdown");
        }
    }

    aura::logging::flush_tracer();
}

/// Build the final JSON response for non-streaming completions.
fn build_json_response(
    response_ctx: ResponseContext,
    max_tokens: Option<u32>,
    collected: CollectedResult,
) -> HttpResponse {
    // Get usage: prefer stream outcome (from Final), fall back to UsageState from hook
    let (prompt_tokens, completion_tokens, total_tokens) = collected
        .outcome
        .usage
        .map(|u| (u.prompt_tokens, u.completion_tokens, u.total_tokens))
        .unwrap_or_else(|| collected.usage_state.get_final_usage());

    let finish_reason = if let Some(max) = max_tokens {
        if completion_tokens >= max as u64 {
            "length"
        } else {
            "stop"
        }
    } else {
        "stop"
    };

    let mut response_metadata = HashMap::new();
    response_metadata.insert(
        "chat_session_id".to_string(),
        response_ctx.chat_session_id.clone(),
    );

    let chat_response = ChatCompletionResponse {
        id: response_ctx.completion_id,
        object: "chat.completion".to_string(),
        created: response_ctx.created_timestamp,
        model: response_ctx.model_str,
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: Role::Assistant,
                content: collected.outcome.content,
            },
            finish_reason: finish_reason.to_string(),
        }],
        usage: Some(Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        }),
        metadata: Some(response_metadata),
    };

    HttpResponse::Ok().json(chat_response)
}

#[tracing::instrument(name = "non_streaming_completion", skip_all, fields(session.id = %setup.chat_session_id))]
async fn handle_non_streaming_completion(
    data: &web::Data<AppState>,
    setup: RequestSetup,
    max_tokens: Option<u32>,
) -> HttpResponse {
    let config = build_completion_config(data, &setup, max_tokens, false, false);

    {
        let span = tracing::Span::current();
        aura::logging::set_span_attribute(&span, "http.request_id", config.request_id.clone());
    }

    let response_ctx = ResponseContext {
        completion_id: setup.completion_id.clone(),
        model_str: setup.model_str.clone(),
        created_timestamp: setup.created_timestamp,
        chat_session_id: setup.chat_session_id.clone(),
    };

    let (result_tx, result_rx) = oneshot::channel();

    tokio::spawn(
        execute_completion(setup, config, DeliveryMode::Collect { result_tx })
            .instrument(tracing::info_span!(parent: None, "agent.stream")),
    );

    match result_rx.await {
        Ok(collected) => build_json_response(response_ctx, max_tokens, collected),
        Err(_) => {
            error!("Completion task panicked or was dropped");
            HttpResponse::InternalServerError().json(ErrorResponse {
                error: ErrorDetail {
                    message: "Internal error during completion".to_string(),
                    error_type: "internal_error".to_string(),
                },
            })
        }
    }
}

/// Handle streaming completion using Server-Sent Events.
#[tracing::instrument(name = "streaming_completion", skip_all, fields(session.id = %setup.chat_session_id))]
async fn handle_streaming_completion(
    data: web::Data<AppState>,
    setup: RequestSetup,
    max_tokens: Option<u32>,
) -> HttpResponse {
    let config = build_completion_config(
        &data,
        &setup,
        max_tokens,
        data.aura_custom_events,
        data.aura_emit_reasoning,
    );

    {
        let span = tracing::Span::current();
        aura::logging::set_span_attribute(&span, "http.request_id", config.request_id.clone());
    }

    let chat_session_id = setup.chat_session_id.clone();

    let (chunk_tx, rx) =
        mpsc::channel::<Result<actix_web::web::Bytes, String>>(data.streaming_buffer_size);

    let heartbeat_interval = std::time::Duration::from_secs(15);

    tokio::spawn(
        execute_completion(
            setup,
            config,
            DeliveryMode::Sse {
                chunk_tx,
                heartbeat_interval,
            },
        )
        .instrument(tracing::info_span!(parent: None, "agent.stream")),
    );

    use futures_util::TryStreamExt;
    let response_stream =
        ReceiverStream::new(rx).map_err(actix_web::error::ErrorInternalServerError);

    HttpResponse::Ok()
        .content_type("text/event-stream")
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("X-Accel-Buffering", "no"))
        .insert_header(("X-Chat-Session-Id", chat_session_id))
        .streaming(response_stream)
}

/// Convert OpenAI-format chat messages to Rig messages, sanitizing the history:
/// - Drops `system` role messages (Aura's preamble is the authoritative system prompt)
/// - Filters messages with empty/whitespace-only content
/// - Maps `user` and `assistant` roles; skips unknown roles
fn convert_chat_messages(messages: &[ChatMessage]) -> Vec<aura::Message> {
    use aura::Message;

    messages
        .iter()
        .filter_map(|msg| match msg.role {
            Role::System => {
                tracing::warn!("Dropping system role message from chat history — Aura's preamble is authoritative");
                None
            }
            _ if msg.content.trim().is_empty() => {
                tracing::warn!(role = %msg.role, "Dropping empty message from chat history");
                None
            }
            Role::User => Some(Message::user(&msg.content)),
            Role::Assistant => Some(Message::assistant(&msg.content)),
            Role::Unknown => {
                tracing::warn!(role = %msg.role, "Skipping message with unknown role");
                None
            }
        })
        .collect()
}

/// Health check endpoint
pub async fn health() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "healthy"
    }))
}

/// OpenAI-compatible model listing endpoint
pub async fn list_models(data: web::Data<AppState>) -> HttpResponse {
    let (provider, model) = data.config.llm.model_info();
    let model_id = format!("{provider}/{model}");

    HttpResponse::Ok().json(serde_json::json!({
        "object": "list",
        "data": [
            {
                "id": model_id,
                "object": "model",
                "created": 1677649963,
                "owned_by": provider
            }
        ]
    }))
}

/// Generate a chat session ID (simple GUID)
fn generate_chat_session_id() -> String {
    format!("cs_{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, Role};

    fn msg(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
        }
    }

    #[test]
    fn test_system_messages_dropped() {
        let messages = vec![
            msg(Role::System, "You are a helpful assistant"),
            msg(Role::User, "Hello"),
        ];
        let result = convert_chat_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_empty_messages_filtered() {
        let messages = vec![
            msg(Role::User, "Hello"),
            msg(Role::Assistant, ""),
            msg(Role::Assistant, "   "),
            msg(Role::User, "How are you?"),
        ];
        let result = convert_chat_messages(&messages);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_no_system_passthrough() {
        let messages = vec![
            msg(Role::User, "Hello"),
            msg(Role::Assistant, "Hi there"),
            msg(Role::User, "How are you?"),
        ];
        let result = convert_chat_messages(&messages);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_mixed_conversation() {
        // Simulates LibreChat-style: system + prior messages + empty assistant
        let messages = vec![
            msg(Role::System, "You are a helpful assistant"),
            msg(Role::User, "What is Rust?"),
            msg(Role::Assistant, ""),
            msg(Role::Assistant, "Rust is a systems programming language."),
            msg(Role::User, "Tell me more"),
        ];
        let result = convert_chat_messages(&messages);
        assert_eq!(result.len(), 3); // user, assistant(non-empty), user
    }

    #[test]
    fn test_unknown_roles_skipped() {
        let messages = vec![
            msg(Role::Unknown, "some tool output"),
            msg(Role::User, "Hello"),
        ];
        let result = convert_chat_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_empty_input() {
        let result = convert_chat_messages(&[]);
        assert!(result.is_empty());
    }

    // --- streaming / response builder tests ---

    use crate::streaming::{
        ChatCompletionChunkDelta, MessageRole, ToolResultMode, truncate_result,
    };
    use crate::streaming::{StreamOutcome, UsageInfo};

    #[test]
    fn test_truncate_result_no_truncation_when_zero() {
        let text = "Hello, world!";
        assert_eq!(truncate_result(text, 0), "Hello, world!");
    }

    #[test]
    fn test_truncate_result_no_truncation_when_under_limit() {
        let text = "Hello, world!";
        assert_eq!(truncate_result(text, 100), "Hello, world!");
    }

    #[test]
    fn test_truncate_result_truncates_when_over_limit() {
        let text = "Hello, world!";
        let result = truncate_result(text, 5);
        assert_eq!(result, "Hello... [truncated]");
    }

    #[test]
    fn test_truncate_result_at_exact_limit() {
        let text = "Hello";
        assert_eq!(truncate_result(text, 5), "Hello");
    }

    #[test]
    fn test_tool_result_mode_default() {
        let mode = ToolResultMode::default();
        assert_eq!(mode, ToolResultMode::None);
    }

    #[test]
    fn test_message_role_serialization() {
        // Test that MessageRole::Assistant serializes to "assistant"
        let delta = ChatCompletionChunkDelta {
            role: Some(MessageRole::Assistant),
            content: Some("Hello".to_string()),
            tool_calls: None,
        };

        let json = serde_json::to_string(&delta).unwrap();
        assert!(json.contains(r#""role":"assistant""#));
        assert!(json.contains(r#""content":"Hello""#));
    }

    #[test]
    fn test_message_role_omitted_when_none() {
        // Test that role field is omitted when None
        let delta = ChatCompletionChunkDelta {
            role: None,
            content: Some("World".to_string()),
            tool_calls: None,
        };

        let json = serde_json::to_string(&delta).unwrap();
        assert!(!json.contains("role"));
        assert!(json.contains(r#""content":"World""#));
    }

    fn make_response_ctx() -> ResponseContext {
        ResponseContext {
            completion_id: "chatcmpl-test-123".to_string(),
            model_str: "openai/gpt-4".to_string(),
            created_timestamp: 1700000000,
            chat_session_id: "cs_test".to_string(),
        }
    }

    #[test]
    fn test_build_json_response_normal_stop() {
        let collected = CollectedResult {
            outcome: StreamOutcome {
                content: "Hello!".to_string(),
                usage: Some(UsageInfo {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                }),
            },
            usage_state: aura::UsageState::new(),
        };

        let resp = build_json_response(make_response_ctx(), None, collected);
        assert_eq!(resp.status(), 200);

        let body = actix_web::body::to_bytes(resp.into_body());
        let body = futures_util::FutureExt::now_or_never(body)
            .unwrap()
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["choices"][0]["finish_reason"], "stop");
        assert_eq!(json["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(json["usage"]["prompt_tokens"], 10);
        assert_eq!(json["usage"]["completion_tokens"], 5);
        assert_eq!(json["usage"]["total_tokens"], 15);
        assert_eq!(json["id"], "chatcmpl-test-123");
        assert_eq!(json["model"], "openai/gpt-4");
        assert_eq!(json["metadata"]["chat_session_id"], "cs_test");
    }

    #[test]
    fn test_build_json_response_finish_reason_length() {
        let collected = CollectedResult {
            outcome: StreamOutcome {
                content: "truncated response".to_string(),
                usage: Some(UsageInfo {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                }),
            },
            usage_state: aura::UsageState::new(),
        };

        // max_tokens = 50, completion_tokens = 50 -> finish_reason = "length"
        let resp = build_json_response(make_response_ctx(), Some(50), collected);
        let body =
            futures_util::FutureExt::now_or_never(actix_web::body::to_bytes(resp.into_body()))
                .unwrap()
                .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn test_build_json_response_usage_fallback_to_usage_state() {
        // When outcome.usage is None, build_json_response falls back to usage_state.
        // A fresh UsageState returns (0, 0, 0) from get_final_usage().
        let collected = CollectedResult {
            outcome: StreamOutcome {
                content: "response".to_string(),
                usage: None,
            },
            usage_state: aura::UsageState::new(),
        };

        let resp = build_json_response(make_response_ctx(), None, collected);
        let body =
            futures_util::FutureExt::now_or_never(actix_web::body::to_bytes(resp.into_body()))
                .unwrap()
                .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Falls back to UsageState::new() which returns all zeros
        assert_eq!(json["usage"]["prompt_tokens"], 0);
        assert_eq!(json["usage"]["completion_tokens"], 0);
        assert_eq!(json["usage"]["total_tokens"], 0);
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
    }
}
