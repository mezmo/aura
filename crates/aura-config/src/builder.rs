use crate::{Config, ConfigError};
use aura::{
    Agent, AgentBuilder, AgentConfig, AgentSettings, EmbeddingModelConfig, LlmConfig, McpConfig,
    McpServerConfig, ReasoningEffort, ToolsConfig, VectorStoreConfig,
};
use std::collections::HashMap;

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
        let llm = match &self.config.llm {
            crate::config::LlmConfig::OpenAI {
                api_key,
                model,
                base_url,
            } => LlmConfig::OpenAI {
                api_key: api_key.clone(),
                model: model.clone(),
                base_url: base_url.clone(),
                max_tokens: None,
            },
            crate::config::LlmConfig::Anthropic {
                api_key,
                model,
                base_url,
            } => LlmConfig::Anthropic {
                api_key: api_key.clone(),
                model: model.clone(),
                base_url: base_url.clone(),
                max_tokens: None,
            },
            crate::config::LlmConfig::Bedrock {
                model,
                region,
                profile,
            } => LlmConfig::Bedrock {
                model: model.clone(),
                region: region.clone(),
                profile: profile.clone(),
                max_tokens: None,
            },
            crate::config::LlmConfig::Gemini {
                api_key,
                model,
                base_url,
            } => LlmConfig::Gemini {
                api_key: api_key.clone(),
                model: model.clone(),
                base_url: base_url.clone(),
                max_tokens: None,
            },
            crate::config::LlmConfig::Ollama {
                model,
                base_url,
                fallback_tool_parsing,
                num_ctx,
                num_predict,
                additional_params,
            } => LlmConfig::Ollama {
                model: model.clone(),
                base_url: Some(base_url.clone()),
                max_tokens: None,
                fallback_tool_parsing: *fallback_tool_parsing,
                num_ctx: *num_ctx,
                num_predict: *num_predict,
                additional_params: additional_params.clone(),
            },
        };

        let reasoning_effort = self.config.agent.reasoning_effort.map(|r| match r {
            crate::config::ReasoningEffort::Minimal => ReasoningEffort::Minimal,
            crate::config::ReasoningEffort::Low => ReasoningEffort::Low,
            crate::config::ReasoningEffort::Medium => ReasoningEffort::Medium,
            crate::config::ReasoningEffort::High => ReasoningEffort::High,
        });

        let agent = AgentSettings {
            name: self.config.agent.name.clone(),
            system_prompt: self.config.agent.system_prompt.clone(),
            context: self.config.agent.context.clone(),
            temperature: self.config.agent.temperature,
            reasoning_effort,
            max_tokens: self.config.agent.max_tokens,
            turn_depth: self.config.agent.turn_depth,
            context_window: self.config.agent.context_window,
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
                        } => McpServerConfig::Stdio {
                            cmd: cmd.first().unwrap_or(&String::new()).clone(),
                            args: args.clone(),
                            env: env.clone(),
                            description: description.clone(),
                        },
                        crate::config::McpServerConfig::HttpStreamable {
                            url,
                            headers,
                            description,
                            headers_from_request,
                        } => McpServerConfig::HttpStreamable {
                            url: url.clone(),
                            headers: headers.clone(),
                            description: description.clone(),
                            headers_from_request: headers_from_request.clone(),
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

        Ok(AgentConfig {
            llm,
            agent,
            vector_stores,
            mcp,
            tools,
        })
    }

    pub async fn build_agent(&self) -> Result<Agent, ConfigError> {
        let agent_config = self.to_agent_config()?;
        self.build_from_config(agent_config).await
    }

    pub async fn build_agent_with_headers(
        &self,
        req_headers: Option<&HashMap<String, String>>,
    ) -> Result<Agent, ConfigError> {
        let mut agent_config = self.to_agent_config()?;
        resolve_mcp_headers(&mut agent_config, req_headers);
        self.build_from_config(agent_config).await
    }

    async fn build_from_config(&self, agent_config: AgentConfig) -> Result<Agent, ConfigError> {
        AgentBuilder::new(agent_config)
            .build_agent()
            .await
            .map_err(|e| ConfigError::Validation(format!("Failed to build agent: {e}")))
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
