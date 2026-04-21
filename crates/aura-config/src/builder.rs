use crate::{Config, ConfigError};
use aura::{
    Agent, AgentConfig, AgentSettings, EmbeddingModelConfig, McpConfig, McpServerConfig,
    OrchestrationConfig, StreamingAgent, ToolsConfig, VectorStoreConfig,
    orchestration::ToolVisibility as AuraToolVisibility,
};
use std::collections::HashMap;
use std::sync::Arc;

pub struct RigBuilder {
    config: Config,
}

impl RigBuilder {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Get the AgentConfig for debugging purposes
    pub fn get_agent_config(&self) -> AgentConfig {
        self.to_agent_config().expect("Failed to convert config")
    }

    fn to_agent_config(&self) -> Result<AgentConfig, ConfigError> {
        let llm = self.config.agent.llm.clone();

        let agent = AgentSettings {
            name: self.config.agent.name.clone(),
            system_prompt: self.config.agent.system_prompt.clone(),
            context: self.config.agent.context.clone(),
            turn_depth: self.config.agent.turn_depth,
            mcp_filter: self.config.agent.mcp_filter.clone(),
            scratchpad: self.config.agent.scratchpad.clone(),
            enable_client_tools: self.config.agent.enable_client_tools,
            client_tool_filter: self.config.agent.client_tool_filter.clone(),
        };

        let vector_stores: Vec<VectorStoreConfig> = self
            .config
            .vector_stores
            .iter()
            .map(|store| VectorStoreConfig {
                name: store.name.clone(),
                store_type: store.store_type.clone(),
                embedding_model: EmbeddingModelConfig {
                    provider: store.embedding_model.provider.clone(),
                    model: store.embedding_model.model.clone(),
                    api_key: store.embedding_model.api_key.clone(),
                    base_url: None,
                },
                connection_string: store.url.clone(),
                url: store.url.clone(),
                collection_name: store.collection_name.clone(),
                context_prefix: store.context_prefix.clone(),
            })
            .collect();

        let mcp = self.config.mcp.as_ref().map(|mcp_config| {
            let servers = mcp_config
                .servers
                .iter()
                .map(|(name, server)| {
                    let converted_server = match server {
                        crate::config::McpServerConfig::Stdio {
                            cmd,
                            args,
                            env,
                            description,
                            scratchpad,
                        } => McpServerConfig::Stdio {
                            cmd: cmd.first().unwrap_or(&String::new()).clone(),
                            args: args.clone(),
                            env: env.clone(),
                            description: description.clone(),
                            scratchpad: scratchpad.clone(),
                        },
                        crate::config::McpServerConfig::HttpStreamable {
                            url,
                            headers,
                            description,
                            headers_from_request,
                            scratchpad,
                        } => McpServerConfig::HttpStreamable {
                            url: url.clone(),
                            headers: headers.clone(),
                            description: description.clone(),
                            headers_from_request: headers_from_request.clone(),
                            scratchpad: scratchpad.clone(),
                        },
                    };
                    (name.clone(), converted_server)
                })
                .collect();

            McpConfig {
                sanitize_schemas: mcp_config.sanitize_schemas,
                servers,
            }
        });

        let tools = self.config.tools.as_ref().map(|tools_config| ToolsConfig {
            filesystem: tools_config.filesystem,
            custom_tools: tools_config.custom_tools.clone(),
        });

        // Convert orchestration config
        let orchestration = self.config.orchestration.as_ref().map(|orch| {
            let workers = orch
                .workers
                .iter()
                .map(|(name, worker)| {
                    (
                        name.clone(),
                        aura::orchestration::WorkerConfig {
                            description: worker.description.clone(),
                            preamble: worker.preamble.clone(),
                            mcp_filter: worker.mcp_filter.clone(),
                            vector_stores: worker.vector_stores.clone(),
                            turn_depth: worker.turn_depth,
                            llm: worker.llm.clone(),
                            scratchpad: worker.scratchpad.clone(),
                        },
                    )
                })
                .collect();

            let tools_in_planning = match orch.tools_in_planning {
                crate::ToolVisibility::None => AuraToolVisibility::None,
                crate::ToolVisibility::Summary => AuraToolVisibility::Summary,
                crate::ToolVisibility::Full => AuraToolVisibility::Full,
            };

            OrchestrationConfig {
                enabled: orch.enabled,
                max_planning_cycles: orch.max_planning_cycles,
                quality_threshold: orch.quality_threshold,
                max_plan_parse_retries: orch.max_plan_parse_retries,
                worker_system_prompt: orch.worker_system_prompt.clone(),
                workers,
                coordinator_vector_stores: orch.coordinator_vector_stores.clone(),
                tools_in_planning,
                max_tools_per_worker: orch.max_tools_per_worker,
                allow_direct_answers: orch.allow_direct_answers,
                allow_clarification: orch.allow_clarification,
                duplicate_call_nudge_threshold: orch.duplicate_call_nudge_threshold,
                duplicate_call_block_threshold: orch.duplicate_call_block_threshold,
                timeouts: aura::orchestration::TimeoutsConfig {
                    per_call_timeout_secs: orch.timeouts.per_call_timeout_secs,
                },
                artifacts: aura::orchestration::ArtifactsConfig {
                    memory_dir: orch.artifacts.memory_dir.clone(),
                    result_artifact_threshold: orch.artifacts.result_artifact_threshold,
                    result_summary_length: orch.artifacts.result_summary_length,
                    session_history_turns: orch.artifacts.session_history_turns,
                },
            }
        });

        Ok(AgentConfig {
            llm,
            agent,
            vector_stores,
            mcp,
            tools,
            memory_dir: self.config.memory_dir.clone(),
            orchestration,
            // Extension fields default to None (set by orchestrator for workers)
            tool_wrapper: None,
            tool_context_factory: None,
            preamble_override: None,
            mcp_filter: None,
            orchestration_persistence: None,
            orchestration_chat_history: None,
            session_id: None,
            scratchpad_tools_config: None,
        })
    }

