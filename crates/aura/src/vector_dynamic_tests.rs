#[cfg(test)]
mod tests {
    use crate::vector_dynamic::*;
    use crate::vector_store::VectorStoreManager;
    use rig::tool::Tool;
    use serde_json::json;
    use std::sync::Arc;

    fn create_test_vector_store(name: &str, context_prefix: Option<String>) -> Arc<VectorStoreManager> {
        Arc::new(VectorStoreManager::new_stub(name, context_prefix))
    }

    #[test]
    fn test_dynamic_vector_search_tool_new() {
        let store = create_test_vector_store("test_store", None);
        let tool = DynamicVectorSearchTool::new(store, "test_store".to_string());
        assert_eq!(tool.name(), "vector_search_test_store");
    }

    #[test]
    fn test_dynamic_vector_search_tool_new_empty_name() {
        let store = create_test_vector_store("", None);
        let tool = DynamicVectorSearchTool::new(store, "".to_string());
        assert_eq!(tool.name(), "vector_search_");
    }

    #[test]
    fn test_dynamic_vector_search_tool_new_unicode_name() {
        let store = create_test_vector_store("テスト", None);
        let tool = DynamicVectorSearchTool::new(store, "テスト".to_string());
        assert_eq!(tool.name(), "vector_search_テスト");
    }

    #[test]
    fn test_dynamic_vector_search_tool_new_name_with_spaces() {
        let store = create_test_vector_store("my store", None);
        let tool = DynamicVectorSearchTool::new(store, "my store".to_string());
        assert_eq!(tool.name(), "vector_search_my store");
    }

    #[test]
    fn test_dynamic_vector_search_tool_new_name_with_special_chars() {
        let store = create_test_vector_store("store-name_123", None);
        let tool = DynamicVectorSearchTool::new(store, "store-name_123".to_string());
        assert_eq!(tool.name(), "vector_search_store-name_123");
    }

