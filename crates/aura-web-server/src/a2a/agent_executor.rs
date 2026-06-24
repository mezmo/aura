use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;

use a2a::{
    A2AError, AgentCapabilities, AgentCard, AgentInterface, AgentSkill, Artifact, ListTasksRequest,
    Message, Part, PartContent, Role, StreamResponse, TRANSPORT_PROTOCOL_HTTP_JSON,
    TRANSPORT_PROTOCOL_JSONRPC, TaskArtifactUpdateEvent, TaskState, TaskStatus,
    TaskStatusUpdateEvent, VERSION,
};
use a2a_server::{AgentExecutor, ExecutorContext, TaskStore};
use aura::RigBuilder;
use aura::{RequestCancellation, StreamItem, StreamedAssistantContent, StreamingAgent};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{Level, event};

use crate::{
    a2a::SharedTaskStore,
    types::{ActiveRequestGuard, AppState},
};

const PLAIN_TEXT: &str = "text/plain";

pub struct AuraAgentExecutor {
    app_state: Arc<AppState>,
    task_store: SharedTaskStore,
    task_cancel_state: Arc<Mutex<HashMap<String, TaskCancelEntry>>>,
}

struct TaskCancelEntry {
    token: CancellationToken,
    agent: Arc<dyn StreamingAgent>,
    request_id: String,
}

impl AuraAgentExecutor {
    pub fn new(app_state: Arc<AppState>, task_store: SharedTaskStore) -> Self {
        Self {
            app_state,
            task_store,
            task_cancel_state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn resolve_config(&self, requested_model: Option<&str>) -> Option<aura_config::Config> {
        let configs = &self.app_state.configs;
        // Single-config: always use it, ignore any requested_model (mirrors chat completions passthrough).
        if configs.len() == 1 {
            return configs.first().cloned();
        }
        // multi-config. If a specific model requested, try to find it
        // otherwise go with the config default.
        let name = requested_model.or(self.app_state.default_agent.as_deref())?;
        configs
            .iter()
            .find(|c| c.agent.alias.as_deref().unwrap_or(&c.agent.name) == name)
            .cloned()
    }

    /// Build the A2A agent card.
    ///
    /// `base_url` is the externally-reachable origin (e.g. `https://aura.example.com`)
    /// used to make the interface endpoints absolute. The A2A spec requires absolute
    /// interface URLs — clients pass them straight to their HTTP layer, which rejects
    /// relative paths.
    pub fn build_agent_card(&self, base_url: &str) -> AgentCard {
        let base = base_url.trim_end_matches('/');
        let config = self.resolve_config(None);
        let name = config
            .as_ref()
            .map(|c| c.agent.name.as_str())
            .unwrap_or("Aura Agent")
            .to_string();
        let description = {
            let raw = config
                .as_ref()
                .map(|c| c.agent.system_prompt.as_str())
                .unwrap_or("Aura AI agent");
            if raw.chars().count() > 200 {
                let truncated: String = raw.chars().take(200).collect();
                format!("{}...", truncated)
            } else {
                raw.to_string()
            }
        };

        AgentCard {
            name,
            description,
            version: VERSION.to_string(),
            provider: None,
            documentation_url: None,
            icon_url: None,
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extensions: None,
                extended_agent_card: None,
            },
            supported_interfaces: vec![
                AgentInterface::new(format!("{base}/a2a/v1"), TRANSPORT_PROTOCOL_HTTP_JSON),
                AgentInterface::new(format!("{base}/a2a/v1/rpc"), TRANSPORT_PROTOCOL_JSONRPC),
            ],
            skills: vec![AgentSkill {
                id: "chat".to_owned(),
                name: "Chat".to_owned(),
                description: "Send a message and receive a task. Use the task to track the progression of the AI to completion.".to_owned(),
                tags: vec![],
                examples: None,
                input_modes: Some(vec![PLAIN_TEXT.into()]),
                output_modes: Some(vec![PLAIN_TEXT.into()]),
                security_requirements: None,
            }],
            default_input_modes: vec![PLAIN_TEXT.into()],
            default_output_modes: vec![PLAIN_TEXT.into()],
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        }
    }
}

impl AgentExecutor for AuraAgentExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let model_requested_model = ctx
            .service_params
            .get("x-aura-model")
            .and_then(|v| v.first())
            .cloned();
        let config = match self.resolve_config(model_requested_model.as_deref()) {
            Some(c) => c,
            None => {
                let msg = match model_requested_model.as_deref() {
                    Some(name) => format!("no agent configuration found for model '{name}'"),
                    None => "no agent configuration available".to_string(),
                };
                return Box::pin(futures_util::stream::once(async move {
                    Err::<StreamResponse, A2AError>(A2AError::invalid_params(msg))
                }));
            }
        };
        let stream_shutdown_token = self.app_state.stream_shutdown_token.clone();
        let task_cancel_state = self.task_cancel_state.clone();
        let active_request_tracker = self.app_state.active_requests.clone();
        let task_store = self.task_store.clone();
        let pending_approvals = self.app_state.pending_approvals.clone();
        let mut append_tracker: HashMap<(String, String, String), bool> = HashMap::new();

