//! High-level builder that converts a parsed [`aura_config::Config`] into a
//! runtime [`Agent`] or streaming agent.
//!
//! This is the bridge between the pure TOML-facing config types in
//! `aura-config` and aura's runtime agent construction. Because the config
//! structs are now shared (no per-field conversion), [`RigBuilder::to_agent_config`]
//! is a straight field copy that layers on the runtime extension fields. It also
//! handles request-scoped MCP header resolution (`headers_from_request`) so the
//! web server can inject per-request credentials into MCP calls.

use crate::builder::{Agent, ClientTool, build_streaming_agent};
use crate::config::{AgentRuntimeConfig, WorkerSkills};
use crate::error::BuilderError;
use crate::streaming::StreamingAgent;
use aura_config::{AgentSettings, Config, McpServerConfig};
use std::collections::HashMap;
use std::sync::Arc;

pub struct RigBuilder {
    config: Config,
}

impl RigBuilder {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Get the runtime [`AgentRuntimeConfig`] (mainly for tests/debugging).
    /// Skips skill discovery; build methods use [`Self::discovered_agent_config`].
    pub fn get_agent_config(&self) -> AgentRuntimeConfig {
        self.to_agent_config()
    }

    /// Project the parsed `Config` into the runtime `AgentRuntimeConfig`.
    ///
    /// The `[agent]` TOML table is split into `AgentSettings` (the runtime
    /// subset) plus the top-level `llm`; all other sections are shared types
    /// and copy across directly. Runtime extension fields start empty and are
    /// populated by callers (e.g. the orchestrator) as needed. Skills start
    /// empty here because discovery does filesystem IO and can fail; the
    /// fallible build paths run it via [`Self::discovered_agent_config`].
    fn to_agent_config(&self) -> AgentRuntimeConfig {
        let agent = AgentSettings {
            name: self.config.agent.name.clone(),
            system_prompt: self.config.agent.system_prompt.clone(),
            context: self.config.agent.context.clone(),
            turn_depth: self.config.agent.turn_depth,
            mcp_filter: self.config.agent.mcp_filter.clone(),
            scratchpad: self.config.agent.scratchpad.clone(),
            enable_client_tools: self.config.agent.enable_client_tools,
            client_tool_filter: self.config.agent.client_tool_filter.clone(),
            skills: Vec::new(),
        };

        AgentRuntimeConfig {
            llm: self.config.agent.llm.clone(),
            agent,
            vector_stores: self.config.vector_stores.clone(),
            mcp: self.config.mcp.clone(),
            tools: self.config.tools.clone(),
            memory_dir: self.config.memory_dir.clone(),
            orchestration: self.config.orchestration.clone(),
            hitl: self
                .config
                .hitl
                .as_ref()
                .map(crate::hitl::HitlRuntime::from_config),
            ..Default::default()
        }
    }

    /// Project the parsed `Config` and run skill discovery.
    ///
    /// Effective skill sources are the explicit `[agent.skills]` config.
    /// Relative sources resolve from the process current working directory.
    fn discovered_agent_config(&self) -> Result<AgentRuntimeConfig, BuilderError> {
        let mut agent_config = self.to_agent_config();

        let skill_sources = self.config.agent.skills.local.clone();
        agent_config.agent.skills = aura_config::skills::discover_skills(&skill_sources)?;

        // Per-worker skill overrides: discover explicit sources only. A worker
        // without a skills key inherits `[agent.skills]` and stays out of the map.
        if let Some(orch) = &self.config.orchestration {
            for (name, worker) in &orch.workers {
                if let Some(worker_skills) = &worker.skills {
                    let discovered = aura_config::skills::discover_skills(&worker_skills.local)?;
                    let override_skills = if discovered.is_empty() {
                        WorkerSkills::Disable
                    } else {
                        WorkerSkills::Override(discovered)
                    };
                    agent_config
                        .worker_skills
                        .insert(name.clone(), override_skills);
                }
            }
        }

        Ok(agent_config)
    }

    /// Build an agent with optional request headers, additional tools, and client-side tools.
    ///
    /// - `req_headers`: HTTP headers for MCP `headers_from_request` resolution. Pass `None` when not in an HTTP context.
    /// - `additional_tools`: Extra rig tools the agent will execute itself (e.g. CLI/library-supplied tools). Pass `vec![]` when none needed.
    /// - `client_tools`: Passthrough tools the LLM may call but the *client* executes. Pass `None` when client-side tools are not in use.
    pub async fn build_agent(
        &self,
        req_headers: Option<&HashMap<String, String>>,
        additional_tools: Vec<Box<dyn rig::tool::ToolDyn>>,
        client_tools: Option<Vec<ClientTool>>,
        request_id: Option<String>,
        session_id: Option<String>,
    ) -> Result<Agent, BuilderError> {
        let mut agent_config = self.discovered_agent_config()?;
        resolve_mcp_headers(&mut agent_config, req_headers);
        agent_config.request_id = request_id;
        agent_config.session_id = session_id;
        Agent::new(&agent_config, additional_tools, client_tools)
            .await
            .map_err(|e| BuilderError::AgentError(format!("Failed to build agent: {e}")))
    }

