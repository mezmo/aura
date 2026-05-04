#[cfg(test)]
mod tests {
    use crate::{config::McpServerConfig, load_config_from_str};
    use aura::ReasoningEffort;

    const TEST_CONFIG: &str = r#"
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

[agent.llm]
provider = "openai"
api_key = "test_openai_key"
model = "gpt-4o-mini"
base_url = "https://api.openai.com/v1"
reasoning_effort = "medium"
max_tokens = 1000
context_window = 200000
temperature = 0.5

[agent.llm.additional_params.thinking]
type = "enabled"
budget_tokens = 8000

"#;

    #[test]
    fn test_config_parsing() {
        println!("\n=== TEST_CONFIG_PARSING ===");
        let config = load_config_from_str(TEST_CONFIG).expect("Failed to parse config");

        println!("\n🔍 Full Config Structure:");
        println!("{config:#?}");

        // Test LLM config
        println!("\n✅ Testing LLM config...");
        match &config.agent.llm {
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
                ..
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

[agent.llm]
provider = "anthropic"
api_key = "test_key"
model = "claude-3-sonnet-20240229"

"#;

        let config = load_config_from_str(minimal_config).expect("Failed to parse minimal config");

        println!("\n🔍 Minimal Config Structure:");
        println!("{config:#?}");

        println!("\n✅ Testing minimal config assertions...");
        match &config.agent.llm {
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

[agent.llm]
provider = "openai"
api_key = ""
model = "gpt-4"

"#;

        let result = load_config_from_str(invalid_config);
        assert!(result.is_err());

        // Test config with missing embedding API key (should fail validation)
        let invalid_config2 = r#"
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

[agent.llm]
provider = "openai"
api_key = "valid_key"
model = "gpt-4"

"#;

        let result2 = load_config_from_str(invalid_config2);
        assert!(result2.is_err());
    }

    #[test]
    fn test_environment_variable_placeholders() {
        let _env_lock = crate::test_env_lock::lock();
        println!("\n=== TEST_ENVIRONMENT_VARIABLE_PLACEHOLDERS ===");
        let env_config = r#"
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

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-4"

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
        match &config.agent.llm {
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

[agent.llm]
provider = "ollama"
model = "llama3.2"

"#;

        let config = load_config_from_str(ollama_config).expect("Failed to parse Ollama config");

        println!("\n🔍 Ollama Config Structure:");
        println!("{config:#?}");

        println!("\n✅ Testing Ollama config assertions...");
        match &config.agent.llm {
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

[agent.llm]
provider = "ollama"
model = "mistral"
base_url = "http://my-ollama-server:11434"
temperature = 0.8

"#;

        let config =
            load_config_from_str(ollama_config).expect("Failed to parse Ollama config with URL");

        println!("\n🔍 Ollama Config with custom URL:");
        println!("{config:#?}");

        println!("\n✅ Testing Ollama custom URL config assertions...");
        match &config.agent.llm {
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
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "ollama"
model = "llama3.2"

[agent.llm.additional_params]
mirostat = 1
seed = 42
top_k = 40
top_p = 0.9

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");

        println!("\n🔍 Ollama Config with additional_params:");
        println!("{config:#?}");

        match &config.agent.llm {
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
        let _env_lock = crate::test_env_lock::lock();
        println!("\n=== TEST_OLLAMA_ADDITIONAL_PARAMS_ENV_RESOLUTION ===");
        // Set up test env var
        unsafe {
            std::env::set_var("TEST_SEED", "12345");
        }

        let config_str = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "ollama"
model = "llama3.2"

[agent.llm.additional_params]
seed = "{{ env.TEST_SEED }}"

"#;

        use crate::resolve_env_vars;
        let resolved = resolve_env_vars(config_str).expect("Failed to resolve env vars");
        println!("\n🔍 Resolved TOML content:");
        println!("{resolved}");

        let config =
            crate::config::Config::parse_toml(&resolved).expect("Failed to parse resolved config");

        println!("\n🔍 Config after env var resolution:");
        println!("{config:#?}");

        match &config.agent.llm {
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
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "ollama"
model = "llama3.2"

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");

        println!("\n🔍 Backwards compatible Ollama Config:");
        println!("{config:#?}");

        match &config.agent.llm {
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
[agent]
name = "Test"
alias = "my-alias"
system_prompt = "Test"

[agent.llm]
provider = "ollama"
model = "llama3.2"

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.alias, Some("my-alias".to_string()));
    }

    #[test]
    fn test_alias_field_defaults_to_none() {
        let config_str = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "ollama"
model = "llama3.2"

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
[agent]
name = "Agent1"
system_prompt = "Hello"

[agent.llm]
provider = "ollama"
model = "llama3.2"

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
[agent]
name = "{agent_name}"
system_prompt = "Hello"

[agent.llm]
provider = "ollama"
model = "llama3.2"

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
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "ollama"
model = "llama3.2"

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
[agent]
name = "Test"
system_prompt = "Test"
created_at = 1677649963000

[agent.llm]
provider = "ollama"
model = "llama3.2"

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.created_at, 1677649963000);
    }

    #[test]
    fn test_model_owner_defaults_to_none() {
        let config_str = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "ollama"
model = "llama3.2"

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.model_owner, None);
    }

    #[test]
    fn test_model_owner_explicit_value() {
        let config_str = r#"
[agent]
name = "Test"
system_prompt = "Test"
model_owner = "mezmo"

[agent.llm]
provider = "ollama"
model = "llama3.2"

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.model_owner, Some("mezmo".to_string()));
    }

    #[test]
    fn test_ollama_config_all_params() {
        println!("\n=== TEST_OLLAMA_CONFIG_ALL_PARAMS ===");
        let config_str = r#"
[agent]
name = "Full Ollama Agent"
system_prompt = "You are helpful."

[agent.llm]
provider = "ollama"
model = "llama3.2"
base_url = "http://localhost:11434"
fallback_tool_parsing = true
temperature = 0.7

[agent.llm.additional_params]
num_ctx = 4096
num_predict = 1024
seed = 42

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");

        println!("\n🔍 Ollama Config with all params:");
        println!("{config:#?}");

        match &config.agent.llm {
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
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"
context_window = 200000

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.llm.context_window(), Some(200_000));
    }

    #[test]
    fn test_context_window_defaults_to_none() {
        let config_str = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.llm.context_window(), None);
    }

    #[test]
    fn test_context_window_accepts_float() {
        // Helm renders integers as floats (e.g. 200000.0)
        let config_str = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"
context_window = 200000.0

"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        assert_eq!(config.agent.llm.context_window(), Some(200_000));
    }

    #[test]
    fn test_removed_agent_configs_caught() {
        // The old agent-level fields should now cause the parsing to fail
        // so the user is aware of the breaking changes.
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
            // Inject the removed field inline in [agent], with [agent.llm] after it.
            let config_str = format!(
                "\n[agent]\nname = \"Test\"\nsystem_prompt = \"Test\"\n{definition}\n\n\
                 [agent.llm]\nprovider = \"openai\"\napi_key = \"test_key\"\nmodel = \"gpt-4o\"\n"
            );
            let err_str = match load_config_from_str(&config_str) {
                Ok(_) => panic!(
                    "config parsing should fail due to removed agent-level field: {removed_field}"
                ),
                Err(e) => e.to_string(),
            };
            assert!(
                err_str.contains(expected_error.as_str()),
                "expected error about `{removed_field}`, got: {err_str}"
            );
        }
    }

    #[test]
    fn test_no_additional_properties_on_llm() {
        let config_str = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"
random_field = "should not be accepted"

"#;

        let config = load_config_from_str(config_str);
        assert!(config.is_err(), "no additional fields allowed");
        assert!(
            config
                .unwrap_err()
                .to_string()
                .contains("unknown field `random_field`")
        );
    }

    #[test]
    fn test_worker_without_llm_inherits_from_agent() {
        let config_str = r#"
[agent]
name = "Orchestrator"
system_prompt = "You coordinate."

[agent.llm]
provider = "openai"
api_key = "coordinator_key"
model = "gpt-5.1"
context_window = 128000

[orchestration]
enabled = true

[orchestration.worker.math]
description = "Does math"
preamble = "You do math."
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        let orch = config.orchestration.expect("orchestration should be set");
        let worker = orch.workers.get("math").expect("math worker should exist");
        assert!(
            worker.llm.is_none(),
            "worker without [llm] override should have llm = None"
        );
    }

    #[test]
    fn test_worker_with_explicit_llm_override() {
        let config_str = r#"
[agent]
name = "Orchestrator"
system_prompt = "You coordinate."

[agent.llm]
provider = "openai"
api_key = "coordinator_key"
model = "gpt-5.1"
context_window = 128000

[orchestration]
enabled = true

[orchestration.worker.formatter]
description = "Formats output"
preamble = "You format."

[orchestration.worker.formatter.llm]
provider = "anthropic"
api_key = "worker_key"
model = "claude-haiku-4-5-20251001"
context_window = 200000
"#;
        let config = load_config_from_str(config_str).expect("Failed to parse config");
        let orch = config.orchestration.expect("orchestration should be set");
        let worker = orch
            .workers
            .get("formatter")
            .expect("formatter worker should exist");
        let worker_llm = worker
            .llm
            .as_ref()
            .expect("worker should have explicit llm override");
        match worker_llm {
            aura::config::LlmConfig::Anthropic {
                model,
                context_window,
                ..
            } => {
                assert_eq!(model, "claude-haiku-4-5-20251001");
                assert_eq!(*context_window, Some(200_000));
            }
            _ => panic!("expected Anthropic worker override"),
        }
    }

    #[test]
    fn test_all_shipped_configs_parse() {
        // Set env vars every shipped config expects so env resolution succeeds.
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test-openai");
            std::env::set_var("ANTHROPIC_API_KEY", "test-anthropic");
            std::env::set_var("GOOGLE_API_KEY", "test-google");
            std::env::set_var("MEZMO_API_KEY", "test-mezmo");
            std::env::set_var("AWS_REGION", "us-east-1");
            std::env::set_var("AWS_PROFILE", "default");
            std::env::set_var("LLM_PROVIDER", "openai");
            std::env::set_var("LLM_API_KEY", "test-key");
            std::env::set_var("LLM_MODEL", "gpt-4o");
            std::env::set_var("DD_API_KEY", "test-dd-api");
            std::env::set_var("DD_APPLICATION_KEY", "test-dd-app");
            std::env::set_var("PAGERDUTY_API_TOKEN", "test-pagerduty");
            std::env::set_var("GITHUB_PERSONAL_ACCESS_TOKEN", "test-github");
            std::env::set_var("MCP_TOKEN", "test-mcp");
        }

        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        let dirs = [
            repo_root.join("configs"),
            repo_root.join("examples/minimal"),
            repo_root.join("examples/complete"),
            repo_root.join("examples/quickstart"),
        ];
        let single_files = [
            repo_root.join("examples/reference.toml"),
            repo_root.join("crates/aura-web-server/tests/test-config.toml"),
        ];

        let mut failures = Vec::new();

        for dir in &dirs {
            for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{dir:?}: {e}")) {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                if let Err(e) = crate::load_config(&path) {
                    failures.push(format!("{}: {e}", path.display()));
                }
            }
        }
        for path in &single_files {
            if let Err(e) = crate::load_config(path) {
                failures.push(format!("{}: {e}", path.display()));
            }
        }

