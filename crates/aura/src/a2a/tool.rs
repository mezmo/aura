//! `ask_agent`: the rig tool through which the model calls a remote AURA
//! agent and waits for its answer.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{Artifact, Message, Part, PartContent, SendMessageResponse, Task, TaskState};
use aura_config::{A2aConfig, A2aRemoteConfig, ASK_AGENT_TOOL_NAME};
use rig::tool::{Tool as RigTool, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::Instrument;

use super::client::{A2aClient, A2aClientError};
use crate::request_cancellation::{RequestCancelToken, RequestCancellation};
use crate::tool_event_broker::{peek_tool_call_id, publish_tool_start};

/// How long a best-effort remote `CancelTask` may take once the local call
/// has already been given up on.
const ABANDON_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive transient `GetTask` failures tolerated before the call fails.
const MAX_TRANSIENT_POLL_FAILURES: u32 = 2;

/// Result prefix the tool-error detector recognizes; shared with MCP tool
/// errors so a failed remote task lights up the same span status.
const TOOL_ERROR_PREFIX: &str = "Tool returned an error: ";

/// One configured remote plus the polling budget for calls to it.
pub struct RemoteAgent {
    pub name: String,
    pub description: Option<String>,
    pub model: Option<String>,
    pub poll_interval: Duration,
    pub timeout: Duration,
    pub max_response_bytes: usize,
    client: A2aClient,
}

impl RemoteAgent {
    pub fn new(
        name: impl Into<String>,
        client: A2aClient,
        description: Option<String>,
        model: Option<String>,
        poll_interval: Duration,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Self {
        Self {
            name: name.into(),
            description,
            model,
            poll_interval,
            timeout,
            max_response_bytes,
            client,
        }
    }

    /// Build the remote named `name` from its `[a2a.remote.<name>]` entry,
    /// with `remote.headers` already carrying any `headers_from_request`
    /// overlay.
    pub fn from_config(
        name: &str,
        a2a: &A2aConfig,
        remote: &A2aRemoteConfig,
    ) -> Result<Self, A2aClientError> {
        let client = A2aClient::new(&remote.url, &remote.headers, &user_agent())?;
        Ok(Self::new(
            name,
            client,
            remote.description.clone(),
            remote.model.clone(),
            Duration::from_secs(a2a.poll_interval_secs(remote)),
            Duration::from_secs(a2a.timeout_secs(remote)),
            a2a.max_response_bytes,
        ))
    }

    pub fn endpoint(&self) -> &str {
        self.client.endpoint()
    }

    /// Send the prompt, poll until the remote task settles, and render the
    /// outcome. Gives up at `self.timeout` or when `cancel` fires, in both
    /// cases asking the remote to cancel the task it was left with.
    ///
    /// The send runs as its own task so that giving up while it is still in
    /// flight does not lose the task id the remote is about to return: the
    /// abandon path waits briefly for that reply and cancels the task it
    /// names. If this future is dropped instead (a caller's own timeout,
    /// for instance), [`OpenTask`] cancels the remote task from its `Drop`.
    async fn ask(
        &self,
        args: &AskAgentArgs,
        cancel: RequestCancelToken,
    ) -> Result<String, ToolError> {
        let open = Arc::new(OpenTask::new(&self.name, self.client.clone()));
        // Set once `run` has taken the send task's output; polling the
        // JoinHandle again after that panics.
        let send_consumed = AtomicBool::new(false);
        let mut send = tokio::spawn({
            let client = self.client.clone();
            let prompt = args.prompt.clone();
            let context_id = args.context_id.clone();
            let model = self.model.clone();
            let open = Arc::clone(&open);
            async move {
                let reply = client
                    .send_message(&prompt, context_id.as_deref(), model.as_deref())
                    .await;
                if let Ok(SendMessageResponse::Task(task)) = &reply {
                    open.record(&task.id);
                }
                reply
            }
        });

        let run = async {
            let sent = (&mut send).await;
            send_consumed.store(true, Ordering::SeqCst);
            let reply = sent
                .map_err(|e| {
                    call_error(format!(
                        "remote agent {:?}: send task failed: {e}",
                        self.name
                    ))
                })?
                .map_err(|e| self.remote_error(e))?;
            let mut task = match reply {
                SendMessageResponse::Message(message) => {
                    return Ok(AskAgentOutcome::from_message(
                        &self.name,
                        &message,
                        self.max_response_bytes,
                    ));
                }
                SendMessageResponse::Task(task) => task,
            };
            tracing::Span::current().record("a2a.task_id", task.id.as_str());
            let task_id = task.id.clone();

            let mut transient_failures = 0u32;
            while !is_settled(&task.status.state) {
                tokio::time::sleep(self.poll_interval).await;
                match self.client.get_task(&task_id).await {
                    Ok(fresh) if fresh.id == task_id => {
                        transient_failures = 0;
                        task = fresh;
                    }
                    Ok(fresh) => {
                        return Err(call_error(format!(
                            "remote agent {:?} answered GetTask for task {:?} instead of {task_id:?}",
                            self.name, fresh.id
                        )));
                    }
                    Err(e)
                        if e.is_transient() && transient_failures < MAX_TRANSIENT_POLL_FAILURES =>
                    {
                        transient_failures += 1;
                        tracing::warn!(
                            remote = %self.name,
                            task_id,
                            attempt = transient_failures,
                            error = %e,
                            "GetTask failed transiently; polling again"
                        );
                    }
                    Err(e) => return Err(self.remote_error(e)),
                }
            }
            Ok(AskAgentOutcome::from_task(
                &self.name,
                &task,
                self.max_response_bytes,
            ))
        };

        let result = tokio::select! {
            finished = tokio::time::timeout(self.timeout, run) => match finished {
                Ok(outcome) => outcome.map(|o| o.render()),
                Err(_elapsed) => Err(call_error(format!(
                    "remote agent {:?} did not finish within {}s",
                    self.name,
                    self.timeout.as_secs()
                ))),
            },
            _ = cancel.cancelled() => Err(call_error("Request cancelled")),
        };

        if result.is_err() {
            if open.task_id().is_none() && !send_consumed.load(Ordering::SeqCst) {
                tracing::debug!(
                    remote = %self.name,
                    "gave up while SendMessage was in flight; waiting for its task id"
                );
                // The send task records the id itself; waiting is only to
                // give it the chance before we look.
                let _ = tokio::time::timeout(ABANDON_TIMEOUT, &mut send).await;
            }
            if let Some(task_id) = open.take() {
                abandon(&self.name, &self.client, &task_id).await;
            }
        } else {
            // Settling suppresses the guard's Drop-cancel. That is only safe
            // on success: a direct message opened no task, and a settled or
            // input-waiting task stays on the remote on purpose. On an error
            // path the slot is empty or was already taken for the inline
            // abandon — unless the send is still in flight, in which case
            // the guard must cancel the task the send goes on to record.
            open.settle();
        }
        result
    }

    fn remote_error(&self, error: A2aClientError) -> ToolError {
        call_error(format!("remote agent {:?}: {error}", self.name))
    }
}

/// Best-effort remote cancel for a task this side has stopped waiting on.
async fn abandon(remote: &str, client: &A2aClient, task_id: &str) {
    match tokio::time::timeout(ABANDON_TIMEOUT, client.cancel_task(task_id)).await {
        Ok(Ok(_)) => tracing::info!(
            remote,
            task_id,
            "cancelled the remote task after giving up on it"
        ),
        Ok(Err(e)) => tracing::warn!(
            remote,
            task_id,
            error = %e,
            "remote task cancel failed after giving up on it"
        ),
        Err(_) => tracing::warn!(
            remote,
            task_id,
            "remote task cancel timed out after giving up on it"
        ),
    }
}

/// The remote task one `ask_agent` call has opened. Shared between the call
/// and its send task; whichever learns the task id records it. Dropping the
/// last handle with the task unsettled (the call's future was dropped
/// mid-poll, e.g. by an orchestration timeout) cancels the task on the
/// remote from a detached tokio task.
struct OpenTask {
    remote: String,
    client: A2aClient,
    task_id: Mutex<Option<String>>,
    settled: AtomicBool,
}

impl OpenTask {
    fn new(remote: &str, client: A2aClient) -> Self {
        Self {
            remote: remote.to_owned(),
            client,
            task_id: Mutex::new(None),
            settled: AtomicBool::new(false),
        }
    }

    fn record(&self, task_id: &str) {
        *self.task_id.lock().unwrap_or_else(|p| p.into_inner()) = Some(task_id.to_owned());
    }

    fn task_id(&self) -> Option<String> {
        self.task_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn take(&self) -> Option<String> {
        self.task_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// The call finished or explicitly abandoned the task; `Drop` has
    /// nothing left to do.
    fn settle(&self) {
        self.settled.store(true, Ordering::SeqCst);
    }
}

impl Drop for OpenTask {
    fn drop(&mut self) {
        if self.settled.load(Ordering::SeqCst) {
            return;
        }
        let Some(task_id) = self.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                remote = %self.remote,
                task_id,
                "call dropped with an open remote task and no runtime to cancel it from"
            );
            return;
        };
        tracing::warn!(
            remote = %self.remote,
            task_id,
            "call dropped with an open remote task; cancelling it in the background"
        );
        let remote = self.remote.clone();
        let client = self.client.clone();
        runtime.spawn(async move { abandon(&remote, &client, &task_id).await });
    }
}

fn user_agent() -> String {
    format!("aura/{}", env!("CARGO_PKG_VERSION"))
}

fn call_error(message: impl Into<String>) -> ToolError {
    ToolError::ToolCallError(message.into().into())
}

/// A task state the caller stops polling at: terminal, or waiting on the
/// caller for input or credentials.
fn is_settled(state: &TaskState) -> bool {
    state.is_terminal() || matches!(state, TaskState::InputRequired | TaskState::AuthRequired)
}

/// Arguments the model passes to `ask_agent`. Which agent config a remote
/// serves is the operator's `model` setting, never the caller's choice.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskAgentArgs {
    agent: String,
    prompt: String,
    #[serde(default)]
    context_id: Option<String>,
}

/// What `ask_agent` hands back to the model.
#[derive(Debug, Serialize, PartialEq)]
pub struct AskAgentOutcome {
    pub agent: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub response: String,
}

impl AskAgentOutcome {
    /// A finished task. The AURA server puts the full reply in its `final`
    /// artifact and the streamed chunks in `response`; other servers get
    /// every text part joined, and lastly the status message. The text is
    /// cut at `max_response_bytes` with a notice, since nothing intercepts
    /// a remote answer the way the scratchpad intercepts MCP output.
    fn from_task(agent: &str, task: &Task, max_response_bytes: usize) -> Self {
        let response = match &task.status.state {
            TaskState::InputRequired | TaskState::AuthRequired => status_text(task),
            _ => final_text(task),
        };
        Self {
            agent: agent.to_owned(),
            state: state_label(&task.status.state).to_owned(),
            task_id: Some(task.id.clone()),
            context_id: Some(task.context_id.clone()),
            response: bounded_response(response, max_response_bytes),
        }
    }

    /// A remote that answered outright instead of opening a task.
    fn from_message(agent: &str, message: &Message, max_response_bytes: usize) -> Self {
        Self {
            agent: agent.to_owned(),
            state: state_label(&TaskState::Completed).to_owned(),
            task_id: message.task_id.clone(),
            context_id: message.context_id.clone(),
            response: bounded_response(parts_text(&message.parts), max_response_bytes),
        }
    }

    /// The tool result string: the answer text with a one-line trailer
    /// carrying the state, task id, and context_id for a usable outcome;
    /// the tool-error convention for a task that failed, was rejected, or
    /// was cancelled on the remote. The answer leads because a JSON envelope
    /// invites the model to relay the envelope rather than the answer.
    fn render(self) -> String {
        match self.state.as_str() {
            "failed" | "rejected" | "canceled" => {
                let detail = if self.response.is_empty() {
                    "no details".to_owned()
                } else {
                    self.response
                };
                format!(
                    "{TOOL_ERROR_PREFIX}remote agent {:?} task {} ended in state {}:\n{detail}",
                    self.agent,
                    self.task_id.as_deref().unwrap_or("?"),
                    self.state,
                )
            }
            _ => {
                let body = if self.response.is_empty() {
                    "(the remote agent returned no text)".to_owned()
                } else {
                    self.response
                };
                let mut trailer = format!("remote agent {:?} · {}", self.agent, self.state);
                if let Some(task_id) = &self.task_id {
                    trailer.push_str(&format!(" · task {task_id}"));
                }
                if let Some(context_id) = &self.context_id {
                    trailer.push_str(&format!(" · context_id {context_id}"));
                }
                format!("{body}\n\n---\n{trailer}")
            }
        }
    }
}

fn state_label(state: &TaskState) -> &'static str {
    match state {
        TaskState::Unspecified => "unspecified",
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::InputRequired => "input_required",
        TaskState::Rejected => "rejected",
        TaskState::AuthRequired => "auth_required",
    }
}