    /// Build a streaming agent with optional dynamic MCP headers and client-side tools.
    ///
    /// Returns either:
    /// - An `Orchestrator` wrapped as `Arc<dyn StreamingAgent>` if `orchestration.enabled = true`
    /// - A standard `Agent` wrapped as `Arc<dyn StreamingAgent>` otherwise
    ///
    /// `client_tools` is the request-supplied passthrough tool definitions. The orchestrator
    /// attaches them only to the coordinator / workers whose TOML config sets
    /// `enable_client_tools = true`, filtered by `client_tool_filter`. In single-agent mode,
    /// callers should attach client tools via `build_agent` instead.
    pub async fn build_streaming_agent_with_headers(
        &self,
        req_headers: Option<&HashMap<String, String>>,
        session_id: Option<String>,
        client_tools: Option<Vec<ClientTool>>,
        request_id: Option<String>,
    ) -> Result<Arc<dyn StreamingAgent>, BuilderError> {
        let mut agent_config = self.discovered_agent_config()?;
        resolve_mcp_headers(&mut agent_config, req_headers);
        agent_config.session_id = session_id;
        agent_config.request_id = request_id;

        build_streaming_agent(&agent_config, client_tools)
            .await
            .map_err(|e| BuilderError::AgentError(format!("Failed to build streaming agent: {e}")))
    }
}

