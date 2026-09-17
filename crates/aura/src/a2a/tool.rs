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
use crate::tool_event_broker::{
    ToolName, peek_tool_call_id, publish_agent_answer, publish_tool_start,
};

/// How long a best-effort remote `CancelTask` may take once the local call
/// has already been given up on.
const ABANDON_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive transient `GetTask` failures tolerated before the call fails.
const MAX_TRANSIENT_POLL_FAILURES: u32 = 2;

/// Most agents one `ask_agent` call may fan out to in parallel.
const MAX_BATCH_CALLS: usize = 8;

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
        call: &AskAgentCall,
        cancel: RequestCancelToken,
    ) -> Result<String, ToolError> {
        let open = Arc::new(OpenTask::new(&self.name, self.client.clone()));
        // Set once `run` has taken the send task's output; polling the
        // JoinHandle again after that panics.
        let send_consumed = AtomicBool::new(false);
        let mut send = tokio::spawn({
            let client = self.client.clone();
            let prompt = call.prompt.clone();
            let context_id = call.context_id.clone();
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

/// Arguments the model passes to `ask_agent`: the single-agent form
/// (`agent` + `prompt`, optional `context_id`) or a `calls` batch of
/// sub-requests.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskAgentArgs {
    agent: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    context_id: Option<String>,
    #[serde(default)]
    calls: Option<Vec<AskAgentCall>>,
}

impl AskAgentArgs {
    /// Normalize the two call forms into the sub-requests `call` runs
    /// concurrently: the single-agent form becomes one entry; a batch is
    /// taken as given, capped at [`MAX_BATCH_CALLS`].
    fn into_calls(self) -> Result<Vec<AskAgentCall>, ToolError> {
        let invalid =
            |msg: &str| call_error(format!("invalid {ASK_AGENT_TOOL_NAME} arguments: {msg}"));
        let single = self.agent.is_some() || self.prompt.is_some() || self.context_id.is_some();
        match (self.calls, single) {
            (Some(calls), false) if calls.is_empty() => Err(invalid("`calls` must not be empty")),
            (Some(calls), false) if calls.len() > MAX_BATCH_CALLS => Err(invalid(&format!(
                "`calls` takes at most {MAX_BATCH_CALLS} entries, got {}",
                calls.len()
            ))),
            (Some(calls), false) => Ok(calls),
            (Some(_), true) => Err(invalid(
                "`calls` cannot be combined with `agent`, `prompt`, or `context_id`",
            )),
            (None, true) => {
                let (agent, prompt) = match (self.agent, self.prompt) {
                    (Some(agent), Some(prompt)) => (agent, prompt),
                    _ => return Err(invalid("both `agent` and `prompt` are required")),
                };
                Ok(vec![AskAgentCall {
                    agent,
                    prompt,
                    context_id: self.context_id,
                }])
            }
            (None, false) => Err(invalid("pass `agent` and `prompt`, or `calls`")),
        }
    }
}

/// One sub-request inside an `ask_agent` call.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskAgentCall {
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
                ToolName::new(ASK_AGENT_TOOL_NAME),
                None,
            )
            .await;
        }
    }

    /// Publish one batch member's section as an `aura.remote_agent_answer`
    /// event as it lands, so a client can show one agent's report while the
    /// rest of the batch is still running (single-agent streaming; workers
    /// report through the orchestration observer).
    fn announce_agent_answer(
        &self,
        remote: &RemoteAgent,
        result: &Result<String, ToolError>,
        elapsed: Duration,
    ) {
        if !self.stream_events {
            return;
        }
        let Some(request_id) = &self.request_id else {
            return;
        };
        let (success, text) = match result {
            Ok(text) => (true, text.clone()),
            Err(e) => (false, e.to_string()),
        };
        publish_agent_answer(
            request_id,
            &remote.name,
            success,
            text,
            elapsed.as_millis() as u64,
        );
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
         ask_agent again with the same agent and the trailer's context_id. To put a request \
         to several agents at once, pass `calls` with one entry per agent instead of \
         `agent` and `prompt`; the agents run in parallel, and the result carries each \
         agent's report under a `## <agent>` heading with its own trailer — write one reply \
         that presents each report, never the raw result. Available agents:",
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
                "description": "Which remote agent to ask. For several agents at once, use \
                                `calls` instead."
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
            },
            "calls": {
                "type": "array",
                "description": "Ask several agents in parallel: one entry per agent. Use \
                                instead of `agent` and `prompt`.",
                "minItems": 1,
                "maxItems": MAX_BATCH_CALLS,
                "items": {
                    "type": "object",
                    "properties": {
                        "agent": {
                            "type": "string",
                            "enum": names,
                            "description": "Which remote agent to ask."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "The complete request for this agent."
                        },
                        "context_id": {
                            "type": "string",
                            "description": "Continue an earlier exchange with this agent."
                        }
                    },
                    "required": ["agent", "prompt"],
                    "additionalProperties": false
                }
            }
        }
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
            let calls = args.into_calls()?;
            let mut resolved = Vec::with_capacity(calls.len());
            for call in &calls {
                let Some(remote) = self.remotes.get(&call.agent) else {
                    return Err(call_error(format!(
                        "unknown remote agent {:?}; configured agents: {}",
                        call.agent,
                        self.remote_names().join(", ")
                    )));
                };
                resolved.push((remote, call));
            }
            self.announce_start().await;
            // Every sub-call runs its own send/poll/cancel lifecycle; the
            // batch waits for all of them, so N agents cost one budget, not
            // N. A batch where every agent failed is a failed call; one with
            // any answer carries each failure as its own section.
            let cancel = self.cancel_token();
            let started = std::time::Instant::now();
            let mut in_flight: futures::stream::FuturesUnordered<_> = resolved
                .iter()
                .enumerate()
                .map(|(idx, (remote, call))| {
                    let cancel = cancel.clone();
                    let span = tracing::info_span!(
                        "a2a.ask_agent",
                        a2a.remote = %remote.name,
                        a2a.endpoint = %remote.endpoint(),
                        a2a.task_id = tracing::field::Empty,
                    );
                    async move { (idx, remote.ask(call, cancel).await) }.instrument(span)
                })
                .collect();
            let mut sections: Vec<Option<Result<String, ToolError>>> =
                (0..resolved.len()).map(|_| None).collect();
            // Sections assemble in request order, but each answer publishes
            // as it lands so a client can show one agent's report while the
            // rest of the batch is still running.
            let multi = resolved.len() > 1;
            while let Some((idx, result)) = futures::StreamExt::next(&mut in_flight).await {
                if multi {
                    self.announce_agent_answer(resolved[idx].0, &result, started.elapsed());
                }
                sections[idx] = Some(result);
            }
            let sections: Vec<Result<String, ToolError>> = sections
                .into_iter()
                .map(|s| s.expect("every sub-call resolves"))
                .collect();
            if sections.iter().all(Result::is_err) {
                let mut sections = sections;
                if sections.len() == 1 {
                    return Err(sections.pop().expect("one section").unwrap_err());
                }
                let count = sections.len();
                let details = sections
                    .into_iter()
                    .map(|r| r.unwrap_err().to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(call_error(format!(
                    "all {count} remote agent calls failed: {details}"
                )));
            }
            // A batch reads as one report per agent under a heading naming
            // it; a single agent's answer needs no heading.
            Ok(sections
                .into_iter()
                .zip(resolved.iter())
                .map(|(r, (remote, _))| {
                    let body = r.unwrap_or_else(|e| e.to_string());
                    if multi {
                        format!("## {}\n\n{body}", remote.name)
                    } else {
                        body
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n"))
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
        assert!(def.description.contains("parallel"));
        assert_eq!(
            def.parameters["properties"]["agent"]["enum"],
            json!(["dev", "prod"])
        );
        assert_eq!(
            def.parameters["properties"]["calls"]["items"]["required"],
            json!(["agent", "prompt"])
        );
        assert!(
            def.parameters.get("required").is_none(),
            "the single form and the batch form are validated in code"
        );
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

    /// Two servers whose tasks each complete only after BOTH have been
    /// polled: a sequential execution would run the first into its timeout.
    #[tokio::test]
    async fn calls_run_in_parallel() {
        let polls = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let mut remotes = Vec::new();
        let mut servers = Vec::new();
        for (i, name) in [(0usize, "dev"), (1usize, "stage")] {
            let polls = Arc::clone(&polls);
            let server = LoopbackA2aServer::start(move |method, _| match method {
                "SendMessage" => Ok(json!({ "task": working_task(&format!("t-{name}"), "c") })),
                "GetTask" => {
                    polls[i].fetch_add(1, Ordering::SeqCst);
                    if polls[0].load(Ordering::SeqCst) > 0 && polls[1].load(Ordering::SeqCst) > 0 {
                        Ok(completed_task(
                            &format!("t-{name}"),
                            "c",
                            &format!("{name} swept"),
                        ))
                    } else {
                        Ok(working_task(&format!("t-{name}"), "c"))
                    }
                }
                other => panic!("unexpected method {other}"),
            })
            .await;
            let client = A2aClient::new(&server.url, &HashMap::new(), "aura/test").unwrap();
            remotes.push(RemoteAgent::new(
                name,
                client,
                Some(format!("the {name} agent")),
                None,
                Duration::from_millis(20),
                Duration::from_millis(800),
                64 * 1024,
            ));
            servers.push(server);
        }
        let tool = RemoteAgentTool::new(remotes, None);

        let out = tool
            .call(json!({ "calls": [
                { "agent": "dev", "prompt": "sweep", "context_id": "c-dev" },
                { "agent": "stage", "prompt": "sweep", "context_id": "c-stage" }
            ]}))
            .await
            .unwrap();

        assert!(out.contains("dev swept"), "{out}");
        assert!(out.contains("stage swept"), "{out}");
        assert!(out.starts_with("## dev\n\n"), "{out}");
        assert!(out.contains("\n\n## stage\n\n"), "{out}");
        assert!(out.contains("remote agent \"dev\" · completed"), "{out}");
        assert!(out.contains("remote agent \"stage\" · completed"), "{out}");
        assert!(
            !out.contains("did not finish within"),
            "sequential execution would have timed out: {out}"
        );
        // Each entry's context_id rode on its own SendMessage.
        for (server, ctx) in servers.iter().zip(["c-dev", "c-stage"]) {
            assert_eq!(
                server.requests()[0].body_json()["params"]["message"]["contextId"],
                json!(ctx)
            );
        }
    }

    #[tokio::test]
    async fn batch_and_single_forms_are_validated() {
        let server = LoopbackA2aServer::start(|_, _| unreachable!()).await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, None)], None);

        let cases = [
            (
                json!({ "agent": "dev", "prompt": "x", "calls": [{ "agent": "dev", "prompt": "y" }] }),
                "cannot be combined",
            ),
            (json!({ "calls": [] }), "`calls` must not be empty"),
            (json!({}), "pass `agent` and `prompt`, or `calls`"),
            (
                json!({ "agent": "dev" }),
                "both `agent` and `prompt` are required",
            ),
            (
                json!({ "calls": [{ "agent": "staging", "prompt": "x" }] }),
                "unknown remote agent \"staging\"",
            ),
            (
                json!({ "calls": (0..=MAX_BATCH_CALLS).map(|_| json!({ "agent": "dev", "prompt": "x" })).collect::<Vec<_>>() }),
                "takes at most 8 entries",
            ),
        ];
        for (args, expected) in cases {
            let err = tool.call(args).await.unwrap_err();
            let text = err.to_string();
            assert!(
                text.contains("invalid ask_agent arguments")
                    || text.contains("unknown remote agent"),
                "{text}"
            );
            assert!(text.contains(expected), "{text}");
        }
        assert!(server.requests().is_empty(), "nothing reached the wire");
    }

    #[tokio::test]
    async fn a_failed_agent_in_a_batch_is_its_own_section() {
        let down = LoopbackA2aServer::start_with_status(503, "gateway down").await;
        let up = completing_server(1).await;
        let tool = RemoteAgentTool::new(
            vec![remote("dev", &up, None), remote("stage", &down, None)],
            None,
        );
        let out = tool
            .call(json!({ "calls": [
                { "agent": "dev", "prompt": "sweep" },
                { "agent": "stage", "prompt": "sweep" }
            ]}))
            .await
            .unwrap();
        assert!(out.contains("## dev"), "{out}");
        assert!(out.contains("42 is the answer"), "{out}");
        assert!(out.contains("## stage"), "{out}");
        assert!(out.contains("remote agent \"stage\":"), "{out}");
        assert!(out.contains("HTTP 503"), "{out}");
    }

    #[tokio::test]
    async fn batch_answers_publish_as_they_land_in_completion_order() {
        let fast = completing_server(1).await;
        let slow = completing_server(4).await;
        let request_id = format!("req_{}", uuid::Uuid::new_v4());
        let mut events = crate::tool_event_broker::subscribe(&request_id).await;
        let tool = RemoteAgentTool::new(
            vec![remote("dev", &slow, None), remote("stage", &fast, None)],
            Some(request_id.clone()),
        );

        let out = tool
            .call(json!({ "calls": [
                { "agent": "dev", "prompt": "sweep" },
                { "agent": "stage", "prompt": "pods" }
            ]}))
            .await
            .unwrap();
        assert!(out.contains("## dev") && out.contains("## stage"), "{out}");

        // The fast remote's event lands first even though it was requested second.
        let first = events.recv().await.expect("stage answer event");
        let crate::tool_event_broker::ToolLifecycleEvent::AgentAnswer {
            remote,
            success,
            text,
            ..
        } = &first
        else {
            panic!("expected an AgentAnswer, got {first:?}");
        };
        assert_eq!(remote, "stage");
        assert!(success);
        assert!(text.contains("42 is the answer"), "{text}");
        let second = events.recv().await.expect("dev answer event");
        let crate::tool_event_broker::ToolLifecycleEvent::AgentAnswer { remote, .. } = &second
        else {
            panic!("expected an AgentAnswer, got {second:?}");
        };
        assert_eq!(remote, "dev");
        crate::tool_event_broker::unsubscribe(&request_id).await;
    }

    #[tokio::test]
    async fn a_single_call_publishes_no_answer_event() {
        let server = completing_server(1).await;
        let request_id = format!("req_{}", uuid::Uuid::new_v4());
        let mut events = crate::tool_event_broker::subscribe(&request_id).await;
        let tool =
            RemoteAgentTool::new(vec![remote("dev", &server, None)], Some(request_id.clone()));
        tool.call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap();
        assert!(
            events.try_recv().is_err(),
            "a lone call answers immediately; no mid-batch event"
        );
        crate::tool_event_broker::unsubscribe(&request_id).await;
    }

    #[tokio::test]
    async fn a_batch_where_every_agent_failed_is_a_tool_error() {
        let a = LoopbackA2aServer::start_with_status(503, "down").await;
        let b = LoopbackA2aServer::start_with_status(503, "also down").await;
        let tool = RemoteAgentTool::new(
            vec![remote("dev", &a, None), remote("stage", &b, None)],
            None,
        );
        let err = tool
            .call(json!({ "calls": [
                { "agent": "dev", "prompt": "x" },
                { "agent": "stage", "prompt": "x" }
            ]}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("all 2 remote agent calls failed"),
            "{err}"
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
