#[cfg(test)]
mod tests {
    use crate::vector_store::*;
    use crate::config::{EmbeddingModelConfig, VectorStoreConfig, VectorStoreType};
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_new_stub() {
        let manager = VectorStoreManager::new_stub("test_store", None);
        assert_eq!(manager.store_name, "test_store");
        assert_eq!(manager.store_type, "in_memory");
        assert!(manager.qdrant_url.is_none());
        assert!(manager.collection_name.is_none());
        assert!(manager.in_memory_store.is_none());
        assert!(manager.context_prefix.is_none());
    }

    #[test]
    fn test_new_stub_with_context_prefix() {
        let prefix = Some("Context:".to_string());
        let manager = VectorStoreManager::new_stub("test_store", prefix.clone());
        assert_eq!(manager.store_name, "test_store");
        assert_eq!(manager.context_prefix, prefix);
    }

    #[test]
    fn test_new_stub_empty_name() {
        let manager = VectorStoreManager::new_stub("", None);
        assert_eq!(manager.store_name, "");
    }

    #[test]
    fn test_get_store_name() {
        let manager = VectorStoreManager::new_stub("my_store", None);
        assert_eq!(manager.get_store_name(), Some("my_store"));
    }

    #[test]
    fn test_get_store_name_empty() {
        let manager = VectorStoreManager::new_stub("", None);
        assert_eq!(manager.get_store_name(), Some(""));
    }

    #[test]
    fn test_get_context_prefix_none() {
        let manager = VectorStoreManager::new_stub("test", None);
        assert_eq!(manager.get_context_prefix(), None);
    }

    #[test]
    fn test_get_context_prefix_some() {
        let manager = VectorStoreManager::new_stub("test", Some("Prefix:".to_string()));
        assert_eq!(manager.get_context_prefix(), Some("Prefix:"));
    }

    #[test]
    fn test_get_context_prefix_empty_string() {
        let manager = VectorStoreManager::new_stub("test", Some("".to_string()));
        assert_eq!(manager.get_context_prefix(), Some(""));
    }

    #[test]
    fn test_get_stats() {
        let manager = VectorStoreManager::new_stub("test_store", None);
        let stats = manager.get_stats();
        assert_eq!(stats.store_type, "in_memory");
        assert_eq!(stats.embedding_provider, "stub");
        assert_eq!(stats.embedding_model, "stub");
        assert_eq!(stats.document_count, 0);
        assert_eq!(stats.index_size, 0);
    }

    #[test]
    fn test_get_stats_clone() {
        let manager = VectorStoreManager::new_stub("test", None);
        let stats1 = manager.get_stats();
        let stats2 = stats1.clone();
        assert_eq!(stats1.store_type, stats2.store_type);
        assert_eq!(stats1.embedding_provider, stats2.embedding_provider);
        assert_eq!(stats1.embedding_model, stats2.embedding_model);
        assert_eq!(stats1.document_count, stats2.document_count);
        assert_eq!(stats1.index_size, stats2.index_size);
    }

