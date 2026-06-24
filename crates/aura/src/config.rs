//! Runtime agent configuration.
//!
//! Pure, serializable config types live in the `aura-config` crate. This module
//! holds the runtime build-context struct (`AgentRuntimeConfig`) that composes
//! those TOML-parsed values with non-serializable runtime fields: tool wrappers,
//! persistence handles, shared chat history, scratchpad runtime state, and the
//! session id.

use crate::hitl::HitlRuntime;
use crate::scratchpad::ScratchpadToolsConfig;
use crate::tool_wrapper::{ToolCallContext, ToolWrapper};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// Re-export the pure config types from `aura-config` so existing
// `crate::config::*` paths keep resolving after the structs moved out. These
// are the single source of truth — `aura` no longer defines its own copies.
pub use aura_config::{
    AgentSettings, EmbeddingConfig, LlmConfig, McpConfig, McpServerConfig, OrchestrationConfig,
    ReasoningEffort, SkillConfig, SkillName, TodoToolsConfig, ToolsConfig, VectorStoreConfig,
    VectorStoreType, glob_match,
};

/// Type alias for tool context factory function.
pub type ToolContextFactory = Arc<dyn Fn(&str) -> ToolCallContext + Send + Sync>;

/// Resolved per-worker skill override.
///
/// Absence from [`AgentRuntimeConfig::worker_skills`] means the worker
/// inherits `[agent.skills]`. There is deliberately no `Inherit` variant and
/// no `Default` impl, so map lookups cannot silently turn "inherit" into
/// "disable" via `unwrap_or_default()`.
#[derive(Debug, Clone)]
pub enum WorkerSkills {
    /// `skills.local = []`: the worker gets no skills.
    Disable,
    /// Discovered skills replacing `[agent.skills]` wholesale (no merging).
    Override(Vec<SkillConfig>),
}

/// Identifier for a chat session — the conversational context an agent runs in.
///
/// Threaded from the web server's `chat_session_id`, but meaningful for any
/// agent run, including library use without the web layer; not every run has
/// one. An opaque, branded string. Serializes as the bare string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Wrap a session-id string. Accepts a `&str` or an owned `String`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the underlying id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Runtime build context for constructing agents.
///
/// This composes the pure TOML-parsed config types from `aura-config` with
/// non-serializable runtime extension points: tool wrappers, persistence
/// handles, shared chat history, scratchpad runtime state, and the session id.
///
/// Note: this is the renamed successor of the old `AgentConfig`. The TOML
/// `[agent]` table shape is now `aura_config::AgentConfig` — a distinct type.
#[derive(Default)]
pub struct AgentRuntimeConfig {
    pub llm: LlmConfig,
    pub agent: AgentSettings,
    pub vector_stores: Vec<VectorStoreConfig>,
    pub mcp: Option<McpConfig>,
    pub tools: Option<ToolsConfig>,
    /// Top-level persistence directory shared by scratchpad and orchestration
    /// artifacts. Scratchpad: `{memory_dir}/scratchpad/` (single-agent) or
    /// `{memory_dir}/{run_id}/iteration-{n}/scratchpad/` (orchestration).
    /// `[orchestration.artifacts].memory_dir` is still honored as a legacy fallback.
    pub memory_dir: Option<String>,
    /// Orchestration mode configuration (multi-agent workflows)
    pub orchestration: Option<OrchestrationConfig>,

    /// Discovered per-worker skill overrides keyed by worker name. Populated
    /// by `RigBuilder` at build time because skill discovery does filesystem
    /// IO and can fail, so it cannot live on the pure config types. A present
    /// key overrides `[agent.skills]` for that worker; an absent key inherits.
    pub worker_skills: std::collections::HashMap<String, WorkerSkills>,

    // === Extension fields ===
    // These allow callers to customize agent building without modifying the builder.
    // The orchestrator uses these to inject tool wrappers and override preambles.
    /// Optional tool wrapper applied to all MCP tools.
    /// When set, all MCP tools are wrapped with this wrapper.
    pub tool_wrapper: Option<Arc<dyn ToolWrapper + Send + Sync>>,

    /// Factory for creating ToolCallContext per tool.
    /// Allows callers to inject metadata (task_id, attempt) into wrapped tool calls.
    pub tool_context_factory: Option<ToolContextFactory>,

    /// Override for system prompt.
    /// When set, this replaces agent.system_prompt entirely.
    pub preamble_override: Option<String>,

    /// Glob patterns for filtering which MCP tools to include.
    /// When set, only tools matching at least one pattern are added.
    /// Supports glob syntax: `*` (any chars), `?` (single char).
    /// Empty or None means all tools are included.
    pub mcp_filter: Option<Vec<String>>,