    #[test]
    fn test_dynamic_vector_search_tool_clone() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let cloned = tool.clone();
        assert_eq!(cloned.name(), tool.name());
        assert_eq!(cloned.name(), "vector_search_test");
    }

    #[test]
    fn test_dynamic_vector_search_tool_static_name() {
        assert_eq!(DynamicVectorSearchTool::NAME, "dynamic_vector_search_tool");
    }

    #[test]
    fn test_dynamic_vector_search_tool_name_method() {
        let store = create_test_vector_store("my_store", None);
        let tool = DynamicVectorSearchTool::new(store, "my_store".to_string());
        assert_eq!(tool.name(), "vector_search_my_store");
    }

    #[test]
    fn test_dynamic_vector_search_tool_name_method_overrides_static() {
        let store = create_test_vector_store("custom", None);
        let tool = DynamicVectorSearchTool::new(store, "custom".to_string());
        assert_ne!(tool.name(), DynamicVectorSearchTool::NAME);
        assert_eq!(tool.name(), "vector_search_custom");
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_basic() {
        let store = create_test_vector_store("test_store", None);
        let tool = DynamicVectorSearchTool::new(store, "test_store".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_search_test_store");
        assert!(definition.description.contains("test_store"));
        assert!(definition.description.contains("vector store"));
        assert!(definition.description.contains("semantically similar"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_with_context_prefix() {
        let store = create_test_vector_store("kb", Some("Based on the following information from the knowledge base:".to_string()));
        let tool = DynamicVectorSearchTool::new(store, "kb".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("knowledge base"));
        assert!(definition.description.contains("This vector store contains"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_without_context_prefix() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert!(!definition.description.contains("This vector store contains"));
        assert!(definition.description.contains("Search the 'test' vector store"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_context_prefix_transformation() {
        let store = create_test_vector_store("docs", Some("Based on the following information from the documentation:".to_string()));
        let tool = DynamicVectorSearchTool::new(store, "docs".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("documentation"));
        assert!(!definition.description.contains("Based on the following information from the"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_parameters_structure() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.parameters["type"], "object");
        let properties = &definition.parameters["properties"];
        assert!(properties.get("query").is_some());
        assert!(properties.get("limit").is_some());
        assert!(properties.get("min_score").is_some());
        assert!(properties.get("label_filters").is_some());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_query_parameter() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        let query = &definition.parameters["properties"]["query"];
        assert_eq!(query["type"], "string");
        assert!(query["description"].as_str().unwrap().contains("query"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_limit_parameter() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        let limit = &definition.parameters["properties"]["limit"];
        assert_eq!(limit["type"], "integer");
        assert_eq!(limit["default"], 5);
        assert_eq!(limit["minimum"], 1);
        assert_eq!(limit["maximum"], 20);
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_min_score_parameter() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        let min_score = &definition.parameters["properties"]["min_score"];
        assert_eq!(min_score["type"], "number");
        assert_eq!(min_score["default"], 0.5);
        assert_eq!(min_score["minimum"], 0.1);
        assert_eq!(min_score["maximum"], 1.0);
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_label_filters_parameter() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        let label_filters = &definition.parameters["properties"]["label_filters"];
        assert_eq!(label_filters["type"], "array");
        assert_eq!(label_filters["default"], json!([]));
        
        let items = &label_filters["items"];
        assert_eq!(items["type"], "object");
        assert_eq!(items["additionalProperties"], false);
        
        let item_props = &items["properties"];
        assert!(item_props.get("key").is_some());
        assert!(item_props.get("value").is_some());
        assert_eq!(item_props["key"]["type"], "string");
        assert_eq!(item_props["value"]["type"], "string");
        
        let required = items["required"].as_array().unwrap();
        assert!(required.contains(&json!("key")));
        assert!(required.contains(&json!("value")));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_required_fields() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        let required = definition.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
        assert!(required.contains(&json!("limit")));
        assert!(required.contains(&json!("min_score")));
        assert!(required.contains(&json!("label_filters")));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_label_filter_warning() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("IMPORTANT"));
        assert!(definition.description.contains("Only use label_filters if"));
        assert!(definition.description.contains("explicitly mentions"));
        assert!(definition.description.contains("DO NOT guess"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_empty_results() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test query".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, "test query");
        assert_eq!(response.total_found, 0);
        assert_eq!(response.results.len(), 0);
        assert!(response.formatted_results.contains("No results found"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_with_limit() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 10,
            min_score: 0.0,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_with_min_score() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.8,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_with_single_label_filter() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![FilterKV {
                key: "type".to_string(),
                value: "document".to_string(),
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_with_multiple_label_filters() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![
                FilterKV {
                    key: "type".to_string(),
                    value: "document".to_string(),
                },
                FilterKV {
                    key: "status".to_string(),
                    value: "active".to_string(),
                },
                FilterKV {
                    key: "priority".to_string(),
                    value: "high".to_string(),
                },
            ],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_empty_query() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_zero_limit() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 0,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_max_limit() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 20,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_min_score_zero() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.0,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_min_score_one() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 1.0,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_unicode_query() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "Hello 世界 🎉".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, "Hello 世界 🎉");
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_multiline_query() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "line1\nline2\nline3".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_very_long_query() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let long_query = "query ".repeat(1000);
        let args = VectorSearchArgs {
            query: long_query.clone(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, long_query);
    }

    #[test]
    fn test_vector_search_args_serde_minimal() {
        let json = r#"{"query":"test"}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, 5);
        assert_eq!(args.min_score, 0.0);
        assert_eq!(args.label_filters.len(), 0);
    }

    #[test]
    fn test_vector_search_args_serde_all_fields() {
        let json = r#"{"query":"test","limit":10,"min_score":0.7,"label_filters":[{"key":"type","value":"doc"}]}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, 10);
        assert_eq!(args.min_score, 0.7);
        assert_eq!(args.label_filters.len(), 1);
        assert_eq!(args.label_filters[0].key, "type");
        assert_eq!(args.label_filters[0].value, "doc");
    }

    #[test]
    fn test_vector_search_args_default_limit() {
        let json = r#"{"query":"test"}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.limit, 5);
    }

    #[test]
    fn test_vector_search_args_default_min_score() {
        let json = r#"{"query":"test","limit":10}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.min_score, 0.0);
    }

    #[test]
    fn test_vector_search_args_default_label_filters() {
        let json = r#"{"query":"test","limit":10,"min_score":0.5}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.label_filters.len(), 0);
    }

    #[test]
    fn test_vector_search_args_debug() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        let debug_str = format!("{:?}", args);
        assert!(debug_str.contains("VectorSearchArgs"));
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_filter_kv_serde() {
        let filter = FilterKV {
            key: "type".to_string(),
            value: "document".to_string(),
        };
        let json = serde_json::to_string(&filter).unwrap();
        let deserialized: FilterKV = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.key, "type");
        assert_eq!(deserialized.value, "document");
    }

    #[test]
    fn test_filter_kv_clone() {
        let filter = FilterKV {
            key: "status".to_string(),
            value: "active".to_string(),
        };
        let cloned = filter.clone();
        assert_eq!(cloned.key, filter.key);
        assert_eq!(cloned.value, filter.value);
    }

    #[test]
    fn test_filter_kv_debug() {
        let filter = FilterKV {
            key: "key".to_string(),
            value: "value".to_string(),
        };
        let debug_str = format!("{:?}", filter);
        assert!(debug_str.contains("FilterKV"));
        assert!(debug_str.contains("key"));
        assert!(debug_str.contains("value"));
    }

    #[test]
    fn test_filter_kv_empty_strings() {
        let filter = FilterKV {
            key: "".to_string(),
            value: "".to_string(),
        };
        assert_eq!(filter.key, "");
        assert_eq!(filter.value, "");
    }

    #[test]
    fn test_filter_kv_unicode() {
        let filter = FilterKV {
            key: "名前".to_string(),
            value: "値".to_string(),
        };
        assert_eq!(filter.key, "名前");
        assert_eq!(filter.value, "値");
    }

    #[test]
    fn test_parse_value_str_string() {
        let value = parse_value_str("hello");
        assert_eq!(value, json!("hello"));
    }

    #[test]
    fn test_parse_value_str_integer() {
        let value = parse_value_str("42");
        assert_eq!(value, json!(42));
    }

    #[test]
    fn test_parse_value_str_negative_integer() {
        let value = parse_value_str("-42");
        assert_eq!(value, json!(-42));
    }

    #[test]
    fn test_parse_value_str_float() {
        let value = parse_value_str("3.14");
        assert_eq!(value, json!(3.14));
    }

    #[test]
    fn test_parse_value_str_boolean_true() {
        let value = parse_value_str("true");
        assert_eq!(value, json!(true));
    }

    #[test]
    fn test_parse_value_str_boolean_false() {
        let value = parse_value_str("false");
        assert_eq!(value, json!(false));
    }

    #[test]
    fn test_parse_value_str_null() {
        let value = parse_value_str("null");
        assert_eq!(value, json!(null));
    }

    #[test]
    fn test_parse_value_str_json_array() {
        let value = parse_value_str(r#"["a","b","c"]"#);
        assert_eq!(value, json!(["a", "b", "c"]));
    }

    #[test]
    fn test_parse_value_str_json_object() {
        let value = parse_value_str(r#"{"key":"value"}"#);
        assert_eq!(value, json!({"key": "value"}));
    }

    #[test]
    fn test_parse_value_str_invalid_json_fallback_to_string() {
        let value = parse_value_str("not valid json");
        assert_eq!(value, json!("not valid json"));
    }

    #[test]
    fn test_parse_value_str_empty_string() {
        let value = parse_value_str("");
        assert_eq!(value, json!(""));
    }

    #[test]
    fn test_parse_value_str_whitespace() {
        let value = parse_value_str("   ");
        assert_eq!(value, json!("   "));
    }

    #[test]
    fn test_parse_value_str_quoted_string() {
        let value = parse_value_str(r#""hello""#);
        assert_eq!(value, json!("hello"));
    }

    #[test]
    fn test_parse_value_str_unicode() {
        let value = parse_value_str("世界");
        assert_eq!(value, json!("世界"));
    }

    #[test]
    fn test_parse_value_str_emoji() {
        let value = parse_value_str("🎉");
        assert_eq!(value, json!("🎉"));
    }

    #[test]
    fn test_parse_value_str_zero() {
        let value = parse_value_str("0");
        assert_eq!(value, json!(0));
    }

    #[test]
    fn test_parse_value_str_negative_zero() {
        let value = parse_value_str("-0");
        assert!(value.is_number());
        let num = value.as_f64().unwrap();
        assert_eq!(num, 0.0);
    }

    #[test]
    fn test_parse_value_str_scientific_notation() {
        let value = parse_value_str("1e10");
        assert_eq!(value, json!(1e10));
    }

    #[test]
    fn test_default_limit() {
        assert_eq!(default_limit(), 5);
    }

    #[test]
    fn test_vector_search_response_serde() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "No results".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("test"));
        assert!(json.contains("No results"));
    }

    #[test]
    fn test_vector_search_response_debug() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "".to_string(),
        };
        let debug_str = format!("{:?}", response);
        assert!(debug_str.contains("VectorSearchResponse"));
    }

    #[test]
    fn test_vector_search_result_serde() {
        let result = VectorSearchResult {
            content: "test content".to_string(),
            score: 0.95,
            metadata: Some(json!({"id": "123"})),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("test content"));
        assert!(json.contains("0.95"));
    }

    #[test]
    fn test_vector_search_result_debug() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: None,
        };
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("VectorSearchResult"));
    }

    #[test]
    fn test_vector_search_result_without_metadata() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: None,
        };
        assert!(result.metadata.is_none());
    }

    #[test]
    fn test_vector_search_result_with_metadata() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({"key": "value"})),
        };
        assert!(result.metadata.is_some());
    }

    #[test]
    fn test_vector_search_result_empty_content() {
        let result = VectorSearchResult {
            content: "".to_string(),
            score: 0.5,
            metadata: None,
        };
        assert_eq!(result.content, "");
    }

    #[test]
    fn test_vector_search_result_unicode_content() {
        let result = VectorSearchResult {
            content: "Hello 世界 🎉".to_string(),
            score: 0.5,
            metadata: None,
        };
        assert_eq!(result.content, "Hello 世界 🎉");
    }

    #[test]
    fn test_vector_search_result_multiline_content() {
        let result = VectorSearchResult {
            content: "line1\nline2\nline3".to_string(),
            score: 0.5,
            metadata: None,
        };
        assert_eq!(result.content, "line1\nline2\nline3");
    }

    #[test]
    fn test_vector_search_result_zero_score() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.0,
            metadata: None,
        };
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn test_vector_search_result_max_score() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 1.0,
            metadata: None,
        };
        assert_eq!(result.score, 1.0);
    }

    #[test]
    fn test_vector_search_result_negative_score() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: -0.5,
            metadata: None,
        };
        assert_eq!(result.score, -0.5);
    }

    #[test]
    fn test_vector_search_result_metadata_complex() {
        let metadata = json!({
            "id": "123",
            "type": "document",
            "tags": ["rust", "test"],
            "nested": {"key": "value"}
        });
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(metadata.clone()),
        };
        assert_eq!(result.metadata, Some(metadata));
    }

    #[test]
    fn test_vector_search_response_empty_results() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "No results".to_string(),
        };
        assert_eq!(response.results.len(), 0);
        assert_eq!(response.total_found, 0);
    }

    #[test]
    fn test_vector_search_response_with_results() {
        let response = VectorSearchResponse {
            results: vec![
                VectorSearchResult {
                    content: "result1".to_string(),
                    score: 0.9,
                    metadata: None,
                },
                VectorSearchResult {
                    content: "result2".to_string(),
                    score: 0.8,
                    metadata: None,
                },
            ],
            query: "test".to_string(),
            total_found: 2,
            formatted_results: "Results".to_string(),
        };
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.total_found, 2);
    }

    #[test]
    fn test_vector_search_response_query_unicode() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "世界".to_string(),
            total_found: 0,
            formatted_results: "".to_string(),
        };
        assert_eq!(response.query, "世界");
    }

    #[test]
    fn test_vector_search_response_query_empty() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "".to_string(),
            total_found: 0,
            formatted_results: "".to_string(),
        };
        assert_eq!(response.query, "");
    }

    #[test]
    fn test_vector_search_response_formatted_results_empty() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "".to_string(),
        };
        assert_eq!(response.formatted_results, "");
    }

    #[test]
    fn test_vector_search_response_formatted_results_multiline() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "line1\nline2\nline3".to_string(),
        };
        assert!(response.formatted_results.contains("\n"));
    }

    #[test]
    fn test_vector_search_result_very_long_content() {
        let content = "a".repeat(100000);
        let result = VectorSearchResult {
            content: content.clone(),
            score: 0.5,
            metadata: None,
        };
        assert_eq!(result.content.len(), 100000);
    }

    #[test]
    fn test_vector_search_args_special_characters_in_query() {
        let args = VectorSearchArgs {
            query: "!@#$%^&*()".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        assert_eq!(args.query, "!@#$%^&*()");
    }

    #[test]
    fn test_vector_search_args_tabs_and_newlines() {
        let args = VectorSearchArgs {
            query: "line1\tcolumn2\nline2".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        assert!(args.query.contains("\t"));
        assert!(args.query.contains("\n"));
    }

    #[test]
    fn test_vector_search_args_min_score_precision() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.123456789,
            label_filters: vec![],
        };
        assert_eq!(args.min_score, 0.123456789);
    }

    #[test]
    fn test_vector_search_result_score_precision() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.987654321,
            metadata: None,
        };
        assert_eq!(result.score, 0.987654321);
    }

    #[test]
    fn test_filter_kv_special_characters() {
        let filter = FilterKV {
            key: "key-with_special.chars!".to_string(),
            value: "value-with_special.chars!".to_string(),
        };
        assert_eq!(filter.key, "key-with_special.chars!");
        assert_eq!(filter.value, "value-with_special.chars!");
    }

    #[test]
    fn test_parse_value_str_partial_json() {
        let value = parse_value_str("{incomplete");
        assert_eq!(value, json!("{incomplete"));
    }

    #[test]
    fn test_parse_value_str_number_as_string() {
        let value = parse_value_str("123abc");
        assert_eq!(value, json!("123abc"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_label_filter_with_number_value() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![FilterKV {
                key: "count".to_string(),
                value: "42".to_string(),
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_label_filter_with_boolean_value() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![FilterKV {
                key: "active".to_string(),
                value: "true".to_string(),
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_label_filter_with_json_value() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![FilterKV {
                key: "tags".to_string(),
                value: r#"["tag1","tag2"]"#.to_string(),
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_label_filter_empty_key() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![FilterKV {
                key: "".to_string(),
                value: "value".to_string(),
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_label_filter_empty_value() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![FilterKV {
                key: "key".to_string(),
                value: "".to_string(),
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_label_filter_unicode() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![FilterKV {
                key: "名前".to_string(),
                value: "値".to_string(),
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[test]
    fn test_vector_search_args_serde_multiple_label_filters() {
        let json = r#"{"query":"test","limit":5,"min_score":0.5,"label_filters":[{"key":"type","value":"doc"},{"key":"status","value":"active"}]}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.label_filters.len(), 2);
        assert_eq!(args.label_filters[0].key, "type");
        assert_eq!(args.label_filters[0].value, "doc");
        assert_eq!(args.label_filters[1].key, "status");
        assert_eq!(args.label_filters[1].value, "active");
    }

    #[test]
    fn test_vector_search_args_serde_empty_label_filters() {
        let json = r#"{"query":"test","limit":5,"min_score":0.5,"label_filters":[]}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.label_filters.len(), 0);
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_prompt_parameter_ignored() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition1 = tool.definition("prompt1".to_string()).await;
        let definition2 = tool.definition("prompt2".to_string()).await;
        
        assert_eq!(definition1.name, definition2.name);
        assert_eq!(definition1.description, definition2.description);
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_response_structure() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, "test");
        assert_eq!(response.total_found, 0);
        assert!(response.results.is_empty());
        assert!(!response.formatted_results.is_empty());
    }

    #[test]
    fn test_vector_search_result_metadata_null_value() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!(null)),
        };
        assert!(result.metadata.is_some());
        assert!(result.metadata.unwrap().is_null());
    }

    #[test]
    fn test_vector_search_result_metadata_empty_object() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({})),
        };
        assert!(result.metadata.is_some());
        let metadata = result.metadata.unwrap();
        assert!(metadata.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_vector_search_response_total_found_mismatch() {
        let response = VectorSearchResponse {
            results: vec![VectorSearchResult {
                content: "test".to_string(),
                score: 0.5,
                metadata: None,
            }],
            query: "test".to_string(),
            total_found: 5,
            formatted_results: "Results".to_string(),
        };
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.total_found, 5);
    }

    #[test]
    fn test_parse_value_str_large_number() {
        let value = parse_value_str("999999999999999999");
        assert!(value.is_number());
    }

    #[test]
    fn test_parse_value_str_negative_float() {
        let value = parse_value_str("-3.14159");
        assert_eq!(value, json!(-3.14159));
    }

    #[test]
    fn test_parse_value_str_exponential_notation() {
        let value = parse_value_str("1.5e-10");
        assert_eq!(value, json!(1.5e-10));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_context_prefix_empty() {
        let store = create_test_vector_store("test", Some("".to_string()));
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("Search the 'test' vector store"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_context_prefix_multiline() {
        let store = create_test_vector_store("test", Some("Based on the following information from the docs:\nLine 2".to_string()));
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("docs"));
    }

    #[test]
    fn test_vector_search_args_limit_boundary_one() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 1,
            min_score: 0.5,
            label_filters: vec![],
        };
        assert_eq!(args.limit, 1);
    }

    #[test]
    fn test_vector_search_args_limit_boundary_twenty() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 20,
            min_score: 0.5,
            label_filters: vec![],
        };
        assert_eq!(args.limit, 20);
    }

    #[test]
    fn test_vector_search_args_min_score_boundary_point_one() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.1,
            label_filters: vec![],
        };
        assert_eq!(args.min_score, 0.1);
    }

    #[test]
    fn test_vector_search_args_min_score_boundary_one() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 1.0,
            label_filters: vec![],
        };
        assert_eq!(args.min_score, 1.0);
    }

    #[test]
    fn test_filter_kv_very_long_key() {
        let key = "k".repeat(10000);
        let filter = FilterKV {
            key: key.clone(),
            value: "value".to_string(),
        };
        assert_eq!(filter.key.len(), 10000);
    }

    #[test]
    fn test_filter_kv_very_long_value() {
        let value = "v".repeat(10000);
        let filter = FilterKV {
            key: "key".to_string(),
            value: value.clone(),
        };
        assert_eq!(filter.value.len(), 10000);
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_large_limit() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 100,
            min_score: 0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_negative_min_score() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: -0.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_call_min_score_above_one() {
        let store = create_test_vector_store("test", None);
        let tool = DynamicVectorSearchTool::new(store, "test".to_string());
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 1.5,
            label_filters: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[test]
    fn test_vector_search_result_content_with_null_bytes() {
        let result = VectorSearchResult {
            content: "test\0content".to_string(),
            score: 0.5,
            metadata: None,
        };
        assert_eq!(result.content, "test\0content");
    }

    #[test]
    fn test_vector_search_args_query_with_null_bytes() {
        let args = VectorSearchArgs {
            query: "test\0query".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        assert_eq!(args.query, "test\0query");
    }

    #[test]
    fn test_parse_value_str_nested_json() {
        let value = parse_value_str(r#"{"outer":{"inner":"value"}}"#);
        assert_eq!(value, json!({"outer": {"inner": "value"}}));
    }

    #[test]
    fn test_parse_value_str_json_with_unicode() {
        let value = parse_value_str(r#"{"key":"世界"}"#);
        assert_eq!(value, json!({"key": "世界"}));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_name_matches_tool_name() {
        let store = create_test_vector_store("my_store", None);
        let tool = DynamicVectorSearchTool::new(store, "my_store".to_string());
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, tool.name());
        assert_eq!(definition.name, "vector_search_my_store");
    }

    #[test]
    fn test_vector_search_response_serde_with_results() {
        let response = VectorSearchResponse {
            results: vec![VectorSearchResult {
                content: "test".to_string(),
                score: 0.9,
                metadata: Some(json!({"id": "123"})),
            }],
            query: "test query".to_string(),
            total_found: 1,
            formatted_results: "Result 1".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("test"));
        assert!(json.contains("0.9"));
    }
}
