#[cfg(test)]
mod tests {
    use crate::builder::*;
    use crate::config::*;
    use std::collections::HashMap;

    #[test]
    fn test_default_max_depth() {
        assert_eq!(DEFAULT_MAX_DEPTH, 8);
    }

    #[test]
    fn test_is_reasoning_model_o1_variants() {
        assert!(is_reasoning_model("o1"));
        assert!(is_reasoning_model("o1-preview"));
        assert!(is_reasoning_model("o1-mini"));
        assert!(is_reasoning_model("o1-2024-12-17"));
    }

    #[test]
    fn test_is_reasoning_model_o3_variants() {
        assert!(is_reasoning_model("o3"));
        assert!(is_reasoning_model("o3-mini"));
        assert!(is_reasoning_model("o3-preview"));
    }

    #[test]
    fn test_is_reasoning_model_o4_variants() {
        assert!(is_reasoning_model("o4"));
        assert!(is_reasoning_model("o4-preview"));
        assert!(is_reasoning_model("o4-turbo"));
    }

    #[test]
    fn test_is_reasoning_model_gpt5_variants() {
        assert!(is_reasoning_model("gpt-5"));
        assert!(is_reasoning_model("gpt-5-turbo"));
        assert!(is_reasoning_model("gpt-5-preview"));
    }

    #[test]
    fn test_is_reasoning_model_non_reasoning_models() {
        assert!(!is_reasoning_model("gpt-4"));
        assert!(!is_reasoning_model("gpt-4o"));
        assert!(!is_reasoning_model("gpt-4o-mini"));
        assert!(!is_reasoning_model("gpt-3.5-turbo"));
        assert!(!is_reasoning_model("claude-3-opus"));
        assert!(!is_reasoning_model("claude-3-sonnet"));
        assert!(!is_reasoning_model("gemini-pro"));
        assert!(!is_reasoning_model("llama2"));
    }

    #[test]
    fn test_is_reasoning_model_empty_string() {
        assert!(!is_reasoning_model(""));
    }

    #[test]
    fn test_is_reasoning_model_case_sensitive() {
        assert!(!is_reasoning_model("O1"));
        assert!(!is_reasoning_model("O3"));
        assert!(!is_reasoning_model("GPT-5"));
        assert!(!is_reasoning_model("Gpt-5"));
    }

    #[test]
    fn test_is_reasoning_model_partial_matches() {
        assert!(!is_reasoning_model("o"));
        assert!(!is_reasoning_model("gpt"));
        assert!(!is_reasoning_model("gpt-"));
        assert!(!is_reasoning_model("gpt-4o1"));
    }

    #[test]
    fn test_build_ollama_params_all_none() {
        let result = build_ollama_params(None, None, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_ollama_params_num_ctx_only() {
        let result = build_ollama_params(Some(4096), None, None);
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(4096));
        assert!(params.get("num_predict").is_none());
    }

    #[test]
    fn test_build_ollama_params_num_predict_only() {
        let result = build_ollama_params(None, Some(2048), None);
        let params = result.unwrap();
        assert_eq!(params["num_predict"], serde_json::json!(2048));
        assert!(params.get("num_ctx").is_none());
    }

    #[test]
    fn test_build_ollama_params_both_ctx_and_predict() {
        let result = build_ollama_params(Some(4096), Some(2048), None);
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
        let params = result.unwrap();
        assert_eq!(params["temperature"], serde_json::json!(0.7));
        assert_eq!(params["top_p"], serde_json::json!(0.9));
        assert!(params.get("num_ctx").is_none());
        assert!(params.get("num_predict").is_none());
    }

    #[test]
    fn test_build_ollama_params_all_params() {
        let mut additional = HashMap::new();
        additional.insert("temperature".to_string(), serde_json::json!(0.7));
        additional.insert("top_k".to_string(), serde_json::json!(40));
        
        let result = build_ollama_params(Some(4096), Some(2048), Some(additional));
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(4096));
        assert_eq!(params["num_predict"], serde_json::json!(2048));
        assert_eq!(params["temperature"], serde_json::json!(0.7));
        assert_eq!(params["top_k"], serde_json::json!(40));
    }

