//! What a caller hands the runtime to start a run, and why a start is refused.

use std::collections::HashMap;
use std::time::Duration;

use aura_events::run::{Liveness, LivenessPolicy};
use aura_events::{RunId, SessionId};
use rig::completion::Message;

use crate::builder::{ClientTool, RunToolFactory, no_run_tools};
use crate::session_store::SessionStoreError;

/// A run to start: which agent, on which session, with what input.
pub struct StartRun {
    /// The agent to run, as `Config::agent_id` names it.
    pub agent: Option<String>,
    /// The headers of the request that started the run.
    pub req_headers: Option<HashMap<String, String>>,
    /// Tools the client executes.
    pub client_tools: Option<Vec<ClientTool>>,
    /// Tools the agent executes that exist for this run only.
    pub run_tools: RunToolFactory,
    pub session: Option<SessionId>,
    /// The run this one continues.
    pub continues: Option<RunId>,
    pub prompt: String,
    pub history: Vec<Message>,
    /// The bound on how long the run takes.
    pub timeout: Option<Duration>,
    pub liveness: Liveness,
}

impl StartRun {
    /// A run of `prompt` on a new session with the default agent, no
    /// history, no extra tools, no timeout, and the default liveness.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            agent: None,
            req_headers: None,
            client_tools: None,
            run_tools: no_run_tools(),
            session: None,
            continues: None,
            prompt: prompt.into(),
            history: Vec::new(),
            timeout: None,
            liveness: Liveness::default(),
        }
    }
}

impl std::fmt::Debug for StartRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartRun")
            .field("agent", &self.agent)
            .field("session", &self.session)
            .field("continues", &self.continues)
            .field("history", &self.history.len())
            .field("timeout", &self.timeout)
            .field("liveness", &self.liveness)
            .finish_non_exhaustive()
    }
}

/// Why the runtime refused to start a run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    #[error("several agents are configured and the start named none")]
    AgentRequired,
    #[error("no agent is configured as `{0}`")]
    NoSuchAgent(String),
    /// The session's live run.
    #[error("the session is already running `{0}`; it runs one run at a time")]
    RunInProgress(RunId),
    #[error("agent `{agent}` cannot honour the {policy:?} liveness policy")]
    PolicyUnsupported {
        policy: LivenessPolicy,
        agent: String,
    },
    #[error("the runtime is shutting down and starts no new runs")]
    ShuttingDown,
    #[error(transparent)]
    Store(#[from] SessionStoreError),
}