    /// Shared persistence for injecting `read_artifact` tool into workers.
    /// When set, workers get access to result artifacts via the read_artifact tool.
    pub orchestration_persistence:
        Option<Arc<tokio::sync::Mutex<crate::orchestration::ExecutionPersistence>>>,

    /// Session ID for grouping orchestration runs under a shared namespace.
    /// When set, persistence paths become `{memory_dir}/{session_id}/{run_id}/...`.
    /// Threaded from the web server's `chat_session_id`.
    pub session_id: Option<String>,

    /// Scratchpad storage/budget handed to the 8 exploration tools.
    /// `Some` when scratchpad is wired up for this agent or worker.
    pub scratchpad_tools_config: Option<ScratchpadToolsConfig>,

    /// Shared decision state for worker `submit_result` tool.
    /// When set, workers get the `submit_result` tool for structured output.
    pub orchestration_submit_result: Option<crate::orchestration::SubmitResultDecision>,

    /// Resolved HITL approval runtime (compiled globs + decision route), built
    /// from `[hitl]` once per request and shared with orchestration workers.
    /// `None` disables approval gating.
    pub hitl: Option<HitlRuntime>,

    /// Request id (`req_…`) for this build, used to stamp HITL approval requests
    /// and route their SSE events. Threaded from the web server so the
    /// single-agent and orchestration paths share one value.
    pub request_id: Option<String>,

    /// The `request_approval` tool, pre-built with the appropriate
    /// [`AgentScope`]. Orchestration workers set this in `create_worker` with
    /// `AgentScope::Worker`; single-agent mode sets it in `Agent::new` with
    /// `AgentScope::Single`. `None` when `[hitl]` is not configured.
    ///
    /// [`AgentScope`]: crate::hitl::AgentScope
    pub hitl_request_approval_tool: Option<crate::hitl::RequestApprovalTool>,
}

// Manual Clone implementation because Arc<dyn Trait> fields require special handling
impl Clone for AgentRuntimeConfig {
    fn clone(&self) -> Self {
        Self {
            llm: self.llm.clone(),
            agent: self.agent.clone(),
            vector_stores: self.vector_stores.clone(),
            mcp: self.mcp.clone(),
            tools: self.tools.clone(),
            memory_dir: self.memory_dir.clone(),
            orchestration: self.orchestration.clone(),
            worker_skills: self.worker_skills.clone(),
            // Arc fields clone the Arc (shared reference)
            tool_wrapper: self.tool_wrapper.clone(),
            tool_context_factory: self.tool_context_factory.clone(),
            preamble_override: self.preamble_override.clone(),
            mcp_filter: self.mcp_filter.clone(),
            orchestration_persistence: self.orchestration_persistence.clone(),
            session_id: self.session_id.clone(),
            scratchpad_tools_config: self.scratchpad_tools_config.clone(),
            orchestration_submit_result: self.orchestration_submit_result.clone(),
            hitl: self.hitl.clone(),
            request_id: self.request_id.clone(),
            hitl_request_approval_tool: self.hitl_request_approval_tool.clone(),
        }
    }
}

// Manual Debug implementation because Arc<dyn Trait> fields don't implement Debug
impl std::fmt::Debug for AgentRuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntimeConfig")
            .field("llm", &self.llm)
            .field("agent", &self.agent)
            .field("vector_stores", &self.vector_stores)
            .field("mcp", &self.mcp)
            .field("tools", &self.tools)
            .field("orchestration", &self.orchestration)
            .field(
                "tool_wrapper",
                &self.tool_wrapper.as_ref().map(|_| "<wrapper>"),
            )
            .field(
                "tool_context_factory",
                &self.tool_context_factory.as_ref().map(|_| "<factory>"),
            )
            .field("preamble_override", &self.preamble_override)
            .field("mcp_filter", &self.mcp_filter)
            .field(
                "orchestration_persistence",
                &self
                    .orchestration_persistence
                    .as_ref()
                    .map(|_| "<persistence>"),
            )
            .field("session_id", &self.session_id)
            .field(
                "orchestration_submit_result",
                &self
                    .orchestration_submit_result
                    .as_ref()
                    .map(|_| "<submit_result>"),
            )
            .field("hitl", &self.hitl.as_ref().map(|_| "<hitl>"))
            .field("request_id", &self.request_id)
            .field(
                "hitl_request_approval_tool",
                &self
                    .hitl_request_approval_tool
                    .as_ref()
                    .map(|_| "<request_approval>"),
            )
            .finish()
    }
}

