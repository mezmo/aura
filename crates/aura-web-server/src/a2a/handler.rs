use a2a_rs_core::{
    AgentCapabilities, AgentCard, AgentInterface, AgentSkill, Artifact, Message, PROTOCOL_VERSION,
    Part, SendMessageResponse, StreamResponse, Task, TaskArtifactUpdateEvent, TaskState,
    TaskStatus, TaskStatusUpdateEvent, now_iso8601,
};
use a2a_rs_server::{AuthContext, HandlerError, HandlerResult, MessageHandler, TaskStore};
use aura::{StreamItem, StreamedAssistantContent};
use aura_config::RigBuilder;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;
use tracing::{Level, event};
use uuid::Uuid;

use crate::types::AppState;

const PLAIN_TEXT: &str = "text/plain";

pub struct AuraMessageHandler {
    // the app state holds the loaded agent configurations
    // and other global state that may be needed for handling messages
    app_state: Arc<AppState>,
    // broadcast channel for sending streaming responses and task updates back to the caller
    event_tx: Arc<OnceLock<broadcast::Sender<StreamResponse>>>,
    // in-memory task store for managing task state and artifacts
    // in-future, can be swapped out for a distributed store such as redis if we choose
    task_store: Arc<OnceLock<TaskStore>>,
}

impl AuraMessageHandler {
    pub fn new(
        app_state: Arc<AppState>,
        event_tx: Arc<OnceLock<broadcast::Sender<StreamResponse>>>,
        task_store: Arc<OnceLock<TaskStore>>,
    ) -> Self {
        Self {
            app_state,
            event_tx,
            task_store,
        }
    }

    fn resolve_config(&self) -> Option<aura_config::Config> {
        if let Some(ref name) = self.app_state.default_agent {
            if let Some(agent_config) = self
                .app_state
                .configs
                .iter()
                .find(|c| c.agent.alias.as_deref().unwrap_or(&c.agent.name) == name)
                .cloned()
            {
                return Some(agent_config);
            }
        }

        self.app_state.configs.first().cloned()
    }
}