    #[test]
    fn test_build_ollama_params_additional_overwrites_num_ctx() {
        let mut additional = HashMap::new();
        additional.insert("num_ctx".to_string(), serde_json::json!(8192));
        
        let result = build_ollama_params(Some(4096), None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(8192));
    }

    #[test]
    fn test_build_ollama_params_additional_overwrites_num_predict() {
        let mut additional = HashMap::new();
        additional.insert("num_predict".to_string(), serde_json::json!(4096));
        
        let result = build_ollama_params(None, Some(2048), Some(additional));
        let params = result.unwrap();
        assert_eq!(params["num_predict"], serde_json::json!(4096));
    }

    #[test]
    fn test_build_ollama_params_zero_values() {
        let result = build_ollama_params(Some(0), Some(0), None);
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
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(u32::MAX));
        assert_eq!(params["num_predict"], serde_json::json!(u32::MAX));
    }

    #[test]
    fn test_build_ollama_params_with_nested_json() {
        let mut additional = HashMap::new();
        additional.insert("options".to_string(), serde_json::json!({"key": "value", "nested": {"deep": true}}));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["options"]["key"], serde_json::json!("value"));
        assert_eq!(params["options"]["nested"]["deep"], serde_json::json!(true));
    }

    #[test]
    fn test_build_ollama_params_with_array_value() {
        let mut additional = HashMap::new();
        additional.insert("stop".to_string(), serde_json::json!(["stop1", "stop2", "stop3"]));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["stop"], serde_json::json!(["stop1", "stop2", "stop3"]));
    }

    #[test]
    fn test_build_ollama_params_with_boolean_value() {
        let mut additional = HashMap::new();
        additional.insert("stream".to_string(), serde_json::json!(true));
        additional.insert("raw".to_string(), serde_json::json!(false));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["stream"], serde_json::json!(true));
        assert_eq!(params["raw"], serde_json::json!(false));
    }

    #[test]
    fn test_build_ollama_params_with_null_value() {
        let mut additional = HashMap::new();
        additional.insert("seed".to_string(), serde_json::json!(null));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["seed"], serde_json::json!(null));
    }

    #[test]
    fn test_build_ollama_params_with_string_value() {
        let mut additional = HashMap::new();
        additional.insert("format".to_string(), serde_json::json!("json"));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["format"], serde_json::json!("json"));
    }

    #[test]
    fn test_build_ollama_params_with_float_value() {
        let mut additional = HashMap::new();
        additional.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["repeat_penalty"], serde_json::json!(1.1));
    }

    #[test]
    fn test_build_ollama_params_multiple_additional() {
        let mut additional = HashMap::new();
        additional.insert("temperature".to_string(), serde_json::json!(0.7));
        additional.insert("top_p".to_string(), serde_json::json!(0.9));
        additional.insert("top_k".to_string(), serde_json::json!(40));
        additional.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        additional.insert("seed".to_string(), serde_json::json!(42));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        let obj = params.as_object().unwrap();
        assert_eq!(obj.len(), 5);
        assert_eq!(params["temperature"], serde_json::json!(0.7));
        assert_eq!(params["top_p"], serde_json::json!(0.9));
        assert_eq!(params["top_k"], serde_json::json!(40));
        assert_eq!(params["repeat_penalty"], serde_json::json!(1.1));
        assert_eq!(params["seed"], serde_json::json!(42));
    }

    #[test]
    fn test_build_ollama_params_one_value() {
        let result = build_ollama_params(Some(1), None, None);
        let params = result.unwrap();
        assert_eq!(params["num_ctx"], serde_json::json!(1));
    }

    #[test]
    fn test_build_ollama_params_returns_json_object() {
        let result = build_ollama_params(Some(4096), None, None);
        let params = result.unwrap();
        assert!(params.is_object());
    }

    #[test]
    fn test_filesystem_tools_clone() {
        let fs_tool = crate::tools::FilesystemTool::new();
        let tools = FilesystemTools {
            read_file: crate::tools::ReadFileTool(fs_tool.clone()),
            list_dir: crate::tools::ListDirTool(fs_tool.clone()),
            write_file: crate::tools::WriteFileTool(fs_tool),
        };
        
        let cloned = tools.clone();
        assert_eq!(cloned.read_file.0.base_dir, tools.read_file.0.base_dir);
        assert_eq!(cloned.list_dir.0.allow_write, tools.list_dir.0.allow_write);
        assert_eq!(cloned.write_file.0.max_file_size, tools.write_file.0.max_file_size);
    }

    #[test]
    fn test_agent_builder_new_default_config() {
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
    fn test_agent_builder_new_with_reasoning_effort_minimal() {
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
    fn test_agent_builder_new_with_reasoning_effort_high() {
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
                additional_params: Some(additional),
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
    fn test_agent_builder_new_with_empty_api_key() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "".to_string(),
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
    fn test_agent_builder_new_with_empty_model() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "".to_string(),
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
    fn test_agent_builder_new_with_empty_region() {
        let config = AgentConfig {
            llm: LlmConfig::Bedrock {
                model: "anthropic.claude-v2".to_string(),
                region: "".to_string(),
                profile: None,
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
    fn test_agent_builder_new_with_empty_base_url() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: Some("".to_string()),
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
    fn test_agent_builder_new_with_large_turn_depth() {
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
                turn_depth: Some(1000),
                context_window: None,
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_large_context_window() {
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
                context_window: Some(1_000_000),
            },
            vector_stores: vec![],
            mcp: None,
            tools: None,
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_agent_builder_new_with_large_max_tokens() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: Some(100_000),
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
    fn test_agent_builder_new_with_negative_temperature() {
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
                temperature: Some(-0.5),
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
    fn test_agent_builder_new_with_very_high_temperature() {
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
                temperature: Some(100.0),
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
    fn test_agent_builder_new_with_multiline_system_prompt() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "Line 1\nLine 2\nLine 3".to_string(),
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
    fn test_agent_builder_new_with_special_characters_in_system_prompt() {
        let config = AgentConfig {
            llm: LlmConfig::OpenAI {
                api_key: "test-key".to_string(),
                model: "gpt-4".to_string(),
                base_url: None,
                max_tokens: None,
            },
            agent: AgentSettings {
                name: "TestAgent".to_string(),
                system_prompt: "!@#$%^&*()_+-=[]{}|;':\",./<>?".to_string(),
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
    fn test_agent_builder_new_with_many_context_items() {
        let context: Vec<String> = (0..100).map(|i| format!("context{}", i)).collect();
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
                context,
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
    fn test_agent_builder_new_with_many_custom_tools() {
        let tools: Vec<String> = (0..50).map(|i| format!("tool{}", i)).collect();
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
                custom_tools: tools,
            }),
        };
        
        let _builder = AgentBuilder::new(config);
    }

    #[test]
    fn test_build_ollama_params_preserves_order() {
        let mut additional = HashMap::new();
        additional.insert("a".to_string(), serde_json::json!(1));
        additional.insert("b".to_string(), serde_json::json!(2));
        additional.insert("c".to_string(), serde_json::json!(3));
        
        let result = build_ollama_params(Some(100), Some(200), Some(additional));
        let params = result.unwrap();
        let obj = params.as_object().unwrap();
        assert_eq!(obj.len(), 5);
        assert!(obj.contains_key("num_ctx"));
        assert!(obj.contains_key("num_predict"));
        assert!(obj.contains_key("a"));
        assert!(obj.contains_key("b"));
        assert!(obj.contains_key("c"));
    }

    #[test]
    fn test_is_reasoning_model_with_version_numbers() {
        assert!(is_reasoning_model("o1-2024-12-17"));
        assert!(is_reasoning_model("o3-2025-01-01"));
        assert!(is_reasoning_model("gpt-5-2025-preview"));
    }

    #[test]
    fn test_is_reasoning_model_does_not_match_substring() {
        assert!(!is_reasoning_model("model-o1"));
        assert!(!is_reasoning_model("my-gpt-5"));
        assert!(is_reasoning_model("o1x"));
    }

    #[test]
    fn test_build_ollama_params_with_empty_string_values() {
        let mut additional = HashMap::new();
        additional.insert("key".to_string(), serde_json::json!(""));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["key"], serde_json::json!(""));
    }

    #[test]
    fn test_build_ollama_params_with_zero_in_additional() {
        let mut additional = HashMap::new();
        additional.insert("seed".to_string(), serde_json::json!(0));
        
        let result = build_ollama_params(None, None, Some(additional));
        let params = result.unwrap();
        assert_eq!(params["seed"], serde_json::json!(0));
    }
}
