//! What a caller hands the runtime to start a run, and why a start is refused.

use std::collections::HashMap;
use std::time::Duration;

use aura_events::run::{Liveness, LivenessPolicy, ObserverId};
use aura_events::{RunId, SessionId};
use rig::completion::Message;

use crate::builder::{ClientTool, RunToolFactory, no_run_tools};
use crate::conversation::{Conversation, LANES_PER_CONVERSATION, Landing, LaneNumber};
use crate::session_store::SessionStoreError;

use super::observe::Subscription;

/// A run to start: which agent, where, with what input.
pub struct StartRun {
    /// The agent to run, by its `Config::agent_id`.
    pub agent: Option<String>,
    /// The headers of the request that started the run.
    pub req_headers: Option<HashMap<String, String>>,
    /// Tools the client executes.
    pub client_tools: Option<Vec<ClientTool>>,
    /// Tools the agent executes that exist for this run only.
    pub run_tools: RunToolFactory,
    pub target: RunTarget,
    /// The run this one continues.
    pub continues: Option<RunId>,
    pub prompt: String,
    pub history: Vec<Message>,
    /// The longest the run may take.
    pub timeout: Option<Duration>,
    pub liveness: Liveness,
    /// Who asked for the run.
    pub caller: Option<Caller>,
    /// The run's `agent.stream` span, which the caller creates, or
    /// `Span::none()` for an untraced run.
    pub span: tracing::Span,
}

/// The session a start lands on.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunTarget {
    /// A new session.
    Fresh,
    Session(SessionId),
    /// A lane of the conversation between `chat` and the run's agent.
    Conversation {
        chat: SessionId,
    },
}

/// Who asked for a run, as given by the request that started it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Caller {
    /// The OpenAI-compatible `user` field.
    pub user: Option<String>,
    /// The request's `metadata`.
    pub metadata: Option<HashMap<String, String>>,
}

impl std::fmt::Debug for Caller {
    /// Prints whether a user id is set and how many metadata keys there are,
    /// but never the values, because a user id identifies an end user.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Caller")
            .field("user", &self.user.is_some())
            .field(
                "metadata_keys",
                &self.metadata.as_ref().map_or(0, HashMap::len),
            )
            .finish()
    }
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
            target: RunTarget::Fresh,
            continues: None,
            prompt: prompt.into(),
            history: Vec::new(),
            timeout: None,
            liveness: Liveness::default(),
            caller: None,
            span: tracing::Span::none(),
        }
    }
}

impl std::fmt::Debug for StartRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartRun")
            .field("agent", &self.agent)
            .field("target", &self.target)
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
    /// A claimed start on a session that already has a claimant.
    #[error("observer `{by}` holds the session's claim; a session has one claimant at a time")]
    AlreadyClaimed {
        /// The current claimant.
        by: ObserverId,
    },
    /// Every lane of the conversation is busy.
    #[error("every lane of {conversation} is busy: {}", busy_lanes(lanes.as_ref()))]
    ConversationBusy {
        conversation: Conversation,
        /// Why each lane is busy, by lane number.
        lanes: Box<[BusyLane; LANES_PER_CONVERSATION]>,
    },
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

/// Why a lane could not take a start.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusyLane {
    /// The lane's live run.
    Running {
        run: RunId,
    },
    /// The lane's claimant.
    Claimed {
        by: ObserverId,
    },
    RunningAndClaimed {
        run: RunId,
        by: ObserverId,
    },
}

impl std::fmt::Display for BusyLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running { run } => write!(f, "running `{run}`"),
            Self::Claimed { by } => write!(f, "claimed by observer `{by}`"),
            Self::RunningAndClaimed { run, by } => {
                write!(f, "running `{run}`, claimed by observer `{by}`")
            }
        }
    }
}

/// Each lane by number and why it is busy: "lane 0 running `…`; lane 1 …".
fn busy_lanes(lanes: &[BusyLane]) -> String {
    LaneNumber::all()
        .zip(lanes)
        .map(|(number, busy)| format!("lane {number} {busy}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// A run started with an observer attached to the session it landed on.
#[derive(Debug)]
#[must_use = "dropping the subscription detaches the observer"]
pub struct Admitted {
    pub run: RunId,
    pub landing: Landing,
    pub subscription: Subscription,
}