#[async_trait::async_trait]
impl MessageHandler for AuraMessageHandler {
    async fn handle_message(
        &self,
        message: Message,
        auth: Option<AuthContext>,
    ) -> HandlerResult<SendMessageResponse> {
        // input validation
        let non_text: Vec<_> = message
            .parts
            .iter()
            .filter(|p| !matches!(p, Part::Text { .. }))
            .collect();

        if !non_text.is_empty() {
            return Err(HandlerError::InvalidInput(
                "All message parts are expected to be text for this implementation; file and data parts are not supported".into(),
            ));
        }

        // take all the message parts and concatenate the text parts together to form the full message text
        let text: String = message
            .parts
            .iter()
            .map(|p| p.as_text().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");

        let task_id = Uuid::new_v4().to_string();
        event!(Level::DEBUG, task_id, text, "Invoking new chat via A2A");

        // resolve the aura config to use for this message
        let config = self
            .resolve_config()
            .ok_or_else(|| HandlerError::processing_failed("no agent configuration available"))?;

        let context_id = message
            .context_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let event_tx = self
            .event_tx
            .get()
            .ok_or_else(|| HandlerError::processing_failed("streaming not initialized"))?
            .clone();

        // task is generated and ran in the background for the caller to subscribe
        // to the events and get the response as it is generated
        let task_store = self
            .task_store
            .get()
            .ok_or_else(|| HandlerError::processing_failed("task store not initialized"))?
            .clone();

        let task = Task {
            kind: "task".to_owned(),
            id: task_id.clone(),
            context_id: context_id.clone(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: Some(now_iso8601()),
            },
            history: Some(vec![message]),
            artifacts: None,
            metadata: None,
        };

        task_store.insert(task.clone()).await;

        // grab all the request headers to provide to the streaming
        // agent so they can be used for tool calls to other services that require auth, etc
        let req_headers: HashMap<String, String> = auth
            .as_ref()
            .and_then(|a| a.metadata.as_ref())
            .and_then(|m| m.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let stream_shutdown_token = self.app_state.stream_shutdown_token.clone();

        // run the task in the background and stream updates to the task store as they are generated
        tokio::spawn(async move {
            let session_id = Some(task_id.clone());

            let builder = RigBuilder::new(config);
            let agent = match builder
                .build_streaming_agent_with_headers(Some(&req_headers), session_id)
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    fail_task(
                        &task_store,
                        &task_id,
                        &context_id,
                        &event_tx,
                        &e.to_string(),
                    )
                    .await;
                    return;
                }
            };

            let request_id = format!("a2a_{}", task_id);
            let cancel_token = stream_shutdown_token.child_token();

            let mut stream = match agent.stream(&text, vec![], cancel_token, &request_id).await {
                Ok(s) => {
                    event!(Level::DEBUG, request_id, "Stream generated successfully");
                    s
                }
                Err(e) => {
                    fail_task(
                        &task_store,
                        &task_id,
                        &context_id,
                        &event_tx,
                        &e.to_string(),
                    )
                    .await;
                    return;
                }
            };

            let mut chunk_index = 0u32;

            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                        event!(Level::DEBUG, request_id, t, "Stream content received");

                        let artifact = Artifact {
                            artifact_id: "response".to_owned(),
                            name: Some("Response".to_owned()),
                            description: None,
                            parts: vec![Part::text(t)],
                            metadata: None,
                            extensions: vec![],
                        };
                        let append = chunk_index > 0;
                        task_store
                            .update(&task_id, |task| {
                                if append {
                                    if let Some(artifacts) = &mut task.artifacts {
                                        if let Some(existing) = artifacts
                                            .iter_mut()
                                            .find(|a| a.artifact_id == artifact.artifact_id)
                                        {
                                            existing.parts.extend(artifact.parts.iter().cloned());
                                        } else {
                                            artifacts.push(artifact.clone());
                                        }
                                    }
                                } else {
                                    task.artifacts = Some(vec![artifact.clone()]);
                                }
                            })
                            .await;
                        let _aur = event_tx.send(StreamResponse::ArtifactUpdate(
                            TaskArtifactUpdateEvent {
                                kind: "artifact-update".to_owned(),
                                task_id: task_id.clone(),
                                context_id: context_id.clone(),
                                artifact,
                                append: Some(append),
                                last_chunk: Some(false),
                                metadata: None,
                            },
                        ));
                        chunk_index += 1;
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall(tc))) => {
                        event!(
                            Level::DEBUG,
                            request_id,
                            tool_name = tc.name.as_str(),
                            "Tool call received"
                        );
                        let artifact = Artifact {
                            artifact_id: format!("tool_call_{}", tc.id),
                            name: Some(tc.name.clone()),
                            description: None,
                            parts: vec![],
                            metadata: Some(serde_json::json!({
                                "type": "tool_call",
                                "id": tc.id,
                                "name": tc.name,
                                "arguments": tc.arguments,
                            })),
                            extensions: vec![],
                        };
                        task_store
                            .update(&task_id, |task| {
                                if let Some(artifacts) = &mut task.artifacts {
                                    artifacts.push(artifact.clone());
                                } else {
                                    task.artifacts = Some(vec![artifact.clone()]);
                                }
                            })
                            .await;
                        let _aur = event_tx.send(StreamResponse::ArtifactUpdate(
                            TaskArtifactUpdateEvent {
                                kind: "artifact-update".to_owned(),
                                task_id: task_id.clone(),
                                context_id: context_id.clone(),
                                artifact,
                                append: Some(true),
                                last_chunk: Some(false),
                                metadata: None,
                            },
                        ));
                    }
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(r))) => {
                        event!(
                            Level::DEBUG,
                            request_id,
                            reasoning = r,
                            "Reasoning received"
                        );
                        let artifact = Artifact {
                            artifact_id: "reasoning".to_owned(),
                            name: Some("Reasoning".to_owned()),
                            description: None,
                            parts: vec![],
                            metadata: Some(serde_json::json!({
                                "type": "reasoning",
                                "content": r,
                            })),
                            extensions: vec![],
                        };
                        task_store
                            .update(&task_id, |task| {
                                if let Some(artifacts) = &mut task.artifacts {
                                    artifacts.push(artifact.clone());
                                } else {
                                    task.artifacts = Some(vec![artifact.clone()]);
                                }
                            })
                            .await;
                        let _aur = event_tx.send(StreamResponse::ArtifactUpdate(
                            TaskArtifactUpdateEvent {
                                kind: "artifact-update".to_owned(),
                                task_id: task_id.clone(),
                                context_id: context_id.clone(),
                                artifact,
                                append: Some(true),
                                last_chunk: Some(false),
                                metadata: None,
                            },
                        ));
                    }
                    Ok(StreamItem::Final(_)) | Ok(StreamItem::FinalMarker) => {
                        event!(Level::DEBUG, request_id, "Stream finalized");
                        break;
                    }
                    Ok(StreamItem::TurnUsage(_)) | Ok(_) => {
                        event!(Level::DEBUG, request_id, "Turn usage or ok received");
                    }
                    Err(e) => {
                        event!(
                            Level::ERROR,
                            request_id,
                            task_id,
                            error = e.to_string(),
                            "Failed task for request"
                        );
                        fail_task(
                            &task_store,
                            &task_id,
                            &context_id,
                            &event_tx,
                            &e.to_string(),
                        )
                        .await;
                        return;
                    }
                }
            }

            complete_task(&task_store, &task_id, &context_id, &event_tx).await;
        });

        Ok(SendMessageResponse::Task(task))
    }

    fn agent_card(&self, _base_url: &str) -> AgentCard {
        let first = self.app_state.configs.first();
        let name = first
            .map(|c| c.agent.name.as_str())
            .unwrap_or("Aura Agent")
            .to_string();
        let description = first
            .map(|c| c.agent.system_prompt.as_str())
            .unwrap_or("Aura AI agent")
            .chars()
            .take(200)
            .collect::<String>();

        AgentCard {
            name,
            description,
            version: PROTOCOL_VERSION.to_string(),
            provider: None,
            documentation_url: None,
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extended_agent_card: Some(false),
                ..Default::default()
            },
            supported_interfaces: vec![
                AgentInterface {
                    url: "/a2a/v1/message:send".to_owned(),
                    protocol_binding: "http+json".to_owned(),
                    protocol_version: PROTOCOL_VERSION.to_owned(),
                    tenant: None,
                },
                AgentInterface {
                    url: "/a2a/v1/rpc".to_owned(),
                    protocol_binding: "jsonrpc".to_owned(),
                    protocol_version: PROTOCOL_VERSION.to_owned(),
                    tenant: None,
                },
            ],
            skills: vec![AgentSkill {
                id: "chat".to_owned(),
                name: "Chat".to_owned(),
                description: "Send a message and receive a response".to_owned(),
                tags: vec![],
                examples: vec![],
                input_modes: vec![PLAIN_TEXT.into()],
                output_modes: vec![PLAIN_TEXT.into()],
                security_requirements: vec![],
            }],
            security_schemes: Default::default(),
            security_requirements: vec![],
            default_input_modes: vec![PLAIN_TEXT.into()],
            default_output_modes: vec![PLAIN_TEXT.into()],
            signatures: vec![],
            icon_url: None,
        }
    }
}

async fn complete_task(
    task_store: &TaskStore,
    task_id: &str,
    context_id: &str,
    event_tx: &broadcast::Sender<StreamResponse>,
) {
    let timestamp = now_iso8601();
    task_store
        .update(task_id, |t| {
            t.status.state = TaskState::Completed;
            t.status.timestamp = Some(timestamp.clone());
        })
        .await;
    let _ = event_tx.send(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        kind: "status-update".to_owned(),
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state: TaskState::Completed,
            message: None,
            timestamp: Some(timestamp),
        },
        metadata: None,
    }));
}

async fn fail_task(
    task_store: &TaskStore,
    task_id: &str,
    context_id: &str,
    event_tx: &broadcast::Sender<StreamResponse>,
    error_msg: &str,
) {
    let timestamp = now_iso8601();
    task_store
        .update(task_id, |t| {
            t.status.state = TaskState::Failed;
            t.status.timestamp = Some(timestamp.clone());
        })
        .await;
    let _ = event_tx.send(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        kind: "status-update".to_owned(),
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state: TaskState::Failed,
            message: None,
            timestamp: Some(timestamp),
        },
        metadata: Some(serde_json::json!({ "error": error_msg })),
    }));
}