/// `text` cut to `limit` bytes on a char boundary, with a notice telling the
/// model what it did not get.
fn bounded_response(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let cut = text.floor_char_boundary(limit);
    format!(
        "{}\n… [truncated by ask_agent: the remote answer was {} bytes, limit {limit}]",
        &text[..cut],
        text.len()
    )
}

fn final_text(task: &Task) -> String {
    let artifacts = task.artifacts.as_deref().unwrap_or_default();
    for wanted in ["final", "response"] {
        if let Some(artifact) = artifacts.iter().find(|a| a.artifact_id == wanted) {
            let text = parts_text(&artifact.parts);
            if !text.is_empty() {
                return text;
            }
        }
    }
    let joined = artifacts
        .iter()
        .map(|a: &Artifact| parts_text(&a.parts))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !joined.is_empty() {
        return joined;
    }
    let status = status_text(task);
    if !status.is_empty() {
        return status;
    }
    let non_text = artifacts
        .iter()
        .flat_map(|a| a.parts.iter())
        .filter(|p| !matches!(p.content, PartContent::Text(_)))
        .count();
    if non_text > 0 {
        format!(
            "[the remote agent returned {non_text} non-text part(s), which ask_agent cannot relay]"
        )
    } else {
        String::new()
    }
}

fn status_text(task: &Task) -> String {
    task.status
        .message
        .as_ref()
        .map(|m| parts_text(&m.parts))
        .unwrap_or_default()
}