    #[tokio::test]
    async fn test_add_documents_bedrock_kb_error() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        
        let docs = vec!["doc1".to_string(), "doc2".to_string()];
        let result = manager.add_documents(docs).await;
        
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    #[tokio::test]
    async fn test_add_documents_empty_vec() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.add_documents(vec![]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_documents_single_document() {
        let manager = VectorStoreManager::new_stub("test", None);
        let docs = vec!["single document".to_string()];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_documents_multiple_documents() {
        let manager = VectorStoreManager::new_stub("test", None);
        let docs = vec![
            "document one".to_string(),
            "document two".to_string(),
            "document three".to_string(),
        ];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_documents_empty_string() {
        let manager = VectorStoreManager::new_stub("test", None);
        let docs = vec!["".to_string()];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_documents_unicode() {
        let manager = VectorStoreManager::new_stub("test", None);
        let docs = vec!["Hello 世界 🎉".to_string()];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_in_memory_returns_empty() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.search("query", 5).await;
        assert!(result.is_ok());
        let results = result.unwrap();
        assert_eq!(results.len(), 0);
    }

    #[tokio::test]
    async fn test_search_in_memory_empty_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.search("", 5).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_search_in_memory_zero_limit() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.search("query", 0).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_search_in_memory_large_limit() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.search("query", 1000).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_search_unsupported_store_type() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "unsupported".to_string();
        
        let result = manager.search("query", 5).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unsupported store type"));
    }

    #[tokio::test]
    async fn test_search_qdrant_not_initialized() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        
        let result = manager.search("query", 5).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[tokio::test]
    async fn test_search_bedrock_kb_client_not_initialized() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        
        let result = manager.search("query", 5).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_with_filter_in_memory() {
        let manager = VectorStoreManager::new_stub("test", None);
        let mut filters = HashMap::new();
        filters.insert("key".to_string(), json!("value"));
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_search_with_filter_empty_filters() {
        let manager = VectorStoreManager::new_stub("test", None);
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_with_filter_bedrock_kb() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_with_filter_qdrant_not_initialized() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_with_filter_unsupported_store() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "unknown".to_string();
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_format_search_results_empty() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![];
        let formatted = manager.format_search_results(&results, "test query");
        assert_eq!(formatted, "No results found for query: 'test query'");
    }

    #[test]
    fn test_format_search_results_empty_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![];
        let formatted = manager.format_search_results(&results, "");
        assert_eq!(formatted, "No results found for query: ''");
    }

    #[test]
    fn test_format_search_results_single_result() {
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
    }

    #[test]
    fn test_format_search_results_multiple_results() {
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
    }

    #[test]
    fn test_format_search_results_with_context_prefix() {
        let manager = VectorStoreManager::new_stub("test", Some("Context:".to_string()));
        let results = vec![SearchResult {
            content: "content".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.starts_with("Context:"));
        assert!(formatted.contains("content"));
    }

    #[test]
    fn test_format_search_results_no_separator_after_last() {
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
        assert!(!formatted.ends_with("---"));
    }

    #[test]
    fn test_format_search_results_score_formatting() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "test".to_string(),
            score: 0.123456,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("0.123"));
    }

    #[test]
    fn test_format_search_results_zero_score() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "test".to_string(),
            score: 0.0,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("0.000"));
    }

