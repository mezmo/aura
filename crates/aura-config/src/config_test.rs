#[cfg(test)]
mod tests {
    use crate::{config::McpServerConfig, load_config_from_str};
    use aura::ReasoningEffort;

    const TEST_CONFIG: &str = r#"
[llm]
provider = "openai"
api_key = "test_openai_key"
model = "gpt-4o-mini"
base_url = "https://api.openai.com/v1"
reasoning_effort = "medium"
max_tokens = 1000
context_window = 200000
temperature = 0.5

[llm.additional_params.thinking]
type = "enabled"
budget_tokens = 8000

[[vector_stores]]
name = "default"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "test_embedding_key"

[mcp.servers.mezmo]
transport = "http_streamable"
url = "https://mcp.mezmo.com/mcp"
headers = { Authorization = "Bearer test_mezmo_key" }
description = "Mezmo MCP server for log analysis and monitoring"

[mcp.servers.mezmo.headers_from_request]
"Authorization" = "x-test-mezmo-token"
"X-Test-Account-ID" = "x-test-account-id"


[mcp.servers.bedrock_kb]
transport = "stdio"
cmd = ["uvx"]
args = ["awslabs.bedrock-kb-retrieval-mcp-server@latest"]
description = "AWS Bedrock Knowledge Base retrieval server for RAG capabilities"

[mcp.servers.bedrock_kb.env]
AWS_PROFILE = "test_profile"
AWS_REGION = "us-east-1"
FASTMCP_LOG_LEVEL = "ERROR"
KB_INCLUSION_TAG_KEY = "mcp_enabled"
BEDROCK_KB_RERANKING_ENABLED = "false"

[tools]
filesystem = true
custom_tools = ["calculator", "web_search"]

[agent]
name = "Test Assistant"
system_prompt = "You are a test assistant."
context = ["Context line 1", "Context line 2"]
"#;