impl AgentRuntimeConfig {
    /// Check if orchestration mode is enabled.
    ///
    /// Returns true if the `[orchestration]` section exists and `enabled = true`.
    /// Used by `build_streaming_agent()` to decide between Agent and Orchestrator.
    pub fn orchestration_enabled(&self) -> bool {
        self.orchestration
            .as_ref()
            .map(|o| o.enabled)
            .unwrap_or(false)
    }

    /// Resolve the effective persistence directory: top-level `memory_dir`,
    /// or `[orchestration.artifacts].memory_dir` as a legacy fallback.
    pub fn effective_memory_dir(&self) -> Option<&str> {
        self.memory_dir
            .as_deref()
            .or_else(|| self.orchestration.as_ref().and_then(|o| o.memory_dir()))
    }

    /// Check if a tool name matches the mcp_filter patterns.
    ///
    /// Returns true if:
    /// - No filter is set (None) - all tools pass
    /// - Filter is empty - all tools pass
    /// - Tool name matches at least one pattern
    ///
    /// Checks the extension field (`self.mcp_filter`, set by orchestrator) first,
    /// then falls back to the TOML-parseable field (`self.agent.mcp_filter`).
    ///
    /// Supports simple glob patterns:
    /// - `*` matches any sequence of characters
    /// - `?` matches any single character
    pub fn tool_matches_filter(&self, tool_name: &str) -> bool {
        let effective = self.mcp_filter.as_ref().or(self.agent.mcp_filter.as_ref());
        match effective {
            None => true,
            Some(patterns) if patterns.is_empty() => true,
            Some(patterns) => patterns.iter().any(|p| glob_match(p, tool_name)),
        }
    }

    /// Check if a client-side tool name matches the configured filter.
    ///
    /// Returns true if:
    /// - No filter is set (None) - all client tools pass
    /// - Filter is empty - all client tools pass
    /// - Tool name matches at least one pattern
    ///
    /// Reads `[agent].client_tool_filter` from the TOML config. Client-side
    /// tools are only supported in single-agent mode.
    pub fn client_tool_matches_filter(&self, tool_name: &str) -> bool {
        match self.agent.client_tool_filter.as_ref() {
            None => true,
            Some(patterns) if patterns.is_empty() => true,
            Some(patterns) => patterns.iter().any(|p| glob_match(p, tool_name)),
        }
    }

    /// Get the effective system prompt for agent building.
    ///
    /// Returns `preamble_override` if set, otherwise `agent.system_prompt`.
    /// This allows callers (like Orchestrator) to customize the preamble
    /// without the builder knowing about orchestration.
    pub fn effective_preamble(&self) -> &str {
        self.preamble_override
            .as_deref()
            .unwrap_or(&self.agent.system_prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_matches_filter_none() {
        let config = AgentRuntimeConfig::default();
        assert!(config.tool_matches_filter("any_tool"));
    }

    #[test]
    fn test_tool_matches_filter_empty() {
        let config = AgentRuntimeConfig {
            mcp_filter: Some(vec![]),
            ..Default::default()
        };
        assert!(config.tool_matches_filter("any_tool"));
    }

    #[test]
    fn test_tool_matches_filter_patterns() {
        let config = AgentRuntimeConfig {
            mcp_filter: Some(vec![
                "mezmo_*".to_string(),
                "QueryKnowledgeBases".to_string(),
            ]),
            ..Default::default()
        };

        assert!(config.tool_matches_filter("mezmo_logs"));
        assert!(config.tool_matches_filter("mezmo_pipelines"));
        assert!(config.tool_matches_filter("QueryKnowledgeBases"));
        assert!(!config.tool_matches_filter("other_tool"));
    }

    #[test]
    fn test_client_tool_matches_filter_none() {
        let config = AgentRuntimeConfig::default();
        assert!(config.client_tool_matches_filter("Read"));
    }

    #[test]
    fn test_client_tool_matches_filter_empty() {
        let config = AgentRuntimeConfig {
            agent: AgentSettings {
                client_tool_filter: Some(vec![]),
                ..AgentSettings::default()
            },
            ..Default::default()
        };
        assert!(config.client_tool_matches_filter("Read"));
    }

    #[test]
    fn test_client_tool_matches_filter_patterns() {
        let config = AgentRuntimeConfig {
            agent: AgentSettings {
                client_tool_filter: Some(vec!["Read".to_string(), "Find*".to_string()]),
                ..AgentSettings::default()
            },
            ..Default::default()
        };
        assert!(config.client_tool_matches_filter("Read"));
        assert!(config.client_tool_matches_filter("FindFiles"));
        assert!(!config.client_tool_matches_filter("Shell"));
    }
}