/// Resolve MCP server headers by applying `headers_from_request` mappings from the
/// incoming request. Static TOML `headers` are already loaded in `server_headers`
/// and serve as fallback when request headers are not present.
fn resolve_mcp_headers(
    agent_config: &mut AgentRuntimeConfig,
    req_headers: Option<&HashMap<String, String>>,
) {
    let empty = HashMap::new();
    let req_headers = req_headers.unwrap_or(&empty);

    let Some(ref mut mcp_config) = agent_config.mcp else {
        return;
    };

    for (server_name, server_config) in mcp_config.servers.iter_mut() {
        let (server_headers, headers_from_request) = match server_config {
            McpServerConfig::HttpStreamable {
                headers,
                headers_from_request,
                ..
            } => (headers, headers_from_request),
            McpServerConfig::Sse {
                headers,
                headers_from_request,
                ..
            } => (headers, headers_from_request),
            McpServerConfig::Stdio { .. } => {
                tracing::debug!(
                    "Server '{}': STDIO transport, skipping header injection",
                    server_name
                );
                continue;
            }
        };

        // Resolve headers_from_request mappings using the incoming request headers.
        // Static TOML headers are already in server_headers; this only overrides
        // when the mapped request header is found.
        for (header_key, req_header_name) in headers_from_request.iter() {
            // Note: HTTP header names are case-insensitive (RFC 7230), so we compare
            // lowercased names. Actix-web lowercases header names, but TOML config
            // values may use any casing.
            let req_header_lower = req_header_name.to_lowercase();
            let Some(value) = req_headers
                .iter()
                .find(|(k, _)| k.to_lowercase() == req_header_lower)
                .map(|(_, v)| v)
            else {
                continue;
            };
            server_headers.insert(header_key.clone(), value.clone());
            tracing::info!(
                "Server '{}': resolved header '{}' from request header '{}'",
                server_name,
                header_key,
                req_header_name
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_config::{McpConfig, load_config_from_str};

    /// Build a minimal AgentRuntimeConfig with one HttpStreamable MCP server.
    fn make_agent_config(
        static_headers: HashMap<String, String>,
        headers_from_request: HashMap<String, String>,
    ) -> AgentRuntimeConfig {
        let mut servers = HashMap::new();
        servers.insert(
            "test_server".to_string(),
            McpServerConfig::HttpStreamable {
                url: "https://example.com/mcp".to_string(),
                headers: static_headers,
                description: None,
                headers_from_request,
                scratchpad: HashMap::new(),
            },
        );

        AgentRuntimeConfig {
            mcp: Some(McpConfig {
                sanitize_schemas: true,
                servers,
            }),
            ..AgentRuntimeConfig::default()
        }
    }

    fn get_server_headers(config: &AgentRuntimeConfig) -> &HashMap<String, String> {
        let mcp = config.mcp.as_ref().unwrap();
        match mcp.servers.get("test_server").unwrap() {
            McpServerConfig::HttpStreamable { headers, .. } => headers,
            _ => panic!("expected HttpStreamable"),
        }
    }

    #[test]
    fn headers_from_request_resolves_from_req_headers() {
        let mut headers_from_request = HashMap::new();
        headers_from_request.insert(
            "x-test-auth-token".to_string(),
            "x-test-mezmo-token".to_string(),
        );

        let mut config = make_agent_config(HashMap::new(), headers_from_request);

        let mut req_headers = HashMap::new();
        req_headers.insert("x-test-mezmo-token".to_string(), "foobar".to_string());

        resolve_mcp_headers(&mut config, Some(&req_headers));

        let headers = get_server_headers(&config);
        assert_eq!(
            headers.get("x-test-auth-token"),
            Some(&"foobar".to_string())
        );
    }

    #[test]
    fn static_headers_preserved_when_no_overrides() {
        let mut static_headers = HashMap::new();
        static_headers.insert("x-static".to_string(), "original".to_string());

        let mut config = make_agent_config(static_headers, HashMap::new());

        resolve_mcp_headers(&mut config, None);

        let headers = get_server_headers(&config);
        assert_eq!(headers.get("x-static"), Some(&"original".to_string()));
    }

    #[test]
    fn headers_from_request_overrides_static_header() {
        let mut static_headers = HashMap::new();
        static_headers.insert("authorization".to_string(), "static-token".to_string());

        let mut headers_from_request = HashMap::new();
        headers_from_request.insert("authorization".to_string(), "x-incoming-auth".to_string());

        let mut config = make_agent_config(static_headers, headers_from_request);

        let mut req_headers = HashMap::new();
        req_headers.insert("x-incoming-auth".to_string(), "dynamic-token".to_string());

        resolve_mcp_headers(&mut config, Some(&req_headers));

        let headers = get_server_headers(&config);
        assert_eq!(
            headers.get("authorization"),
            Some(&"dynamic-token".to_string()),
            "headers_from_request should override static headers"
        );
    }

    #[test]
    fn static_header_used_when_request_header_missing() {
        let mut static_headers = HashMap::new();
        static_headers.insert("authorization".to_string(), "static-fallback".to_string());

        let mut headers_from_request = HashMap::new();
        headers_from_request.insert("authorization".to_string(), "x-incoming-auth".to_string());

        let mut config = make_agent_config(static_headers, headers_from_request);

        // req_headers does NOT contain "x-incoming-auth"
        let req_headers = HashMap::new();

        resolve_mcp_headers(&mut config, Some(&req_headers));

        let headers = get_server_headers(&config);
        assert_eq!(
            headers.get("authorization"),
            Some(&"static-fallback".to_string()),
            "static TOML header should be used when request header is absent"
        );
    }

    #[test]
    fn no_mcp_config_is_noop() {
        let mut config = AgentRuntimeConfig::default(); // mcp is None
        resolve_mcp_headers(&mut config, None);
        assert!(config.mcp.is_none());
    }

    #[test]
    fn headers_from_request_case_insensitive_lookup() {
        // TOML config uses "Authorization" (capitalized) but actix-web lowercases
        // header names to "authorization". The lookup must be case-insensitive.
        let mut headers_from_request = HashMap::new();
        headers_from_request.insert("Authorization".to_string(), "Authorization".to_string());

        let mut config = make_agent_config(HashMap::new(), headers_from_request);

        let mut req_headers = HashMap::new();
        req_headers.insert("authorization".to_string(), "Token my-token".to_string());

        resolve_mcp_headers(&mut config, Some(&req_headers));

        let headers = get_server_headers(&config);
        assert_eq!(
            headers.get("Authorization"),
            Some(&"Token my-token".to_string()),
            "case-insensitive lookup should resolve lowercased request header"
        );
    }

    // ------------------------------------------------------------------
    // effective_memory_dir: top-level memory_dir vs legacy
    // [orchestration.artifacts].memory_dir fallback. These exercise the full
    // load -> RigBuilder -> AgentRuntimeConfig path.
    // ------------------------------------------------------------------

    #[test]
    fn scratchpad_effective_memory_dir_prefers_top_level() {
        // When both top-level memory_dir AND [orchestration.artifacts].memory_dir
        // are set, the top-level one wins.
        let config = r#"
memory_dir = "/tmp/top-level"

[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
context_window = 128000

[orchestration]
enabled = true

[orchestration.artifacts]
memory_dir = "/tmp/legacy"

[orchestration.worker.alpha]
description = "worker"
preamble = "alpha"
"#;
        let loaded = load_config_from_str(config).expect("should parse");
        let built = RigBuilder::new(loaded).get_agent_config();
        assert_eq!(
            built.effective_memory_dir(),
            Some("/tmp/top-level"),
            "top-level memory_dir should win over legacy artifacts.memory_dir"
        );
    }

    #[test]
    fn scratchpad_effective_memory_dir_falls_back_to_legacy() {
        // No top-level memory_dir — should fall back to the legacy one.
        let config = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
context_window = 128000

[orchestration]
enabled = true

[orchestration.artifacts]
memory_dir = "/tmp/legacy"

[orchestration.worker.alpha]
description = "worker"
preamble = "alpha"
"#;
        let loaded = load_config_from_str(config).expect("should parse");
        let built = RigBuilder::new(loaded).get_agent_config();
        assert_eq!(built.effective_memory_dir(), Some("/tmp/legacy"));
    }

    #[test]
    fn scratchpad_effective_memory_dir_falls_back_in_single_agent_mode() {
        // Orchestration section present but disabled. effective_memory_dir()
        // should still honor the legacy artifacts fallback so single-agent
        // scratchpad setup resolves memory_dir the same way orchestration
        // persistence and config validation do.
        let config = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
context_window = 128000

[agent.scratchpad]
enabled = true

[orchestration]
enabled = false

[orchestration.artifacts]
memory_dir = "/tmp/legacy-single-agent"
"#;
        let loaded = load_config_from_str(config).expect("should parse");
        let built = RigBuilder::new(loaded).get_agent_config();
        assert_eq!(
            built.effective_memory_dir(),
            Some("/tmp/legacy-single-agent"),
            "single-agent mode must honor [orchestration.artifacts].memory_dir fallback",
        );
        assert!(
            !built.orchestration_enabled(),
            "orchestration should be disabled in this test",
        );
    }

    #[test]
    fn scratchpad_effective_memory_dir_none_when_unset() {
        let config = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
"#;
        let loaded = load_config_from_str(config).expect("should parse");
        let built = RigBuilder::new(loaded).get_agent_config();
        assert_eq!(built.effective_memory_dir(), None);
    }

    // -----------------------------------------------------------------------
    // per-worker skills discovery tests
    // -----------------------------------------------------------------------

    fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\nContent."),
        )
        .unwrap();
    }

    #[test]
    fn worker_skills_toml_round_trip() {
        // Agent-level skills come from explicit [agent.skills] sources. Workers
        // override, inherit, or disable independently.
        let agent_skills_dir = tempfile::TempDir::new().unwrap();
        write_skill(agent_skills_dir.path(), "agent-skill", "Agent skill");

        let worker_skills_dir = tempfile::TempDir::new().unwrap();
        write_skill(worker_skills_dir.path(), "alpha", "Alpha skill");

        let config_str = format!(
            r#"
[agent]
name = "Orchestrator"
system_prompt = "You coordinate."

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-5.1"

[[agent.skills.local]]
source = '{}'

[orchestration]
enabled = true

[orchestration.worker.skilled]
description = "Has its own skills"
preamble = "You use skills."

[[orchestration.worker.skilled.skills.local]]
source = '{}'

[orchestration.worker.inheriting]
description = "No skills key"
preamble = "You inherit."

[orchestration.worker.disabled]
description = "Explicitly disabled skills"
preamble = "You have none."
skills.local = []
"#,
            agent_skills_dir.path().display(),
            worker_skills_dir.path().display()
        );

        let config = aura_config::Config::parse_toml(&config_str).expect("config should parse");
        let agent_config = RigBuilder::new(config)
            .discovered_agent_config()
            .expect("discovery should succeed");

        match agent_config
            .worker_skills
            .get("skilled")
            .expect("explicit sources should discover skills into the map")
        {
            WorkerSkills::Override(skills) => {
                assert_eq!(skills.len(), 1);
                assert_eq!(skills[0].name, "alpha");
            }
            WorkerSkills::Disable => panic!("explicit sources should override, not disable"),
        }

        assert!(
            !agent_config.worker_skills.contains_key("inheriting"),
            "worker without skills key should inherit (absent) from [agent.skills]"
        );

        assert!(
            matches!(
                agent_config.worker_skills.get("disabled"),
                Some(WorkerSkills::Disable)
            ),
            "skills.local = [] should disable skills, not inherit"
        );

        assert_eq!(agent_config.agent.skills.len(), 1);
        assert_eq!(agent_config.agent.skills[0].name, "agent-skill");
    }

    #[test]
    fn worker_skills_nonexistent_source_fails_conversion() {
        let config_str = r#"
[agent]
name = "Orchestrator"
system_prompt = "You coordinate."

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-5.1"

[orchestration]
enabled = true

[orchestration.worker.broken]
description = "Bad skill source"
preamble = "You fail."

[[orchestration.worker.broken.skills.local]]
source = '/nonexistent/path/to/worker/skills'
"#;
        let config = aura_config::Config::parse_toml(config_str).expect("config should parse");
        let err = RigBuilder::new(config)
            .discovered_agent_config()
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