    #[test]
    fn test_config_parsing() {
        println!("\n=== TEST_CONFIG_PARSING ===");
        let config = load_config_from_str(TEST_CONFIG).expect("Failed to parse config");

        println!("\n🔍 Full Config Structure:");
        println!("{config:#?}");

        // Test LLM config
        println!("\n✅ Testing LLM config...");
        match &config.llm {
            aura::config::LlmConfig::OpenAI {
                api_key,
                model,
                base_url,
                reasoning_effort,
                max_tokens,
                context_window,
                temperature,
                additional_params,
            } => {
                assert_eq!(api_key, "test_openai_key");
                assert_eq!(model, "gpt-4o-mini");
                assert_eq!(base_url, &Some("https://api.openai.com/v1".to_string()));
                assert_eq!(reasoning_effort, &Some(ReasoningEffort::Medium));
                assert_eq!(max_tokens, &Some(1000));
                assert_eq!(context_window, &Some(200_000));
                assert_eq!(temperature, &Some(0.5));

                assert!(
                    additional_params.is_some(),
                    "additional_params should be present"
                );
                let params = additional_params.as_ref().unwrap();
                let thinking = params
                    .get("thinking")
                    .expect("thinking params should be present");
                assert_eq!(thinking.get("type"), Some(&serde_json::json!("enabled")));
                assert_eq!(
                    thinking.get("budget_tokens"),
                    Some(&serde_json::json!(8000))
                );
            }
            _ => panic!("Expected OpenAI LLM config"),
        }

        // Test vector store config
        assert!(
            !config.vector_stores.is_empty(),
            "Should have at least one vector store"
        );
        let vector_store = &config.vector_stores[0];
        assert_eq!(vector_store.store_type, "in_memory");
        assert_eq!(vector_store.embedding_model.provider, "openai");
        assert_eq!(vector_store.embedding_model.model, "text-embedding-3-small");
        assert_eq!(vector_store.embedding_model.api_key, "test_embedding_key");

        // Test MCP servers
        println!("\n✅ Testing MCP servers...");
        let mcp_config = config.mcp.expect("MCP config should be present");
        println!("MCP servers count: {}", mcp_config.servers.len());
        for (name, server) in &mcp_config.servers {
            println!("  Server '{name}': {server:?}");
        }
        assert_eq!(mcp_config.servers.len(), 2);

        // Test HTTP Streamable MCP server (Mezmo)
        let mezmo = mcp_config
            .servers
            .get("mezmo")
            .expect("Mezmo server should exist");
        match mezmo {
            McpServerConfig::HttpStreamable {
                url,
                headers,
                description,
                headers_from_request,
                ..
            } => {
                assert_eq!(url, "https://mcp.mezmo.com/mcp");
                assert_eq!(
                    headers.get("Authorization"),
                    Some(&"Bearer test_mezmo_key".to_string())
                );
                assert_eq!(
                    description.as_ref().unwrap(),
                    "Mezmo MCP server for log analysis and monitoring"
                );
                assert_eq!(
                    headers_from_request.get("Authorization"),
                    Some(&"x-test-mezmo-token".to_string())
                );
                assert_eq!(
                    headers_from_request.get("X-Test-Account-ID"),
                    Some(&"x-test-account-id".to_string())
                );
                // Verify original casing is preserved (lowercase keys should not match)
                assert!(
                    headers_from_request.get("authorization").is_none(),
                    "headers_from_request keys should preserve original TOML casing"
                );
                assert!(
                    headers_from_request.get("x-test-account-id").is_none(),
                    "headers_from_request keys should preserve original TOML casing"
                );
            }
            _ => panic!("Mezmo server should be HttpStreamable"),
        }

        // Test STDIO MCP server (Bedrock KB)
        let bedrock = mcp_config
            .servers
            .get("bedrock_kb")
            .expect("Bedrock server should exist");
        match bedrock {
            McpServerConfig::Stdio {
                cmd,
                args,
                env,
                description,
            } => {
                assert_eq!(cmd, &vec!["uvx"]);
                assert_eq!(
                    args,
                    &vec!["awslabs.bedrock-kb-retrieval-mcp-server@latest"]
                );
                assert_eq!(env.get("AWS_PROFILE"), Some(&"test_profile".to_string()));
                assert_eq!(env.get("AWS_REGION"), Some(&"us-east-1".to_string()));
                assert_eq!(env.get("FASTMCP_LOG_LEVEL"), Some(&"ERROR".to_string()));
                assert_eq!(
                    env.get("KB_INCLUSION_TAG_KEY"),
                    Some(&"mcp_enabled".to_string())
                );
                assert_eq!(
                    env.get("BEDROCK_KB_RERANKING_ENABLED"),
                    Some(&"false".to_string())
                );
                assert_eq!(
                    description.as_ref().unwrap(),
                    "AWS Bedrock Knowledge Base retrieval server for RAG capabilities"
                );
            }
            _ => panic!("Bedrock server should be Stdio"),
        }

        // Test tools config
        let tools = config.tools.expect("Tools config should be present");
        assert!(tools.filesystem);
        assert_eq!(tools.custom_tools, vec!["calculator", "web_search"]);

        // Test agent config
        assert_eq!(config.agent.name, "Test Assistant");
        assert_eq!(config.agent.system_prompt, "You are a test assistant.");
        assert_eq!(
            config.agent.context,
            vec!["Context line 1", "Context line 2"]
        );
    }

    #[test]
    fn test_minimal_config() {
        println!("\n=== TEST_MINIMAL_CONFIG ===");
        let minimal_config = r#"
[llm]
provider = "anthropic"
api_key = "test_key"
model = "claude-3-sonnet-20240229"

[[vector_stores]]
name = "default"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "embed_key"

[agent]
name = "Minimal Agent"
system_prompt = "Basic prompt"
"#;

        let config = load_config_from_str(minimal_config).expect("Failed to parse minimal config");

        println!("\n🔍 Minimal Config Structure:");
        println!("{config:#?}");

        println!("\n✅ Testing minimal config assertions...");
        match &config.llm {
            aura::config::LlmConfig::Anthropic {
                model, temperature, ..
            } => {
                assert_eq!(model, "claude-3-sonnet-20240229");
                assert!(temperature.is_none());
            }
            _ => panic!("Expected Anthropic LLM config"),
        }
        assert!(config.mcp.is_none());
        assert!(config.tools.is_none());
        assert_eq!(config.agent.context.len(), 0);
    }

