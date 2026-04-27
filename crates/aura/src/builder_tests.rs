#[cfg(test)]
mod tests {
    use crate::builder::*;
    use crate::config::*;
    use crate::tools::{FilesystemTool, ReadFileTool, ListDirTool, WriteFileTool};
    use std::collections::HashMap;

    #[test]
    fn test_default_max_depth() {
        assert_eq!(DEFAULT_MAX_DEPTH, 8);
    }

    #[test]
    fn test_is_reasoning_model_o1() {
        assert!(is_reasoning_model("o1"));
        assert!(is_reasoning_model("o1-preview"));
        assert!(is_reasoning_model("o1-mini"));
    }

    #[test]
    fn test_is_reasoning_model_o3() {
        assert!(is_reasoning_model("o3"));
        assert!(is_reasoning_model("o3-mini"));
    }

    #[test]
    fn test_is_reasoning_model_o4() {
        assert!(is_reasoning_model("o4"));
        assert!(is_reasoning_model("o4-preview"));
    }

    #[test]
    fn test_is_reasoning_model_gpt5() {
        assert!(is_reasoning_model("gpt-5"));
        assert!(is_reasoning_model("gpt-5-turbo"));
    }

    #[test]
    fn test_is_reasoning_model_non_reasoning() {
        assert!(!is_reasoning_model("gpt-4"));
        assert!(!is_reasoning_model("gpt-4o"));
        assert!(!is_reasoning_model("gpt-4o-mini"));
        assert!(!is_reasoning_model("gpt-3.5-turbo"));
        assert!(!is_reasoning_model("claude-3-opus"));
        assert!(!is_reasoning_model("gemini-pro"));
    }

    #[test]
    fn test_is_reasoning_model_empty() {
        assert!(!is_reasoning_model(""));
    }

    #[test]
    fn test_is_reasoning_model_case_sensitive() {
        assert!(!is_reasoning_model("O1"));
        assert!(!is_reasoning_model("GPT-5"));
    }

    #[test]
    fn test_build_ollama_params_all_none() {
        let result = build_ollama_params(None, None, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_ollama_params_num_ctx_only() {
        let result = build_ollama_params(Some(4096), None, None);
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(4096));
        assert!(params.get("num_predict").is_none());
    }

    #[test]
    fn test_build_ollama_params_num_predict_only() {
        let result = build_ollama_params(None, Some(2048), None);
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["num_predict"], serde_json::json!(2048));
        assert!(params.get("num_ctx").is_none());
    }