    /// Build an agent with optional request headers, additional tools, and client-side tools.
    ///
    /// - `req_headers`: HTTP headers for MCP `headers_from_request` resolution. Pass `None` when not in an HTTP context.
    /// - `additional_tools`: Extra rig tools the agent will execute itself (e.g. CLI/library-supplied tools). Pass `vec![]` when none needed.
    /// - `client_tools`: Passthrough tools the LLM may call but the *client* executes. Pass `None` when client-side tools are not in use.
    pub async fn build_agent(
        &self,
        req_headers: Option<&HashMap<String, String>>,
        additional_tools: Vec<Box<dyn aura::ToolDyn>>,
        client_tools: Option<Vec<aura::builder::ClientTool>>,
    ) -> Result<Agent, ConfigError> {
        let mut agent_config = self.to_agent_config()?;
        resolve_mcp_headers(&mut agent_config, req_headers);
        Agent::new(&agent_config, additional_tools, client_tools)
            .await
            .map_err(|e| ConfigError::Validation(format!("Failed to build agent: {e}")))
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
        client_tools: Option<Vec<aura::builder::ClientTool>>,
    ) -> Result<Arc<dyn StreamingAgent>, ConfigError> {
        let mut agent_config = self.to_agent_config()?;
        resolve_mcp_headers(&mut agent_config, req_headers);
        agent_config.session_id = session_id;

        aura::build_streaming_agent(&agent_config, client_tools)
            .await
            .map_err(|e| ConfigError::Validation(format!("Failed to build streaming agent: {e}")))
    }
}

/// Resolve MCP server headers by applying `headers_from_request` mappings from the
/// incoming request. Static TOML `headers` are already loaded in `server_headers`
/// and serve as fallback when request headers are not present.
fn resolve_mcp_headers(
    agent_config: &mut AgentConfig,
    req_headers: Option<&HashMap<String, String>>,
) {
    let empty = HashMap::new();
    let req_headers = req_headers.unwrap_or(&empty);

    let Some(ref mut mcp_config) = agent_config.mcp else {
        return;
    };

    for (server_name, server_config) in mcp_config.servers.iter_mut() {
        let McpServerConfig::HttpStreamable {
            headers: server_headers,
            headers_from_request,
            ..
        } = server_config
        else {
            tracing::debug!(
                "Server '{}': STDIO transport, skipping header injection",
                server_name
            );
            continue;
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
    use std::collections::HashMap;

    /// Build a minimal AgentConfig with one HttpStreamable MCP server.
    fn make_agent_config(
        static_headers: HashMap<String, String>,
        headers_from_request: HashMap<String, String>,
    ) -> AgentConfig {
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

        AgentConfig {
            mcp: Some(McpConfig {
                sanitize_schemas: true,
                servers,
            }),
            ..AgentConfig::default()
        }
    }

    fn get_server_headers(config: &AgentConfig) -> &HashMap<String, String> {
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
        let mut config = AgentConfig::default(); // mcp is None
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
}