    #[test]
    fn test_config_validation() {
        // Test config with missing API key (should fail validation)
        let invalid_config = r#"
[llm]
provider = "openai"
api_key = ""
model = "gpt-4"

[[vector_stores]]
name = "default"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "valid_key"

[agent]
name = "Test"
system_prompt = "Test"
"#;

        let result = load_config_from_str(invalid_config);
        assert!(result.is_err());

        // Test config with missing embedding API key (should fail validation)
        let invalid_config2 = r#"
[llm]
provider = "openai"
api_key = "valid_key"
model = "gpt-4"

[[vector_stores]]
name = "default"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = ""

[agent]
name = "Test"
system_prompt = "Test"
"#;

        let result2 = load_config_from_str(invalid_config2);
        assert!(result2.is_err());
    }

    #[test]
    fn test_environment_variable_placeholders() {
        println!("\n=== TEST_ENVIRONMENT_VARIABLE_PLACEHOLDERS ===");
        let env_config = r#"
[llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4"

[[vector_stores]]
name = "default"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"

[mcp.servers.test]
transport = "http_streamable"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer {{ env.TEST_API_KEY }}" }

[agent]
name = "Env Test"
system_prompt = "Test with env vars"
"#;

        // This should work with environment variable resolution
        // Note: The actual resolution happens in the env module
        use crate::resolve_env_vars;

        // Mock environment variables for testing
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "mock_openai_key");
            std::env::set_var("TEST_API_KEY", "mock_test_key");
        }

        let resolved = resolve_env_vars(env_config).expect("Failed to resolve env vars");
        println!("\n🔍 Resolved TOML content:");
        println!("{resolved}");

        let config =
            crate::config::Config::parse_toml(&resolved).expect("Failed to parse resolved config");

        println!("\n🔍 Config after env var resolution:");
        println!("{config:#?}");

        println!("\n✅ Testing environment variable resolution...");
        match &config.llm {
            aura::config::LlmConfig::OpenAI { api_key, .. } => {
                assert_eq!(api_key, "mock_openai_key");
            }
            _ => panic!("Expected OpenAI LLM config"),
        }
        assert_eq!(
            config.vector_stores[0].embedding_model.api_key,
            "mock_openai_key"
        );

        let mcp_config = config.mcp.expect("MCP config should be present");
        let test_server = mcp_config
            .servers
            .get("test")
            .expect("Test server should exist");
        match test_server {
            McpServerConfig::HttpStreamable { headers, .. } => {
                assert_eq!(
                    headers.get("Authorization"),
                    Some(&"Bearer mock_test_key".to_string())
                );
            }
            _ => panic!("Test server should be HttpStreamable"),
        }

