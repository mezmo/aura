//! How the runtime turns a configured agent into one that can run.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use aura_config::Config;
use aura_events::{RunId, SessionId};

use crate::builder::{ClientTool, PreparedAgent, RunToolFactory};
use crate::error::BuilderError;
use crate::hitl::{PendingApprovals, WebhookHmac};
use crate::rig_builder::RigBuilder;
use crate::skill_tool::SkillInvocationRecorder;
use crate::streaming::StreamingAgent;

use super::StartError;

/// An agent ready to serve a run.
pub(crate) enum PreparedForRun {
    /// A single agent's reusable half.
    Prepared(Arc<PreparedAgent>),
    /// An agent built for this run only, such as an orchestrator, which has
    /// no reusable half.
    Built(Arc<dyn StreamingAgent>),
}

/// What the preparer needs from a start to prepare its agent.
pub(crate) struct PrepareRequest<'a> {
    pub session: &'a SessionId,
    pub run_id: RunId,
    pub req_headers: Option<&'a HashMap<String, String>>,
    pub client_tools: Option<Vec<ClientTool>>,
    pub run_tools: RunToolFactory,
    pub skill_recorder: Option<Arc<SkillInvocationRecorder>>,
}

/// Resolves and prepares the agents the runtime runs.
#[async_trait]
pub(crate) trait AgentPreparer: Send + Sync {
    /// The config for the agent a start names, or for the default agent
    /// when it names none.
    fn config(&self, agent: Option<&str>) -> Result<Config, StartError>;

    async fn prepare(
        &self,
        config: &Config,
        request: PrepareRequest<'_>,
    ) -> Result<PreparedForRun, BuilderError>;
}

/// Prepares agents from the loaded configs through [`RigBuilder`], the same
/// way the server prepares one for a request.
pub struct RigBuilderPreparer {
    configs: Arc<Vec<Config>>,
    default_agent: Option<String>,
    pending_approvals: PendingApprovals,
    hitl_hmac: Option<WebhookHmac>,
}

impl RigBuilderPreparer {
    pub fn new(configs: Arc<Vec<Config>>, pending_approvals: PendingApprovals) -> Self {
        Self {
            configs,
            default_agent: None,
            pending_approvals,
            hitl_hmac: None,
        }
    }

    /// The agent to run when a start names none and several configs are
    /// loaded.
    #[must_use]
    pub fn with_default_agent(mut self, agent: Option<String>) -> Self {
        self.default_agent = agent;
        self
    }

    /// The HMAC the HITL webhook route signs with.
    #[must_use]
    pub fn with_hitl_hmac(mut self, hmac: Option<WebhookHmac>) -> Self {
        self.hitl_hmac = hmac;
        self
    }

    pub(crate) fn pending_approvals(&self) -> &PendingApprovals {
        &self.pending_approvals
    }
}

#[async_trait]
impl AgentPreparer for RigBuilderPreparer {
    /// With one config loaded, that config serves any name, just as a
    /// one-agent server answers any `model`. Otherwise the name the start
    /// gives, or the default agent when it gives none, must match a config's
    /// `agent_id`.
    fn config(&self, agent: Option<&str>) -> Result<Config, StartError> {
        if let [only] = self.configs.as_slice() {
            return Ok(only.clone());
        }
        let name = agent
            .or(self.default_agent.as_deref())
            .ok_or(StartError::AgentRequired)?;
        self.configs
            .iter()
            .find(|config| config.agent_id() == name)
            .cloned()
            .ok_or_else(|| StartError::NoSuchAgent(name.to_owned()))
    }

    /// An orchestrated config is built fresh for the run, and every worker
    /// gets its own set of the run's tools. A single agent is prepared with
    /// the run's tools added to its own. The client's tools are passed
    /// along, and `PreparedAgent::prepare` attaches only those the config
    /// allows.
    async fn prepare(
        &self,
        config: &Config,
        request: PrepareRequest<'_>,
    ) -> Result<PreparedForRun, BuilderError> {
        let builder = RigBuilder::new(config.clone(), self.pending_approvals.clone())
            .with_hitl_hmac(self.hitl_hmac.clone())
            .with_skill_recorder(request.skill_recorder);
        let session = Some(request.session.clone());

        if config.orchestration_enabled() {
            return builder
                .build_streaming_agent_with_tools(
                    request.req_headers,
                    session,
                    request.client_tools,
                    Some(request.run_id),
                    request.run_tools,
                )
                .await
                .map(PreparedForRun::Built);
        }

        builder
            .prepare_agent(
                request.req_headers,
                (request.run_tools)(),
                request.client_tools,
                session,
            )
            .await
            .map(PreparedForRun::Prepared)
    }
}
