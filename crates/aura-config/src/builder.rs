use crate::{config::LocalSkillSource, Config, ConfigError};
use aura::{
    Agent, AgentBuilder, AgentConfig, AgentSettings, EmbeddingModelConfig, LlmConfig, McpConfig,
    McpServerConfig, ReasoningEffort, SkillConfig, ToolsConfig, VectorStoreConfig,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// YAML frontmatter parsed from SKILL.md files.
///
/// Follows the Agent Skills specification (<https://agentskills.io/specification>).
#[derive(Debug, serde::Deserialize)]
struct SkillFrontmatter {
    /// Required: must match the parent directory name, 1-64 chars,
    /// lowercase alphanumeric and hyphens only.
    name: String,
    /// Required: 1-1024 chars describing what the skill does and when to use it.
    description: String,
}

/// Validate a skill name per the Agent Skills specification.
///
/// Rules:
/// - 1-64 characters
/// - Lowercase alphanumeric and hyphens only
/// - Must not start or end with a hyphen
/// - Must not contain consecutive hyphens
fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!(
            "Skill name must be 1-64 characters, got {} characters",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "Skill name '{}' contains invalid characters (only lowercase alphanumeric and hyphens allowed)",
            name
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(format!(
            "Skill name '{}' must not start or end with a hyphen",
            name
        ));
    }
    if name.contains("--") {
        return Err(format!(
            "Skill name '{}' must not contain consecutive hyphens",
            name
        ));
    }
    Ok(())
}

/// Parse YAML frontmatter delimited by `---` from a SKILL.md file.
///
/// Returns the parsed frontmatter struct.
fn parse_skill_frontmatter(content: &str) -> Result<SkillFrontmatter, ConfigError> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(ConfigError::Validation(
            "SKILL.md must start with YAML frontmatter (---)".to_string(),
        ));
    }

    let after_first = &trimmed[3..];
    let closing = after_first.find("---").ok_or_else(|| {
        ConfigError::Validation("SKILL.md frontmatter missing closing ---".to_string())
    })?;

    let yaml_str = &after_first[..closing];

    let frontmatter: SkillFrontmatter = serde_yaml::from_str(yaml_str).map_err(|e| {
        ConfigError::Validation(format!("Failed to parse SKILL.md frontmatter: {e}"))
    })?;

    if frontmatter.description.is_empty() {
        return Err(ConfigError::Validation(
            "SKILL.md frontmatter must include a non-empty 'description' field".to_string(),
        ));
    }

    if frontmatter.description.len() > 1024 {
        return Err(ConfigError::Validation(format!(
            "Skill description exceeds 1024 character limit (got {} characters)",
            frontmatter.description.len()
        )));
    }

    validate_skill_name(&frontmatter.name).map_err(ConfigError::Validation)?;

    Ok(frontmatter)
}

/// Discover skills from local source directories.
///
/// Scans each source directory for subdirectories containing a `SKILL.md` file.
/// The `name` field in frontmatter must match the directory name (per the spec).
fn discover_skills(
    sources: &[LocalSkillSource],
    config_dir: Option<&Path>,
) -> Result<Vec<SkillConfig>, ConfigError> {
    let mut skills = Vec::new();

    for source in sources {
        let source_path = PathBuf::from(&source.source);
        let resolved = if source_path.is_absolute() {
            source_path
        } else if let Some(base) = config_dir {
            base.join(&source_path)
        } else {
            std::env::current_dir()
                .map_err(|e| {
                    ConfigError::Validation(format!("Cannot resolve relative skill path: {e}"))
                })?
                .join(&source_path)
        };

        let resolved = resolved.canonicalize().map_err(|e| {
            ConfigError::Validation(format!(
                "Skill source directory '{}' not found: {e}",
                resolved.display()
            ))
        })?;

        tracing::info!("Discovering skills from: {}", resolved.display());

        let entries = std::fs::read_dir(&resolved).map_err(|e| {
            ConfigError::Validation(format!(
                "Cannot read skill source directory '{}': {e}",
                resolved.display()
            ))
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| {
                ConfigError::Validation(format!("Error reading skill directory entry: {e}"))
            })?;

            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let skill_file = path.join("SKILL.md");
            if !skill_file.exists() {
                tracing::debug!("Skipping directory '{}' (no SKILL.md)", path.display());
                continue;
            }

            let dir_name = entry.file_name().to_string_lossy().to_string();

            let content = std::fs::read_to_string(&skill_file).map_err(|e| {
                ConfigError::Validation(format!(
                    "Failed to read SKILL.md in '{}': {e}",
                    path.display()
                ))
            })?;

            let frontmatter = parse_skill_frontmatter(&content)?;

            // Per spec: name must match the parent directory name
            if frontmatter.name != dir_name {
                return Err(ConfigError::Validation(format!(
                    "Skill name '{}' in SKILL.md does not match directory name '{}'",
                    frontmatter.name, dir_name
                )));
            }

            tracing::info!(
                "  Discovered skill '{}': {}",
                dir_name,
                frontmatter.description
            );

            skills.push(SkillConfig {
                name: dir_name,
                description: frontmatter.description,
                path: path.clone(),
            });
        }
    }

    // Deduplicate: keep the first occurrence, warn on duplicates
    let mut seen: HashMap<String, PathBuf> = HashMap::new();
    skills.retain(|skill| {
        if let Some(existing_path) = seen.get(&skill.name) {
            tracing::warn!(
                "Duplicate skill '{}' found in '{}' (already loaded from '{}'), skipping",
                skill.name,
                skill.path.display(),
                existing_path.display()
            );
            false
        } else {
            seen.insert(skill.name.clone(), skill.path.clone());
            true
        }
    });

    // Sort by name for deterministic ordering
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(skills)
}