        // Clean up
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("TEST_API_KEY");
        }
    }

    #[test]
    fn test_ollama_config_minimal() {
        println!("\n=== TEST_OLLAMA_CONFIG_MINIMAL ===");
        // Ollama with default base_url (localhost:11434)
        // Note: Ollama doesn't require an API key
        let ollama_config = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[[vector_stores]]
name = "default"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "test_key"

[agent]
name = "Ollama Agent"
system_prompt = "You are a helpful assistant."
"#;

        let config = load_config_from_str(ollama_config).expect("Failed to parse Ollama config");

        println!("\n🔍 Ollama Config Structure:");
        println!("{config:#?}");

        println!("\n✅ Testing Ollama config assertions...");
        match &config.llm {
            aura::config::LlmConfig::Ollama {
                model, base_url, ..
            } => {
                assert_eq!(model, "llama3.2");
                assert_eq!(base_url, &Some("http://localhost:11434".into())); // default value
            }
            _ => panic!("Expected Ollama LLM config"),
        }
        assert!(config.mcp.is_none());
        assert!(config.tools.is_none());
        assert_eq!(config.agent.name, "Ollama Agent");
    }

    #[test]
    fn test_ollama_config_custom_url() {
        println!("\n=== TEST_OLLAMA_CONFIG_CUSTOM_URL ===");
        // Ollama with custom base_url
        let ollama_config = r#"
[llm]
provider = "ollama"
model = "mistral"
base_url = "http://my-ollama-server:11434"
temperature = 0.8

[[vector_stores]]
name = "default"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "test_key"

[agent]
name = "Remote Ollama Agent"
system_prompt = "You are a remote assistant."
"#;

        let config =
            load_config_from_str(ollama_config).expect("Failed to parse Ollama config with URL");

        println!("\n🔍 Ollama Config with custom URL:");
        println!("{config:#?}");

        println!("\n✅ Testing Ollama custom URL config assertions...");
        match &config.llm {
            aura::config::LlmConfig::Ollama {
                model,
                base_url,
                temperature,
                ..
            } => {
                assert_eq!(model, "mistral");
                assert_eq!(base_url, &Some("http://my-ollama-server:11434".into()));
                assert_eq!(temperature, &Some(0.8));
            }
            _ => panic!("Expected Ollama LLM config"),
        }
    }

    #[test]
    fn test_ollama_config_with_additional_params() {
        println!("\n=== TEST_OLLAMA_CONFIG_WITH_ADDITIONAL_PARAMS ===");
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[llm.additional_params]
mirostat = 1
seed = 42
top_k = 40
top_p = 0.9

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");

        println!("\n🔍 Ollama Config with additional_params:");
        println!("{config:#?}");

        match &config.llm {
            aura::config::LlmConfig::Ollama {
                additional_params, ..
            } => {
                let params = additional_params
                    .as_ref()
                    .expect("additional_params should be set");
                assert_eq!(params.get("mirostat"), Some(&serde_json::json!(1)));
                assert_eq!(params.get("seed"), Some(&serde_json::json!(42)));
                assert_eq!(params.get("top_k"), Some(&serde_json::json!(40)));
                assert_eq!(params.get("top_p"), Some(&serde_json::json!(0.9)));
            }
            _ => panic!("Expected Ollama config"),
        }
    }

    #[test]
    fn test_ollama_additional_params_env_resolution() {
        println!("\n=== TEST_OLLAMA_ADDITIONAL_PARAMS_ENV_RESOLUTION ===");
        // Set up test env var
        unsafe {
            std::env::set_var("TEST_SEED", "12345");
        }

        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[llm.additional_params]
seed = "{{ env.TEST_SEED }}"

[agent]
name = "Test"
system_prompt = "Test"
"#;

        use crate::resolve_env_vars;
        let resolved = resolve_env_vars(config_str).expect("Failed to resolve env vars");
        println!("\n🔍 Resolved TOML content:");
        println!("{resolved}");

        let config =
            crate::config::Config::parse_toml(&resolved).expect("Failed to parse resolved config");

        println!("\n🔍 Config after env var resolution:");
        println!("{config:#?}");

        match &config.llm {
            aura::config::LlmConfig::Ollama {
                additional_params, ..
            } => {
                let params = additional_params
                    .as_ref()
                    .expect("additional_params should be set");
                // Note: env var resolves to a string "12345", not an integer
                assert_eq!(params.get("seed"), Some(&serde_json::json!("12345")));
            }
            _ => panic!("Expected Ollama config"),
        }

        // Clean up
        unsafe {
            std::env::remove_var("TEST_SEED");
        }
    }

    #[test]
    fn test_ollama_config_backwards_compatible() {
        println!("\n=== TEST_OLLAMA_CONFIG_BACKWARDS_COMPATIBLE ===");
        // Minimal Ollama config without any new fields - should still work
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");

        println!("\n🔍 Backwards compatible Ollama Config:");
        println!("{config:#?}");

        match &config.llm {
            aura::config::LlmConfig::Ollama {
                model,
                additional_params,
                ..
            } => {
                assert_eq!(model, "llama3.2");
                assert!(additional_params.is_none());
            }
            _ => panic!("Expected Ollama config"),
        }
    }

    #[test]
    fn test_alias_field_parsing() {
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "Test"
alias = "my-alias"
system_prompt = "Test"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.alias, Some("my-alias".to_string()));
    }

    #[test]
    fn test_alias_field_defaults_to_none() {
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.alias, None);
    }

    #[test]
    fn test_load_config_single_file() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&file_path).unwrap();
        write!(
            f,
            r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "Agent1"
system_prompt = "Hello"
"#
        )
        .unwrap();

        let configs = crate::load_config(&file_path).expect("Failed to load config");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].agent.name, "Agent1");
    }

    #[test]
    fn test_load_config_directory() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        for (name, agent_name) in [("a.toml", "Agent A"), ("b.toml", "Agent B")] {
            let mut f = std::fs::File::create(dir.path().join(name)).unwrap();
            write!(
                f,
                r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "{agent_name}"
system_prompt = "Hello"
"#
            )
            .unwrap();
        }

        // Non-toml files should be ignored
        std::fs::File::create(dir.path().join("readme.md")).unwrap();

        let configs = crate::load_config(dir.path()).expect("Failed to load configs");
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].agent.name, "Agent A");
        assert_eq!(configs[1].agent.name, "Agent B");
    }

    #[test]
    fn test_load_config_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = crate::load_config(dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No .toml configuration files found"));
    }

    #[test]
    fn test_duplicate_alias_validation() {
        use crate::validate_unique_identifiers;
        let mut c1 = crate::Config::default();
        c1.agent.alias = Some("same-alias".to_string());
        let mut c2 = crate::Config::default();
        c2.agent.alias = Some("same-alias".to_string());

        let result = validate_unique_identifiers(&[c1, c2]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unique alias"));
    }

    #[test]
    fn test_duplicate_name_without_alias_validation() {
        use crate::validate_unique_identifiers;
        let mut c1 = crate::Config::default();
        c1.agent.name = "Same Name".to_string();
        let mut c2 = crate::Config::default();
        c2.agent.name = "Same Name".to_string();

        let result = validate_unique_identifiers(&[c1, c2]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("same agent name"));
    }

    #[test]
    fn test_alias_collides_with_name_validation() {
        use crate::validate_unique_identifiers;
        let mut c1 = crate::Config::default();
        c1.agent.name = "MyAgent".to_string();
        // c1 has no alias, so "MyAgent" is its identifier

        let mut c2 = crate::Config::default();
        c2.agent.name = "Other".to_string();
        c2.agent.alias = Some("MyAgent".to_string());

        let result = validate_unique_identifiers(&[c1, c2]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("same agent name"));
    }

    #[test]
    fn test_same_name_with_different_aliases_is_ok() {
        use crate::validate_unique_identifiers;
        let mut c1 = crate::Config::default();
        c1.agent.name = "Same".to_string();
        c1.agent.alias = Some("alias-1".to_string());

        let mut c2 = crate::Config::default();
        c2.agent.name = "Same".to_string();
        c2.agent.alias = Some("alias-2".to_string());

        let result = validate_unique_identifiers(&[c1, c2]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_created_at_defaults_to_current_time() {
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        assert!(
            config.agent.created_at >= before && config.agent.created_at <= after,
            "created_at should default to current time in ms"
        );
    }

    #[test]
    fn test_created_at_explicit_value() {
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "Test"
system_prompt = "Test"
created_at = 1677649963000
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.created_at, 1677649963000);
    }

    #[test]
    fn test_model_owner_defaults_to_none() {
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.model_owner, None);
    }

    #[test]
    fn test_model_owner_explicit_value() {
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"

[agent]
name = "Test"
system_prompt = "Test"
model_owner = "mezmo"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.model_owner, Some("mezmo".to_string()));
    }

    #[test]
    fn test_ollama_config_all_params() {
        println!("\n=== TEST_OLLAMA_CONFIG_ALL_PARAMS ===");
        let config_str = r#"
[llm]
provider = "ollama"
model = "llama3.2"
base_url = "http://localhost:11434"
fallback_tool_parsing = true
temperature = 0.7

[llm.additional_params]
num_ctx = 4096
num_predict = 1024
seed = 42

[agent]
name = "Full Ollama Agent"
system_prompt = "You are helpful."
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");

        println!("\n🔍 Ollama Config with all params:");
        println!("{config:#?}");

        match &config.llm {
            aura::config::LlmConfig::Ollama {
                model,
                base_url,
                fallback_tool_parsing,
                additional_params,
                max_tokens,
                context_window,
                temperature,
            } => {
                assert_eq!(model, "llama3.2");
                assert_eq!(base_url, &Some("http://localhost:11434".into()));
                assert_eq!(*max_tokens, None);
                assert_eq!(*context_window, None);
                assert_eq!(temperature, &Some(0.7));
                assert!(*fallback_tool_parsing);

                let params = additional_params
                    .as_ref()
                    .expect("additional_params should be set");
                assert_eq!(params.get("num_ctx"), Some(&serde_json::json!(4096)));
                assert_eq!(params.get("num_predict"), Some(&serde_json::json!(1024)));
                assert_eq!(params.get("seed"), Some(&serde_json::json!(42)));
            }
            _ => panic!("Expected Ollama config"),
        }
    }

    #[test]
    fn test_context_window_deserializes_from_toml() {
        let config_str = r#"
[llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"
context_window = 200000

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.llm.context_window(), Some(200_000));
    }

    #[test]
    fn test_context_window_defaults_to_none() {
        let config_str = r#"
[llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.llm.context_window(), None);
    }

    #[test]
    fn test_context_window_accepts_float() {
        // Helm renders integers as floats (e.g. 200000.0)
        let config_str = r#"
[llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"
context_window = 200000.0

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.llm.context_window(), Some(200_000));
    }

    #[test]
    fn test_removed_agent_configs_caught() {
        // The old agent-level fields should now cause the parsing to fail
        // so the user is aware of the breaking changes
        let valid_config_str = r#"
[llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"

[agent]
name = "Test"
system_prompt = "Test"
"#;

        let test_cases = [
            ("temperature", "temperature = 0.5"),
            ("max_tokens", "max_tokens = 1000"),
            ("reasoning_effect", "reasoning_effect = \"medium\""),
            ("context_window", "context_window = 200000.0"),
            (
                "additional_params",
                "additional_params = { thinking = { type = \"enabled\", budget_tokens = 8000 } }",
            ),
        ];

        for (removed_field, definition) in test_cases {
            let expected_error = format!("unknown field `{removed_field}`");
            let config_str = String::from(valid_config_str) + "\n" + definition;

            let config = load_config_from_str(&config_str);
            assert!(
                config.is_err(),
                "config parsing should fail due to removed agent-level field"
            );
            assert!(
                config
                    .unwrap_err()
                    .to_string()
                    .contains(expected_error.as_str())
            );
        }
    }

    #[test]
    fn test_no_additional_properties_on_llm() {
        let config_str = r#"
[llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"
random_field = "should not be accepted"

[agent]
name = "Test"
system_prompt = "Test"
"#;

        let config = load_config_from_str(&config_str);
        assert!(config.is_err(), "no additional fields allowed");
        assert!(
            config
                .unwrap_err()
                .to_string()
                .contains("unknown field `random_field`")
        );
    }
}