    #[test]
    fn test_format_search_results_max_score() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "test".to_string(),
            score: 1.0,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("1.000"));
    }

    #[test]
    fn test_format_search_results_unicode_content() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "Hello 世界 🎉".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("Hello 世界 🎉"));
    }

    #[test]
    fn test_format_search_results_empty_content() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("Result 1"));
    }

    #[test]
    fn test_format_search_results_multiline_content() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "line1\nline2\nline3".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("line1\nline2\nline3"));
    }

    #[test]
    fn test_search_result_clone() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({"key": "value"})),
        };
        let cloned = result.clone();
        assert_eq!(cloned.content, result.content);
        assert_eq!(cloned.score, result.score);
        assert_eq!(cloned.metadata, result.metadata);
    }

    #[test]
    fn test_search_result_with_metadata() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({"id": "123", "type": "doc"})),
        };
        assert!(result.metadata.is_some());
        let metadata = result.metadata.unwrap();
        assert_eq!(metadata["id"], "123");
        assert_eq!(metadata["type"], "doc");
    }

    #[test]
    fn test_search_result_without_metadata() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: None,
        };
        assert!(result.metadata.is_none());
    }

    #[test]
    fn test_search_result_debug() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: None,
        };
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("SearchResult"));
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_vector_store_stats_clone() {
        let stats = VectorStoreStats {
            store_type: "qdrant".to_string(),
            embedding_provider: "openai".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
            document_count: 100,
            index_size: 1024,
        };
        let cloned = stats.clone();
        assert_eq!(cloned.store_type, stats.store_type);
        assert_eq!(cloned.embedding_provider, stats.embedding_provider);
        assert_eq!(cloned.embedding_model, stats.embedding_model);
        assert_eq!(cloned.document_count, stats.document_count);
        assert_eq!(cloned.index_size, stats.index_size);
    }

    #[test]
    fn test_vector_store_stats_debug() {
        let stats = VectorStoreStats {
            store_type: "in_memory".to_string(),
            embedding_provider: "openai".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
            document_count: 10,
            index_size: 512,
        };
        let debug_str = format!("{:?}", stats);
        assert!(debug_str.contains("VectorStoreStats"));
        assert!(debug_str.contains("in_memory"));
    }

    #[test]
    fn test_vector_store_stats_with_counts() {
        let stats = VectorStoreStats {
            store_type: "qdrant".to_string(),
            embedding_provider: "bedrock".to_string(),
            embedding_model: "amazon.titan-embed-text-v1".to_string(),
            document_count: 500,
            index_size: 2048,
        };
        assert_eq!(stats.document_count, 500);
        assert_eq!(stats.index_size, 2048);
    }

    #[test]
    fn test_document_new() {
        let doc = Document::new("test content".to_string());
        assert_eq!(doc.content, "test content");
        assert!(doc.id.is_none());
        assert!(doc.metadata.is_none());
    }

    #[test]
    fn test_document_new_empty() {
        let doc = Document::new("".to_string());
        assert_eq!(doc.content, "");
        assert!(doc.id.is_none());
        assert!(doc.metadata.is_none());
    }

    #[test]
    fn test_document_with_id() {
        let doc = Document::new("content".to_string()).with_id("doc123".to_string());
        assert_eq!(doc.content, "content");
        assert_eq!(doc.id, Some("doc123".to_string()));
        assert!(doc.metadata.is_none());
    }

    #[test]
    fn test_document_with_id_empty() {
        let doc = Document::new("content".to_string()).with_id("".to_string());
        assert_eq!(doc.id, Some("".to_string()));
    }

    #[test]
    fn test_document_with_metadata() {
        let metadata = json!({"key": "value"});
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.content, "content");
        assert!(doc.id.is_none());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_with_metadata_empty_object() {
        let metadata = json!({});
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_builder_chain() {
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
    fn test_document_clone() {
        let metadata = json!({"key": "value"});
        let doc = Document::new("content".to_string())
            .with_id("123".to_string())
            .with_metadata(metadata.clone());
        let cloned = doc.clone();
        assert_eq!(cloned.content, doc.content);
        assert_eq!(cloned.id, doc.id);
        assert_eq!(cloned.metadata, doc.metadata);
    }

    #[test]
    fn test_document_debug() {
        let doc = Document::new("test".to_string());
        let debug_str = format!("{:?}", doc);
        assert!(debug_str.contains("Document"));
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_document_unicode_content() {
        let doc = Document::new("Hello 世界 🎉".to_string());
        assert_eq!(doc.content, "Hello 世界 🎉");
    }

    #[test]
    fn test_document_multiline_content() {
        let doc = Document::new("line1\nline2\nline3".to_string());
        assert_eq!(doc.content, "line1\nline2\nline3");
    }

    #[test]
    fn test_document_metadata_complex() {
        let metadata = json!({
            "id": "123",
            "type": "article",
            "tags": ["rust", "programming"],
            "nested": {
                "key": "value"
            }
        });
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_metadata_null() {
        let metadata = json!(null);
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_metadata_array() {
        let metadata = json!(["tag1", "tag2", "tag3"]);
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_metadata_string() {
        let metadata = json!("simple string");
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_metadata_number() {
        let metadata = json!(42);
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_document_metadata_boolean() {
        let metadata = json!(true);
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_search_result_negative_score() {
        let result = SearchResult {
            content: "test".to_string(),
            score: -0.5,
            metadata: None,
        };
        assert_eq!(result.score, -0.5);
    }

    #[test]
    fn test_search_result_large_score() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 999.999,
            metadata: None,
        };
        assert_eq!(result.score, 999.999);
    }

    #[test]
    fn test_vector_store_stats_zero_counts() {
        let stats = VectorStoreStats {
            store_type: "in_memory".to_string(),
            embedding_provider: "openai".to_string(),
            embedding_model: "model".to_string(),
            document_count: 0,
            index_size: 0,
        };
        assert_eq!(stats.document_count, 0);
        assert_eq!(stats.index_size, 0);
    }

    #[test]
    fn test_vector_store_stats_large_counts() {
        let stats = VectorStoreStats {
            store_type: "qdrant".to_string(),
            embedding_provider: "bedrock".to_string(),
            embedding_model: "model".to_string(),
            document_count: 1_000_000,
            index_size: 10_000_000,
        };
        assert_eq!(stats.document_count, 1_000_000);
        assert_eq!(stats.index_size, 10_000_000);
    }

    #[test]
    fn test_format_search_results_three_results() {
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
            SearchResult {
                content: "third".to_string(),
                score: 0.7,
                metadata: None,
            },
        ];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("Result 1"));
        assert!(formatted.contains("Result 2"));
        assert!(formatted.contains("Result 3"));
        let separator_count = formatted.matches("---").count();
        assert_eq!(separator_count, 2);
    }

    #[test]
    fn test_format_search_results_with_empty_prefix() {
        let manager = VectorStoreManager::new_stub("test", Some("".to_string()));
        let results = vec![SearchResult {
            content: "content".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("content"));
    }

    #[test]
    fn test_format_search_results_with_multiline_prefix() {
        let manager = VectorStoreManager::new_stub("test", Some("Line1\nLine2".to_string()));
        let results = vec![SearchResult {
            content: "content".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.starts_with("Line1\nLine2"));
    }

    #[test]
    fn test_format_search_results_special_characters_in_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![];
        let formatted = manager.format_search_results(&results, "query with 'quotes' and \"double\"");
        assert!(formatted.contains("query with 'quotes' and \"double\""));
    }

    #[test]
    fn test_format_search_results_newline_in_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![];
        let formatted = manager.format_search_results(&results, "query\nwith\nnewlines");
        assert!(formatted.contains("query\nwith\nnewlines"));
    }

    #[test]
    fn test_document_very_long_content() {
        let content = "a".repeat(10000);
        let doc = Document::new(content.clone());
        assert_eq!(doc.content.len(), 10000);
    }

    #[test]
    fn test_document_very_long_id() {
        let id = "x".repeat(1000);
        let doc = Document::new("content".to_string()).with_id(id.clone());
        assert_eq!(doc.id, Some(id));
    }

    #[test]
    fn test_search_result_very_long_content() {
        let content = "b".repeat(10000);
        let result = SearchResult {
            content: content.clone(),
            score: 0.5,
            metadata: None,
        };
        assert_eq!(result.content.len(), 10000);
    }

    #[test]
    fn test_vector_store_manager_store_type_values() {
        let manager = VectorStoreManager::new_stub("test", None);
        assert_eq!(manager.store_type, "in_memory");
        
        let mut manager2 = VectorStoreManager::new_stub("test", None);
        manager2.store_type = "qdrant".to_string();
        assert_eq!(manager2.store_type, "qdrant");
        
        let mut manager3 = VectorStoreManager::new_stub("test", None);
        manager3.store_type = "bedrock_kb".to_string();
        assert_eq!(manager3.store_type, "bedrock_kb");
    }

    #[test]
    fn test_vector_store_manager_get_stats_embedding_provider() {
        let manager = VectorStoreManager::new_stub("test", None);
        let stats = manager.get_stats();
        assert_eq!(stats.embedding_provider, "stub");
    }

    #[test]
    fn test_vector_store_manager_get_stats_embedding_model() {
        let manager = VectorStoreManager::new_stub("test", None);
        let stats = manager.get_stats();
        assert_eq!(stats.embedding_model, "stub");
    }

    #[tokio::test]
    async fn test_add_documents_large_document() {
        let manager = VectorStoreManager::new_stub("test", None);
        let large_doc = "x".repeat(100000);
        let docs = vec![large_doc];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_documents_many_documents() {
        let manager = VectorStoreManager::new_stub("test", None);
        let docs: Vec<String> = (0..100).map(|i| format!("document {}", i)).collect();
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_unicode_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.search("世界", 5).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_emoji_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.search("🎉", 5).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_multiline_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.search("line1\nline2", 5).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_with_filter_multiple_filters() {
        let manager = VectorStoreManager::new_stub("test", None);
        let mut filters = HashMap::new();
        filters.insert("key1".to_string(), json!("value1"));
        filters.insert("key2".to_string(), json!(42));
        filters.insert("key3".to_string(), json!(true));
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_with_filter_complex_values() {
        let manager = VectorStoreManager::new_stub("test", None);
        let mut filters = HashMap::new();
        filters.insert("array".to_string(), json!(["a", "b", "c"]));
        filters.insert("object".to_string(), json!({"nested": "value"}));
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_format_search_results_very_high_score() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "test".to_string(),
            score: 1000.0,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("1000.000"));
    }

    #[test]
    fn test_format_search_results_very_low_score() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "test".to_string(),
            score: 0.001,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("0.001"));
    }

    #[test]
    fn test_document_with_special_characters() {
        let doc = Document::new("!@#$%^&*()".to_string());
        assert_eq!(doc.content, "!@#$%^&*()");
    }

    #[test]
    fn test_document_with_tabs_and_newlines() {
        let doc = Document::new("line1\tcolumn2\nline2\tcolumn2".to_string());
        assert_eq!(doc.content, "line1\tcolumn2\nline2\tcolumn2");
    }

    #[test]
    fn test_search_result_metadata_empty_object() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({})),
        };
        assert!(result.metadata.is_some());
        let metadata = result.metadata.unwrap();
        assert!(metadata.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_vector_store_stats_empty_strings() {
        let stats = VectorStoreStats {
            store_type: "".to_string(),
            embedding_provider: "".to_string(),
            embedding_model: "".to_string(),
            document_count: 0,
            index_size: 0,
        };
        assert_eq!(stats.store_type, "");
        assert_eq!(stats.embedding_provider, "");
        assert_eq!(stats.embedding_model, "");
    }

    #[test]
    fn test_new_stub_unicode_name() {
        let manager = VectorStoreManager::new_stub("テスト", None);
        assert_eq!(manager.store_name, "テスト");
    }

    #[test]
    fn test_new_stub_unicode_prefix() {
        let manager = VectorStoreManager::new_stub("test", Some("前置き:".to_string()));
        assert_eq!(manager.context_prefix, Some("前置き:".to_string()));
    }

    #[test]
    fn test_format_search_results_single_result_no_separator() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "only one".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(!formatted.contains("---"));
    }

    #[test]
    fn test_document_id_with_special_characters() {
        let doc = Document::new("content".to_string()).with_id("id-with-dashes_and_underscores.123".to_string());
        assert_eq!(doc.id, Some("id-with-dashes_and_underscores.123".to_string()));
    }

    #[test]
    fn test_document_metadata_with_unicode() {
        let metadata = json!({"名前": "値", "emoji": "🎉"});
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert_eq!(doc.metadata, Some(metadata));
    }

    // from_config tests removed — they require real service connections
    // from_config tests removed — they require real service connections
    // (OpenAI API key, Qdrant server, AWS credentials) and belong in integration tests.

    #[tokio::test]
    async fn test_search_bedrock_kb_missing_client() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        // bedrock_kb_id is private; the stub has it as None which is the error case
        
        let result = manager.search("query", 5).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[tokio::test]
    async fn test_search_bedrock_kb_missing_kb_id() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        
        let result = manager.search("query", 5).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_bedrock_kb_zero_limit() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        
        let result = manager.search("query", 0).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_bedrock_kb_empty_query() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "bedrock_kb".to_string();
        
        let result = manager.search("", 5).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_with_filter_qdrant_empty_query() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("", 5, filters).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_with_filter_qdrant_zero_limit() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        let filters = HashMap::new();
        
        let result = manager.search_with_filter("query", 0, filters).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_embedding_model_config_provider_openai() {
        let config = EmbeddingModelConfig::OpenAI {
            api_key: "sk-test".to_string(),
            model: "text-embedding-3-small".to_string(),
            base_url: None,
        };
        assert_eq!(config.provider(), "openai");
    }

    #[test]
    fn test_embedding_model_config_provider_bedrock() {
        let config = EmbeddingModelConfig::Bedrock {
            model: "amazon.titan-embed-text-v1".to_string(),
            region: "us-east-1".to_string(),
            profile: None,
        };
        assert_eq!(config.provider(), "bedrock");
    }

    #[test]
    fn test_embedding_model_config_model_openai() {
        let config = EmbeddingModelConfig::OpenAI {
            api_key: "sk-test".to_string(),
            model: "text-embedding-3-small".to_string(),
            base_url: None,
        };
        assert_eq!(config.model(), "text-embedding-3-small");
    }

    #[test]
    fn test_embedding_model_config_model_bedrock() {
        let config = EmbeddingModelConfig::Bedrock {
            model: "amazon.titan-embed-text-v1".to_string(),
            region: "us-east-1".to_string(),
            profile: None,
        };
        assert_eq!(config.model(), "amazon.titan-embed-text-v1");
    }

    #[test]
    fn test_vector_store_config_with_context_prefix() {
        let config = VectorStoreConfig {
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
        assert_eq!(config.context_prefix, Some("Context:".to_string()));
    }

    #[test]
    fn test_vector_store_config_without_context_prefix() {
        let config = VectorStoreConfig {
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
        assert!(config.context_prefix.is_none());
    }

    #[tokio::test]
    async fn test_add_documents_qdrant_not_initialized() {
        let mut manager = VectorStoreManager::new_stub("test", None);
        manager.store_type = "qdrant".to_string();
        
        let docs = vec!["doc1".to_string()];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_add_documents_in_memory_not_initialized() {
        let manager = VectorStoreManager::new_stub("test", None);
        let docs = vec!["doc1".to_string()];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_search_result_metadata_with_id() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({"id": "doc-123"})),
        };
        assert!(result.metadata.is_some());
        let metadata = result.metadata.unwrap();
        assert_eq!(metadata["id"], "doc-123");
    }

    #[test]
    fn test_search_result_metadata_complex() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: Some(json!({
                "id": "123",
                "type": "document",
                "tags": ["rust", "test"],
                "nested": {"key": "value"}
            })),
        };
        assert!(result.metadata.is_some());
        let metadata = result.metadata.unwrap();
        assert_eq!(metadata["id"], "123");
        assert_eq!(metadata["type"], "document");
    }

    #[test]
    fn test_format_search_results_with_metadata() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "test content".to_string(),
            score: 0.95,
            metadata: Some(json!({"id": "doc-1"})),
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("test content"));
        assert!(formatted.contains("0.950"));
    }

    #[tokio::test]
    async fn test_search_in_memory_with_limit_one() {
        let manager = VectorStoreManager::new_stub("test", None);
        let result = manager.search("query", 1).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_search_with_filter_in_memory_single_filter() {
        let manager = VectorStoreManager::new_stub("test", None);
        let mut filters = HashMap::new();
        filters.insert("type".to_string(), json!("document"));
        
        let result = manager.search_with_filter("query", 5, filters).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_document_builder_only_id() {
        let doc = Document::new("content".to_string()).with_id("123".to_string());
        assert_eq!(doc.id, Some("123".to_string()));
        assert!(doc.metadata.is_none());
    }

    #[test]
    fn test_document_builder_only_metadata() {
        let metadata = json!({"key": "value"});
        let doc = Document::new("content".to_string()).with_metadata(metadata.clone());
        assert!(doc.id.is_none());
        assert_eq!(doc.metadata, Some(metadata));
    }

    #[test]
    fn test_vector_store_stats_all_fields() {
        let stats = VectorStoreStats {
            store_type: "qdrant".to_string(),
            embedding_provider: "openai".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
            document_count: 42,
            index_size: 1024,
        };
        assert_eq!(stats.store_type, "qdrant");
        assert_eq!(stats.embedding_provider, "openai");
        assert_eq!(stats.embedding_model, "text-embedding-3-small");
        assert_eq!(stats.document_count, 42);
        assert_eq!(stats.index_size, 1024);
    }

    #[test]
    fn test_format_search_results_boundary_score() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results = vec![SearchResult {
            content: "test".to_string(),
            score: 0.5,
            metadata: None,
        }];
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("0.500"));
    }

    #[tokio::test]
    async fn test_search_very_long_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let long_query = "query ".repeat(1000);
        let result = manager.search(&long_query, 5).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_with_filter_very_long_query() {
        let manager = VectorStoreManager::new_stub("test", None);
        let long_query = "query ".repeat(1000);
        let filters = HashMap::new();
        let result = manager.search_with_filter(&long_query, 5, filters).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_format_search_results_ten_results() {
        let manager = VectorStoreManager::new_stub("test", None);
        let results: Vec<SearchResult> = (0..10)
            .map(|i| SearchResult {
                content: format!("content {}", i),
                score: 0.9 - (i as f32 * 0.05),
                metadata: None,
            })
            .collect();
        let formatted = manager.format_search_results(&results, "query");
        assert!(formatted.contains("Result 1"));
        assert!(formatted.contains("Result 10"));
        let separator_count = formatted.matches("---").count();
        assert_eq!(separator_count, 9);
    }

    #[test]
    fn test_search_result_score_precision() {
        let result = SearchResult {
            content: "test".to_string(),
            score: 0.123456789,
            metadata: None,
        };
        assert_eq!(result.score, 0.123456789);
    }

    #[test]
    fn test_document_content_with_null_bytes() {
        let doc = Document::new("test\0content".to_string());
        assert_eq!(doc.content, "test\0content");
    }

    #[tokio::test]
    async fn test_add_documents_with_newlines() {
        let manager = VectorStoreManager::new_stub("test", None);
        let docs = vec!["line1\nline2\nline3".to_string()];
        let result = manager.add_documents(docs).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_store_name_unicode() {
        let manager = VectorStoreManager::new_stub("テスト店", None);
        assert_eq!(manager.get_store_name(), Some("テスト店"));
    }

    #[test]
    fn test_get_context_prefix_unicode() {
        let manager = VectorStoreManager::new_stub("test", Some("コンテキスト:".to_string()));
        assert_eq!(manager.get_context_prefix(), Some("コンテキスト:"));
    }
}