pub struct RigBuilder {
    config: Config,
    /// Base directory for resolving relative paths (derived from config file path)
    config_dir: Option<PathBuf>,
}

impl RigBuilder {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            config_dir: None,
        }
    }

    /// Set the base directory for resolving relative paths in config.
    ///
    /// This is typically derived from the config file's parent directory.
    /// Used for resolving relative skill source paths.
    pub fn with_config_dir(mut self, config_dir: PathBuf) -> Self {
        self.config_dir = Some(config_dir);
        self
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

        // Build effective skill sources: explicit config > AURA_SKILLS_DIR env fallback
        let mut skill_sources = self.config.agent.skills.local.clone();
        if skill_sources.is_empty()
            && let Ok(env_dir) = std::env::var("AURA_SKILLS_DIR")
        {
            tracing::info!("Using AURA_SKILLS_DIR fallback: {}", env_dir);
            skill_sources.push(LocalSkillSource { source: env_dir });
        }

        let skills = discover_skills(&skill_sources, self.config_dir.as_deref())?;

        let agent = AgentSettings {
            name: self.config.agent.name.clone(),
            system_prompt: self.config.agent.system_prompt.clone(),
            context: self.config.agent.context.clone(),
            temperature: self.config.agent.temperature,
            reasoning_effort,
            max_tokens: self.config.agent.max_tokens,
            turn_depth: self.config.agent.turn_depth,
            skills,
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
    use std::io::Write;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // validate_skill_name tests
    // -----------------------------------------------------------------------

    #[test]
    fn validate_skill_name_valid() {
        assert!(validate_skill_name("code-review").is_ok());
        assert!(validate_skill_name("a").is_ok());
        assert!(validate_skill_name("my-skill-123").is_ok());
    }

    #[test]
    fn validate_skill_name_empty() {
        let err = validate_skill_name("").unwrap_err();
        assert!(err.contains("1-64 characters"));
    }

    #[test]
    fn validate_skill_name_too_long() {
        let long_name = "a".repeat(65);
        let err = validate_skill_name(&long_name).unwrap_err();
        assert!(err.contains("1-64 characters"));
    }

    #[test]
    fn validate_skill_name_max_length_ok() {
        let name = "a".repeat(64);
        assert!(validate_skill_name(&name).is_ok());
    }

    #[test]
    fn validate_skill_name_uppercase_rejected() {
        let err = validate_skill_name("Code-Review").unwrap_err();
        assert!(err.contains("invalid characters"));
    }

    #[test]
    fn validate_skill_name_leading_hyphen() {
        let err = validate_skill_name("-code").unwrap_err();
        assert!(err.contains("must not start or end with a hyphen"));
    }

    #[test]
    fn validate_skill_name_trailing_hyphen() {
        let err = validate_skill_name("code-").unwrap_err();
        assert!(err.contains("must not start or end with a hyphen"));
    }

    #[test]
    fn validate_skill_name_consecutive_hyphens() {
        let err = validate_skill_name("code--review").unwrap_err();
        assert!(err.contains("consecutive hyphens"));
    }

    #[test]
    fn validate_skill_name_underscore_rejected() {
        let err = validate_skill_name("code_review").unwrap_err();
        assert!(err.contains("invalid characters"));
    }

    // -----------------------------------------------------------------------
    // parse_skill_frontmatter tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frontmatter_valid() {
        let content = "---\nname: my-skill\ndescription: A test skill\n---\n# Body";
        let fm = parse_skill_frontmatter(content).unwrap();
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "A test skill");
    }

    #[test]
    fn parse_frontmatter_missing_opening() {
        let content = "name: my-skill\ndescription: A test skill\n---\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("must start with YAML frontmatter"));
    }

    #[test]
    fn parse_frontmatter_missing_closing() {
        let content = "---\nname: my-skill\ndescription: A test skill\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("missing closing"));
    }

    #[test]
    fn parse_frontmatter_empty_description() {
        let content = "---\nname: my-skill\ndescription: \"\"\n---\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("non-empty 'description'"));
    }

    #[test]
    fn parse_frontmatter_description_too_long() {
        let long_desc = "a".repeat(1025);
        let content = format!("---\nname: my-skill\ndescription: {long_desc}\n---\n# Body");
        let err = parse_skill_frontmatter(&content).unwrap_err();
        assert!(err.to_string().contains("1024 character limit"));
    }

    #[test]
    fn parse_frontmatter_invalid_name() {
        let content = "---\nname: Code-Review\ndescription: A skill\n---\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn parse_frontmatter_missing_name_field() {
        let content = "---\ndescription: A skill\n---\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // -----------------------------------------------------------------------
    // discover_skills tests
    // -----------------------------------------------------------------------

    fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut f = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        write!(
            f,
            "---\nname: {name}\ndescription: {description}\n---\n# {name}\nContent."
        )
        .unwrap();
    }

    #[test]
    fn discover_skills_happy_path() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "alpha", "Alpha skill");
        write_skill(dir.path(), "beta", "Beta skill");

        let sources = vec![LocalSkillSource {
            source: dir.path().to_string_lossy().to_string(),
        }];
        let skills = discover_skills(&sources, None).unwrap();

        assert_eq!(skills.len(), 2);
        // Should be sorted by name
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[1].name, "beta");
    }

    #[test]
    fn discover_skills_skips_non_skill_dirs() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "real-skill", "A real skill");
        // Create a directory without SKILL.md
        std::fs::create_dir_all(dir.path().join("not-a-skill")).unwrap();
        // Create a plain file (not a directory)
        std::fs::write(dir.path().join("readme.md"), "# README").unwrap();

        let sources = vec![LocalSkillSource {
            source: dir.path().to_string_lossy().to_string(),
        }];
        let skills = discover_skills(&sources, None).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "real-skill");
    }

    #[test]
    fn discover_skills_name_mismatch_errors() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("wrong-name");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut f = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        write!(
            f,
            "---\nname: correct-name\ndescription: Mismatched\n---\n# Content"
        )
        .unwrap();

        let sources = vec![LocalSkillSource {
            source: dir.path().to_string_lossy().to_string(),
        }];
        let err = discover_skills(&sources, None).unwrap_err();
        assert!(err.to_string().contains("does not match directory name"));
    }

    #[test]
    fn discover_skills_deduplicates() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        write_skill(dir1.path(), "shared", "First copy");
        write_skill(dir2.path(), "shared", "Second copy");

        let sources = vec![
            LocalSkillSource {
                source: dir1.path().to_string_lossy().to_string(),
            },
            LocalSkillSource {
                source: dir2.path().to_string_lossy().to_string(),
            },
        ];
        let skills = discover_skills(&sources, None).unwrap();

        // Duplicate should be dropped, keeping first
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "First copy");
    }

    #[test]
    fn discover_skills_relative_path_with_config_dir() {
        let base = TempDir::new().unwrap();
        let skills_dir = base.path().join("my-skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        write_skill(&skills_dir, "test-skill", "A test");

        let sources = vec![LocalSkillSource {
            source: "my-skills".to_string(),
        }];
        let skills = discover_skills(&sources, Some(base.path())).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");
    }

    #[test]
    fn discover_skills_nonexistent_source_errors() {
        let sources = vec![LocalSkillSource {
            source: "/nonexistent/path/to/skills".to_string(),
        }];
        let err = discover_skills(&sources, None).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // -----------------------------------------------------------------------
    // resolve_mcp_headers tests
    // -----------------------------------------------------------------------

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
