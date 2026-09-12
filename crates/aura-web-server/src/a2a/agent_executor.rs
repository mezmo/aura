use std::collections::HashMap;
use std::sync::Arc;
// Under `--cfg aura_loom` the cancel map's mutex is loom's, so its model can
// explore every interleaving of the code that locks it. The cfg is ours rather
// than loom's usual `loom`, which tokio also reacts to.
#[cfg(aura_loom)]
use loom::sync::{Mutex, MutexGuard};
#[cfg(not(aura_loom))]
use std::sync::{Mutex, MutexGuard};

use a2a::{
    A2AError, AgentCapabilities, AgentCard, AgentInterface, AgentSkill, Artifact, ListTasksRequest,
    Message, Part, PartContent, Role, StreamResponse, TRANSPORT_PROTOCOL_HTTP_JSON,
    TRANSPORT_PROTOCOL_JSONRPC, TaskArtifactUpdateEvent, TaskState, TaskStatus,
    TaskStatusUpdateEvent, VERSION,
};
use a2a_server::{AgentExecutor, ExecutorContext, TaskStore};
use aura::RigBuilder;
use aura::{StreamItem, StreamedAssistantContent, StreamingAgent};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{Level, event};

use crate::{
    a2a::{LEGACY_PROTOCOL_VERSION, SharedTaskStore},
    types::{ActiveRequestGuard, AppState},
};

const PLAIN_TEXT: &str = "text/plain";

/// Artifact id of the assistant's reply text, one part per streamed chunk.
const RESPONSE_ARTIFACT_ID: &str = "response";

/// Artifact id of the assistant's complete reply and the turn's token usage.
const FINAL_ARTIFACT_ID: &str = "final";

pub struct AuraAgentExecutor {
    app_state: Arc<AppState>,
    task_store: SharedTaskStore,
    task_cancel_state: Arc<TaskCancelState>,
}

/// A live execution's cancellation handle and what a cancel needs to clean up.
struct TaskCancelEntry {
    token: CancellationToken,
    agent: Option<Arc<dyn StreamingAgent>>,
    request_id: String,
}

/// Whether `cancel()` got to a task before the code asking.
#[derive(PartialEq, Eq)]
enum CancelRaced {
    Yes,
    No,
}

/// Hands the built agent to the task's entry, reporting whether a cancel beat it.
///
/// One lock acquisition, because a cancel landing between the write and the
/// answer would take the entry with the agent set, close its MCP calls, and
/// leave the caller to close them again.
fn claim_agent(
    state: &TaskCancelState,
    task_id: &str,
    agent: &Arc<dyn StreamingAgent>,
) -> CancelRaced {
    match lock_cancel_state(state).get_mut(task_id) {
        Some(entry) => {
            entry.agent = Some(Arc::clone(agent));
            CancelRaced::No
        }
        // `cancel()` takes the entry before firing the token, so a missing
        // entry means it has already run.
        None => CancelRaced::Yes,
    }
}

/// The terminal status this run emits once its loop ends, if any.
///
/// Taking the entry claims the terminal status. `cancel()` takes it before
/// answering `Canceled`, so a run that finds it gone answers nothing, however
/// its loop ended. A run that still holds it answers `Canceled` when shutdown
/// stopped it, and `Completed` when it succeeded.
fn post_loop_status(
    success: bool,
    stopped_by_cancel: bool,
    shutdown: &CancellationToken,
    state: &TaskCancelState,
    task_id: &str,
) -> Option<TaskState> {
    // Taking rather than reading keeps a cancel from landing between the two
    // and leaving both sides to answer.
    lock_cancel_state(state).remove(task_id)?;
    if stopped_by_cancel {
        return shutdown.is_cancelled().then_some(TaskState::Canceled);
    }
    success.then_some(TaskState::Completed)
}

/// The live executions, keyed by task id.
type TaskCancelState = Mutex<HashMap<String, TaskCancelEntry>>;

