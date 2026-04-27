#[cfg(test)]
mod tests {
    use crate::vector_store::*;
    use crate::config::{EmbeddingModelConfig, VectorStoreConfig, VectorStoreType};
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_new_stub_initializes_all_fields() {
        let manager = VectorStoreManager::new_stub("test_store", None);
        assert_eq!(manager.get_store_name(), Some("test_store"));
        assert_eq!(manager.get_context_prefix(), None);
        let stats = manager.get_stats();
        assert_eq!(stats.store_type, "in_memory");
        assert_eq!(stats.embedding_provider, "stub");
        assert_eq!(stats.embedding_model, "stub");
        assert_eq!(stats.document_count, 0);
        assert_eq!(stats.index_size, 0);
    }

    #[test]
    fn test_new_stub_with_context_prefix_stores_prefix() {
        let manager = VectorStoreManager::new_stub("test", Some("Context:".to_string()));
        assert_eq!(manager.get_context_prefix(), Some("Context:"));
    }

    #[test]
    fn test_get_store_name_returns_various_names() {
        let manager1 = VectorStoreManager::new_stub("my_store", None);
        assert_eq!(manager1.get_store_name(), Some("my_store"));
        
        let manager2 = VectorStoreManager::new_stub("", None);
        assert_eq!(manager2.get_store_name(), Some(""));
        
        let manager3 = VectorStoreManager::new_stub("テスト店", None);
        assert_eq!(manager3.get_store_name(), Some("テスト店"));
    }

    #[test]
    fn test_get_context_prefix_returns_various_prefixes() {
        let manager1 = VectorStoreManager::new_stub("test", None);
        assert_eq!(manager1.get_context_prefix(), None);
        
        let manager2 = VectorStoreManager::new_stub("test", Some("Prefix:".to_string()));
        assert_eq!(manager2.get_context_prefix(), Some("Prefix:"));
        
        let manager3 = VectorStoreManager::new_stub("test", Some("".to_string()));
        assert_eq!(manager3.get_context_prefix(), Some(""));
        
        let manager4 = VectorStoreManager::new_stub("test", Some("コンテキスト:".to_string()));
        assert_eq!(manager4.get_context_prefix(), Some("コンテキスト:"));
    }

    #[tokio::test]
    async fn test_add_documents_bedrock_kb_returns_read_only_error() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        
        let docs = vec!["doc1".to_string(), "doc2".to_string()];
        let result = manager.add_documents(docs).await;
        