        Box::pin(async_stream::stream! {
            let task_id = ctx.task_id.clone();
            let context_id = ctx.context_id.clone();

            let text = ctx.message
                .ok_or_else(|| A2AError::invalid_params("Message has no parts to use as a command."))
                .and_then(|msg| extract_text(msg.parts))?;

            let req_headers: HashMap<String, String> = ctx
                .service_params
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| (k.clone(), v.join(", ")))
                .collect();

            yield Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                metadata: None,
            }));

            let request_id = format!("a2a_{}", task_id);
            let session_id = Some(context_id.clone());
            let builder = RigBuilder::new(config, pending_approvals);
            let agent = match builder
                .build_streaming_agent_with_headers(
                    Some(&req_headers),
                    session_id,
                    None,
                    Some(request_id.clone()),
                )
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    yield Ok(fail_status(&task_id, &context_id, &e.to_string()));
                    return;
                }
            };

            // build any history for this context that can be used in further aura reasoning
            let history = get_history_for_context(task_store.clone(), &request_id, &context_id, &task_id).await?;

            let cancel_token = stream_shutdown_token.child_token();
            // Register with the global cancellation registry for parity with the OpenAI handler
            // and to let any future code address this request by id.
            RequestCancellation::register(request_id.clone());
            {
                let mut cancel_map = task_cancel_state.lock().await;
                cancel_map.insert(task_id.clone(), TaskCancelEntry {
                    token: cancel_token.clone(),
                    agent: agent.clone(),
                    request_id: request_id.clone(),
                });
            }

            let mut stream = match agent.stream(&text, history, cancel_token.clone(), &request_id).await {
                Ok(s) => s,
                Err(e) => {
                    {
                        let mut cancel_map = task_cancel_state.lock().await;
                        cancel_map.remove(&task_id);
                    }
                    RequestCancellation::unregister(&request_id);

                    yield Ok(fail_status(&task_id, &context_id, &e.to_string()));
                    return;
                }
            };

            // RAII guard: drop on any generator exit (loop break, early return, panic,
            // consumer drop) produces exactly one decrement. Replaces the manual
            // increment/decrement pair that previously raced with cancel() — a fast
            // cancel before this line, or a cancel mid-loop followed by natural
            // loop-exit cleanup, could double-decrement and wrap the counter.
            let _request_guard = ActiveRequestGuard::new(active_request_tracker);

            let mut success = true; // assume everything is successful

            let mut reasoning_num = 0;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => break,
                    next = stream.next() => next,
                };
                let Some(item) = next else { break };
                match item {
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                        event!(Level::DEBUG, request_id, t, "stream content received");

                        const ARTIFACT_ID: &str = "response";
                        let append = append_tracker.entry((task_id.clone(), context_id.clone(), ARTIFACT_ID.to_owned()))
                            .and_modify(|e| *e = true)
                            .or_insert(false);

                        event!(Level::DEBUG, request_id, "response returned and should be appended: {}", *append);

                        let artifact = Artifact {
                            artifact_id: ARTIFACT_ID.to_owned(),
                            name: Some("Response".to_owned()),
                            description: None,
                            parts: vec![Part::text(t)],
                            metadata: None,
                            extensions: None,
                        };
                        yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                            task_id: task_id.clone(),
                            context_id: context_id.clone(),
                            artifact,
                            append: Some(*append),
                            last_chunk: Some(false),
                            metadata: None,
                        }));
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall(tc))) => {
                        event!(Level::DEBUG, request_id, tool_name = tc.name.as_str(), "tool call received");

                        let artifact_id: String = format!("tool_call_{}", tc.id);
                        let append = append_tracker.entry((task_id.clone(), context_id.clone(), artifact_id.to_owned()))
                            .and_modify(|e| *e = true)
                            .or_insert(false);

                        let artifact = Artifact {
                            artifact_id: artifact_id.to_owned(),
                            name: Some(tc.name.clone()),
                            description: None,
                            parts: vec![Part::text(format!("Tool was called: {}", tc.name.clone()))],
                            metadata: Some(HashMap::from([
                                ("type".into(), Value::String("tool_call".into())),
                                ("id".into(), Value::String(tc.id.clone())),
                                ("name".into(), Value::String(tc.name.clone())),
                                ("arguments".into(), Value::String(tc.arguments.clone())),
                            ])),
                            extensions: None,
                        };
                        yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                            task_id: task_id.clone(),
                            context_id: context_id.clone(),
                            artifact,
                            append: Some(*append),
                            last_chunk: Some(false),
                            metadata: None,
                        }));
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(r))) => {
                        event!(Level::DEBUG, request_id, reasoning = r, "reasoning received");
                        reasoning_num += 1;

                        let artifact_id: String = format!("reasoning_{}", reasoning_num);
                        let append = append_tracker.entry((task_id.clone(), context_id.clone(), artifact_id.to_owned()))
                            .and_modify(|e| *e = true)
                            .or_insert(false);

                        let artifact = Artifact {
                            artifact_id: artifact_id.to_owned(),
                            name: Some("reasoning".into()),
                            description: None,
                            parts: vec![Part::text(r)],
                            metadata: None,
                            extensions: None,
                        };
                        yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                            task_id: task_id.clone(),
                            context_id: context_id.clone(),
                            artifact,
                            append: Some(*append),
                            last_chunk: Some(false),
                            metadata: None,
                        }));
                    }
                    Ok(StreamItem::ScratchpadUsage { agent_id, tokens_intercepted, tokens_extracted }) => {
                        event!(Level::DEBUG, request_id, "scratchpad usage");

                        let artifact_id: String = format!("scratchpad_{}", agent_id);
                        let append = append_tracker.entry((task_id.clone(), context_id.clone(), artifact_id.to_owned()))
                            .and_modify(|e| *e = true)
                            .or_insert(false);

                        let artifact = Artifact {
                            artifact_id: artifact_id.to_owned(),
                            name: Some("Scratchpad Usage".into()),
                            description: None,
                            parts: vec![],
                            metadata: Some(HashMap::from([
                                ("tokens_intercepted".into(), Value::Number(tokens_intercepted.into())),
                                ("tokens_extracted".into(), Value::Number(tokens_extracted.into()))
                            ])),
                            extensions: None,
                        };
                        yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                            task_id: task_id.clone(),
                            context_id: context_id.clone(),
                            artifact,
                            append: Some(*append),
                            last_chunk: Some(false),
                            metadata: None,
                        }));
                    }
                    Ok(StreamItem::TurnUsage(_)) => {
                        event!(Level::DEBUG, request_id, "turn usage");
                    }
                    Ok(StreamItem::OrchestratorEvent(_)) => {
                        event!(Level::DEBUG, request_id, "orchestration event");
                    }
                    Ok(StreamItem::McpStatus(_)) => {
                        event!(Level::DEBUG, request_id, "mcp status");
                    }
                    Ok(StreamItem::StreamUserItem(_)) => {
                        event!(Level::DEBUG, request_id, "stream user item");
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCallDelta { .. })) => {
                        event!(Level::DEBUG, request_id, "stream assistant item");
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ReasoningDelta { .. })) => {
                        event!(Level::DEBUG, request_id, "reasoning delta");
                    }
                    Ok(StreamItem::Final(final_info)) => {
                        const ARTIFACT_ID: &str = "final";
                        let append = append_tracker.entry((task_id.clone(), context_id.clone(), ARTIFACT_ID.to_owned()))
                            .and_modify(|e| *e = true)
                            .or_insert(false);

                        let artifact = Artifact {
                            artifact_id: ARTIFACT_ID.to_owned(),
                            name: Some("Final Info".into()),
                            description: None,
                            parts: vec![Part::text(final_info.content)],
                            metadata: Some(HashMap::from([
                                ("input_tokens".into(), Value::Number(final_info.usage.input_tokens.into())),
                                ("output_tokens".into(), Value::Number(final_info.usage.output_tokens.into())),
                                ("total_tokens".into(), Value::Number(final_info.usage.total_tokens.into()))
                            ])),
                            extensions: None,
                        };
                        yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                            task_id: task_id.clone(),
                            context_id: context_id.clone(),
                            artifact,
                            append: Some(*append),
                            last_chunk: Some(true),
                            metadata: None,
                        }));
                        break; // done processing
                    }
                    Ok(StreamItem::FinalMarker) => {
                        event!(Level::DEBUG, request_id, "stream final marker");
                        break; // done processing
                    }
                    Err(e) => {
                        event!(Level::ERROR, request_id, task_id, error = e.to_string(), "stream error");
                        yield Ok(fail_status(&task_id, &context_id, &e.to_string()));
                        success = false;
                        break; // done processing
                    }
                }
            }

            // If cancel_token fired but our entry is still in the map, the cancel came
            // from the parent stream_shutdown_token (server shutdown), not from our
            // cancel() hook — cancel() removes its entry before firing the token.
            // In that case the executor has to drive MCP cleanup itself and emit a
            // terminal Canceled status (the OpenAI handler does the equivalent in its
            // Shutdown post-loop arm).
            let entry_still_present = {
                let mut cancel_map = task_cancel_state.lock().await;
                cancel_map.remove(&task_id).is_some()
            };
            let shutdown_initiated_cancel = cancel_token.is_cancelled() && entry_still_present;
            RequestCancellation::unregister(&request_id);

            if shutdown_initiated_cancel {
                agent
                    .cancel_and_close_mcp(&request_id, "server shutdown")
                    .await;

                yield Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: task_id.clone(),
                    context_id: context_id.clone(),
                    status: TaskStatus {
                        state: TaskState::Canceled,
                        message: None,
                        timestamp: Some(chrono::Utc::now()),
                    },
                    metadata: None,
                }));
            }

            // _request_guard drops at end of generator scope → exactly one decrement.

            // Skip Completed if cancel() or the shutdown path already emitted Canceled —
            // yielding here would clobber it.
            if success && !cancel_token.is_cancelled() {
                yield Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id,
                    context_id,
                    status: TaskStatus {
                        state: TaskState::Completed,
                        message: None,
                        timestamp: Some(chrono::Utc::now()),
                    },
                    metadata: None,
                }));
            }
        })
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();
        let task_cancel_state = self.task_cancel_state.clone();

        Box::pin(futures_util::stream::once(async move {
            let entry = {
                let mut cancel_map = task_cancel_state.lock().await;
                cancel_map.remove(&task_id)
            };

            // Token-cancel wakes execute()'s select! → loop breaks → generator drops
            // → ActiveRequestGuard drops → exactly one decrement. cancel() never
            // touches the tracker directly, which closes the prior double-decrement
            // / underflow race.
            if let Some(entry) = entry {
                // Send notifications/cancelled to in-flight MCP tool calls. No-op in
                // orchestration mode (workers manage their own MCP cancellation).
                entry
                    .agent
                    .cancel_and_close_mcp(&entry.request_id, "A2A cancelTask")
                    .await;
                entry.token.cancel();
                RequestCancellation::unregister(&entry.request_id);
            }

            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                context_id,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                metadata: None,
            }))
        }))
    }
}