/// Lock the cancel map, taking a poisoned lock's contents rather than
/// panicking: nothing awaits while the map is held, so a panicking holder
/// cannot have left it half-updated.
fn lock_cancel_state(state: &TaskCancelState) -> MutexGuard<'_, HashMap<String, TaskCancelEntry>> {
    state.lock().unwrap_or_else(|poisoned| {
        event!(Level::ERROR, "task cancel state lock poisoned, recovering");
        poisoned.into_inner()
    })
}

/// Owns one execution's cancel-map entry.
struct TaskCancelGuard {
    state: Arc<TaskCancelState>,
    task_id: String,
}

impl Drop for TaskCancelGuard {
    /// Releases it on any generator exit — loop break, early return, panic,
    /// or a consumer that stops polling after a terminal event and drops the
    /// generator where it stands, which cleanup at the end of the body would
    /// never reach. Releasing what another path already took is a no-op, so
    /// this composes with the explicit removals.
    fn drop(&mut self) {
        lock_cancel_state(&self.state).remove(&self.task_id);
    }
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
                // v0.3 clients address an agent by a single base URL, so the
                // legacy binding is advertised at the root (see `a2a::legacy`).
                AgentInterface {
                    url: format!("{base}/"),
                    protocol_binding: TRANSPORT_PROTOCOL_JSONRPC.to_string(),
                    protocol_version: LEGACY_PROTOCOL_VERSION.to_string(),
                    tenant: None,
                },
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
        let hitl_hmac = self.app_state.hitl_webhook_hmac.clone();
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

            // Registered before the agent build and history fetch, both of which
            // await, so a cancelTask during those has a token to cancel. Its
            // guard comes first, because those paths can return early.
            let cancel_token = stream_shutdown_token.child_token();
            let _cancel_guard = TaskCancelGuard {
                state: task_cancel_state.clone(),
                task_id: task_id.clone(),
            };
            lock_cancel_state(&task_cancel_state).insert(
                task_id.clone(),
                TaskCancelEntry {
                    token: cancel_token.clone(),
                    agent: None,
                    request_id: request_id.clone(),
                },
            );

            let session_id = Some(context_id.clone());
            let builder = RigBuilder::new(config, pending_approvals).with_hitl_hmac(hitl_hmac);
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

            // The agent exists now, so a cancel from here can close its MCP calls.
            // A missing entry means `cancel()` already ran while the build was in
            // flight, with no agent to close — so the run does it instead.
            if claim_agent(&task_cancel_state, &task_id, &agent) == CancelRaced::Yes {
                agent
                    .cancel_and_close_mcp(&request_id, "A2A cancelTask during build")
                    .await;
            }

            // A2A tasks have their own lifetime, so the run is unbounded here and
            // ends on cancelTask or shutdown.
            let run = agent
                .stream(
                    &text,
                    history,
                    aura::streaming::RunOptions::default()
                        .cancelled_by(&cancel_token),
                    &request_id,
                )
                .await;
            let mut stream = run.into_events();

            // RAII guard: drop on any generator exit (loop break, early return, panic,
            // consumer drop) produces exactly one decrement. Replaces the manual
            // increment/decrement pair that previously raced with cancel() — a fast
            // cancel before this line, or a cancel mid-loop followed by natural
            // loop-exit cleanup, could double-decrement and wrap the counter.
            let _request_guard = ActiveRequestGuard::new(active_request_tracker);

            let mut success = true; // assume everything is successful

