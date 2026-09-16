//! `ask_agent`: the rig tool through which the model calls a remote AURA
//! agent and waits for its answer.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{Artifact, Message, PartContent, SendMessageResponse, Task, TaskState};
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
    ) -> Self {
        Self {
            name: name.into(),
            description,
            model,
            poll_interval,
            timeout,
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
        ))
    }

    pub fn endpoint(&self) -> &str {
        self.client.endpoint()
    }

    /// Send the prompt, poll until the remote task settles, and render the
    /// outcome. Gives up at `self.timeout` or when `cancel` fires, in both
    /// cases asking the remote to cancel the task it was left with.
    async fn ask(
        &self,
        args: &AskAgentArgs,
        cancel: RequestCancelToken,
    ) -> Result<String, ToolError> {
        let model = args.model.as_deref().or(self.model.as_deref());
        let started_task: Mutex<Option<String>> = Mutex::new(None);

        let run = async {
            let reply = self
                .client
                .send_message(&args.prompt, args.context_id.as_deref(), model)
                .await
                .map_err(|e| self.remote_error(e))?;
            let mut task = match reply {
                SendMessageResponse::Message(message) => {
                    return Ok(AskAgentOutcome::from_message(&self.name, &message));
                }
                SendMessageResponse::Task(task) => task,
            };
            tracing::Span::current().record("a2a.task_id", task.id.as_str());
            {
                let mut slot = started_task.lock().unwrap_or_else(|p| p.into_inner());
                *slot = Some(task.id.clone());
            }

            while !is_settled(&task.status.state) {
                tokio::time::sleep(self.poll_interval).await;
                task = self
                    .client
                    .get_task(&task.id)
                    .await
                    .map_err(|e| self.remote_error(e))?;
            }
            Ok(AskAgentOutcome::from_task(&self.name, &task))
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

        let abandoned = if result.is_err() {
            started_task
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
        } else {
            None
        };
        if let Some(task_id) = abandoned {
            self.abandon(&task_id).await;
        }
        result
    }

    /// Best-effort remote cancel for a task this side has stopped waiting on.
    async fn abandon(&self, task_id: &str) {
        match tokio::time::timeout(ABANDON_TIMEOUT, self.client.cancel_task(task_id)).await {
            Ok(Ok(_)) => tracing::info!(
                remote = %self.name,
                task_id,
                "cancelled the remote task after giving up on it"
            ),
            Ok(Err(e)) => tracing::warn!(
                remote = %self.name,
                task_id,
                error = %e,
                "remote task cancel failed after giving up on it"
            ),
            Err(_) => tracing::warn!(
                remote = %self.name,
                task_id,
                "remote task cancel timed out after giving up on it"
            ),
        }
    }

    fn remote_error(&self, error: A2aClientError) -> ToolError {
        call_error(format!("remote agent {:?}: {error}", self.name))
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

/// Arguments the model passes to `ask_agent`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskAgentArgs {
    agent: String,
    prompt: String,
    #[serde(default)]
    context_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
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
    /// every text part joined, and lastly the status message.
    fn from_task(agent: &str, task: &Task) -> Self {
        let response = match &task.status.state {
            TaskState::InputRequired | TaskState::AuthRequired => status_text(task),
            _ => final_text(task),
        };
        Self {
            agent: agent.to_owned(),
            state: state_label(&task.status.state).to_owned(),
            task_id: Some(task.id.clone()),
            context_id: Some(task.context_id.clone()),
            response,
        }
    }

    /// A remote that answered outright instead of opening a task.
    fn from_message(agent: &str, message: &Message) -> Self {
        Self {
            agent: agent.to_owned(),
            state: state_label(&TaskState::Completed).to_owned(),
            task_id: message.task_id.clone(),
            context_id: message.context_id.clone(),
            response: text_parts(&message.parts),
        }
    }

    /// The tool result string: a JSON object for a usable outcome, the
    /// tool-error convention for a task that failed, was rejected, or was
    /// cancelled on the remote.
    fn render(self) -> String {
        match self.state.as_str() {
            "failed" | "rejected" | "canceled" => {
                let detail = if self.response.is_empty() {
                    "no details".to_owned()
                } else {
                    self.response
                };
                format!(
                    "{TOOL_ERROR_PREFIX}remote agent {:?} task {} ended in state {}: {detail}",
                    self.agent,
                    self.task_id.as_deref().unwrap_or("?"),
                    self.state,
                )
            }
            _ => serde_json::to_string_pretty(&self)
                .unwrap_or_else(|e| format!("{TOOL_ERROR_PREFIX}could not render outcome: {e}")),
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

fn final_text(task: &Task) -> String {
    let artifacts = task.artifacts.as_deref().unwrap_or_default();
    for wanted in ["final", "response"] {
        if let Some(artifact) = artifacts.iter().find(|a| a.artifact_id == wanted) {
            let text = artifact_text(artifact);
            if !text.is_empty() {
                return text;
            }
        }
    }
    let joined = artifacts
        .iter()
        .map(artifact_text)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !joined.is_empty() {
        return joined;
    }
    status_text(task)
}

fn status_text(task: &Task) -> String {
    task.status
        .message
        .as_ref()
        .map(|m| text_parts(&m.parts))
        .unwrap_or_default()
}

fn artifact_text(artifact: &Artifact) -> String {
    text_parts(&artifact.parts)
}

fn text_parts(parts: &[a2a::Part]) -> String {
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
    description: String,
    parameters: Value,
}

impl RemoteAgentTool {
    /// `request_id` is the request this tool serves; it binds the call to
    /// that request's cancellation and its `aura.tool_start` stream.
    pub fn new(remotes: Vec<RemoteAgent>, request_id: Option<String>) -> Self {
        let remotes: BTreeMap<String, RemoteAgent> =
            remotes.into_iter().map(|r| (r.name.clone(), r)).collect();
        let description = describe(&remotes);
        let parameters = parameters(&remotes);
        Self {
            remotes: Arc::new(remotes),
            request_id,
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
    /// tool_call_id for the request (single-agent streaming); workers stream
    /// without the hook and report through the orchestration observer instead.
    async fn announce_start(&self) {
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

fn describe(remotes: &BTreeMap<String, RemoteAgent>) -> String {
    let mut text = String::from(
        "Ask a remote AURA agent to handle a request and wait for its answer. The remote \
         agent runs its own tools and returns a text response. It does not see this \
         conversation, so the prompt must be self-contained. Available agents:",
    );
    for (name, remote) in remotes {
        text.push_str("\n- ");
        text.push_str(name);
        if let Some(description) = remote.description.as_deref().filter(|d| !d.is_empty()) {
            text.push_str(": ");
            text.push_str(description);
        }
    }
    text
}

fn parameters(remotes: &BTreeMap<String, RemoteAgent>) -> Value {
    let names: Vec<&str> = remotes.keys().map(String::as_str).collect();
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
            },
            "model": {
                "type": "string",
                "description": "Agent name or alias to select on the remote, overriding \
                                the configured default."
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let outcome: Value = serde_json::from_str(&out).expect("outcome is JSON");
        assert_eq!(outcome["agent"], "dev");
        assert_eq!(outcome["state"], "completed");
        assert_eq!(outcome["task_id"], "task-1");
        assert_eq!(outcome["context_id"], "ctx-new");
        assert_eq!(outcome["response"], "42 is the answer");
        assert_eq!(server.methods(), ["SendMessage", "GetTask", "GetTask"]);
    }

    #[tokio::test]
    async fn model_argument_overrides_the_configured_model() {
        let server = completing_server(1).await;
        let tool = RemoteAgentTool::new(vec![remote("dev", &server, Some("default"))], None);

        tool.call(json!({ "agent": "dev", "prompt": "x" }))
            .await
            .unwrap();
        tool.call(
            json!({ "agent": "dev", "prompt": "x", "model": "verifier", "context_id": "ctx-7" }),
        )
        .await
        .unwrap();

        let sends: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.method() == "SendMessage")
            .collect();
        assert_eq!(sends[0].header_values("x-aura-model"), vec!["default"]);
        assert_eq!(sends[1].header_values("x-aura-model"), vec!["verifier"]);
        assert_eq!(
            sends[1].body_json()["params"]["message"]["contextId"],
            "ctx-7"
        );
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
        let outcome: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(outcome["state"], "completed");
        assert_eq!(outcome["context_id"], "ctx-m");
        assert_eq!(outcome["response"], "instant answer");
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
            "Tool returned an error: remote agent \"dev\" task t9 ended in state failed: LLM quota exceeded"
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
        let outcome: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(outcome["state"], "input_required");
        assert_eq!(outcome["context_id"], "c2");
        assert_eq!(outcome["response"], "Which cluster?");
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
}