        assert!(
            failures.is_empty(),
            "Some shipped configs failed to parse:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn test_legacy_top_level_llm_produces_migration_error() {
        let legacy_config = r#"
[llm]
provider = "openai"
api_key = "test_key"
model = "gpt-4o"

[agent]
name = "Test"
system_prompt = "Test"
"#;
        let err = load_config_from_str(legacy_config)
            .expect_err("legacy top-level [llm] should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("legacy top-level [llm]"),
            "error should mention legacy top-level [llm]: {msg}"
        );
        assert!(
            msg.contains("[agent.llm]"),
            "error should tell the user to move it under [agent.llm]: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // Scratchpad configuration tests
    //
    // Covers:
    //   * Top-level `memory_dir` parsing
    //   * `[agent.scratchpad]` and `[orchestration.worker.<name>.scratchpad]`
    //     TOML round-trip
    //   * `[mcp.servers.<name>.scratchpad]` per-tool thresholds
    //   * Validation rules (`validate_scratchpad`)
    //   * `AgentConfig::effective_memory_dir()` resolution (top-level wins,
    //     legacy `[orchestration.artifacts].memory_dir` as fallback)
    // -----------------------------------------------------------------------

    /// Minimal single-agent config template with `{SP}` placeholder for the
    /// `[agent.scratchpad]` block and `{TOP}` for any top-level keys (e.g.
    /// `memory_dir = "..."`).
    fn single_agent_config(top: &str, scratchpad: &str) -> String {
        format!(
            r#"
{top}

[agent]
name = "Test"
system_prompt = "You are a test agent"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
context_window = 128000

{scratchpad}
"#
        )
    }

    #[test]
    fn scratchpad_top_level_memory_dir_parses() {
        let config = single_agent_config(
            r#"memory_dir = "/tmp/aura-test""#,
            r#"
[agent.scratchpad]
enabled = true
"#,
        );
        let loaded = load_config_from_str(&config).expect("config should parse");
        assert_eq!(loaded.memory_dir.as_deref(), Some("/tmp/aura-test"));
    }

    #[test]
    fn scratchpad_agent_scratchpad_parses_with_defaults() {
        let config = single_agent_config(
            r#"memory_dir = "/tmp/aura-test""#,
            r#"
[agent.scratchpad]
enabled = true
"#,
        );
        let loaded = load_config_from_str(&config).expect("config should parse");
        let sp = loaded
            .agent
            .scratchpad
            .as_ref()
            .expect("agent.scratchpad should be populated");
        assert!(sp.enabled);
        // Defaults are applied for the unspecified fields
        assert!((sp.context_safety_margin - 0.20).abs() < f32::EPSILON);
        assert_eq!(sp.max_extraction_tokens, 10_000);
        assert_eq!(sp.turn_depth_bonus, 6);
    }

    #[test]
    fn scratchpad_agent_scratchpad_parses_all_overrides() {
        let config = single_agent_config(
            r#"memory_dir = "/tmp/aura-test""#,
            r#"
[agent.scratchpad]
enabled = true
context_safety_margin = 0.30
max_extraction_tokens = 5000
turn_depth_bonus = 12
"#,
        );
        let loaded = load_config_from_str(&config).expect("config should parse");
        let sp = loaded.agent.scratchpad.as_ref().unwrap();
        assert!(sp.enabled);
        assert!((sp.context_safety_margin - 0.30).abs() < f32::EPSILON);
        assert_eq!(sp.max_extraction_tokens, 5000);
        assert_eq!(sp.turn_depth_bonus, 12);
    }

    #[test]
    fn scratchpad_worker_override_parses() {
        let config = r#"
memory_dir = "/tmp/aura"

[agent]
name = "Coordinator"
system_prompt = "coordinate"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
context_window = 200000

[agent.scratchpad]
enabled = true
max_extraction_tokens = 10000

[orchestration]
enabled = true

[orchestration.worker.tight]
description = "needs less extraction"
preamble = "you are tight"

[orchestration.worker.tight.scratchpad]
enabled = true
max_extraction_tokens = 2000
turn_depth_bonus = 3
"#;
        let loaded = load_config_from_str(config).expect("config should parse");
        let worker = loaded
            .orchestration
            .as_ref()
            .expect("orchestration present")
            .workers
            .get("tight")
            .expect("tight worker present");
        let wsp = worker
            .scratchpad
            .as_ref()
            .expect("worker scratchpad override present");
        assert!(wsp.enabled);
        assert_eq!(wsp.max_extraction_tokens, 2000);
        assert_eq!(wsp.turn_depth_bonus, 3);
    }

    #[test]
    fn scratchpad_mcp_server_thresholds_parse() {
        let config = r#"
memory_dir = "/tmp/aura"

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

[mcp.servers.example]
transport = "http_streamable"
url = "http://localhost:8000/mcp"

[mcp.servers.example.scratchpad]
"*_list_*" = { min_tokens = 512 }
"foo_get_bar" = { min_tokens = 128 }
"defaults_tool" = {}
"#;
        let loaded = load_config_from_str(config).expect("config should parse");
        let mcp = loaded.mcp.as_ref().expect("mcp present");
        let server = mcp.servers.get("example").expect("server present");
        let thresholds = match server {
            McpServerConfig::HttpStreamable { scratchpad, .. }
            | McpServerConfig::Stdio { scratchpad, .. } => scratchpad,
        };
        assert_eq!(thresholds.get("*_list_*").unwrap().min_tokens, 512);
        assert_eq!(thresholds.get("foo_get_bar").unwrap().min_tokens, 128);
        // Empty entry should use default
        assert_eq!(thresholds.get("defaults_tool").unwrap().min_tokens, 5_120);
    }

    #[test]
    fn scratchpad_validation_single_agent_rejects_missing_memory_dir() {
        let config = single_agent_config(
            "", // no top-level memory_dir
            r#"
[agent.scratchpad]
enabled = true
"#,
        );
        let err = load_config_from_str(&config).expect_err("should reject");
        let msg = err.to_string();
        assert!(
            msg.contains("memory_dir"),
            "error should mention memory_dir: {msg}"
        );
    }

    #[test]
    fn scratchpad_validation_single_agent_rejects_missing_context_window() {
        let config = r#"
memory_dir = "/tmp/aura"

[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
# context_window intentionally omitted

[agent.scratchpad]
enabled = true
"#;
        let err = load_config_from_str(config).expect_err("should reject");
        let msg = err.to_string();
        assert!(
            msg.contains("context_window"),
            "error should mention context_window: {msg}"
        );
    }

    #[test]
    fn scratchpad_validation_passes_when_disabled() {
        // No memory_dir, no context_window, but scratchpad disabled → OK
        let config = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"

[agent.scratchpad]
enabled = false
"#;
        load_config_from_str(config).expect("disabled scratchpad should not require memory_dir");
    }

    #[test]
    fn scratchpad_validation_passes_when_unconfigured() {
        // No [agent.scratchpad] at all → OK even without memory_dir
        let config = r#"
[agent]
name = "Test"
system_prompt = "Test"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
"#;
        load_config_from_str(config).expect("unconfigured scratchpad should parse");
    }

    #[test]
    fn scratchpad_validation_orchestration_agent_level_requires_memory_dir() {
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
enabled = true

[orchestration.worker.alpha]
description = "test worker"
preamble = "alpha"
"#;
        let err = load_config_from_str(config).expect_err("should reject");
        let msg = err.to_string();
        assert!(
            msg.contains("memory_dir"),
            "error should mention memory_dir: {msg}"
        );
    }

    #[test]
    fn scratchpad_validation_orchestration_worker_only_requires_memory_dir() {
        // Agent-level scratchpad disabled but worker has it enabled — still
        // requires memory_dir.
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

[orchestration.worker.alpha]
description = "test worker"
preamble = "alpha"

[orchestration.worker.alpha.scratchpad]
enabled = true
"#;
        let err = load_config_from_str(config).expect_err("should reject");
        let msg = err.to_string();
        assert!(
            msg.contains("memory_dir"),
            "error should mention memory_dir: {msg}"
        );
    }

    #[test]
    fn scratchpad_validation_accepts_legacy_artifacts_memory_dir() {
        // Legacy `[orchestration.artifacts].memory_dir` should satisfy the
        // memory_dir requirement for backward compatibility.
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
enabled = true

[orchestration.artifacts]
memory_dir = "/tmp/legacy"

[orchestration.worker.alpha]
description = "test worker"
preamble = "alpha"
"#;
        load_config_from_str(config)
            .expect("legacy [orchestration.artifacts].memory_dir should satisfy the requirement");
    }

    #[test]
    fn scratchpad_validation_worker_llm_override_requires_context_window() {
        // Worker overrides the LLM with one that has no context_window while
        // scratchpad is enabled → error.
        let config = r#"
memory_dir = "/tmp/aura"

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
enabled = true

[orchestration.worker.alpha]
description = "worker with different model"
preamble = "alpha"

[orchestration.worker.alpha.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o-mini"
# no context_window here
"#;
        let err = load_config_from_str(config).expect_err("should reject");
        let msg = err.to_string();
        assert!(
            msg.contains("context_window"),
            "error should mention context_window: {msg}"
        );
        assert!(
            msg.contains("alpha"),
            "error should mention the offending worker name: {msg}"
        );
    }

    #[test]
    fn scratchpad_validation_complete_orchestration_config_passes() {
        let config = r#"
memory_dir = "/tmp/aura"

[agent]
name = "Coord"
system_prompt = "coord"

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
context_window = 200000

[agent.scratchpad]
enabled = true
context_safety_margin = 0.15
max_extraction_tokens = 20000

[orchestration]
enabled = true

[orchestration.worker.analyst]
description = "analyst"
preamble = "you analyze"

[orchestration.worker.analyst.scratchpad]
enabled = true
max_extraction_tokens = 4000
"#;
        load_config_from_str(config).expect("valid orchestration config should parse");
    }

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
        let built = crate::RigBuilder::new(loaded).get_agent_config();
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
        let built = crate::RigBuilder::new(loaded).get_agent_config();
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
        let built = crate::RigBuilder::new(loaded).get_agent_config();
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
        let built = crate::RigBuilder::new(loaded).get_agent_config();
        assert_eq!(built.effective_memory_dir(), None);
    }
}
