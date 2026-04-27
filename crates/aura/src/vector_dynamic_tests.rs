#[cfg(test)]
mod tests {
    use crate::vector_dynamic::*;
    use crate::vector_store::VectorStoreManager;
    use rig::tool::Tool as RigTool;
    use serde_json::json;
    use std::sync::Arc;

    fn create_mock_vector_store() -> Arc<VectorStoreManager> {
        Arc::new(VectorStoreManager::new_stub("test_store", None))
    }

    fn create_mock_vector_store_with_context(context: String) -> Arc<VectorStoreManager> {
        Arc::new(VectorStoreManager::new_stub("test_store", Some(context)))
    }

    #[test]
    fn test_dynamic_vector_search_tool_new() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "my_store".to_string());
        
        assert_eq!(tool.name(), "vector_search_my_store");
    }

    #[test]
    fn test_dynamic_vector_search_tool_new_empty_name() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "".to_string());
        
        assert_eq!(tool.name(), "vector_search_");
    }

    #[test]
    fn test_dynamic_vector_search_tool_new_special_chars() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "my-store_123".to_string());
        
        assert_eq!(tool.name(), "vector_search_my-store_123");
    }

    #[test]
    fn test_dynamic_vector_search_tool_clone() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "test".to_string());
        let cloned = tool.clone();
        
        assert_eq!(cloned.name(), tool.name());
    }

    #[test]
    fn test_dynamic_vector_search_tool_name_override() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "custom".to_string());
        
        assert_eq!(tool.name(), "vector_search_custom");
        assert_eq!(DynamicVectorSearchTool::NAME, "dynamic_vector_search_tool");
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_basic() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "test_store".to_string());
        
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_search_test_store");
        assert!(definition.description.contains("test_store"));
        assert!(definition.description.contains("vector store"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_with_context() {
        let context = "Based on the following information from the documentation:".to_string();
        let store = create_mock_vector_store_with_context(context);
        let tool = DynamicVectorSearchTool::new(store.clone(), "docs".to_string());
        
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("documentation"));
        assert!(definition.description.contains("This vector store contains"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_parameters() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "test".to_string());
        
        let definition = tool.definition(String::new()).await;
        let params = definition.parameters;
        
        assert_eq!(params["type"], "object");
        assert!(params["properties"]["query"].is_object());
        assert!(params["properties"]["limit"].is_object());
        assert!(params["properties"]["min_score"].is_object());
        assert!(params["properties"]["label_filters"].is_object());
        
        let required = params["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
        assert!(required.contains(&json!("limit")));
        assert!(required.contains(&json!("min_score")));
        assert!(required.contains(&json!("label_filters")));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_query_param() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "test".to_string());
        
        let definition = tool.definition(String::new()).await;
        let query_param = &definition.parameters["properties"]["query"];
        
        assert_eq!(query_param["type"], "string");
        assert!(query_param["description"].as_str().unwrap().contains("query"));
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_limit_param() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "test".to_string());
        
        let definition = tool.definition(String::new()).await;
        let limit_param = &definition.parameters["properties"]["limit"];
        
        assert_eq!(limit_param["type"], "integer");
        assert_eq!(limit_param["default"], 5);
        assert_eq!(limit_param["minimum"], 1);
        assert_eq!(limit_param["maximum"], 20);
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_min_score_param() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "test".to_string());
        
        let definition = tool.definition(String::new()).await;
        let min_score_param = &definition.parameters["properties"]["min_score"];
        
        assert_eq!(min_score_param["type"], "number");
        assert_eq!(min_score_param["default"], 0.5);
        assert_eq!(min_score_param["minimum"], 0.1);
        assert_eq!(min_score_param["maximum"], 1.0);
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_label_filters_param() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "test".to_string());
        
        let definition = tool.definition(String::new()).await;
        let filters_param = &definition.parameters["properties"]["label_filters"];
        
        assert_eq!(filters_param["type"], "array");
        assert_eq!(filters_param["default"], json!([]));
        assert!(filters_param["items"].is_object());
        
        let item_props = &filters_param["items"]["properties"];
        assert!(item_props["key"].is_object());
        assert!(item_props["value"].is_object());
    }

    #[test]
    fn test_vector_search_args_default_limit() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: default_limit(),
            min_score: 0.0,
            label_filters: vec![],
        };
        
        assert_eq!(args.limit, 5);
    }

    #[test]
    fn test_vector_search_args_serde() {
        let args = VectorSearchArgs {
            query: "test query".to_string(),
            limit: 10,
            min_score: 0.7,
            label_filters: vec![],
        };
        
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: VectorSearchArgs = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.query, "test query");
        assert_eq!(deserialized.limit, 10);
        assert_eq!(deserialized.min_score, 0.7);
        assert_eq!(deserialized.label_filters.len(), 0);
    }

    #[test]
    fn test_vector_search_args_serde_with_filters() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![
                FilterKV {
                    key: "category".to_string(),
                    value: "docs".to_string(),
                },
                FilterKV {
                    key: "version".to_string(),
                    value: "1.0".to_string(),
                },
            ],
        };
        
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: VectorSearchArgs = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.label_filters.len(), 2);
        assert_eq!(deserialized.label_filters[0].key, "category");
        assert_eq!(deserialized.label_filters[0].value, "docs");
        assert_eq!(deserialized.label_filters[1].key, "version");
        assert_eq!(deserialized.label_filters[1].value, "1.0");
    }

    #[test]
    fn test_vector_search_args_empty_query() {
        let args = VectorSearchArgs {
            query: "".to_string(),
            limit: 5,
            min_score: 0.0,
            label_filters: vec![],
        };
        
        assert_eq!(args.query, "");
    }

    #[test]
    fn test_vector_search_args_zero_limit() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 0,
            min_score: 0.0,
            label_filters: vec![],
        };
        
        assert_eq!(args.limit, 0);
    }

    #[test]
    fn test_vector_search_args_max_limit() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 20,
            min_score: 0.0,
            label_filters: vec![],
        };
        
        assert_eq!(args.limit, 20);
    }

    #[test]
    fn test_vector_search_args_min_score_zero() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.0,
            label_filters: vec![],
        };
        
        assert_eq!(args.min_score, 0.0);
    }

    #[test]
    fn test_vector_search_args_min_score_one() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 1.0,
            label_filters: vec![],
        };
        
        assert_eq!(args.min_score, 1.0);
    }

    #[test]
    fn test_filter_kv_clone() {
        let filter = FilterKV {
            key: "test".to_string(),
            value: "value".to_string(),
        };
        let cloned = filter.clone();
        
        assert_eq!(cloned.key, "test");
        assert_eq!(cloned.value, "value");
    }

    #[test]
    fn test_filter_kv_serde() {
        let filter = FilterKV {
            key: "category".to_string(),
            value: "documentation".to_string(),
        };
        
        let json = serde_json::to_string(&filter).unwrap();
        let deserialized: FilterKV = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.key, "category");
        assert_eq!(deserialized.value, "documentation");
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
    fn test_parse_value_str_string() {
        let result = parse_value_str("hello");
        assert_eq!(result, json!("hello"));
    }

    #[test]
    fn test_parse_value_str_number() {
        let result = parse_value_str("42");
        assert_eq!(result, json!(42));
    }

    #[test]
    fn test_parse_value_str_float() {
        let result = parse_value_str("3.14");
        assert_eq!(result, json!(3.14));
    }

    #[test]
    fn test_parse_value_str_boolean_true() {
        let result = parse_value_str("true");
        assert_eq!(result, json!(true));
    }

    #[test]
    fn test_parse_value_str_boolean_false() {
        let result = parse_value_str("false");
        assert_eq!(result, json!(false));
    }

    #[test]
    fn test_parse_value_str_null() {
        let result = parse_value_str("null");
        assert_eq!(result, json!(null));
    }

    #[test]
    fn test_parse_value_str_invalid_json() {
        let result = parse_value_str("not-a-number");
        assert_eq!(result, json!("not-a-number"));
    }

    #[test]
    fn test_parse_value_str_empty() {
        let result = parse_value_str("");
        assert_eq!(result, json!(""));
    }

    #[test]
    fn test_parse_value_str_json_object() {
        let result = parse_value_str(r#"{"key":"value"}"#);
        assert_eq!(result, json!({"key": "value"}));
    }

    #[test]
    fn test_parse_value_str_json_array() {
        let result = parse_value_str(r#"[1,2,3]"#);
        assert_eq!(result, json!([1, 2, 3]));
    }

    #[test]
    fn test_parse_value_str_negative_number() {
        let result = parse_value_str("-42");
        assert_eq!(result, json!(-42));
    }

    #[test]
    fn test_parse_value_str_zero() {
        let result = parse_value_str("0");
        assert_eq!(result, json!(0));
    }

    #[test]
    fn test_parse_value_str_whitespace() {
        let result = parse_value_str("  hello  ");
        assert_eq!(result, json!("  hello  "));
    }

    #[test]
    fn test_parse_value_str_special_chars() {
        let result = parse_value_str("hello@world.com");
        assert_eq!(result, json!("hello@world.com"));
    }

    #[test]
    fn test_default_limit() {
        assert_eq!(default_limit(), 5);
    }

    #[test]
    fn test_vector_search_response_serde() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test query".to_string(),
            total_found: 0,
            formatted_results: "No results".to_string(),
        };
        
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("test query"));
        assert!(json.contains("No results"));
    }

    #[test]
    fn test_vector_search_response_with_results() {
        let response = VectorSearchResponse {
            results: vec![
                VectorSearchResult {
                    content: "result 1".to_string(),
                    score: 0.9,
                    metadata: None,
                },
                VectorSearchResult {
                    content: "result 2".to_string(),
                    score: 0.8,
                    metadata: Some(json!({"key": "value"})),
                },
            ],
            query: "test".to_string(),
            total_found: 2,
            formatted_results: "formatted".to_string(),
        };
        
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.total_found, 2);
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
    fn test_vector_search_result_no_metadata() {
        let result = VectorSearchResult {
            content: "content".to_string(),
            score: 0.5,
            metadata: None,
        };
        
        assert!(result.metadata.is_none());
    }

    #[test]
    fn test_vector_search_result_empty_content() {
        let result = VectorSearchResult {
            content: "".to_string(),
            score: 0.0,
            metadata: None,
        };
        
        assert_eq!(result.content, "");
    }

    #[test]
    fn test_vector_search_result_score_zero() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.0,
            metadata: None,
        };
        
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn test_vector_search_result_score_one() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 1.0,
            metadata: None,
        };
        
        assert_eq!(result.score, 1.0);
    }

    #[test]
    fn test_vector_search_result_unicode_content() {
        let result = VectorSearchResult {
            content: "Hello 世界 🎉".to_string(),
            score: 0.8,
            metadata: None,
        };
        
        assert_eq!(result.content, "Hello 世界 🎉");
    }

    #[test]
    fn test_vector_search_result_complex_metadata() {
        let metadata = json!({
            "id": "123",
            "category": "docs",
            "tags": ["rust", "testing"],
            "nested": {
                "key": "value"
            }
        });
        
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.7,
            metadata: Some(metadata.clone()),
        };
        
        assert_eq!(result.metadata, Some(metadata));
    }

    #[test]
    fn test_vector_search_args_unicode_query() {
        let args = VectorSearchArgs {
            query: "日本語クエリ".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        
        assert_eq!(args.query, "日本語クエリ");
    }

    #[test]
    fn test_vector_search_args_long_query() {
        let long_query = "a".repeat(10000);
        let args = VectorSearchArgs {
            query: long_query.clone(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![],
        };
        
        assert_eq!(args.query.len(), 10000);
    }

    #[test]
    fn test_filter_kv_unicode() {
        let filter = FilterKV {
            key: "カテゴリ".to_string(),
            value: "ドキュメント".to_string(),
        };
        
        assert_eq!(filter.key, "カテゴリ");
        assert_eq!(filter.value, "ドキュメント");
    }

    #[test]
    fn test_filter_kv_special_characters() {
        let filter = FilterKV {
            key: "key-with_special.chars".to_string(),
            value: "value@with#special$chars".to_string(),
        };
        
        assert_eq!(filter.key, "key-with_special.chars");
        assert_eq!(filter.value, "value@with#special$chars");
    }

    #[test]
    fn test_vector_search_args_many_filters() {
        let filters: Vec<FilterKV> = (0..100)
            .map(|i| FilterKV {
                key: format!("key{}", i),
                value: format!("value{}", i),
            })
            .collect();
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: filters,
        };
        
        assert_eq!(args.label_filters.len(), 100);
    }

    #[test]
    fn test_parse_value_str_scientific_notation() {
        let result = parse_value_str("1.5e10");
        assert_eq!(result, json!(1.5e10));
    }

    #[test]
    fn test_parse_value_str_negative_float() {
        let result = parse_value_str("-3.14");
        assert_eq!(result, json!(-3.14));
    }

    #[test]
    fn test_parse_value_str_quoted_number() {
        let result = parse_value_str(r#""42""#);
        assert_eq!(result, json!("42"));
    }

    #[test]
    fn test_vector_search_response_empty_query() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "".to_string(),
            total_found: 0,
            formatted_results: "".to_string(),
        };
        
        assert_eq!(response.query, "");
    }

    #[test]
    fn test_vector_search_response_large_total_found() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: usize::MAX,
            formatted_results: "".to_string(),
        };
        
        assert_eq!(response.total_found, usize::MAX);
    }

    #[test]
    fn test_dynamic_vector_search_tool_const_name() {
        assert_eq!(DynamicVectorSearchTool::NAME, "dynamic_vector_search_tool");
    }

    #[test]
    fn test_vector_search_args_default_deserialization() {
        let json = r#"{"query":"test"}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, 5);
        assert_eq!(args.min_score, 0.0);
        assert_eq!(args.label_filters.len(), 0);
    }

    #[test]
    fn test_vector_search_args_partial_deserialization() {
        let json = r#"{"query":"test","limit":10}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, 10);
        assert_eq!(args.min_score, 0.0);
        assert_eq!(args.label_filters.len(), 0);
    }

    #[test]
    fn test_parse_value_str_large_number() {
        let result = parse_value_str("999999999999999");
        assert!(result.is_number());
    }

    #[test]
    fn test_parse_value_str_decimal_zero() {
        let result = parse_value_str("0.0");
        assert_eq!(result, json!(0.0));
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
    fn test_vector_search_result_large_score() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 999.99,
            metadata: None,
        };
        
        assert_eq!(result.score, 999.99);
    }

    #[test]
    fn test_filter_kv_newlines() {
        let filter = FilterKV {
            key: "key\nwith\nnewlines".to_string(),
            value: "value\nwith\nnewlines".to_string(),
        };
        
        assert!(filter.key.contains('\n'));
        assert!(filter.value.contains('\n'));
    }

    #[test]
    fn test_vector_search_response_multiline_formatted() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "line1\nline2\nline3".to_string(),
        };
        
        assert!(response.formatted_results.contains('\n'));
    }

    #[test]
    fn test_parse_value_str_json_with_spaces() {
        let result = parse_value_str(r#"{ "key" : "value" }"#);
        assert_eq!(result, json!({"key": "value"}));
    }

    #[test]
    fn test_parse_value_str_escaped_quotes() {
        let result = parse_value_str(r#""hello \"world\"""#);
        assert_eq!(result, json!("hello \"world\""));
    }

    #[test]
    fn test_vector_search_args_boundary_min_score() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.1,
            label_filters: vec![],
        };
        
        assert_eq!(args.min_score, 0.1);
    }

    #[test]
    fn test_vector_search_args_mid_range_values() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 10,
            min_score: 0.5,
            label_filters: vec![],
        };
        
        assert_eq!(args.limit, 10);
        assert_eq!(args.min_score, 0.5);
    }

    #[test]
    fn test_vector_search_response_serialization_structure() {
        let response = VectorSearchResponse {
            results: vec![
                VectorSearchResult {
                    content: "test".to_string(),
                    score: 0.9,
                    metadata: None,
                },
            ],
            query: "query".to_string(),
            total_found: 1,
            formatted_results: "formatted".to_string(),
        };
        
        let json = serde_json::to_value(&response).unwrap();
        assert!(json["results"].is_array());
        assert_eq!(json["query"], "query");
        assert_eq!(json["total_found"], 1);
        assert_eq!(json["formatted_results"], "formatted");
    }

    #[test]
    fn test_vector_search_result_serialization_structure() {
        let result = VectorSearchResult {
            content: "content".to_string(),
            score: 0.8,
            metadata: Some(json!({"key": "value"})),
        };
        
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["content"], "content");
        assert!(json["score"].as_f64().unwrap() > 0.79 && json["score"].as_f64().unwrap() < 0.81);
        assert!(json["metadata"].is_object());
    }

    #[test]
    fn test_filter_kv_debug() {
        let filter = FilterKV {
            key: "test".to_string(),
            value: "value".to_string(),
        };
        
        let debug_str = format!("{:?}", filter);
        assert!(debug_str.contains("test"));
        assert!(debug_str.contains("value"));
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
        assert!(debug_str.contains("test"));
        assert!(debug_str.contains("5"));
    }

    #[test]
    fn test_vector_search_response_debug() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "none".to_string(),
        };
        
        let debug_str = format!("{:?}", response);
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_vector_search_result_debug() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.9,
            metadata: None,
        };
        
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("test"));
        assert!(debug_str.contains("0.9"));
    }

    #[test]
    fn test_parse_value_str_unicode() {
        let result = parse_value_str("こんにちは");
        assert_eq!(result, json!("こんにちは"));
    }

    #[test]
    fn test_parse_value_str_emoji() {
        let result = parse_value_str("🚀");
        assert_eq!(result, json!("🚀"));
    }

    #[test]
    fn test_vector_search_args_limit_one() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 1,
            min_score: 0.0,
            label_filters: vec![],
        };
        
        assert_eq!(args.limit, 1);
    }

    #[test]
    fn test_vector_search_args_min_score_boundary() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.999,
            label_filters: vec![],
        };
        
        assert_eq!(args.min_score, 0.999);
    }

    #[test]
    fn test_filter_kv_long_strings() {
        let long_key = "k".repeat(1000);
        let long_value = "v".repeat(1000);
        let filter = FilterKV {
            key: long_key.clone(),
            value: long_value.clone(),
        };
        
        assert_eq!(filter.key.len(), 1000);
        assert_eq!(filter.value.len(), 1000);
    }

    #[test]
    fn test_vector_search_response_many_results() {
        let results: Vec<VectorSearchResult> = (0..100)
            .map(|i| VectorSearchResult {
                content: format!("result {}", i),
                score: 0.5,
                metadata: None,
            })
            .collect();
        
        let response = VectorSearchResponse {
            results,
            query: "test".to_string(),
            total_found: 100,
            formatted_results: "many".to_string(),
        };
        
        assert_eq!(response.results.len(), 100);
        assert_eq!(response.total_found, 100);
    }

    #[test]
    fn test_parse_value_str_nested_json() {
        let result = parse_value_str(r#"{"outer":{"inner":"value"}}"#);
        assert_eq!(result, json!({"outer": {"inner": "value"}}));
    }

    #[test]
    fn test_parse_value_str_mixed_array() {
        let result = parse_value_str(r#"[1,"two",true,null]"#);
        assert_eq!(result, json!([1, "two", true, null]));
    }

    #[test]
    fn test_vector_search_args_single_filter() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
            label_filters: vec![FilterKV {
                key: "single".to_string(),
                value: "filter".to_string(),
            }],
        };
        
        assert_eq!(args.label_filters.len(), 1);
    }

    #[test]
    fn test_dynamic_vector_search_tool_unicode_name() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "日本語".to_string());
        
        assert_eq!(tool.name(), "vector_search_日本語");
    }

    #[tokio::test]
    async fn test_dynamic_vector_search_tool_definition_empty_name() {
        let store = create_mock_vector_store();
        let tool = DynamicVectorSearchTool::new(store.clone(), "".to_string());
        
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_search_");
    }

    #[test]
    fn test_vector_search_result_metadata_empty_object() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({})),
        };
        
        assert!(result.metadata.is_some());
        assert!(result.metadata.unwrap().is_object());
    }

    #[test]
    fn test_vector_search_result_metadata_array() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!(["tag1", "tag2"])),
        };
        
        assert!(result.metadata.is_some());
        assert!(result.metadata.unwrap().is_array());
    }

    #[test]
    fn test_parse_value_str_leading_zeros() {
        // "007" is not valid JSON, falls back to string
        let result = parse_value_str("007");
        assert_eq!(result, json!("007"));
    }

    #[test]
    fn test_parse_value_str_plus_sign() {
        // "+42" is not valid JSON, falls back to string
        let result = parse_value_str("+42");
        assert_eq!(result, json!("+42"));
    }

    #[test]
    fn test_vector_search_args_fractional_min_score() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.123456,
            label_filters: vec![],
        };
        
        assert_eq!(args.min_score, 0.123456);
    }

    #[test]
    fn test_filter_kv_whitespace_only() {
        let filter = FilterKV {
            key: "   ".to_string(),
            value: "\t\n".to_string(),
        };
        
        assert_eq!(filter.key, "   ");
        assert_eq!(filter.value, "\t\n");
    }

    #[test]
    fn test_vector_search_response_zero_total_found() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "".to_string(),
        };
        
        assert_eq!(response.total_found, 0);
        assert_eq!(response.results.len(), 0);
    }
}