            let mut reasoning_num = 0;
            // Read after the loop, because a token cancelled once the stream has
            // already ended says nothing about how this run finished.
            let mut stopped_by_cancel = false;
            loop {
                let next = tokio::select! {
                    biased;
                    // A child of the shutdown token, so this covers both.
                    () = cancel_token.cancelled() => {
                        stopped_by_cancel = true;
                        break;
                    }
                    next = stream.next() => next,
                };
                let Some(item) = next else { break };
                match item {
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                        event!(Level::DEBUG, request_id, t, "stream content received");

                        let append = append_tracker.entry((task_id.clone(), context_id.clone(), RESPONSE_ARTIFACT_ID.to_owned()))
                            .and_modify(|e| *e = true)
                            .or_insert(false);

                        event!(Level::DEBUG, request_id, "response returned and should be appended: {}", *append);

                        let artifact = Artifact {
                            artifact_id: RESPONSE_ARTIFACT_ID.to_owned(),
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
                    Ok(StreamItem::TurnUsage(..)) => {
                        event!(Level::DEBUG, request_id, "turn usage");
                    }
                    Ok(StreamItem::ContextUsage { .. }) => {
                        event!(Level::DEBUG, request_id, "context usage");
                    }
                    Ok(StreamItem::AgentEvent(_)) => {
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
                        let append = append_tracker.entry((task_id.clone(), context_id.clone(), FINAL_ARTIFACT_ID.to_owned()))
                            .and_modify(|e| *e = true)
                            .or_insert(false);

                        let artifact = Artifact {
                            artifact_id: FINAL_ARTIFACT_ID.to_owned(),
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

            let status = post_loop_status(
                success,
                stopped_by_cancel,
                &stream_shutdown_token,
                &task_cancel_state,
                &task_id,
            );

            // Shutdown is the one cancel the executor cleans up after itself;
            // `cancel()` drives its own.
            if status == Some(TaskState::Canceled) {
                agent
                    .cancel_and_close_mcp(&request_id, "server shutdown")
                    .await;
            }

            // _request_guard drops at end of generator scope → exactly one decrement.

            if let Some(state) = status {
                yield Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id,
                    context_id,
                    status: TaskStatus {
                        state,
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
            let entry = lock_cancel_state(&task_cancel_state).remove(&task_id);

            // Token-cancel wakes execute()'s select! → loop breaks → generator drops
            // → ActiveRequestGuard drops → exactly one decrement. cancel() never
            // touches the tracker directly, which closes the prior double-decrement
            // / underflow race.
            if let Some(entry) = entry {
                // Send notifications/cancelled to in-flight MCP tool calls. No-op in
                // orchestration mode (workers manage their own MCP cancellation).
                // A run cancelled before its agent was built has no MCP calls yet.
                if let Some(agent) = &entry.agent {
                    agent
                        .cancel_and_close_mcp(&entry.request_id, "A2A cancelTask")
                        .await;
                }
                entry.token.cancel();
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

pub(super) fn fail_status(task_id: &str, context_id: &str, error_msg: &str) -> StreamResponse {
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
    task_id: &str,
) -> Result<Vec<aura::Message>, A2AError> {
    let mut tasks: Vec<a2a::Task> = Vec::new();

    let mut next_page_token: Option<String> = None;
    loop {
        event!(
            Level::DEBUG,
            request_id,
            context_id,
            "processing history for context"
        );

        let page = task_store
            .list(&ListTasksRequest {
                context_id: Some(context_id.into()),
                history_length: None, // get all history in one shot
                // The assistant turn is rebuilt from the artifacts (see `task_turns`).
                include_artifacts: Some(true),
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
            page.tasks.len(),
            page.next_page_token
        );
        tasks.extend(page.tasks);

        if !page.next_page_token.is_empty() {
            next_page_token = Some(page.next_page_token);
        } else {
            break;
        }
    }

    // Skip the task being executed: its only recorded turn is the prompt this
    // call is about to send. Everything else replays oldest exchange first.
    tasks.retain(|t| t.id != task_id);
    tasks.sort_by_key(|t| t.status.timestamp);

    let chat_history: Vec<aura::Message> = tasks.iter().flat_map(task_turns).collect();

    event!(Level::DEBUG, request_id, context_id, chat_history = ?chat_history, "determined this following history to use");
    Ok(chat_history)
}

/// One task's conversation turns, oldest first.
///
/// The request handler records only the prompt in `Task::history`; the agent's
/// reply is streamed out as artifacts and never written back. So the assistant
/// turn is rebuilt from the text the client saw — the streamed
/// [`RESPONSE_ARTIFACT_ID`] chunks, or [`FINAL_ARTIFACT_ID`] for a task whose
/// reply only arrived as a final response. A task that already carries an
/// agent message in `history` is taken at its word instead.
fn task_turns(task: &a2a::Task) -> Vec<aura::Message> {
    let recorded = task.history.as_deref().unwrap_or_default();
    let mut turns: Vec<aura::Message> = recorded
        .iter()
        .filter_map(convert_a2a_msg_to_aura)
        .collect();

    if !recorded.iter().any(|m| matches!(m.role, Role::Agent))
        && let Some(answer) = assistant_answer(task)
    {
        turns.push(aura::Message::assistant(&answer));
    }

    turns
}

/// The assistant text a task produced, or `None` if it produced none.
fn assistant_answer(task: &a2a::Task) -> Option<String> {
    let artifacts = task.artifacts.as_deref()?;

    // Parts of one artifact are fragments of a single message — streamed mid-word,
    // so they concatenate with no separator.
    let joined = |artifact_id: &str| {
        artifacts
            .iter()
            .filter(|a| a.artifact_id == artifact_id)
            .flat_map(|a| a.parts.iter())
            .filter_map(|p| match &p.content {
                PartContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<String>()
    };

    [RESPONSE_ARTIFACT_ID, FINAL_ARTIFACT_ID]
        .into_iter()
        .map(joined)
        .find(|text| !text.trim().is_empty())
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
    use aura_test_utils::mock_agent::MockAgent;

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
            debug_provider_errors: false,
            streaming_timeout_secs: 0,
            first_chunk_timeout_secs: 0,
            stream_inactivity_timeout_secs: 0,
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            stream_shutdown_token: tokio_util::sync::CancellationToken::new(),
            active_requests: Arc::new(ActiveRequestTracker::default()),
            additional_tools: Arc::new(Vec::new),
            pending_approvals: aura::hitl::PendingApprovals::new(),
            hitl_webhook_hmac: None,
            session_store: Arc::new(crate::session_store::InMemorySessionStore::new()),
        });
        AuraAgentExecutor::new(app_state, SharedTaskStore::default())
    }

    fn make_config(name: &str, alias: Option<&str>) -> aura_config::Config {
        aura_config::Config {
            memory_dir: None,
            identity_header: None,
            mcp: None,
            vector_stores: vec![],
            tools: None,
            orchestration: None,
            hitl: None,
            governance: None,
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

    /// A consumer that stops polling after a terminal event drops the
    /// execution generator mid-body, so the entry has to be released by the
    /// guard rather than by cleanup the generator never reaches.
    #[test]
    fn dropping_the_cancel_guard_releases_the_entry() {
        let task_id = format!("t_{}", uuid::Uuid::new_v4());
        let request_id = format!("a2a_{task_id}");
        let state: Arc<TaskCancelState> = Arc::new(Mutex::new(HashMap::new()));

        let guard = TaskCancelGuard {
            state: state.clone(),
            task_id: task_id.clone(),
        };
        lock_cancel_state(&state).insert(
            task_id.clone(),
            TaskCancelEntry {
                token: CancellationToken::new(),
                agent: Some(Arc::new(MockAgent::pending())),
                request_id: request_id.clone(),
            },
        );
        assert!(lock_cancel_state(&state).contains_key(&task_id));

        drop(guard);

        assert!(!lock_cancel_state(&state).contains_key(&task_id));
    }

    /// Every way a run's loop can end, and the status each one answers with.
    #[test]
    fn the_post_loop_status_follows_how_the_run_ended() {
        // Deciding takes the entry, so each case starts from its own.
        let registered = || {
            let task_id = format!("t_{}", uuid::Uuid::new_v4());
            let state: Arc<TaskCancelState> = Arc::new(Mutex::new(HashMap::new()));
            lock_cancel_state(&state).insert(
                task_id.clone(),
                TaskCancelEntry {
                    token: CancellationToken::new(),
                    agent: None,
                    request_id: format!("a2a_{task_id}"),
                },
            );
            (state, task_id)
        };
        let down = || {
            let token = CancellationToken::new();
            token.cancel();
            token
        };

        let (state, task_id) = registered();
        assert_eq!(
            post_loop_status(true, false, &CancellationToken::new(), &state, &task_id),
            Some(TaskState::Completed),
            "a run that reached the end of its stream completed"
        );

        let (state, task_id) = registered();
        assert_eq!(
            post_loop_status(true, false, &down(), &state, &task_id),
            Some(TaskState::Completed),
            "shutdown firing after a run finished does not make it cancelled"
        );

        let (state, task_id) = registered();
        assert_eq!(
            post_loop_status(false, false, &CancellationToken::new(), &state, &task_id),
            None,
            "a run that failed has already answered"
        );

        let (state, task_id) = registered();
        assert_eq!(
            post_loop_status(true, true, &down(), &state, &task_id),
            Some(TaskState::Canceled),
            "a run that stopped on its token with its entry intact was stopped \
             by shutdown"
        );

        // `cancel()` takes the entry, then fires the token.
        let (state, task_id) = registered();
        lock_cancel_state(&state).remove(&task_id);
        assert_eq!(
            post_loop_status(true, true, &down(), &state, &task_id),
            None,
            "cancel() answers for itself"
        );
    }

    /// `cancel()` can take the entry while the run is parked yielding its final
    /// artifact, after which the loop ends on its own. The cancel has already
    /// answered `Canceled`, so the run must not also answer `Completed`.
    #[test]
    fn a_run_does_not_complete_after_a_cancel_took_its_entry() {
        let state: Arc<TaskCancelState> = Arc::new(Mutex::new(HashMap::new()));
        lock_cancel_state(&state).insert(
            "t".to_string(),
            TaskCancelEntry {
                token: CancellationToken::new(),
                agent: None,
                request_id: "a2a_t".to_string(),
            },
        );
        lock_cancel_state(&state).remove("t");

        assert_eq!(
            post_loop_status(true, false, &CancellationToken::new(), &state, "t"),
            None
        );
    }

    /// Covers the contract in both directions. `loom_tests` covers the
    /// interleaving, which this cannot reach.
    #[test]
    fn claiming_the_agent_reports_a_cancel_that_already_ran() {
        let task_id = format!("t_{}", uuid::Uuid::new_v4());
        let request_id = format!("a2a_{task_id}");
        let state: Arc<TaskCancelState> = Arc::new(Mutex::new(HashMap::new()));
        let agent: Arc<dyn StreamingAgent> = Arc::new(MockAgent::pending());

        lock_cancel_state(&state).insert(
            task_id.clone(),
            TaskCancelEntry {
                token: CancellationToken::new(),
                agent: None,
                request_id,
            },
        );

        assert!(claim_agent(&state, &task_id, &agent) == CancelRaced::No);
        assert!(
            lock_cancel_state(&state)
                .get(&task_id)
                .is_some_and(|entry| entry.agent.is_some()),
            "the entry carries the agent a later cancel closes"
        );

        // What `cancel()` does: take the entry, leaving nothing to claim.
        lock_cancel_state(&state).remove(&task_id);
        assert!(claim_agent(&state, &task_id, &agent) == CancelRaced::Yes);
    }

    /// The guard is created before the entry, because the agent build and the
    /// history fetch can both return early and would otherwise leave it behind.
    #[test]
    fn an_early_return_before_the_run_releases_the_entry() {
        let task_id = format!("t_{}", uuid::Uuid::new_v4());
        let request_id = format!("a2a_{task_id}");
        let state: Arc<TaskCancelState> = Arc::new(Mutex::new(HashMap::new()));

        {
            let _guard = TaskCancelGuard {
                state: state.clone(),
                task_id: task_id.clone(),
            };
            lock_cancel_state(&state).insert(
                task_id.clone(),
                TaskCancelEntry {
                    token: CancellationToken::new(),
                    agent: None,
                    request_id: request_id.clone(),
                },
            );
            // The build fails here and the generator returns.
        }

        assert!(
            !lock_cancel_state(&state).contains_key(&task_id),
            "a run that never started leaves no entry"
        );
    }

    /// `cancel()` takes the entry before the generator unwinds, so the guard
    /// has to tolerate finding it already released.
    #[test]
    fn dropping_the_cancel_guard_after_an_explicit_cleanup_is_a_noop() {
        let task_id = format!("t_{}", uuid::Uuid::new_v4());
        let state: Arc<TaskCancelState> = Arc::new(Mutex::new(HashMap::new()));

        let guard = TaskCancelGuard {
            state: state.clone(),
            task_id: task_id.clone(),
        };
        lock_cancel_state(&state).remove(&task_id);

        drop(guard);

        assert!(!lock_cancel_state(&state).contains_key(&task_id));
    }

    fn at(secs: i64) -> Option<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::from_timestamp(secs, 0)
    }

    fn artifact(artifact_id: &str, chunks: &[&str]) -> Artifact {
        Artifact {
            artifact_id: artifact_id.to_owned(),
            name: None,
            description: None,
            parts: chunks.iter().map(|c| Part::text(*c)).collect(),
            metadata: None,
            extensions: None,
        }
    }

    /// A completed task shaped like one the executor produces: the prompt in
    /// `history`, the reply only in artifacts.
    fn completed_task(id: &str, prompt: &str, artifacts: Vec<Artifact>, secs: i64) -> a2a::Task {
        a2a::Task {
            id: id.to_owned(),
            context_id: "ctx".to_owned(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: at(secs),
            },
            artifacts: Some(artifacts),
            history: Some(vec![Message::new(Role::User, vec![Part::text(prompt)])]),
            metadata: None,
        }
    }

    /// The turn as `(role, text)`, so assertions read as the conversation does.
    fn turn(message: &aura::Message) -> (&'static str, String) {
        match message {
            aura::Message::User { content } => (
                "user",
                content
                    .iter()
                    .filter_map(|c| match c {
                        aura::UserContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            aura::Message::Assistant { content, .. } => (
                "assistant",
                content
                    .iter()
                    .filter_map(|c| match c {
                        aura::AssistantContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
        }
    }

    fn turns(messages: &[aura::Message]) -> Vec<(&'static str, String)> {
        messages.iter().map(turn).collect()
    }

    async fn history_of(tasks: Vec<a2a::Task>, executing: &str) -> Vec<aura::Message> {
        let store = SharedTaskStore::default();
        for task in tasks {
            store.create(task).await.expect("task created");
        }
        get_history_for_context(store, "req", "ctx", executing)
            .await
            .expect("history built")
    }

    #[tokio::test]
    async fn history_carries_the_assistant_turn_from_streamed_artifacts() {
        let history = history_of(
            vec![completed_task(
                "t1",
                "how many sentences?",
                vec![
                    artifact(RESPONSE_ARTIFACT_ID, &["Three ", "sen", "tences."]),
                    artifact(FINAL_ARTIFACT_ID, &["Three sentences."]),
                ],
                10,
            )],
            "t2",
        )
        .await;

        assert_eq!(
            turns(&history),
            vec![
                ("user", "how many sentences?".to_owned()),
                ("assistant", "Three sentences.".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn history_falls_back_to_the_final_artifact() {
        let history = history_of(
            vec![completed_task(
                "t1",
                "hi",
                vec![
                    artifact(RESPONSE_ARTIFACT_ID, &[]),
                    artifact(FINAL_ARTIFACT_ID, &["hello"]),
                ],
                10,
            )],
            "t2",
        )
        .await;

        assert_eq!(
            turns(&history),
            vec![("user", "hi".to_owned()), ("assistant", "hello".to_owned()),]
        );
    }

    #[tokio::test]
    async fn history_ignores_non_response_artifacts() {
        let history = history_of(
            vec![completed_task(
                "t1",
                "call the tool",
                vec![
                    artifact("tool_call_1", &["Tool was called: mock_tool"]),
                    artifact("reasoning_1", &["thinking about it"]),
                ],
                10,
            )],
            "t2",
        )
        .await;

        assert_eq!(turns(&history), vec![("user", "call the tool".to_owned())]);
    }

    #[tokio::test]
    async fn history_replays_exchanges_oldest_first() {
        let history = history_of(
            vec![
                completed_task("t2", "second", vec![artifact("final", &["two"])], 20),
                completed_task("t1", "first", vec![artifact("final", &["one"])], 10),
            ],
            "t3",
        )
        .await;

        assert_eq!(
            turns(&history),
            vec![
                ("user", "first".to_owned()),
                ("assistant", "one".to_owned()),
                ("user", "second".to_owned()),
                ("assistant", "two".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn history_skips_the_executing_task() {
        let history = history_of(
            vec![
                completed_task("t1", "first", vec![artifact("final", &["one"])], 10),
                completed_task("t2", "second", vec![], 20),
            ],
            "t2",
        )
        .await;

        assert_eq!(
            turns(&history),
            vec![
                ("user", "first".to_owned()),
                ("assistant", "one".to_owned()),
            ]
        );
    }

    /// A store that records the agent turn in `history` must not have it
    /// duplicated from the artifacts.
    #[tokio::test]
    async fn history_prefers_a_recorded_agent_message() {
        let mut task = completed_task("t1", "hi", vec![artifact("final", &["hello"])], 10);
        task.history
            .as_mut()
            .unwrap()
            .push(Message::new(Role::Agent, vec![Part::text("hello")]));

        let history = history_of(vec![task], "t2").await;

        assert_eq!(
            turns(&history),
            vec![("user", "hi".to_owned()), ("assistant", "hello".to_owned()),]
        );
    }
}

/// Exhaustive interleaving checks for the task-cancel map.
///
/// Run with `RUSTFLAGS="--cfg aura_loom" cargo test -p aura-web-server loom_`.
/// A stress test cannot reach these windows: the one that matters is a single
/// mutex release and reacquire wide.
#[cfg(all(test, aura_loom))]
mod loom_tests {
    use super::*;
    use aura_test_utils::mock_agent::MockAgent;

    /// A cancel and the post-loop check race over a run shutdown stopped.
    /// Exactly one of them takes the entry, and with it the job of closing the
    /// agent.
    #[test]
    fn loom_a_cancel_racing_the_shutdown_check_yields_one_closer() {
        loom::model(|| {
            let shutdown = CancellationToken::new();
            shutdown.cancel();

            let state: loom::sync::Arc<TaskCancelState> =
                loom::sync::Arc::new(Mutex::new(HashMap::new()));
            lock_cancel_state(&state).insert(
                "t1".to_string(),
                TaskCancelEntry {
                    token: CancellationToken::new(),
                    agent: None,
                    request_id: "a2a_t1".to_string(),
                },
            );

            let closes = loom::sync::Arc::new(loom::sync::atomic::AtomicUsize::new(0));

            let post_loop = {
                let (state, closes, shutdown) = (state.clone(), closes.clone(), shutdown.clone());
                loom::thread::spawn(move || {
                    if post_loop_status(true, true, &shutdown, &state, "t1")
                        == Some(TaskState::Canceled)
                    {
                        closes.fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
                    }
                })
            };

            let canceller = {
                let (state, closes) = (state.clone(), closes.clone());
                loom::thread::spawn(move || {
                    if lock_cancel_state(&state).remove("t1").is_some() {
                        closes.fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
                    }
                })
            };

            post_loop.join().unwrap();
            canceller.join().unwrap();

            assert_eq!(
                closes.load(loom::sync::atomic::Ordering::SeqCst),
                1,
                "exactly one side takes responsibility for the terminal status"
            );
        });
    }

    /// A run that reached the end of its stream races a `cancel()` for its
    /// entry. Exactly one of them takes it, and the run answers `Completed`
    /// only when it did. This counts claims rather than statuses, since
    /// `cancel()` answers `Canceled` even when it finds no entry.
    #[test]
    fn loom_a_cancel_racing_a_finished_run_yields_one_claimant() {
        loom::model(|| {
            let shutdown = CancellationToken::new();

            let state: loom::sync::Arc<TaskCancelState> =
                loom::sync::Arc::new(Mutex::new(HashMap::new()));
            lock_cancel_state(&state).insert(
                "t1".to_string(),
                TaskCancelEntry {
                    token: CancellationToken::new(),
                    agent: None,
                    request_id: "a2a_t1".to_string(),
                },
            );

            let claims = loom::sync::Arc::new(loom::sync::atomic::AtomicUsize::new(0));

            let post_loop = {
                let (state, claims, shutdown) = (state.clone(), claims.clone(), shutdown.clone());
                loom::thread::spawn(move || {
                    if post_loop_status(true, false, &shutdown, &state, "t1")
                        == Some(TaskState::Completed)
                    {
                        claims.fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
                    }
                })
            };

            let canceller = {
                let (state, claims) = (state.clone(), claims.clone());
                loom::thread::spawn(move || {
                    if lock_cancel_state(&state).remove("t1").is_some() {
                        claims.fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
                    }
                })
            };

            post_loop.join().unwrap();
            canceller.join().unwrap();

            assert_eq!(
                claims.load(loom::sync::atomic::Ordering::SeqCst),
                1,
                "exactly one side claims the terminal status"
            );
        });
    }

    /// A cancel and a claim race. Exactly one of them takes responsibility for
    /// closing the agent — the claim when it finds the entry already gone, the
    /// cancel when it finds the entry carrying an agent. The close itself is
    /// async and outside the model; this is the branch that decides who runs it.
    #[test]
    fn loom_a_cancel_racing_a_claim_yields_one_closer() {
        loom::model(|| {
            let state: loom::sync::Arc<TaskCancelState> =
                loom::sync::Arc::new(Mutex::new(HashMap::new()));
            lock_cancel_state(&state).insert(
                "t1".to_string(),
                TaskCancelEntry {
                    token: CancellationToken::new(),
                    agent: None,
                    request_id: "a2a_t1".to_string(),
                },
            );

            let closes = loom::sync::Arc::new(loom::sync::atomic::AtomicUsize::new(0));

            let claimer = {
                let (state, closes) = (state.clone(), closes.clone());
                loom::thread::spawn(move || {
                    let agent: Arc<dyn StreamingAgent> = Arc::new(MockAgent::pending());
                    if claim_agent(&state, "t1", &agent) == CancelRaced::Yes {
                        closes.fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
                    }
                })
            };

            let canceller = {
                let (state, closes) = (state.clone(), closes.clone());
                loom::thread::spawn(move || {
                    // What `cancel()` does: take the entry, close what it holds.
                    let entry = lock_cancel_state(&state).remove("t1");
                    if entry.is_some_and(|entry| entry.agent.is_some()) {
                        closes.fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
                    }
                })
            };

            claimer.join().unwrap();
            canceller.join().unwrap();

            assert_eq!(
                closes.load(loom::sync::atomic::Ordering::SeqCst),
                1,
                "exactly one side takes responsibility for closing the agent"
            );
        });
    }
}