        let err = result.unwrap_err();
        assert!(err.to_string().contains("read-only") || err.to_string().contains("Bedrock Knowledge Base"));
    }

    #[tokio::test]
    async fn test_add_documents_logs_but_succeeds_for_various_inputs() {
        let manager = VectorStoreManager::new_stub("test", None);
        
        let empty_result = manager.add_documents(vec![]).await;
        assert!(empty_result.is_ok());
        
        let single_result = manager.add_documents(vec!["single document".to_string()]).await;
        assert!(single_result.is_ok());
        
        let multiple_result = manager.add_documents(vec![
            "document one".to_string(),
            "document two".to_string(),
            "document three".to_string(),
        ]).await;
        assert!(multiple_result.is_ok());
        
        let empty_string_result = manager.add_documents(vec!["".to_string()]).await;
        assert!(empty_string_result.is_ok());
        
        let unicode_result = manager.add_documents(vec!["Hello 世界 🎉".to_string()]).await;
        assert!(unicode_result.is_ok());
        
        let large_doc = "x".repeat(100000);
        let large_result = manager.add_documents(vec![large_doc]).await;
        assert!(large_result.is_ok());
        
        let many_docs: Vec<String> = (0..100).map(|i| format!("document {}", i)).collect();
        let many_result = manager.add_documents(many_docs).await;
        assert!(many_result.is_ok());
        
        let mixed_docs = vec![
            "".to_string(),
            "short".to_string(),
            "a".repeat(10000),
            "unicode 世界".to_string(),
            "newlines\n\n\n".to_string(),
            "line1\tcolumn2\nline2\tcolumn2".to_string(),
            "!@#$%^&*()".to_string(),
        ];
        let mixed_result = manager.add_documents(mixed_docs).await;
        assert!(mixed_result.is_ok());
    }

    #[tokio::test]
    async fn test_add_documents_qdrant_not_initialized_logs_but_succeeds() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        
        let docs = vec!["doc1".to_string()];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_in_memory_returns_empty_for_various_queries() {
        let manager = VectorStoreManager::new_stub("test", None);
        
        let result1 = manager.search("query", 5).await.unwrap();
        assert_eq!(result1.len(), 0);
        
        let result2 = manager.search("", 5).await.unwrap();
        assert_eq!(result2.len(), 0);
        
        let result3 = manager.search("世界", 5).await.unwrap();
        assert_eq!(result3.len(), 0);
        
        let result4 = manager.search("🎉", 5).await.unwrap();
        assert_eq!(result4.len(), 0);
        
        let result5 = manager.search("line1\nline2", 5).await.unwrap();
        assert_eq!(result5.len(), 0);
        
        let result6 = manager.search("!@#$%^&*()", 5).await.unwrap();
        assert_eq!(result6.len(), 0);
        
        let result7 = manager.search("query\twith\ttabs", 5).await.unwrap();
        assert_eq!(result7.len(), 0);
        
        let long_query = "query ".repeat(1000);
        let result8 = manager.search(&long_query, 5).await.unwrap();
        assert_eq!(result8.len(), 0);
    }

    #[tokio::test]
    async fn test_search_in_memory_respects_various_limits() {
        let manager = VectorStoreManager::new_stub("test", None);
        
        let result1 = manager.search("query", 0).await.unwrap();
        assert_eq!(result1.len(), 0);
        
        let result2 = manager.search("query", 1).await.unwrap();
        assert_eq!(result2.len(), 0);
        
        let result3 = manager.search("query", 1000).await.unwrap();
        assert_eq!(result3.len(), 0);
        
        let result4 = manager.search("query", usize::MAX).await.unwrap();
        assert_eq!(result4.len(), 0);
    }

    #[tokio::test]
    async fn test_search_unsupported_store_type_returns_error() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "unsupported".to_string();
        
        let result = manager.search("query", 5).await;
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("Unsupported") || err_msg.contains("unsupported"));
    }

    #[tokio::test]
    async fn test_search_qdrant_not_initialized_returns_error() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        
        let result = manager.search("query", 5).await;
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("not initialized") || err_msg.contains("Qdrant"));
    }

    #[tokio::test]
    async fn test_search_bedrock_kb_missing_client_returns_error() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        
        let result = manager.search("query", 5).await;
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("not initialized") || err_msg.contains("client") || err_msg.contains("KB"));
    }

    #[tokio::test]
    async fn test_search_bedrock_kb_various_error_conditions() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        
        let result1 = manager.search("query", 0).await;
        assert!(result1.is_err());
        
        let result2 = manager.search("", 5).await;
        assert!(result2.is_err());
    }

    #[tokio::test]
    async fn test_search_with_filter_in_memory_returns_empty_for_various_filters() {
        let manager = VectorStoreManager::new_stub("test", None);
        
        let empty_filters = HashMap::new();
        let result1 = manager.search_with_filter("query", 5, empty_filters).await.unwrap();
        assert_eq!(result1.len(), 0);
        
        let mut single_filter = HashMap::new();
        single_filter.insert("type".to_string(), json!("document"));
        let result2 = manager.search_with_filter("query", 5, single_filter).await.unwrap();
        assert_eq!(result2.len(), 0);
        
        let mut multiple_filters = HashMap::new();
        multiple_filters.insert("key1".to_string(), json!("value1"));
        multiple_filters.insert("key2".to_string(), json!(42));
        multiple_filters.insert("key3".to_string(), json!(true));
        let result3 = manager.search_with_filter("query", 5, multiple_filters).await.unwrap();
        assert_eq!(result3.len(), 0);
        
        let mut complex_filters = HashMap::new();
        complex_filters.insert("array".to_string(), json!(["a", "b", "c"]));
        complex_filters.insert("object".to_string(), json!({"nested": "value"}));
        let result4 = manager.search_with_filter("query", 5, complex_filters).await.unwrap();
        assert_eq!(result4.len(), 0);
        
        let mut string_filter = HashMap::new();
        string_filter.insert("status".to_string(), json!("active"));
        let result5 = manager.search_with_filter("query", 5, string_filter).await.unwrap();
        assert_eq!(result5.len(), 0);
        
        let mut number_filter = HashMap::new();
        number_filter.insert("count".to_string(), json!(100));
        let result6 = manager.search_with_filter("query", 5, number_filter).await.unwrap();
        assert_eq!(result6.len(), 0);
        
        let mut boolean_filter = HashMap::new();
        boolean_filter.insert("active".to_string(), json!(true));
        let result7 = manager.search_with_filter("query", 5, boolean_filter).await.unwrap();
        assert_eq!(result7.len(), 0);
        
        let mut null_filter = HashMap::new();
        null_filter.insert("key".to_string(), json!(null));
        let result8 = manager.search_with_filter("query", 5, null_filter).await.unwrap();
        assert_eq!(result8.len(), 0);
    }

    #[tokio::test]
    async fn test_search_with_filter_bedrock_kb_falls_back_to_unfiltered() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_with_filter_qdrant_not_initialized_returns_error() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("query", 5, filters).await;
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("not initialized") || err_msg.contains("Qdrant"));
    }

    #[tokio::test]
    async fn test_search_with_filter_qdrant_various_error_conditions() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        let filters = HashMap::new();
        
        let result1 = manager.search_with_filter("", 5, filters.clone()).await;
        assert!(result1.is_err());
        
        let result2 = manager.search_with_filter("query", 0, filters).await;
        assert!(result2.is_err());
    }

    #[tokio::test]
    async fn test_search_with_filter_unsupported_store_returns_error() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "unknown".to_string();
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("query", 5, filters).await;
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("Unsupported") || err_msg.contains("unknown"));
    }

    #[tokio::test]
    async fn test_search_with_filter_various_queries_and_limits() {
        let manager = VectorStoreManager::new_stub("test", None);
        let filters = HashMap::new();
        
        let long_query = "query ".repeat(1000);
        let result1 = manager.search_with_filter(&long_query, 5, filters.clone()).await.unwrap();
        assert_eq!(result1.len(), 0);
        
        let result2 = manager.search_with_filter("query", 1, filters.clone()).await.unwrap();
        assert_eq!(result2.len(), 0);
        
        let result3 = manager.search_with_filter("query", usize::MAX, filters).await.unwrap();
        assert_eq!(result3.len(), 0);
    }

    #[test]
    fn test_format_search_results_empty_returns_no_results_message() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![];
        
        let formatted1 = manager.format_search_results(&results, "test query");
        assert_eq!(formatted1, "No results found for query: 'test query'");
        
        let formatted2 = manager.format_search_results(&results, "");
        assert_eq!(formatted2, "No results found for query: ''");
        
        let formatted3 = manager.format_search_results(&results, "query with 'quotes' and \"double\"");
        assert!(formatted3.contains("query with 'quotes' and \"double\""));
        
        let formatted4 = manager.format_search_results(&results, "query\nwith\nnewlines");
        assert!(formatted4.contains("query\nwith\nnewlines"));
    }

    #[test]
    fn test_format_search_results_empty_with_prefix_omits_prefix() {
        let manager = VectorStoreManager::new_stub("test", Some("Prefix:".to_string()));
        let results = vec![];
        let formatted = manager.format_search_results(&results, "query");
        assert_eq!(formatted, "No results found for query: 'query'");
        assert!(!formatted.contains("Prefix:"));
    }

    #[test]
    fn test_format_search_results_single_result_includes_all_components() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "test content".to_string(),
            score: 0.95,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("Result 1"));
        assert!(formatted.contains("0.950"));
        assert!(formatted.contains("test content"));
        assert!(!formatted.contains("---"));
    }

    #[test]
    fn test_format_search_results_multiple_results_includes_separators() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![
            SearchResult {
                content: "first".to_string(),
                score: 0.9,
                metadata: None,
            },
            SearchResult {
                content: "second".to_string(),
                score: 0.8,
                metadata: None,
            },
        ];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("Result 1"));
        assert!(formatted.contains("Result 2"));
        assert!(formatted.contains("first"));
        assert!(formatted.contains("second"));
        assert!(formatted.contains("---"));
        assert!(!formatted.ends_with("---"));
    }

    #[test]
    fn test_format_search_results_separator_count_matches_result_count() {
        let manager = VectorStoreManager::new_stub("test", None);
        
        let results3: Vec<SearchResult> = (0..3)
            .map(|i| SearchResult {
                content: format!("content {}", i),
                score: 0.9 - (i as f32 * 0.1),
                metadata: None,
            })
            .collect();
        let formatted3 = manager.format_search_results(&results3, "query");
        assert_eq!(formatted3.matches("---").count(), 2);
        
        let results10: Vec<SearchResult> = (0..10)
            .map(|i| SearchResult {
                content: format!("content {}", i),
                score: 0.9 - (i as f32 * 0.05),
                metadata: None,
            })
            .collect();
        let formatted10 = manager.format_search_results(&results10, "query");
        assert_eq!(formatted10.matches("---").count(), 9);
        
        let results20: Vec<SearchResult> = (0..20)
            .map(|i| SearchResult {
                content: format!("content {}", i),
                score: 1.0 - (i as f32 * 0.05),
                metadata: None,
            })
            .collect();
        let formatted20 = manager.format_search_results(&results20, "query");
        assert_eq!(formatted20.matches("---").count(), 19);
    }

    #[test]
    fn test_format_search_results_with_context_prefix_prepends_prefix() {
        let manager = VectorStoreManager::new_stub("test", Some("Context:".to_string()));
        let results = vec![SearchResult {
            content: "content".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.starts_with("Context:\n\n"));
        assert!(formatted.contains("content"));
    }

    #[test]
    fn test_format_search_results_prefix_variations() {
        let manager1 = VectorStoreManager::new_stub("test", Some("".to_string()));
        let results = vec![SearchResult {
            content: "content".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted1 = manager1.format_search_results(&results, "query");
        assert!(formatted1.starts_with("\n\n"));
        
        let manager2 = VectorStoreManager::new_stub("test", Some("Line1\nLine2".to_string()));
        let formatted2 = manager2.format_search_results(&results, "query");
        assert!(formatted2.starts_with("Line1\nLine2\n\n"));
        
        let manager3 = VectorStoreManager::new_stub("test", Some("Line1\nLine2\nLine3".to_string()));
        let formatted3 = manager3.format_search_results(&results, "query");
        assert!(formatted3.starts_with("Line1\nLine2\nLine3\n\n"));
    }

    #[test]
    fn test_format_search_results_score_formatting_three_decimals() {
        let manager = VectorStoreManager::new_stub("test", None);
        
        let test_cases = vec![
            (0.0, "0.000"),
            (1.0, "1.000"),
            (0.123456, "0.123"),
            (0.987654321, "0.988"),
            (0.5, "0.500"),
            (0.001, "0.001"),
            (1000.0, "1000.000"),
            (-0.5, "-0.500"),
        ];
        
        for (score, expected) in test_cases {
            let results = vec![SearchResult {
                content: "test".to_string(),
                score,
                metadata: None,
            }];
            let formatted = manager.format_search_results(&results, "query");
            assert!(formatted.contains(expected), "Score {} should format to {}, but formatted output was: {}", score, expected, formatted);
        }
    }

    #[test]
    fn test_format_search_results_various_content_types() {
        let manager = VectorStoreManager::new_stub("test", None);
        
        let results1 = vec![SearchResult {
            content: "Hello 世界 🎉".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted1 = manager.format_search_results(&results1, "query");
        assert!(formatted1.contains("Hello 世界 🎉"));
        
        let results2 = vec![SearchResult {
            content: "".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted2 = manager.format_search_results(&results2, "query");
        assert!(formatted2.contains("Result 1"));
        
        let results3 = vec![SearchResult {
            content: "line1\nline2\nline3".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted3 = manager.format_search_results(&results3, "query");
        assert!(formatted3.contains("line1\nline2\nline3"));
        
        let results4 = vec![SearchResult {
            content: "line1\rline2".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted4 = manager.format_search_results(&results4, "query");
        assert!(formatted4.contains("line1\rline2"));
    }

    #[test]
    fn test_search_result_construction_and_fields() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({"id": "123", "type": "doc"})),
        };
        assert_eq!(result.content, "test");
        assert_eq!(result.score, 0.5);
        assert!(result.metadata.is_some());
        let metadata = result.metadata.unwrap();
        assert_eq!(metadata["id"], "123");
        assert_eq!(metadata["type"], "doc");
    }

    #[test]
    fn test_search_result_metadata_variations() {
        let result1 = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: None,
        };
        assert!(result1.metadata.is_none());
        
        let result2 = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({})),
        };
        assert!(result2.metadata.is_some());
        assert!(result2.metadata.unwrap().as_object().unwrap().is_empty());
        
        let result3 = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!(null)),
        };
        assert!(result3.metadata.is_some());
        assert!(result3.metadata.unwrap().is_null());
        
        let result4 = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!(["tag1", "tag2"])),
        };
        assert!(result4.metadata.is_some());
        let metadata4 = result4.metadata.unwrap();
        assert!(metadata4.is_array());
        assert_eq!(metadata4.as_array().unwrap().len(), 2);
        
        let result5 = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!("metadata string")),
        };
        assert_eq!(result5.metadata.unwrap(), "metadata string");
        
        let result6 = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!(42)),
        };
        assert_eq!(result6.metadata.unwrap(), 42);
        
        let result7 = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!(true)),
        };
        assert_eq!(result7.metadata.unwrap(), true);
    }

    #[test]
    fn test_search_result_score_edge_cases() {
        let result1 = SearchResult {
            content: "test".to_string(),
            score: 0.123456789,
            metadata: None,
        };
        assert_eq!(result1.score, 0.123456789);
        
        let result2 = SearchResult {
            content: "test".to_string(),
            score: -0.5,
            metadata: None,
        };
        assert_eq!(result2.score, -0.5);
        
        let result3 = SearchResult {
            content: "test".to_string(),
            score: 999.999,
            metadata: None,
        };
        assert_eq!(result3.score, 999.999);
    }

    #[test]
    fn test_vector_store_stats_construction_and_fields() {
        let stats = VectorStoreStats {
            store_type: "qdrant".to_string(),
            embedding_provider: "openai".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
            document_count: 100,
            index_size: 1024,
        };
        assert_eq!(stats.store_type, "qdrant");
        assert_eq!(stats.embedding_provider, "openai");
        assert_eq!(stats.embedding_model, "text-embedding-3-small");
        assert_eq!(stats.document_count, 100);
        assert_eq!(stats.index_size, 1024);
    }

    #[test]
    fn test_vector_store_stats_various_values() {
        let stats1 = VectorStoreStats {
            store_type: "in_memory".to_string(),
            embedding_provider: "openai".to_string(),
            embedding_model: "model".to_string(),
            document_count: 0,
            index_size: 0,
        };
        assert_eq!(stats1.document_count, 0);
        assert_eq!(stats1.index_size, 0);
        
        let stats2 = VectorStoreStats {
            store_type: "qdrant".to_string(),
            embedding_provider: "bedrock".to_string(),
            embedding_model: "amazon.titan-embed-text-v1".to_string(),
            document_count: 500,
            index_size: 2048,
        };
        assert_eq!(stats2.document_count, 500);
        assert_eq!(stats2.index_size, 2048);
        
        let stats3 = VectorStoreStats {
            store_type: "qdrant".to_string(),
            embedding_provider: "bedrock".to_string(),
            embedding_model: "model".to_string(),
            document_count: 1_000_000,
            index_size: 10_000_000,
        };
        assert_eq!(stats3.document_count, 1_000_000);
        assert_eq!(stats3.index_size, 10_000_000);
        
        let stats4 = VectorStoreStats {
            store_type: "".to_string(),
            embedding_provider: "".to_string(),
            embedding_model: "".to_string(),
            document_count: 0,
            index_size: 0,
        };
        assert_eq!(stats4.store_type, "");
        assert_eq!(stats4.embedding_provider, "");
        assert_eq!(stats4.embedding_model, "");
        
        let stats5 = VectorStoreStats {
            store_type: "ベクトル".to_string(),
            embedding_provider: "プロバイダー".to_string(),
            embedding_model: "モデル".to_string(),
            document_count: 10,
            index_size: 100,
        };
        assert_eq!(stats5.store_type, "ベクトル");
        assert_eq!(stats5.embedding_provider, "プロバイダー");
        assert_eq!(stats5.embedding_model, "モデル");
    }

    #[test]
    fn test_document_new_creates_document_with_content_only() {
        let doc = Document::new("test content".to_string());
        assert_eq!(doc.content, "test content");
        assert!(doc.id.is_none());
        assert!(doc.metadata.is_none());
    }

    #[test]
    fn test_document_new_with_various_content() {
        let doc1 = Document::new("".to_string());
        assert_eq!(doc1.content, "");
        
        let doc2 = Document::new("Hello 世界 🎉".to_string());
        assert_eq!(doc2.content, "Hello 世界 🎉");
        
        let doc3 = Document::new("line1\nline2\nline3".to_string());
        assert_eq!(doc3.content, "line1\nline2\nline3");
        
        let doc4 = Document::new("!@#$%^&*()".to_string());
        assert_eq!(doc4.content, "!@#$%^&*()");
        
        let doc5 = Document::new("line1\tcolumn2\nline2\tcolumn2".to_string());
        assert_eq!(doc5.content, "line1\tcolumn2\nline2\tcolumn2");
        
        let doc6 = Document::new("line1\rline2".to_string());
        assert_eq!(doc6.content, "line1\rline2");
        
        let doc7 = Document::new("line1\n\tline2  \r\nline3".to_string());
        assert_eq!(doc7.content, "line1\n\tline2  \r\nline3");
        
        let doc8 = Document::new("test\0content".to_string());
        assert_eq!(doc8.content, "test\0content");
        
        let content = "a".repeat(10000);
        let doc9 = Document::new(content.clone());
        assert_eq!(doc9.content.len(), 10000);
    }

    #[test]
    fn test_document_with_id_sets_id() {
        let doc = Document::new("content".to_string()).with_id("doc123".to_string());
        assert_eq!(doc.content, "content");
        assert_eq!(doc.id, Some("doc123".to_string()));
        assert!(doc.metadata.is_none());
    }

    #[test]
    fn test_document_with_id_various_ids() {
        let doc1 = Document::new("content".to_string()).with_id("".to_string());
        assert_eq!(doc1.id, Some("".to_string()));
        
        let doc2 = Document::new("content".to_string()).with_id("id-with-dashes_and_underscores.123".to_string());
        assert_eq!(doc2.id, Some("id-with-dashes_and_underscores.123".to_string()));
        
        let id = "x".repeat(1000);
        let doc3 = Document::new("content".to_string()).with_id(id.clone());
        assert_eq!(doc3.id, Some(id));
    }

    #[test]
    fn test_document_with_metadata_sets_metadata() {
        let metadata = json!({"key": "value"});
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.content, "content");
        assert!(doc.id.is_none());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_with_metadata_various_types() {
        let doc1 = Document::new("content".to_string()).with_metadata(json!({}));
        assert_eq!(doc1.metadata, Some(json!({})));
        
        let doc2 = Document::new("content".to_string()).with_metadata(json!(null));
        assert_eq!(doc2.metadata, Some(json!(null)));
        
        let doc3 = Document::new("content".to_string()).with_metadata(json!(["tag1", "tag2", "tag3"]));
        assert_eq!(doc3.metadata, Some(json!(["tag1", "tag2", "tag3"])));
        
        let doc4 = Document::new("content".to_string()).with_metadata(json!("simple string"));
        assert_eq!(doc4.metadata, Some(json!("simple string")));
        
        let doc5 = Document::new("content".to_string()).with_metadata(json!(42));
        assert_eq!(doc5.metadata, Some(json!(42)));
        
        let doc6 = Document::new("content".to_string()).with_metadata(json!(true));
        assert_eq!(doc6.metadata, Some(json!(true)));
        
        let metadata = json!({
            "id": "123",
            "type": "article",
            "tags": ["rust", "programming"],
            "nested": {
                "key": "value"
            }
        });
        let doc7 = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc7.metadata, Some(metadata));
        
        let metadata8 = json!({"名前": "値", "emoji": "🎉"});
        let doc8 = Document::new("content".to_string()).with_metadata(metadata8.clone());
        assert_eq!(doc8.metadata, Some(metadata8));
    }

    #[test]
    fn test_document_builder_chain_both_id_and_metadata() {
        let metadata = json!({"type": "article"});
        let doc = Document::new("content".to_string())
            .with_id("123".to_string())
            .with_metadata(metadata.clone());
        assert_eq!(doc.content, "content");
        assert_eq!(doc.id, Some("123".to_string()));
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_builder_chain_reverse_order() {
        let metadata = json!({"type": "article"});
        let doc = Document::new("content".to_string())
            .with_metadata(metadata.clone())
            .with_id("123".to_string());
        assert_eq!(doc.content, "content");
        assert_eq!(doc.id, Some("123".to_string()));
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_builder_multiple_calls_overwrites() {
        let doc1 = Document::new("content".to_string())
            .with_id("first".to_string())
            .with_id("second".to_string());
        assert_eq!(doc1.id, Some("second".to_string()));
        
        let doc2 = Document::new("content".to_string())
            .with_metadata(json!({"first": "value"}))
            .with_metadata(json!({"second": "value"}));
        assert_eq!(doc2.metadata, Some(json!({"second": "value"})));
    }

    #[test]
    fn test_embedding_model_config_provider_and_model() {
        let config1 = EmbeddingModelConfig::OpenAI {
            api_key: "sk-test".to_string(),
            model: "text-embedding-3-small".to_string(),
            base_url: None,
        };
        assert_eq!(config1.provider(), "openai");
        assert_eq!(config1.model(), "text-embedding-3-small");
        
        let config2 = EmbeddingModelConfig::Bedrock {
            model: "amazon.titan-embed-text-v1".to_string(),
            region: "us-east-1".to_string(),
            profile: None,
        };
        assert_eq!(config2.provider(), "bedrock");
        assert_eq!(config2.model(), "amazon.titan-embed-text-v1");
        
        let config3 = EmbeddingModelConfig::OpenAI {
            api_key: "sk-test".to_string(),
            model: "text-embedding-3-small".to_string(),
            base_url: Some("https://custom.openai.com".to_string()),
        };
        assert_eq!(config3.provider(), "openai");
        assert_eq!(config3.model(), "text-embedding-3-small");
        
        let config4 = EmbeddingModelConfig::Bedrock {
            model: "amazon.titan-embed-text-v1".to_string(),
            region: "us-west-2".to_string(),
            profile: Some("my-profile".to_string()),
        };
        assert_eq!(config4.provider(), "bedrock");
        assert_eq!(config4.model(), "amazon.titan-embed-text-v1");
    }

    #[test]
    fn test_vector_store_config_construction_with_context_prefix() {
        let config1 = VectorStoreConfig {
            name: "test".to_string(),
            context_prefix: Some("Context:".to_string()),
            store: VectorStoreType::InMemory {
                embedding_model: EmbeddingModelConfig::OpenAI {
                    api_key: "sk-test".to_string(),
                    model: "text-embedding-3-small".to_string(),
                    base_url: None,
                },
            },
        };
        assert_eq!(config1.context_prefix, Some("Context:".to_string()));
        
        let config2 = VectorStoreConfig {
            name: "test".to_string(),
            context_prefix: None,
            store: VectorStoreType::InMemory {
                embedding_model: EmbeddingModelConfig::OpenAI {
                    api_key: "sk-test".to_string(),
                    model: "text-embedding-3-small".to_string(),
                    base_url: None,
                },
            },
        };
        assert!(config2.context_prefix.is_none());
    }

    #[test]
    fn test_vector_store_config_various_store_types() {
        let config1 = VectorStoreConfig {
            name: "test".to_string(),
            context_prefix: None,
            store: VectorStoreType::InMemory {
                embedding_model: EmbeddingModelConfig::OpenAI {
                    api_key: "sk-test".to_string(),
                    model: "text-embedding-3-small".to_string(),
                    base_url: None,
                },
            },
        };
        assert_eq!(config1.name, "test");
        
        let config2 = VectorStoreConfig {
            name: "test".to_string(),
            context_prefix: None,
            store: VectorStoreType::Qdrant {
                embedding_model: EmbeddingModelConfig::OpenAI {
                    api_key: "sk-test".to_string(),
                    model: "text-embedding-3-small".to_string(),
                    base_url: None,
                },
                url: "http://localhost:6334".to_string(),
                collection_name: "test_collection".to_string(),
            },
        };
        assert_eq!(config2.name, "test");
        
        let config3 = VectorStoreConfig {
            name: "test".to_string(),
            context_prefix: None,
            store: VectorStoreType::BedrockKb {
                knowledge_base_id: "kb-123".to_string(),
                region: "us-east-1".to_string(),
                profile: None,
            },
        };
        assert_eq!(config3.name, "test");
    }
}
