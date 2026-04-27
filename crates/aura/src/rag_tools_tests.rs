#[cfg(test)]
mod tests {
    use crate::rag_tools::*;
    use crate::vector_store::VectorStoreManager;
    use rig::tool::Tool;
    use serde_json::json;
    use std::sync::Arc;

    fn create_test_vector_store(name: &str, context_prefix: Option<String>) -> Arc<VectorStoreManager> {
        Arc::new(VectorStoreManager::new_stub(name, context_prefix))
    }

    #[test]
    fn test_vector_search_mezmo_kb_tool_new() {
        let store = create_test_vector_store("test", None);
        let _tool = VectorSearchMezmoKbTool::new(store);
    }

    #[test]
    fn test_vector_search_mezmo_runbooks_tool_new() {
        let store = create_test_vector_store("test", None);
        let _tool = VectorSearchMezmoRunbooksTool::new(store);
    }

    #[test]
    fn test_vector_search_mezmo_kb_tool_clone() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let _cloned = tool.clone();
    }

    #[test]
    fn test_vector_search_mezmo_runbooks_tool_clone() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoRunbooksTool::new(store);
        let _cloned = tool.clone();
    }

    #[test]
    fn test_vector_search_mezmo_kb_tool_name() {
        assert_eq!(VectorSearchMezmoKbTool::NAME, "vector_search_mezmo_kb");
    }

    #[test]
    fn test_vector_search_mezmo_runbooks_tool_name() {
        assert_eq!(VectorSearchMezmoRunbooksTool::NAME, "vector_search_mezmo_runbooks");
    }

    #[tokio::test]
    async fn test_vector_search_mezmo_kb_tool_definition() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_search_mezmo_kb");
        assert!(definition.description.contains("Search the vector store"));
        assert!(definition.parameters.get("type").is_some());
        assert_eq!(definition.parameters["type"], "object");
    }

    #[tokio::test]
    async fn test_vector_search_mezmo_runbooks_tool_definition() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoRunbooksTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_search_mezmo_runbooks");
        assert!(definition.description.contains("Search the vector store"));
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_with_context_prefix() {
        let store = create_test_vector_store("test", Some("Based on the following information from the knowledge base:".to_string()));
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("knowledge base"));
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_without_context_prefix() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("Search the vector store"));
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_parameters() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let properties = &definition.parameters["properties"];
        assert!(properties.get("query").is_some());
        assert!(properties.get("limit").is_some());
        assert!(properties.get("min_score").is_some());
        
        assert_eq!(properties["query"]["type"], "string");
        assert_eq!(properties["limit"]["type"], "integer");
        assert_eq!(properties["min_score"]["type"], "number");
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_required_fields() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let required = definition.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
        assert!(required.contains(&json!("limit")));
        assert!(required.contains(&json!("min_score")));
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_empty_results() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test query".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.results.len(), 0);
        assert_eq!(response.query, "test query");
        assert_eq!(response.total_found, 0);
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_with_limit() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 10,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_with_min_score() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_empty_query() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_zero_limit() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 0,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_max_limit() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 20,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_min_score_zero() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_min_score_one() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 1.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_unicode_query() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "Hello 世界 🎉".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.query, "Hello 世界 🎉");
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_multiline_query() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "line1\nline2\nline3".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_very_long_query() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let long_query = "query ".repeat(1000);
        let args = VectorSearchArgs {
            query: long_query.clone(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.query, long_query);
    }

    #[test]
    fn test_vector_search_args_serde() {
        let args = VectorSearchArgs {
            query: "test query".to_string(),
            limit: 10,
            min_score: 0.5,
        };
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: VectorSearchArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.query, "test query");
        assert_eq!(deserialized.limit, 10);
        assert_eq!(deserialized.min_score, 0.5);
    }

    #[test]
    fn test_vector_search_args_default_limit() {
        let json = r#"{"query":"test"}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.limit, 5);
        assert_eq!(args.min_score, 0.0);
    }

    #[test]
    fn test_vector_search_args_default_min_score() {
        let json = r#"{"query":"test","limit":10}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.min_score, 0.0);
    }

    #[test]
    fn test_vector_search_args_debug() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let debug_str = format!("{:?}", args);
        assert!(debug_str.contains("VectorSearchArgs"));
        assert!(debug_str.contains("test"));
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
    }

    #[test]
    fn test_vector_search_response_debug() {
        let response = VectorSearchResponse {
            results: vec![],
            query: "test".to_string(),
            total_found: 0,
            formatted_results: "No results".to_string(),
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
    fn test_vector_ingest_tool_new() {
        let store = create_test_vector_store("test", None);
        let _tool = VectorIngestTool::new(store);
    }

    #[test]
    fn test_vector_ingest_tool_clone() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let _cloned = tool.clone();
    }

    #[test]
    fn test_vector_ingest_tool_name() {
        assert_eq!(VectorIngestTool::NAME, "vector_ingest");
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_definition() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_ingest");
        assert!(definition.description.contains("Ingest documents"));
        assert!(definition.parameters.get("type").is_some());
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_definition_parameters() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let properties = &definition.parameters["properties"];
        assert!(properties.get("documents").is_some());
        assert_eq!(properties["documents"]["type"], "array");
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_single_document() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: None,
                content: "test content".to_string(),
                metadata: None,
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 1);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_multiple_documents() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![
                IngestDocument {
                    id: None,
                    content: "doc1".to_string(),
                    metadata: None,
                },
                IngestDocument {
                    id: None,
                    content: "doc2".to_string(),
                    metadata: None,
                },
                IngestDocument {
                    id: None,
                    content: "doc3".to_string(),
                    metadata: None,
                },
            ],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 3);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_empty_documents() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 0);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_with_id() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: Some("doc123".to_string()),
                content: "test content".to_string(),
                metadata: None,
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_with_metadata() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: None,
                content: "test content".to_string(),
                metadata: Some(json!({"type": "article"})),
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_empty_content() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: None,
                content: "".to_string(),
                metadata: None,
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_unicode_content() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: None,
                content: "Hello 世界 🎉".to_string(),
                metadata: None,
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_multiline_content() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: None,
                content: "line1\nline2\nline3".to_string(),
                metadata: None,
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_large_document() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let large_content = "a".repeat(100000);
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: None,
                content: large_content,
                metadata: None,
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_many_documents() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let documents: Vec<IngestDocument> = (0..100)
            .map(|i| IngestDocument {
                id: Some(format!("doc{}", i)),
                content: format!("content {}", i),
                metadata: None,
            })
            .collect();
        let args = VectorIngestArgs { documents };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 100);
    }

    #[test]
    fn test_ingest_document_serde() {
        let doc = IngestDocument {
            id: Some("123".to_string()),
            content: "test".to_string(),
            metadata: Some(json!({"key": "value"})),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let deserialized: IngestDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, Some("123".to_string()));
        assert_eq!(deserialized.content, "test");
    }

    #[test]
    fn test_ingest_document_debug() {
        let doc = IngestDocument {
            id: None,
            content: "test".to_string(),
            metadata: None,
        };
        let debug_str = format!("{:?}", doc);
        assert!(debug_str.contains("IngestDocument"));
    }

    #[test]
    fn test_vector_ingest_args_serde() {
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: None,
                content: "test".to_string(),
                metadata: None,
            }],
        };
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: VectorIngestArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.documents.len(), 1);
    }

    #[test]
    fn test_vector_ingest_args_debug() {
        let args = VectorIngestArgs {
            documents: vec![],
        };
        let debug_str = format!("{:?}", args);
        assert!(debug_str.contains("VectorIngestArgs"));
    }

    #[test]
    fn test_vector_ingest_response_serde() {
        let response = VectorIngestResponse {
            ingested_count: 5,
            success: true,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("5"));
        assert!(json.contains("true"));
    }

    #[test]
    fn test_vector_ingest_response_debug() {
        let response = VectorIngestResponse {
            ingested_count: 0,
            success: true,
        };
        let debug_str = format!("{:?}", response);
        assert!(debug_str.contains("VectorIngestResponse"));
    }

    #[test]
    fn test_vector_ingest_response_failure() {
        let response = VectorIngestResponse {
            ingested_count: 0,
            success: false,
        };
        assert!(!response.success);
    }

    #[test]
    fn test_auto_ingest_new() {
        let store = create_test_vector_store("test", None);
        let _auto_ingest = AutoIngest::new(store);
    }

    #[tokio::test]
    async fn test_auto_ingest_load_from_json_nonexistent_file() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.load_from_json("/nonexistent/file.json").await;
        
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_empty_sources() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.auto_load(&[]).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_unsupported_format() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec!["file.txt".to_string()];
        let result = auto_ingest.auto_load(&sources).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_nonexistent_json() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec!["/nonexistent/file.json".to_string()];
        let result = auto_ingest.auto_load(&sources).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_multiple_unsupported() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec![
            "file1.txt".to_string(),
            "file2.csv".to_string(),
            "file3.xml".to_string(),
        ];
        let result = auto_ingest.auto_load(&sources).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_vector_search_args_empty_query() {
        let args = VectorSearchArgs {
            query: "".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        assert_eq!(args.query, "");
    }

    #[test]
    fn test_vector_search_args_limit_one() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 1,
            min_score: 0.0,
        };
        assert_eq!(args.limit, 1);
    }

    #[test]
    fn test_vector_search_args_limit_twenty() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 20,
            min_score: 0.0,
        };
        assert_eq!(args.limit, 20);
    }

    #[test]
    fn test_vector_search_args_min_score_boundary() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
        };
        assert_eq!(args.min_score, 0.5);
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
    fn test_ingest_document_empty_id() {
        let doc = IngestDocument {
            id: Some("".to_string()),
            content: "test".to_string(),
            metadata: None,
        };
        assert_eq!(doc.id, Some("".to_string()));
    }

    #[test]
    fn test_ingest_document_empty_content() {
        let doc = IngestDocument {
            id: None,
            content: "".to_string(),
            metadata: None,
        };
        assert_eq!(doc.content, "");
    }

    #[test]
    fn test_ingest_document_metadata_empty_object() {
        let doc = IngestDocument {
            id: None,
            content: "test".to_string(),
            metadata: Some(json!({})),
        };
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_ingest_document_metadata_null() {
        let doc = IngestDocument {
            id: None,
            content: "test".to_string(),
            metadata: Some(json!(null)),
        };
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_ingest_document_metadata_array() {
        let doc = IngestDocument {
            id: None,
            content: "test".to_string(),
            metadata: Some(json!(["tag1", "tag2"])),
        };
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_ingest_document_metadata_string() {
        let doc = IngestDocument {
            id: None,
            content: "test".to_string(),
            metadata: Some(json!("metadata string")),
        };
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_ingest_document_metadata_number() {
        let doc = IngestDocument {
            id: None,
            content: "test".to_string(),
            metadata: Some(json!(42)),
        };
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_ingest_document_metadata_boolean() {
        let doc = IngestDocument {
            id: None,
            content: "test".to_string(),
            metadata: Some(json!(true)),
        };
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_vector_ingest_response_zero_count() {
        let response = VectorIngestResponse {
            ingested_count: 0,
            success: true,
        };
        assert_eq!(response.ingested_count, 0);
    }

    #[test]
    fn test_vector_ingest_response_large_count() {
        let response = VectorIngestResponse {
            ingested_count: 1000000,
            success: true,
        };
        assert_eq!(response.ingested_count, 1000000);
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
    fn test_ingest_document_very_long_content() {
        let content = "b".repeat(100000);
        let doc = IngestDocument {
            id: None,
            content: content.clone(),
            metadata: None,
        };
        assert_eq!(doc.content.len(), 100000);
    }

    #[test]
    fn test_ingest_document_very_long_id() {
        let id = "x".repeat(10000);
        let doc = IngestDocument {
            id: Some(id.clone()),
            content: "test".to_string(),
            metadata: None,
        };
        assert_eq!(doc.id.unwrap().len(), 10000);
    }

    #[test]
    fn test_vector_search_args_special_characters_in_query() {
        let args = VectorSearchArgs {
            query: "!@#$%^&*()".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        assert_eq!(args.query, "!@#$%^&*()");
    }

    #[test]
    fn test_ingest_document_special_characters_in_content() {
        let doc = IngestDocument {
            id: None,
            content: "!@#$%^&*()".to_string(),
            metadata: None,
        };
        assert_eq!(doc.content, "!@#$%^&*()");
    }

    #[test]
    fn test_ingest_document_special_characters_in_id() {
        let doc = IngestDocument {
            id: Some("id-with_special.chars!".to_string()),
            content: "test".to_string(),
            metadata: None,
        };
        assert_eq!(doc.id, Some("id-with_special.chars!".to_string()));
    }

    #[test]
    fn test_vector_search_args_tabs_and_newlines() {
        let args = VectorSearchArgs {
            query: "line1\tcolumn2\nline2".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        assert!(args.query.contains("\t"));
        assert!(args.query.contains("\n"));
    }

    #[test]
    fn test_ingest_document_tabs_and_newlines() {
        let doc = IngestDocument {
            id: None,
            content: "line1\tcolumn2\nline2".to_string(),
            metadata: None,
        };
        assert!(doc.content.contains("\t"));
        assert!(doc.content.contains("\n"));
    }

    #[test]
    fn test_vector_search_args_min_score_precision() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.123456789,
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

    #[tokio::test]
    async fn test_vector_search_tool_definition_limit_constraints() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let limit_props = &definition.parameters["properties"]["limit"];
        assert_eq!(limit_props["minimum"], 1);
        assert_eq!(limit_props["maximum"], 20);
        assert_eq!(limit_props["default"], 5);
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_min_score_constraints() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let min_score_props = &definition.parameters["properties"]["min_score"];
        assert_eq!(min_score_props["minimum"], 0.0);
        assert_eq!(min_score_props["maximum"], 1.0);
        assert_eq!(min_score_props["default"], 0.0);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_definition_required_fields() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let required = definition.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("documents")));
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_definition_document_schema() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let items = &definition.parameters["properties"]["documents"]["items"];
        let properties = &items["properties"];
        assert!(properties.get("id").is_some());
        assert!(properties.get("content").is_some());
        assert!(properties.get("metadata").is_some());
        
        let required = items["required"].as_array().unwrap();
        assert!(required.contains(&json!("content")));
    }

    #[test]
    fn test_vector_search_args_serde_with_defaults() {
        let json = r#"{"query":"test query"}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.query, "test query");
        assert_eq!(args.limit, 5);
        assert_eq!(args.min_score, 0.0);
    }

    #[test]
    fn test_vector_search_args_serde_all_fields() {
        let json = r#"{"query":"test","limit":10,"min_score":0.7}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, 10);
        assert_eq!(args.min_score, 0.7);
    }

    #[test]
    fn test_ingest_document_serde_minimal() {
        let json = r#"{"content":"test content"}"#;
        let doc: IngestDocument = serde_json::from_str(json).unwrap();
        assert_eq!(doc.content, "test content");
        assert!(doc.id.is_none());
        assert!(doc.metadata.is_none());
    }

    #[test]
    fn test_ingest_document_serde_all_fields() {
        let json = r#"{"id":"123","content":"test","metadata":{"key":"value"}}"#;
        let doc: IngestDocument = serde_json::from_str(json).unwrap();
        assert_eq!(doc.id, Some("123".to_string()));
        assert_eq!(doc.content, "test");
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_vector_ingest_args_serde_empty_documents() {
        let json = r#"{"documents":[]}"#;
        let args: VectorIngestArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.documents.len(), 0);
    }

    #[test]
    fn test_vector_ingest_args_serde_multiple_documents() {
        let json = r#"{"documents":[{"content":"doc1"},{"content":"doc2"}]}"#;
        let args: VectorIngestArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.documents.len(), 2);
        assert_eq!(args.documents[0].content, "doc1");
        assert_eq!(args.documents[1].content, "doc2");
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

    #[test]
    fn test_vector_ingest_response_serde_success_false() {
        let response = VectorIngestResponse {
            ingested_count: 0,
            success: false,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("false"));
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_limit_boundary_values() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 1,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        assert!(result.is_ok());
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 20,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_min_score_boundary_values() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        assert!(result.is_ok());
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 1.0,
        };
        let result = tool.call(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_vector_search_mezmo_runbooks_tool_call() {
        let store = create_test_vector_store("runbooks", None);
        let tool = VectorSearchMezmoRunbooksTool::new(store);
        let args = VectorSearchArgs {
            query: "test query".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.query, "test query");
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_response_structure() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.query, "test");
        assert_eq!(response.total_found, 0);
        assert!(response.results.is_empty());
        assert!(!response.formatted_results.is_empty());
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_response_structure() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![IngestDocument {
                id: None,
                content: "test".to_string(),
                metadata: None,
            }],
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 1);
        assert!(response.success);
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
    fn test_ingest_document_with_all_fields() {
        let doc = IngestDocument {
            id: Some("doc-123".to_string()),
            content: "test content".to_string(),
            metadata: Some(json!({"type": "article", "tags": ["rust"]})),
        };
        assert_eq!(doc.id, Some("doc-123".to_string()));
        assert_eq!(doc.content, "test content");
        assert!(doc.metadata.is_some());
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
}