fn extract_text(parts: Vec<Part>) -> Result<String, A2AError> {
    let mut strings: Vec<&str> = Vec::new();

    for part_content in parts.iter() {
        if let PartContent::Text(t) = &part_content.content {
            strings.push(t)
        } else {
            return Err(A2AError::invalid_params(
                "All message parts are expected to be text for this implementation; file and data parts are not supported.",
            ));
        }
    }

    if strings.is_empty() {
        return Err(A2AError::invalid_params(
            "Message has no parts to use as a command.",
        ));
    }

    Ok(strings.join("\n"))
}

fn fail_status(task_id: &str, context_id: &str, error_msg: &str) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state: TaskState::Failed,
            message: Some(Message::new(
                Role::Agent,
                vec![Part::text(error_msg.to_string())],
            )),
            timestamp: Some(chrono::Utc::now()),
        },
        metadata: Some(HashMap::from([(
            "error".into(),
            Value::String(error_msg.to_string()),
        )])),
    })
}

async fn get_history_for_context(
    task_store: SharedTaskStore,
    request_id: &str,
    context_id: &str,
    task_id: &String,
) -> Result<Vec<aura::Message>, A2AError> {
    let mut task_history: Vec<a2a::Task> = Vec::new();

    let mut next_page_token: Option<String> = None;
    loop {
        event!(
            Level::DEBUG,
            request_id,
            context_id,
            "processing history for context"
        );

        let loop_history = task_store
            .list(&ListTasksRequest {
                context_id: Some(context_id.into()),
                history_length: None, // get all history in one shot
                include_artifacts: Some(false),
                page_size: Some(1000), // override the default of 50
                page_token: next_page_token,
                status: None,
                status_timestamp_after: None,
                tenant: None,
            })
            .await?;

        event!(
            Level::DEBUG,
            request_id,
            context_id,
            "found {} tasks, continue token '{}'",
            loop_history.tasks.len(),
            loop_history.next_page_token
        );
        task_history.extend(loop_history.tasks);

        if !loop_history.next_page_token.is_empty() {
            next_page_token = Some(loop_history.next_page_token);
        } else {
            break;
        }
    }

    // keep all other tasks and sort by descending
    task_history.retain(|t| t.id != *task_id && t.history.clone().is_some_and(|h| !h.is_empty()));
    task_history.sort_by_key(|t| Reverse(t.status.timestamp));

    let chat_history: Vec<aura::Message> = task_history
        .into_iter()
        .flat_map(|t| t.history.unwrap())
        .filter_map(|m| convert_a2a_msg_to_aura(&m))
        .collect();

    event!(Level::DEBUG, request_id, context_id, chat_history = ?chat_history, "determined this following history to use");
    Ok(chat_history)
}