fn parts_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| match &p.content {
            PartContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// The `ask_agent` tool over every configured remote.
#[derive(Clone)]
pub struct RemoteAgentTool {
    remotes: Arc<BTreeMap<String, RemoteAgent>>,
    request_id: Option<String>,
    stream_events: bool,
    description: String,
    parameters: Value,
}

impl RemoteAgentTool {
    /// `request_id` is the request this tool serves; it binds the call to
    /// that request's cancellation and its `aura.tool_start` stream.
    pub fn new(remotes: Vec<RemoteAgent>, request_id: Option<String>) -> Self {
        let remotes: BTreeMap<String, RemoteAgent> =
            remotes.into_iter().map(|r| (r.name.clone(), r)).collect();
        let listing: BTreeMap<&str, Option<&str>> = remotes
            .iter()
            .map(|(name, r)| (name.as_str(), r.description.as_deref()))
            .collect();
        let description = describe(&listing);
        let parameters = parameters(&listing);
        Self {
            remotes: Arc::new(remotes),
            request_id,
            stream_events: true,
            description,
            parameters,
        }
    }

    /// Every `[a2a.remote.<name>]` entry as one tool.
    pub fn from_config(
        a2a: &A2aConfig,
        request_id: Option<String>,
    ) -> Result<Self, A2aClientError> {
        let mut remotes = Vec::with_capacity(a2a.remote.len());
        for (name, remote) in &a2a.remote {
            remotes.push(RemoteAgent::from_config(name, a2a, remote)?);
        }
        Ok(Self::new(remotes, request_id))
    }

    /// The tool's description and parameter schema for `a2a`, computed from
    /// the config alone: what a planner needs without building HTTP clients.
    pub fn planning_definition(a2a: &A2aConfig) -> (String, Value) {
        let listing: BTreeMap<&str, Option<&str>> = a2a
            .remote
            .iter()
            .map(|(name, r)| (name.as_str(), r.description.as_deref()))
            .collect();
        (describe(&listing), parameters(&listing))
    }

    /// Whether a call publishes `aura.tool_start` on the request's event
    /// stream. Orchestration workers stream under their own ids and report
    /// through the observer wrapper, so their tools must not peek the live
    /// request's tool-call queue.
    pub fn with_stream_events(mut self, stream_events: bool) -> Self {
        self.stream_events = stream_events;
        self
    }

    pub fn remote_names(&self) -> Vec<&str> {
        self.remotes.keys().map(String::as_str).collect()
    }

    /// The tool description as the model sees it.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The tool's JSON schema as the model sees it.
    pub fn parameters(&self) -> &Value {
        &self.parameters
    }

    /// Emit `aura.tool_start` for this call when a streaming hook queued a
    /// tool_call_id for the request (single-agent streaming).
    async fn announce_start(&self) {
        if !self.stream_events {
            return;
        }
        let Some(request_id) = &self.request_id else {
            return;
        };
        if let Some(tool_call_id) = peek_tool_call_id(request_id).await {
            publish_tool_start(
                request_id,
                tool_call_id,
                ASK_AGENT_TOOL_NAME.to_owned(),
                None,
            )
            .await;
        }
    }

    fn cancel_token(&self) -> RequestCancelToken {
        self.request_id
            .as_deref()
            .and_then(RequestCancellation::token_for_id)
            .unwrap_or_else(RequestCancelToken::unbound)
    }
}

fn describe(remotes: &BTreeMap<&str, Option<&str>>) -> String {
    let mut text = String::from(
        "Ask a remote AURA agent to handle a request and wait for its answer. The remote \
         agent runs its own tools and returns a text response. It does not see this \
         conversation, so the prompt must be self-contained. The result is the answer text \
         followed by a one-line trailer carrying the task state, task id, and context_id; \
         present the answer in your own words and do not quote the trailer. If a call fails, \
         the result says why — summarize the failure for the user in your own words instead \
         of relaying the raw error. To continue the \
         exchange — including answering an input_required or auth_required question — call \
         ask_agent again with the same agent and the trailer's context_id. Available agents:",
    );
    for (name, description) in remotes {
        text.push_str("\n- ");
        text.push_str(name);
        if let Some(description) = description.filter(|d| !d.is_empty()) {
            text.push_str(": ");
            text.push_str(description);
        }
    }
    text
}

fn parameters(remotes: &BTreeMap<&str, Option<&str>>) -> Value {
    let names: Vec<&str> = remotes.keys().copied().collect();
    json!({
        "type": "object",
        "properties": {
            "agent": {
                "type": "string",
                "enum": names,
                "description": "Which remote agent to ask."
            },
            "prompt": {
                "type": "string",
                "description": "The complete request for the remote agent, including every \
                                detail it needs; it has no access to this conversation."
            },
            "context_id": {
                "type": "string",
                "description": "Continue an earlier exchange with the same agent: the \
                                context_id a previous ask_agent result returned."
            }
        },
        "required": ["agent", "prompt"]
    })
}

impl RigTool for RemoteAgentTool {
    type Error = ToolError;
    type Args = Value;
    type Output = String;

    const NAME: &'static str = ASK_AGENT_TOOL_NAME;

    #[allow(refining_impl_trait)]
    fn definition(
        &self,
        _prompt: String,
    ) -> Pin<Box<dyn Future<Output = rig::completion::ToolDefinition> + Send + Sync + '_>> {
        let definition = rig::completion::ToolDefinition {
            name: ASK_AGENT_TOOL_NAME.to_owned(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        };
        Box::pin(async move { definition })
    }

    #[allow(refining_impl_trait)]
    fn call(
        &self,
        args: Self::Args,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + '_>> {
        Box::pin(async move {
            let args: AskAgentArgs = serde_json::from_value(args)
                .map_err(|e| call_error(format!("invalid {ASK_AGENT_TOOL_NAME} arguments: {e}")))?;
            let Some(remote) = self.remotes.get(&args.agent) else {
                return Err(call_error(format!(
                    "unknown remote agent {:?}; configured agents: {}",
                    args.agent,
                    self.remote_names().join(", ")
                )));
            };
            self.announce_start().await;
            let span = tracing::info_span!(
                "a2a.ask_agent",
                a2a.remote = %remote.name,
                a2a.endpoint = %remote.endpoint(),
                a2a.task_id = tracing::field::Empty,
            );
            remote
                .ask(&args, self.cancel_token())
                .instrument(span)
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::test_server::{LoopbackA2aServer, completed_task, working_task};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    fn remote(name: &str, server: &LoopbackA2aServer, model: Option<&str>) -> RemoteAgent {
        remote_with_budget(
            name,
            server,
            model,
            Duration::from_millis(20),
            Duration::from_secs(5),
        )
    }

    fn remote_with_budget(
        name: &str,
        server: &LoopbackA2aServer,
        model: Option<&str>,
        poll_interval: Duration,
        timeout: Duration,
    ) -> RemoteAgent {
        let client = A2aClient::new(&server.url, &HashMap::new(), "aura/test").unwrap();
        RemoteAgent::new(
            name,
            client,
            Some(format!("the {name} agent")),
            model.map(str::to_owned),
            poll_interval,
            timeout,
            64 * 1024,
        )
    }

    /// A server whose task completes on the `polls_until_done`-th GetTask.
    async fn completing_server(polls_until_done: usize) -> LoopbackA2aServer {
        let polls = Arc::new(AtomicUsize::new(0));
        LoopbackA2aServer::start(move |method, params| match method {
            "SendMessage" => {
                let ctx = params["message"]["contextId"]
                    .as_str()
                    .unwrap_or("ctx-new")
                    .to_owned();
                Ok(json!({ "task": working_task("task-1", &ctx) }))
            }
            "GetTask" => {
                assert_eq!(params["id"], "task-1");
                if polls.fetch_add(1, Ordering::SeqCst) + 1 >= polls_until_done {
                    Ok(completed_task("task-1", "ctx-new", "42 is the answer"))
                } else {
                    Ok(working_task("task-1", "ctx-new"))
                }
            }
            other => panic!("unexpected method {other}"),
        })
        .await
    }

    #[tokio::test]
    async fn definition_lists_every_remote() {
        let a = LoopbackA2aServer::start(|_, _| unreachable!()).await;
        let tool = RemoteAgentTool::new(
            vec![remote("prod", &a, None), remote("dev", &a, None)],
            None,
        );
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "ask_agent");
        assert!(def.description.contains("- dev: the dev agent"));
        assert!(def.description.contains("- prod: the prod agent"));
        assert_eq!(
            def.parameters["properties"]["agent"]["enum"],
            json!(["dev", "prod"])
        );
        assert_eq!(def.parameters["required"], json!(["agent", "prompt"]));
    }

    #[tokio::test]
    async fn polls_until_the_task_completes_and_returns_the_final_text() {
        let server = completing_server(2).await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);

        let out = tool
            .call(json!({ "agent": "dev", "prompt": "what is 6 * 7?" }))
            .await
            .unwrap();
        assert!(out.starts_with("42 is the answer"), "{out}");
        assert!(
            out.contains("remote agent \"dev\" · completed · task task-1 · context_id ctx-new"),
            "{out}"
        );
        assert_eq!(server.methods(), ["SendMessage", "GetTask", "GetTask"]);
    }

    #[tokio::test]
    async fn configured_model_is_sent_and_the_caller_cannot_override_it() {
        let server = completing_server(1).await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, Some("verifier"))], None);

        tool.call(json!({ "agent": "dev", "prompt": "x", "context_id": "ctx-7" }))
            .await
            .unwrap();
        let err = tool
            .call(json!({ "agent": "dev", "prompt": "x", "model": "admin" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid ask_agent arguments"),
            "{err}"
        );

        let sends: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.method() == "SendMessage")
            .collect();
        assert_eq!(sends.len(), 1, "the rejected call never reached the wire");
        assert_eq!(sends[0].header_values("x-aura-model"), vec!["verifier"]);
        assert_eq!(
            sends[0].body_json()["params"]["message"]["contextId"],
            "ctx-7"
        );
        assert!(
            !tool.parameters()["properties"]
                .as_object()
                .unwrap()
                .contains_key("model")
        );
    }

    #[tokio::test]
    async fn cancellation_during_send_still_cancels_the_task_the_remote_opened() {
        let server =
            LoopbackA2aServer::start_slow_send(
                Duration::from_millis(200),
                |method, _| match method {
                    "SendMessage" => Ok(json!({ "task": working_task("late", "c") })),
                    "GetTask" => Ok(working_task("late", "c")),
                    "CancelTask" => Ok(json!({
                        "id": "late", "contextId": "c", "status": { "state": "TASK_STATE_CANCELED" }
                    })),
                    other => panic!("unexpected method {other}"),
                },
            )
            .await;
        let request_id = format!("req_{}", uuid::Uuid::new_v4());
        let registration = RequestCancellation::register(request_id.clone());
        let tool =
            RemoteAgentTool::new(vec![remote("dev", &server, None)], Some(request_id.clone()));

        let call =
            tokio::spawn(async move { tool.call(json!({ "agent": "dev", "prompt": "x" })).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        registration.token.cancel();
        let err = call.await.unwrap().unwrap_err();
        RequestCancellation::unregister(&request_id);

        assert!(err.to_string().contains("Request cancelled"), "{err}");
        assert_eq!(server.methods(), ["SendMessage", "CancelTask"]);
        assert_eq!(server.requests()[1].body_json()["params"]["id"], "late");
    }

    #[tokio::test]
    async fn direct_message_reply_is_a_completed_outcome() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "message": {
                "messageId": "m1", "role": "ROLE_AGENT", "contextId": "ctx-m",
                "parts": [ { "text": "instant " }, { "text": "answer" } ]
            }})),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let out = tool
            .call(json!({ "agent": "dev", "prompt": "hi" }))
            .await
            .unwrap();
        assert!(out.starts_with("instant answer"), "{out}");
        assert!(
            out.contains("remote agent \"dev\" · completed · context_id ctx-m"),
            "{out}"
        );
        assert_eq!(server.methods(), ["SendMessage"]);
    }

    #[tokio::test]
    async fn failed_task_uses_the_tool_error_convention() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("t9", "c") })),
            "GetTask" => Ok(json!({
                "id": "t9", "contextId": "c",
                "status": { "state": "TASK_STATE_FAILED", "message": {
                    "messageId": "m", "role": "ROLE_AGENT", "parts": [ { "text": "LLM quota exceeded" } ]
                }}
            })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let out = tool
            .call(json!({ "agent": "dev", "prompt": "hi" }))
            .await
            .unwrap();
        assert_eq!(
            out,
            "Tool returned an error: remote agent \"dev\" task t9 ended in state failed:\nLLM quota exceeded"
        );
    }

    #[tokio::test]
    async fn input_required_hands_the_question_back_with_the_context() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": {
                "id": "t2", "contextId": "c2",
                "status": { "state": "TASK_STATE_INPUT_REQUIRED", "message": {
                    "messageId": "m", "role": "ROLE_AGENT", "parts": [ { "text": "Which cluster?" } ]
                }}
            }})),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let out = tool
            .call(json!({ "agent": "dev", "prompt": "verify" }))
            .await
            .unwrap();
        assert!(out.starts_with("Which cluster?"), "{out}");
        assert!(
            out.contains("remote agent \"dev\" · input_required · task t2 · context_id c2"),
            "{out}"
        );
        assert_eq!(server.methods(), ["SendMessage"]);
    }

    #[tokio::test]
    async fn unknown_agent_and_bad_arguments_are_rejected_before_any_call() {
        let server = LoopbackA2aServer::start(|_, _| unreachable!()).await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);

        let err = tool
            .call(json!({ "agent": "staging", "prompt": "x" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown remote agent \"staging\""),
            "{err}"
        );
        assert!(err.to_string().contains("configured agents: dev"), "{err}");

        let err = tool.call(json!({ "agent": "dev" })).await.unwrap_err();
        assert!(
            err.to_string().contains("invalid ask_agent arguments"),
            "{err}"
        );
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn transport_failures_name_the_remote() {
        let server = LoopbackA2aServer::start_with_status(503, "gateway down").await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let err = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("remote agent \"dev\""), "{text}");
        assert!(text.contains("HTTP 503"), "{text}");
        assert!(text.contains("gateway down"), "{text}");
    }

    #[tokio::test]
    async fn timeout_gives_up_and_cancels_the_remote_task() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("slow", "c") })),
            "GetTask" => Ok(working_task("slow", "c")),
            "CancelTask" => Ok(json!({
                "id": "slow", "contextId": "c", "status": { "state": "TASK_STATE_CANCELED" }
            })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(
            vec![remote_with_budget(
                "dev",
                &server,
                None,
                Duration::from_millis(20),
                Duration::from_millis(150),
            )],
            None,
        );
        let err = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not finish within"), "{err}");
        let methods = server.methods();
        assert_eq!(methods.first().map(String::as_str), Some("SendMessage"));
        assert_eq!(methods.last().map(String::as_str), Some("CancelTask"));
    }

    #[tokio::test]
    async fn request_cancellation_stops_polling_and_cancels_the_remote_task() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("slow", "c") })),
            "GetTask" => Ok(working_task("slow", "c")),
            "CancelTask" => Ok(json!({
                "id": "slow", "contextId": "c", "status": { "state": "TASK_STATE_CANCELED" }
            })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let request_id = format!("req_{}", uuid::Uuid::new_v4());
        let registration = RequestCancellation::register(request_id.clone());
        let tool =
            RemoteAgentTool::new(vec![remote("dev", &server, None)], Some(request_id.clone()));

        let call =
            tokio::spawn(async move { tool.call(json!({ "agent": "dev", "prompt": "x" })).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        registration.token.cancel();
        let err = call.await.unwrap().unwrap_err();
        RequestCancellation::unregister(&request_id);

        assert!(err.to_string().contains("Request cancelled"), "{err}");
        assert_eq!(
            server.methods().last().map(String::as_str),
            Some("CancelTask")
        );
    }

    #[tokio::test]
    async fn a_drifting_task_id_fails_the_call_and_cancels_the_original() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("mine", "c") })),
            "GetTask" => Ok(working_task("theirs", "c")),
            "CancelTask" => Ok(json!({
                "id": "mine", "contextId": "c", "status": { "state": "TASK_STATE_CANCELED" }
            })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let err = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("answered GetTask for task \"theirs\" instead of \"mine\""),
            "{err}"
        );
        assert_eq!(server.methods(), ["SendMessage", "GetTask", "CancelTask"]);
        assert_eq!(server.requests()[2].body_json()["params"]["id"], "mine");
    }

    #[tokio::test]
    async fn transient_poll_failures_are_retried_within_the_budget() {
        let polls = Arc::new(AtomicUsize::new(0));
        let server = LoopbackA2aServer::start(move |method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("t", "c") })),
            "GetTask" => match polls.fetch_add(1, Ordering::SeqCst) {
                0 => Err((503, "gateway busy".to_owned())),
                _ => Ok(completed_task("t", "c", "done")),
            },
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let out = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert_eq!(server.methods(), ["SendMessage", "GetTask", "GetTask"]);
    }

    #[tokio::test]
    async fn repeated_transient_failures_give_up() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("t", "c") })),
            "GetTask" => Err((503, "gateway busy".to_owned())),
            "CancelTask" => Ok(json!({
                "id": "t", "contextId": "c", "status": { "state": "TASK_STATE_CANCELED" }
            })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let err = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 503"), "{err}");
        assert_eq!(
            server.methods(),
            ["SendMessage", "GetTask", "GetTask", "GetTask", "CancelTask"]
        );
    }

    #[tokio::test]
    async fn dropping_the_call_mid_poll_cancels_the_remote_task() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("orphan", "c") })),
            "GetTask" => Ok(working_task("orphan", "c")),
            "CancelTask" => Ok(json!({
                "id": "orphan", "contextId": "c", "status": { "state": "TASK_STATE_CANCELED" }
            })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);

        // No cancellation token, no timeout: the only signal is the future
        // being dropped, as an orchestration per-call timeout does.
        let call =
            tokio::spawn(async move { tool.call(json!({ "agent": "dev", "prompt": "x" })).await });
        tokio::time::sleep(Duration::from_millis(80)).await;
        call.abort();
        let _ = call.await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while server.methods().last().map(String::as_str) != Some("CancelTask") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no CancelTask after drop"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let cancel = server.requests().into_iter().last().unwrap();
        assert_eq!(cancel.body_json()["params"]["id"], "orphan");
    }

    #[tokio::test]
    async fn a_send_slower_than_the_abandon_wait_is_cancelled_once_it_lands() {
        // The send outlives the abandon wait (ABANDON_TIMEOUT is 5s, hence
        // the 7s delay), so the call gives up without a task id. When the
        // send finally lands, the OpenTask guard's Drop must still cancel
        // the task it records.
        let server =
            LoopbackA2aServer::start_slow_send(Duration::from_secs(7), |method, _| match method {
                "SendMessage" => Ok(json!({ "task": working_task("slow", "c") })),
                "CancelTask" => Ok(json!({
                    "id": "slow", "contextId": "c", "status": { "state": "TASK_STATE_CANCELED" }
                })),
                other => panic!("unexpected method {other}"),
            })
            .await;
        let tool = RemoteAgentTool::new(
            vec![remote_with_budget(
                "dev",
                &server,
                None,
                Duration::from_millis(20),
                Duration::from_millis(100),
            )],
            None,
        );

        let err = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not finish within"), "{err}");

        // The call returned with the send still in flight; the guard fires
        // when the send lands and records the task id.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while server.methods().last().map(String::as_str) != Some("CancelTask") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no CancelTask after the slow send landed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let cancel = server.requests().into_iter().last().unwrap();
        assert_eq!(cancel.body_json()["params"]["id"], "slow");
    }

    #[tokio::test]
    async fn protojson_omitted_parts_do_not_fail_the_call() {
        // A run whose workers used the scratchpad reports metadata-only
        // scratchpad artifacts, and the v1.0 wire omits their empty `parts`.
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("t", "c") })),
            "GetTask" => Ok(json!({
                "id": "t", "contextId": "c",
                "status": { "state": "TASK_STATE_COMPLETED" },
                "artifacts": [
                    { "artifactId": "scratchpad_worker", "name": "Scratchpad Usage",
                      "metadata": { "tokens_intercepted": 10, "tokens_extracted": 3 } },
                    { "artifactId": "final", "name": "Final Info",
                      "parts": [ { "text": "sweep complete" } ] }
                ]
            })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let out = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap();
        assert!(out.starts_with("sweep complete"), "{out}");
        assert!(out.contains("· completed · task t · context_id c"), "{out}");
    }

    #[tokio::test]
    async fn long_answers_are_truncated_with_a_notice() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("t", "c") })),
            "GetTask" => Ok(completed_task("t", "c", &"é".repeat(2000))),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let client = A2aClient::new(&server.url, &HashMap::new(), "aura/test").unwrap();
        let tool = RemoteAgentTool::new(
            vec![RemoteAgent::new(
                "dev",
                client,
                None,
                None,
                Duration::from_millis(20),
                Duration::from_secs(5),
                1024,
            )],
            None,
        );
        let out = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap();
        assert!(out.starts_with("éé"), "{out}");
        assert!(
            out.contains("[truncated by ask_agent: the remote answer was 4000 bytes, limit 1024]"),
            "{out}"
        );
        assert!(
            out.ends_with("remote agent \"dev\" · completed · task t · context_id c"),
            "{out}"
        );
        assert!(out.len() < 1024 + 200, "{}", out.len());
    }

    #[tokio::test]
    async fn non_text_only_answers_say_so() {
        let server = LoopbackA2aServer::start(|method, _| match method {
            "SendMessage" => Ok(json!({ "task": working_task("t", "c") })),
            "GetTask" => Ok(json!({
                "id": "t", "contextId": "c",
                "status": { "state": "TASK_STATE_COMPLETED" },
                "artifacts": [ { "artifactId": "final", "parts": [ { "data": { "healthy": true } } ] } ]
            })),
            other => panic!("unexpected method {other}"),
        })
        .await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);
        let out = tool
            .call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap();
        assert!(
            out.starts_with(
                "[the remote agent returned 1 non-text part(s), which ask_agent cannot relay]"
            ),
            "{out}"
        );
    }

    #[test]
    fn planning_definition_matches_the_built_tool_without_clients() {
        let mut remote = HashMap::new();
        remote.insert(
            "dev".to_owned(),
            aura_config::A2aRemoteConfig {
                url: "http://dev".to_owned(),
                description: Some("dev cluster".to_owned()),
                model: None,
                headers: HashMap::new(),
                headers_from_request: HashMap::new(),
                poll_interval_secs: None,
                timeout_secs: None,
            },
        );
        let a2a = A2aConfig {
            remote,
            ..A2aConfig::default()
        };
        let (description, parameters) = RemoteAgentTool::planning_definition(&a2a);
        let built = RemoteAgentTool::from_config(&a2a, None).unwrap();
        assert_eq!(description, built.description());
        assert_eq!(&parameters, built.parameters());
        assert!(description.contains("- dev: dev cluster"));
    }
}