    #[test]
    fn test_build_ollama_params_both_ctx_and_predict() {
        let result = build_ollama_params(Some(4096), Some(2048), None);
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(4096));
        assert_eq!(params["num_predict"], serde_json::json!(2048));
    }

    #[test]
    fn test_build_ollama_params_additional_only() {
        let mut additional = HashMap::new();
        additional.insert("temperature".to_string(), serde_json::json!(0.7));
        additional.insert("top_p".to_string(), serde_json::json!(0.9));
        
        let result = build_ollama_params(None, None, Some(additional));
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["temperature"], serde_json::json!(0.7));
        assert_eq!(params["top_p"], serde_json::json!(0.9));
    }

    #[test]
    fn test_build_ollama_params_all_params() {
        let mut additional = HashMap::new();
        additional.insert("temperature".to_string(), serde_json::json!(0.7));
        
        let result = build_ollama_params(Some(4096), Some(2048), Some(additional));
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(4096));
        assert_eq!(params["num_predict"], serde_json::json!(2048));
        assert_eq!(params["temperature"], serde_json::json!(0.7));
    }

    #[test]
    fn test_build_ollama_params_additional_overwrites() {
        let mut additional = HashMap::new();
        additional.insert("num_ctx".to_string(), serde_json::json!(8192));
        
        let result = build_ollama_params(Some(4096), None, Some(additional));
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(8192));
    }

    #[test]
    fn test_build_ollama_params_zero_values() {
        let result = build_ollama_params(Some(0), Some(0), None);
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(0));
        assert_eq!(params["num_predict"], serde_json::json!(0));
    }

    #[test]
    fn test_build_ollama_params_empty_additional() {
        let additional = HashMap::new();
        let result = build_ollama_params(None, None, Some(additional));
        assert!(result.is_none());
    }

    #[test]
    fn test_build_ollama_params_max_values() {
        let result = build_ollama_params(Some(u32::MAX), Some(u32::MAX), None);
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(u32::MAX));
        assert_eq!(params["num_predict"], serde_json::json!(u32::MAX));
    }

    #[test]
    fn test_filesystem_tools_clone() {
        let read_tool = ReadFileTool(FilesystemTool::new());
        let list_tool = ListDirTool(FilesystemTool::new());
        let write_tool = WriteFileTool(FilesystemTool::new());
        
        let tools = FilesystemTools {
            read_file: read_tool,
            list_dir: list_tool,
            write_file: write_tool,
        };
        
        let _cloned = tools.clone();
    }

    #[test]
    fn test_agent_builder_new() {
        let config = AgentConfig::default();
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_openai() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: Some(0.5),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(5),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_anthropic() {
        let config = AgentConfig {
            llm: LlmConfig::Anthropic {
                api_key: "test-key".to_string(),
                model: "claude-3-opus".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: Some(0.5),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(5),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_bedrock() {
        let config = AgentConfig {
            llm: LlmConfig::Bedrock {
                model: "anthropic.claude-v2".to_string(),
                region: "us-east-1".to_string(),
                profile: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: Some(0.5),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(5),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_gemini() {
        let config = AgentConfig {
            llm: LlmConfig::Gemini {
                api_key: "test-key".to_string(),
                model: "gemini-pro".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: Some(0.5),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(5),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_ollama() {
        let config = AgentConfig {
            llm: LlmConfig::Ollama {
                model: "llama2".to_string(),
                base_url: Some("http://localhost:11434".to_string()),
                max_tokens: None,
                fallback_tool_parsing: false,
                num_ctx: Some(4096),
                num_predict: Some(2048),
                additional_params: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: Some(0.5),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(5),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_custom_base_url() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: Some("https://custom.openai.com".to_string()),
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_reasoning_effort() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "o1-preview".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: Some(ReasoningEffort::High),
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_max_tokens() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: Some(8000),
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: Some(4000),
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_turn_depth() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(10),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_context_window() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: Some(128000),
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_temperature() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: Some(0.9),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_empty_system_prompt() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_context() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec!["context1".to_string(), "context2".to_string()],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_empty_context() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_tools_config() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: Some(ToolsConfig {
                filesystem: true,
                filesystem_write: false,
                custom_tools: vec![],
            }),
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_filesystem_write() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: Some(ToolsConfig {
                filesystem: true,
                filesystem_write: true,
                custom_tools: vec![],
            }),
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_custom_tools() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: Some(ToolsConfig {
                filesystem: false,
                filesystem_write: false,
                custom_tools: vec!["tool1".to_string(), "tool2".to_string()],
            }),
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_ollama_fallback_parsing() {
        let config = AgentConfig {
            llm: LlmConfig::Ollama {
                model: "qwen3-coder".to_string(),
                base_url: None,
                max_tokens: None,
                fallback_tool_parsing: true,
                num_ctx: None,
                num_predict: None,
                additional_params: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_ollama_additional_params() {
        let mut additional = HashMap::new();
        additional.insert("temperature".to_string(), serde_json::json!(0.8));
        
        let config = AgentConfig {
            llm: LlmConfig::Ollama {
                model: "llama2".to_string(),
                base_url: None,
                max_tokens: None,
                fallback_tool_parsing: false,
                num_ctx: None,
                num_predict: None,
                additional_params: Some(additional.clone()),
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_bedrock_profile() {
        let config = AgentConfig {
            llm: LlmConfig::Bedrock {
                model: "anthropic.claude-v2".to_string(),
                region: "us-west-2".to_string(),
                profile: Some("my-profile".to_string()),
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_zero_temperature() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: Some(0.0),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_max_temperature() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: Some(2.0),
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_zero_turn_depth() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(0),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_reasoning_effort_minimal() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "o1".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: Some(ReasoningEffort::Minimal),
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_reasoning_effort_low() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "o1".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: Some(ReasoningEffort::Low),
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_reasoning_effort_medium() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "o1".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: Some(ReasoningEffort::Medium),
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_build_ollama_params_with_nested_json() {
        let mut additional = HashMap::new();
        additional.insert("options".to_string(), serde_json::json!({"key": "value"}));
        
        let result = build_ollama_params(None, None, Some(additional));
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["options"], serde_json::json!({"key": "value"}));
    }

    #[test]
    fn test_build_ollama_params_with_array_value() {
        let mut additional = HashMap::new();
        additional.insert("stop".to_string(), serde_json::json!(["stop1", "stop2"]));
        
        let result = build_ollama_params(None, None, Some(additional));
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["stop"], serde_json::json!(["stop1", "stop2"]));
    }

    #[test]
    fn test_build_ollama_params_with_boolean_value() {
        let mut additional = HashMap::new();
        additional.insert("stream".to_string(), serde_json::json!(true));
        
        let result = build_ollama_params(None, None, Some(additional));
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["stream"], serde_json::json!(true));
    }

    #[test]
    fn test_build_ollama_params_with_null_value() {
        let mut additional = HashMap::new();
        additional.insert("seed".to_string(), serde_json::json!(null));
        
        let result = build_ollama_params(None, None, Some(additional));
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["seed"], serde_json::json!(null));
    }

    #[test]
    fn test_is_reasoning_model_partial_match() {
        assert!(!is_reasoning_model("o"));
        assert!(!is_reasoning_model("gpt"));
        assert!(!is_reasoning_model("gpt-"));
    }

    #[test]
    fn test_is_reasoning_model_with_suffix() {
        assert!(is_reasoning_model("o1-2024-12-17"));
        assert!(is_reasoning_model("gpt-5-turbo-preview"));
    }

    #[test]
    fn test_build_ollama_params_multiple_additional() {
        let mut additional = HashMap::new();
        additional.insert("temperature".to_string(), serde_json::json!(0.7));
        additional.insert("top_p".to_string(), serde_json::json!(0.9));
        additional.insert("top_k".to_string(), serde_json::json!(40));
        additional.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        
        let result = build_ollama_params(None, None, Some(additional));
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params.as_object().unwrap().len(), 4);
    }

    #[test]
    fn test_agent_builder_new_with_unicode_system_prompt() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "你好世界 🌍".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_unicode_agent_name() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "助手".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_build_ollama_params_one_value() {
        let result = build_ollama_params(Some(1), None, None);
        assert!(result.is_some());
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(1));
    }

    #[test]
    fn test_agent_builder_new_with_one_turn_depth() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: Some(1),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_one_max_token() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: Some(1),
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_one_context_window() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Test prompt".to_string(),
                context: vec![],
                temperature: None,
                reasoning_effort: None,
                max_tokens: None,
                turn_depth: None,
                context_window: Some(1),
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }
}