fn convert_a2a_msg_to_aura(msg: &a2a::Message) -> Option<aura::Message> {
    let mut text_parts: Vec<&str> = Vec::new();

    for part_content in msg.parts.iter() {
        if let PartContent::Text(t) = &part_content.content {
            text_parts.push(t)
        } else {
            // skipping for now until we determine how to handle other types
        }
    }

    if text_parts.is_empty() {
        None
    } else {
        let text = text_parts.join("\n");
        match msg.role {
            a2a::Role::User => Some(aura::Message::user(&text)),
            a2a::Role::Agent => Some(aura::Message::assistant(&text)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::SharedTaskStore;
    use crate::streaming::ToolResultMode;
    use crate::types::{ActiveRequestTracker, AppState};

    fn make_executor(
        configs: Vec<aura_config::Config>,
        default_agent: Option<&str>,
    ) -> AuraAgentExecutor {
        let app_state = Arc::new(AppState {
            configs: Arc::new(configs),
            default_agent: default_agent.map(str::to_owned),
            tool_result_mode: ToolResultMode::default(),
            tool_result_max_length: 0,
            streaming_buffer_size: 0,
            aura_custom_events: false,
            aura_emit_reasoning: false,
            streaming_timeout_secs: 0,
            first_chunk_timeout_secs: 0,
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            stream_shutdown_token: tokio_util::sync::CancellationToken::new(),
            active_requests: Arc::new(ActiveRequestTracker::default()),
            additional_tools: Arc::new(Vec::new),
            pending_approvals: aura::hitl::PendingApprovals::new(),
        });
        AuraAgentExecutor::new(app_state, SharedTaskStore::default())
    }

    fn make_config(name: &str, alias: Option<&str>) -> aura_config::Config {
        aura_config::Config {
            memory_dir: None,
            mcp: None,
            vector_stores: vec![],
            tools: None,
            orchestration: None,
            hitl: None,
            agent: aura_config::AgentConfig {
                name: name.to_owned(),
                alias: alias.map(str::to_owned),
                ..aura_config::AgentConfig::default()
            },
        }
    }

    #[test]
    fn empty_configs_returns_none() {
        let ex = make_executor(vec![], None);
        assert!(ex.resolve_config(None).is_none());
    }

    #[test]
    fn single_config_no_requested_model_returns_it() {
        let ex = make_executor(vec![make_config("A", None)], None);
        let result = ex.resolve_config(None);
        assert_eq!(result.map(|c| c.agent.name), Some("A".to_owned()));
    }

    #[test]
    fn single_config_requested_model_ignored() {
        // Branch A: single-config always wins, the requested_model is irrelevant
        let ex = make_executor(vec![make_config("A", None)], None);
        let result = ex.resolve_config(Some("B"));
        assert_eq!(result.map(|c| c.agent.name), Some("A".to_owned()));
    }

    #[test]
    fn multi_config_no_requested_model_no_default_returns_none() {
        let ex = make_executor(vec![make_config("A", None), make_config("B", None)], None);
        assert!(ex.resolve_config(None).is_none());
    }

    #[test]
    fn multi_config_default_agent_matches_by_name() {
        let ex = make_executor(
            vec![make_config("A", None), make_config("B", None)],
            Some("B"),
        );
        let result = ex.resolve_config(None);
        assert_eq!(result.map(|c| c.agent.name), Some("B".to_owned()));
    }

    #[test]
    fn multi_config_default_agent_matches_by_alias() {
        let ex = make_executor(
            vec![make_config("A", None), make_config("B", Some("b-alias"))],
            Some("b-alias"),
        );
        let result = ex.resolve_config(None);
        assert_eq!(result.map(|c| c.agent.name), Some("B".to_owned()));
    }

    #[test]
    fn multi_config_model_requested_model_matches_by_name() {
        let ex = make_executor(vec![make_config("A", None), make_config("B", None)], None);
        let result = ex.resolve_config(Some("B"));
        assert_eq!(result.map(|c| c.agent.name), Some("B".to_owned()));
    }

    #[test]
    fn multi_config_model_requested_model_matches_by_alias() {
        let ex = make_executor(
            vec![make_config("A", None), make_config("B", Some("b-alias"))],
            None,
        );
        let result = ex.resolve_config(Some("b-alias"));
        assert_eq!(result.map(|c| c.agent.name), Some("B".to_owned()));
    }

    #[test]
    fn multi_config_no_match_returns_none() {
        let ex = make_executor(vec![make_config("A", None), make_config("B", None)], None);
        assert!(ex.resolve_config(Some("C")).is_none());
    }

    #[test]
    fn multi_config_requested_model_overrides_default_agent() {
        let ex = make_executor(
            vec![make_config("A", None), make_config("B", None)],
            Some("A"),
        );
        let result = ex.resolve_config(Some("B"));
        assert_eq!(result.map(|c| c.agent.name), Some("B".to_owned()));
    }
}
