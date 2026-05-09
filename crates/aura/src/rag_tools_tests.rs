#[cfg(test)]
mod tests {
    use crate::rag_tools::*;
    use crate::vector_store::VectorStoreManager;
    use rig::tool::Tool;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn create_temp_dir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("aura_test_{}_{}", pid, id));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn create_test_vector_store(name: &str, context_prefix: Option<String>) -> Arc<VectorStoreManager> {
        Arc::new(VectorStoreManager::new_stub(name, context_prefix))
    }

    #[test]
    fn test_vector_search_mezmo_kb_tool_name() {
        assert_eq!(VectorSearchMezmoKbTool::NAME, "vector_search_mezmo_kb");
    }

    #[test]
    fn test_vector_search_mezmo_runbooks_tool_name() {
        assert_eq!(VectorSearchMezmoRunbooksTool::NAME, "vector_search_mezmo_runbooks");
    }

    #[test]
    fn test_vector_ingest_tool_name() {
        assert_eq!(VectorIngestTool::NAME, "vector_ingest");
    }

    #[test]
    fn test_vector_search_mezmo_kb_tool_new() {
        let store = create_test_vector_store("test", None);
        let _tool = VectorSearchMezmoKbTool::new(store.clone());
        assert_eq!(Arc::strong_count(&store), 2);
    }

    #[test]
    fn test_vector_search_mezmo_runbooks_tool_new() {
        let store = create_test_vector_store("test", None);
        let _tool = VectorSearchMezmoRunbooksTool::new(store.clone());
        assert_eq!(Arc::strong_count(&store), 2);
    }

    #[test]
    fn test_vector_ingest_tool_new() {
        let store = create_test_vector_store("test", None);
        let _tool = VectorIngestTool::new(store.clone());
        assert_eq!(Arc::strong_count(&store), 2);
    }

    #[test]
    fn test_auto_ingest_new() {
        let store = create_test_vector_store("test", None);
        let _auto_ingest = AutoIngest::new(store.clone());
        assert_eq!(Arc::strong_count(&store), 2);
    }

    #[test]
    fn test_vector_search_args_serde_with_all_fields() {
        let args = VectorSearchArgs {
            query: "test query".to_string(),
            limit: 10,
            min_score: 0.7,
        };
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: VectorSearchArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.query, "test query");
        assert_eq!(deserialized.limit, 10);
        assert_eq!(deserialized.min_score, 0.7);
    }

    #[test]
    fn test_vector_search_args_serde_with_defaults() {
        let json = r#"{"query":"test"}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, 5);
        assert_eq!(args.min_score, 0.0);
    }

    #[test]
    fn test_vector_search_args_serde_partial_defaults() {
        let json = r#"{"query":"test","limit":10}"#;
        let args: VectorSearchArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, 10);
        assert_eq!(args.min_score, 0.0);
    }

    #[test]
    fn test_vector_search_args_boundary_values() {
        let args = VectorSearchArgs {
            query: "".to_string(),
            limit: 1,
            min_score: 0.0,
        };
        assert_eq!(args.query, "");
        assert_eq!(args.limit, 1);
        assert_eq!(args.min_score, 0.0);

        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 20,
            min_score: 1.0,
        };
        assert_eq!(args.limit, 20);
        assert_eq!(args.min_score, 1.0);
    }

    #[test]
    fn test_vector_search_args_unicode() {
        let args = VectorSearchArgs {
            query: "Hello 世界 🎉".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        assert_eq!(args.query, "Hello 世界 🎉");
    }

    #[test]
    fn test_vector_search_response_serde() {
        let response = VectorSearchResponse {
            results: vec![VectorSearchResult {
                content: "test content".to_string(),
                score: 0.95,
                metadata: Some(json!({"id": "123"})),
            }],
            query: "test query".to_string(),
            total_found: 1,
            formatted_results: "Result 1".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("test content"));
        assert!(json.contains("0.95"));
        assert!(json.contains("test query"));
        assert!(json.contains("Result 1"));
    }

    #[test]
    fn test_vector_search_result_serde() {
        let result = VectorSearchResult {
            content: "test content".to_string(),
            score: 0.95,
            metadata: Some(json!({"id": "123", "type": "doc"})),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("test content"));
        assert!(json.contains("0.95"));
        assert!(json.contains("\"id\":\"123\""));
    }

    #[test]
    fn test_vector_search_result_without_metadata() {
        let result = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: None,
        };
        assert!(result.metadata.is_none());
        assert_eq!(result.content, "test");
        assert_eq!(result.score, 0.5);
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
        let json = r#"{"id":"doc-123","content":"test","metadata":{"type":"article"}}"#;
        let doc: IngestDocument = serde_json::from_str(json).unwrap();
        assert_eq!(doc.id, Some("doc-123".to_string()));
        assert_eq!(doc.content, "test");
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_ingest_document_with_complex_metadata() {
        let doc = IngestDocument {
            id: Some("123".to_string()),
            content: "test".to_string(),
            metadata: Some(json!({
                "type": "document",
                "tags": ["rust", "test"],
                "nested": {"key": "value"}
            })),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let deserialized: IngestDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, Some("123".to_string()));
        assert_eq!(deserialized.content, "test");
        assert!(deserialized.metadata.is_some());
    }

    #[test]
    fn test_vector_ingest_args_serde() {
        let args = VectorIngestArgs {
            documents: vec![
                IngestDocument {
                    id: None,
                    content: "doc1".to_string(),
                    metadata: None,
                },
                IngestDocument {
                    id: Some("2".to_string()),
                    content: "doc2".to_string(),
                    metadata: Some(json!({"key": "value"})),
                },
            ],
        };
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: VectorIngestArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.documents.len(), 2);
        assert_eq!(deserialized.documents[0].content, "doc1");
        assert_eq!(deserialized.documents[1].content, "doc2");
    }

    #[test]
    fn test_vector_ingest_response_serde() {
        let response = VectorIngestResponse {
            ingested_count: 42,
            success: true,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("42"));
        assert!(json.contains("true"));
    }

    #[tokio::test]
    async fn test_vector_search_mezmo_kb_tool_definition_without_context() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_search_mezmo_kb");
        assert!(definition.description.contains("Search the vector store"));
        assert!(definition.description.contains("semantically similar"));
        assert!(!definition.description.contains("This vector store contains"));
    }

    #[tokio::test]
    async fn test_vector_search_mezmo_kb_tool_definition_with_context() {
        let store = create_test_vector_store(
            "test",
            Some("Based on the following information from the knowledge base:".to_string())
        );
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_search_mezmo_kb");
        assert!(definition.description.contains("knowledge base"));
        assert!(definition.description.contains("This vector store contains"));
    }

    #[tokio::test]
    async fn test_vector_search_mezmo_runbooks_tool_definition() {
        let store = create_test_vector_store("runbooks", None);
        let tool = VectorSearchMezmoRunbooksTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_search_mezmo_runbooks");
        assert!(definition.description.contains("Search the vector store"));
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_parameters_structure() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.parameters["type"], "object");
        let properties = &definition.parameters["properties"];
        assert!(properties.get("query").is_some());
        assert!(properties.get("limit").is_some());
        assert!(properties.get("min_score").is_some());
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_query_parameter() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let query_param = &definition.parameters["properties"]["query"];
        assert_eq!(query_param["type"], "string");
        assert!(query_param["description"].as_str().unwrap().contains("text query"));
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_limit_parameter() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let limit_param = &definition.parameters["properties"]["limit"];
        assert_eq!(limit_param["type"], "integer");
        assert_eq!(limit_param["default"], 5);
        assert_eq!(limit_param["minimum"], 1);
        assert_eq!(limit_param["maximum"], 20);
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_min_score_parameter() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let min_score_param = &definition.parameters["properties"]["min_score"];
        assert_eq!(min_score_param["type"], "number");
        assert_eq!(min_score_param["default"], 0.0);
        assert_eq!(min_score_param["minimum"], 0.0);
        assert_eq!(min_score_param["maximum"], 1.0);
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_required_fields() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let required = definition.parameters["required"].as_array().unwrap();
        assert_eq!(required.len(), 3);
        assert!(required.contains(&json!("query")));
        assert!(required.contains(&json!("limit")));
        assert!(required.contains(&json!("min_score")));
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_returns_empty_results() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test query".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, "test query");
        assert_eq!(response.total_found, 0);
        assert_eq!(response.results.len(), 0);
        assert!(response.formatted_results.contains("No results found"));
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_preserves_query() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "Hello 世界 🎉".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, "Hello 世界 🎉");
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_with_empty_query() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, "");
        assert_eq!(response.total_found, 0);
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_with_limit_boundaries() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 1,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        let response = result.unwrap();
        assert_eq!(response.total_found, 0);
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 20,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        let response = result.unwrap();
        assert_eq!(response.total_found, 0);
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_with_min_score_boundaries() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        let response = result.unwrap();
        assert_eq!(response.total_found, 0);
        
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 1.0,
        };
        let result = tool.call(args).await;
        let response = result.unwrap();
        assert_eq!(response.total_found, 0);
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_with_multiline_query() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "line1\nline2\nline3".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, "line1\nline2\nline3");
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_with_very_long_query() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let long_query = "query ".repeat(1000);
        let args = VectorSearchArgs {
            query: long_query.clone(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, long_query);
    }

    #[tokio::test]
    async fn test_vector_search_mezmo_runbooks_tool_call() {
        let store = create_test_vector_store("runbooks", None);
        let tool = VectorSearchMezmoRunbooksTool::new(store);
        let args = VectorSearchArgs {
            query: "runbook query".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.query, "runbook query");
        assert_eq!(response.total_found, 0);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_definition() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "vector_ingest");
        assert!(definition.description.contains("Ingest documents"));
        assert!(definition.description.contains("vector store"));
        assert!(definition.description.contains("semantic search"));
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_definition_parameters() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.parameters["type"], "object");
        let properties = &definition.parameters["properties"];
        assert!(properties.get("documents").is_some());
        assert_eq!(properties["documents"]["type"], "array");
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
        assert_eq!(required.len(), 1);
        assert!(required.contains(&json!("content")));
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_definition_required_fields() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        let required = definition.parameters["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert!(required.contains(&json!("documents")));
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
                    id: Some("2".to_string()),
                    content: "doc2".to_string(),
                    metadata: Some(json!({"type": "article"})),
                },
                IngestDocument {
                    id: None,
                    content: "doc3".to_string(),
                    metadata: None,
                },
            ],
        };
        let result = tool.call(args).await;
        
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
        
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 0);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_with_empty_content() {
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
        
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 1);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_with_unicode_content() {
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
        
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 1);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_with_multiline_content() {
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
        
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 1);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_with_large_document() {
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
        
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 1);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_with_many_documents() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let documents: Vec<IngestDocument> = (0..100)
            .map(|i| IngestDocument {
                id: Some(format!("doc{}", i)),
                content: format!("content {}", i),
                metadata: Some(json!({"index": i})),
            })
            .collect();
        let args = VectorIngestArgs { documents };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 100);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_auto_ingest_load_from_json_nonexistent_file() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.load_from_json("/nonexistent/file.json").await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, crate::error::BuilderError::VectorStoreError(_)));
        let err_msg = err.to_string();
        assert!(err_msg.contains("Failed to read file"));
    }

    #[tokio::test]
    async fn test_auto_ingest_load_from_json_invalid_json() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.path().join("invalid.json");
        fs::write(&file_path, "not valid json").unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.load_from_json(file_path.to_str().unwrap()).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, crate::error::BuilderError::VectorStoreError(_)));
        let err_msg = err.to_string();
        assert!(err_msg.contains("Failed to parse JSON"));
    }

    #[tokio::test]
    async fn test_auto_ingest_load_from_json_valid_documents() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.path().join("docs.json");
        let json_content = r#"[
            {"content": "document 1"},
            {"content": "document 2"},
            {"content": "document 3"}
        ]"#;
        fs::write(&file_path, json_content).unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.load_from_json(file_path.to_str().unwrap()).await;
        
        let count = result.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_auto_ingest_load_from_json_documents_without_content_field() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.path().join("docs.json");
        let json_content = r#"[
            {"text": "document 1"},
            {"data": "document 2"}
        ]"#;
        fs::write(&file_path, json_content).unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.load_from_json(file_path.to_str().unwrap()).await;
        
        let count = result.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_auto_ingest_load_from_json_empty_array() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.path().join("empty.json");
        fs::write(&file_path, "[]").unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.load_from_json(file_path.to_str().unwrap()).await;
        
        let count = result.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_empty_sources() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.auto_load(&[]).await;
        
        let count = result.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_unsupported_format() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec!["file.txt".to_string(), "file.csv".to_string()];
        let result = auto_ingest.auto_load(&sources).await;
        
        let count = result.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_nonexistent_json() {
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec!["/nonexistent/file.json".to_string()];
        let result = auto_ingest.auto_load(&sources).await;
        
        let count = result.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_valid_json_file() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.path().join("docs.json");
        let json_content = r#"[
            {"content": "doc1"},
            {"content": "doc2"}
        ]"#;
        fs::write(&file_path, json_content).unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec![file_path.to_str().unwrap().to_string()];
        let result = auto_ingest.auto_load(&sources).await;
        
        let count = result.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_multiple_json_files() {
        let temp_dir = create_temp_dir();
        
        let file1 = temp_dir.path().join("docs1.json");
        fs::write(&file1, r#"[{"content": "doc1"}]"#).unwrap();
        
        let file2 = temp_dir.path().join("docs2.json");
        fs::write(&file2, r#"[{"content": "doc2"}, {"content": "doc3"}]"#).unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec![
            file1.to_str().unwrap().to_string(),
            file2.to_str().unwrap().to_string(),
        ];
        let result = auto_ingest.auto_load(&sources).await;
        
        let count = result.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_mixed_valid_and_invalid() {
        let temp_dir = create_temp_dir();
        
        let valid_file = temp_dir.path().join("valid.json");
        fs::write(&valid_file, r#"[{"content": "doc1"}]"#).unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec![
            valid_file.to_str().unwrap().to_string(),
            "/nonexistent/file.json".to_string(),
            "unsupported.txt".to_string(),
        ];
        let result = auto_ingest.auto_load(&sources).await;
        
        let count = result.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_auto_ingest_load_from_json_with_unicode() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.path().join("unicode.json");
        let json_content = r#"[
            {"content": "Hello 世界"},
            {"content": "🎉 emoji test"}
        ]"#;
        fs::write(&file_path, json_content).unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.load_from_json(file_path.to_str().unwrap()).await;
        
        let count = result.unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_vector_search_args_debug() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 5,
            min_score: 0.5,
        };
        let debug_str = format!("{:?}", args);
        assert!(debug_str.contains("VectorSearchArgs"));
        assert!(debug_str.contains("test"));
        assert!(debug_str.contains("5"));
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
        assert!(debug_str.contains("test"));
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
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_ingest_document_debug() {
        let doc = IngestDocument {
            id: Some("123".to_string()),
            content: "test".to_string(),
            metadata: None,
        };
        let debug_str = format!("{:?}", doc);
        assert!(debug_str.contains("IngestDocument"));
        assert!(debug_str.contains("123"));
        assert!(debug_str.contains("test"));
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
    fn test_vector_ingest_response_debug() {
        let response = VectorIngestResponse {
            ingested_count: 5,
            success: true,
        };
        let debug_str = format!("{:?}", response);
        assert!(debug_str.contains("VectorIngestResponse"));
        assert!(debug_str.contains("5"));
    }

    #[test]
    fn test_vector_search_result_score_boundaries() {
        let result_zero = VectorSearchResult {
            content: "test".to_string(),
            score: 0.0,
            metadata: None,
        };
        assert_eq!(result_zero.score, 0.0);
        
        let result_one = VectorSearchResult {
            content: "test".to_string(),
            score: 1.0,
            metadata: None,
        };
        assert_eq!(result_one.score, 1.0);
        
        let result_negative = VectorSearchResult {
            content: "test".to_string(),
            score: -0.5,
            metadata: None,
        };
        assert_eq!(result_negative.score, -0.5);
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
    fn test_vector_search_result_metadata_types() {
        let result_null = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!(null)),
        };
        assert!(result_null.metadata.is_some());
        assert!(result_null.metadata.unwrap().is_null());
        
        let result_array = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!(["tag1", "tag2"])),
        };
        assert!(result_array.metadata.unwrap().is_array());
        
        let result_string = VectorSearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!("metadata string")),
        };
        assert!(result_string.metadata.unwrap().is_string());
    }

    #[test]
    fn test_ingest_document_empty_values() {
        let doc = IngestDocument {
            id: Some("".to_string()),
            content: "".to_string(),
            metadata: Some(json!({})),
        };
        assert_eq!(doc.id, Some("".to_string()));
        assert_eq!(doc.content, "");
        assert!(doc.metadata.is_some());
    }

    #[test]
    fn test_vector_ingest_response_failure() {
        let response = VectorIngestResponse {
            ingested_count: 0,
            success: false,
        };
        assert!(!response.success);
        assert_eq!(response.ingested_count, 0);
    }

    #[test]
    fn test_vector_ingest_response_large_count() {
        let response = VectorIngestResponse {
            ingested_count: 1000000,
            success: true,
        };
        assert_eq!(response.ingested_count, 1000000);
        assert!(response.success);
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
    fn test_vector_search_args_special_characters() {
        let args = VectorSearchArgs {
            query: "!@#$%^&*()".to_string(),
            limit: 5,
            min_score: 0.0,
        };
        assert_eq!(args.query, "!@#$%^&*()");
    }

    #[test]
    fn test_ingest_document_special_characters() {
        let doc = IngestDocument {
            id: Some("id-with_special.chars!".to_string()),
            content: "!@#$%^&*()".to_string(),
            metadata: None,
        };
        assert_eq!(doc.id, Some("id-with_special.chars!".to_string()));
        assert_eq!(doc.content, "!@#$%^&*()");
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

    #[test]
    fn test_ingest_document_very_long_values() {
        let long_id = "x".repeat(10000);
        let long_content = "a".repeat(100000);
        let doc = IngestDocument {
            id: Some(long_id.clone()),
            content: long_content.clone(),
            metadata: None,
        };
        assert_eq!(doc.id.unwrap().len(), 10000);
        assert_eq!(doc.content.len(), 100000);
    }

    #[tokio::test]
    async fn test_vector_search_tool_definition_context_prefix_formatting() {
        let store = create_test_vector_store(
            "test",
            Some("Based on the following information from the runbooks:".to_string())
        );
        let tool = VectorSearchMezmoKbTool::new(store);
        let definition = tool.definition(String::new()).await;
        
        assert!(definition.description.contains("runbooks"));
        assert!(!definition.description.contains("Based on the following information from the"));
    }

    #[tokio::test]
    async fn test_vector_search_tool_call_with_zero_limit() {
        let store = create_test_vector_store("test", None);
        let tool = VectorSearchMezmoKbTool::new(store);
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 0,
            min_score: 0.0,
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.total_found, 0);
    }

    #[tokio::test]
    async fn test_vector_ingest_tool_call_documents_with_all_fields() {
        let store = create_test_vector_store("test", None);
        let tool = VectorIngestTool::new(store);
        let args = VectorIngestArgs {
            documents: vec![
                IngestDocument {
                    id: Some("doc-1".to_string()),
                    content: "content 1".to_string(),
                    metadata: Some(json!({"type": "article", "tags": ["rust"]})),
                },
                IngestDocument {
                    id: Some("doc-2".to_string()),
                    content: "content 2".to_string(),
                    metadata: Some(json!({"type": "blog"})),
                },
            ],
        };
        let result = tool.call(args).await;
        
        let response = result.unwrap();
        assert_eq!(response.ingested_count, 2);
        assert!(response.success);
    }

    #[tokio::test]
    async fn test_auto_ingest_load_from_json_not_array() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.path().join("object.json");
        fs::write(&file_path, r#"{"content": "single doc"}"#).unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let result = auto_ingest.load_from_json(file_path.to_str().unwrap()).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, crate::error::BuilderError::VectorStoreError(_)));
    }

    #[tokio::test]
    async fn test_auto_ingest_auto_load_only_json_files_processed() {
        let temp_dir = create_temp_dir();
        
        let json_file = temp_dir.path().join("docs.json");
        fs::write(&json_file, r#"[{"content": "doc1"}]"#).unwrap();
        
        let store = create_test_vector_store("test", None);
        let auto_ingest = AutoIngest::new(store);
        let sources = vec![
            json_file.to_str().unwrap().to_string(),
            "file.txt".to_string(),
            "file.xml".to_string(),
            "file.yaml".to_string(),
        ];
        let result = auto_ingest.auto_load(&sources).await;
        
        let count = result.unwrap();
        assert_eq!(count, 1);
    }
}
