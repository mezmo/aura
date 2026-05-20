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
use aura::{StreamItem, StreamedAssistantContent};
use aura_config::RigBuilder;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{Level, event};

use crate::{a2a::SharedTaskStore, types::AppState};

const PLAIN_TEXT: &str = "text/plain";

pub struct AuraAgentExecutor {
    app_state: Arc<AppState>,
    task_store: SharedTaskStore,
    task_cancel_tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl AuraAgentExecutor {
    pub fn new(app_state: Arc<AppState>, task_store: SharedTaskStore) -> Self {
        Self {
            app_state,
            task_store,
            task_cancel_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn resolve_config(&self) -> Option<aura_config::Config> {
        if let Some(ref name) = self.app_state.default_agent
            && let Some(agent_config) = self
                .app_state
                .configs
                .iter()
                .find(|c| c.agent.alias.as_deref().unwrap_or(&c.agent.name) == name)
                .cloned()
        {
            return Some(agent_config);
        }
        self.app_state.configs.first().cloned()
    }

    pub fn build_agent_card(&self) -> AgentCard {
        let config = self.resolve_config();
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
                AgentInterface::new("/a2a/v1", TRANSPORT_PROTOCOL_HTTP_JSON),
                AgentInterface::new("/a2a/v1/rpc", TRANSPORT_PROTOCOL_JSONRPC),
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
        let config = self.resolve_config();
        let stream_shutdown_token = self.app_state.stream_shutdown_token.clone();
        let task_cancel_tokens = self.task_cancel_tokens.clone();
        let active_request_tracker = self.app_state.active_requests.clone();
        let task_store = self.task_store.clone();

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

            let config = config.ok_or_else(|| A2AError::invalid_params("no agent configuration available"))?;

            let session_id = Some(context_id.clone());
            let builder = RigBuilder::new(config);
            let agent = match builder
                .build_streaming_agent_with_headers(Some(&req_headers), session_id, None)
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    yield Ok(fail_status(&task_id, &context_id, &e.to_string()));
                    return;
                }
            };

            let request_id = format!("a2a_{}", task_id);

            // build any history for this context that can be used in further aura reasoning
            let history = get_history_for_context(task_store.clone(), &request_id, &context_id, &task_id).await?;

            let cancel_token = stream_shutdown_token.child_token();
            {
                // keep track of this cancel token in case the a2a framework
                // receives a cancel call and we need to propagate
                let mut cancel_map = task_cancel_tokens.lock().await;
                cancel_map.insert(task_id.clone(), cancel_token.clone());
            }

            let mut stream = match agent.stream(&text, history, cancel_token, &request_id).await {
                Ok(s) => s,
                Err(e) => {
                    // processing never started, so we can remove the cancel token from tracking
                    let mut cancel_map = task_cancel_tokens.lock().await;
                    cancel_map.remove(&task_id);

                    yield Ok(fail_status(&task_id, &context_id, &e.to_string()));
                    return;
                }
            };

            // let the main state machinery know we've got a job in progress
            active_request_tracker.increment();

            let mut first_chunk = true;
            let mut success = true; // assume everything is successful

            let mut reasoning_num = 0;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                        event!(Level::DEBUG, request_id, t, "stream content received");
                        let artifact = Artifact {
                            artifact_id: "response".to_owned(),
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
                            append: Some(!first_chunk),
                            last_chunk: Some(false),
                            metadata: None,
                        }));
                        first_chunk = false;
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall(tc))) => {
                        event!(Level::DEBUG, request_id, tool_name = tc.name.as_str(), "tool call received");
                        let artifact = Artifact {
                            artifact_id: format!("tool_call_{}", tc.id),
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
                            append: Some(!first_chunk),
                            last_chunk: Some(false),
                            metadata: None,
                        }));
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(r))) => {
                        event!(Level::DEBUG, request_id, reasoning = r, "reasoning received");
                        reasoning_num += 1;
                        let artifact = Artifact {
                            artifact_id: format!("reasoning_{}", reasoning_num),
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
                            append: Some(!first_chunk),
                            last_chunk: Some(false),
                            metadata: None,
                        }));
                    }
                    Ok(StreamItem::ScratchpadUsage { agent_id, tokens_intercepted, tokens_extracted }) => {
                        event!(Level::DEBUG, request_id, "scratchpad usage");
                        let artifact = Artifact {
                            artifact_id: format!("scratchpad_{}", agent_id),
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
                            append: Some(!first_chunk),
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
                        let artifact = Artifact {
                            artifact_id: "final".into(),
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
                            append: Some(!first_chunk),
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

            // processing done, remove the token from the map, no longer need to track
            {
                let mut cancel_map = task_cancel_tokens.lock().await;
                cancel_map.remove(&task_id);
            }

            // let the main state machinery know we've completed this particular job
            active_request_tracker.decrement();

            // if successful (hit either finalizer), mark the task as such.
            if success {
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
        let task_cancel_tokens = self.task_cancel_tokens.clone();
        let active_request_tracker = self.app_state.active_requests.clone();

        Box::pin(futures_util::stream::once(async move {
            // a2a got a cancel call, so make sure rig cancels as well
            // and remove the token from tracking
            let mut cancel_map = task_cancel_tokens.lock().await;
            if let Some(token) = cancel_map.remove(&task_id) {
                token.cancel();
            }

            // let the main state machinery know this task is effectively complete
            active_request_tracker.decrement();

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
