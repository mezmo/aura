use crate::{config::VectorStoreConfig, error::BuilderError};
use qdrant_client::{qdrant::QueryPoints, Qdrant};
use rig::{
    client::EmbeddingsClient,
    embeddings::Embedding,
    providers::openai::{Client, EmbeddingModel as OpenAIEmbeddingModel},
    vector_store::{
        in_memory_store::InMemoryVectorStore, request::VectorSearchRequest, VectorStoreIndex,
    },
    OneOrMany,
};
use rig_qdrant::QdrantVectorStore;
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, info};

/// Vector store manager for handling document retrieval via vector search
pub struct VectorStoreManager {
    pub store_name: String,
    pub store_type: String,
    pub qdrant_url: Option<String>,
    pub collection_name: Option<String>,
    pub in_memory_store: Option<Arc<InMemoryVectorStore<String>>>,
    pub qdrant_store: Option<Arc<QdrantVectorStore<OpenAIEmbeddingModel>>>,
    pub embedding_model: OpenAIEmbeddingModel,
    pub context_prefix: Option<String>,
}

impl VectorStoreManager {
    /// Create a new vector store from configuration
    pub async fn from_config(config: &VectorStoreConfig) -> Result<Self, BuilderError> {
        info!("Initializing vector store: {}", config.store_type);

        match config.store_type.as_str() {
            "in_memory" => Self::create_in_memory_store(config).await,
            "qdrant" => Self::create_qdrant_store(config).await,
            store_type => Err(BuilderError::VectorStoreError(format!(
                "Unsupported vector store type: {store_type}"
            ))),
        }
    }

    /// Create an in-memory vector store
    async fn create_in_memory_store(config: &VectorStoreConfig) -> Result<Self, BuilderError> {
        info!(
            "Creating in-memory vector store with {} embeddings",
            config.embedding_model.model
        );

        // Create OpenAI embedding model
        let embedding_model = if config.embedding_model.provider == "openai" {
            let client = Client::new(&config.embedding_model.api_key).map_err(|e| {
                BuilderError::VectorStoreError(format!("Failed to create OpenAI client: {e}"))
            })?;
            client.embedding_model(&config.embedding_model.model)
        } else {
            return Err(BuilderError::VectorStoreError(format!(
                "Unsupported embedding provider: {}. Only 'openai' is supported for now.",
                config.embedding_model.provider
            )));
        };

        // Create an empty in-memory vector store for now
        let store = InMemoryVectorStore::from_documents(std::iter::empty::<(
            String,
            OneOrMany<Embedding>,
        )>());

        info!("In-memory vector store initialized successfully");

        Ok(Self {
            store_name: config.name.clone(),
            store_type: "in_memory".to_string(),
            qdrant_url: None,
            collection_name: None,
            qdrant_store: None,
            in_memory_store: Some(Arc::new(store)),
            embedding_model,
            context_prefix: config.context_prefix.clone(),
        })
    }

    /// Create a Qdrant vector store
    async fn create_qdrant_store(config: &VectorStoreConfig) -> Result<Self, BuilderError> {
        info!(
            "Creating Qdrant vector store with {} embeddings",
            config.embedding_model.model
        );

        let url = config.url.as_ref().ok_or_else(|| {
            BuilderError::VectorStoreError("URL is required for Qdrant".to_string())
        })?;

        let collection_name = config.collection_name.as_ref().ok_or_else(|| {
            BuilderError::VectorStoreError("Collection name is required for Qdrant".to_string())
        })?;

        info!("Connecting to Qdrant at: {}", url);
        info!("Using collection: {}", collection_name);

        // Create the Qdrant client using gRPC (default and preferred)
        let qdrant_client = match Qdrant::from_url(url).build() {
            Ok(client) => client,
            Err(e) => {
                return Err(BuilderError::VectorStoreError(format!(
                    "Failed to create Qdrant gRPC client: {e}"
                )));
            }
        };

        // Create OpenAI embedding model
        let embedding_model = if config.embedding_model.provider == "openai" {
            let client = Client::new(&config.embedding_model.api_key).map_err(|e| {
                BuilderError::VectorStoreError(format!("Failed to create OpenAI client: {e}"))
            })?;
            client.embedding_model(&config.embedding_model.model)
        } else {
            return Err(BuilderError::VectorStoreError(format!(
                "Unsupported embedding provider: {}. Only 'openai' is supported for now.",
                config.embedding_model.provider
            )));
        };

        // Create default query parameters for the collection
        let query_params = QueryPoints {
            collection_name: collection_name.clone(),
            query: None,
            limit: Some(5),
            offset: None,
            with_payload: Some(true.into()),
            with_vectors: Some(false.into()),
            score_threshold: None,
            ..Default::default()
        };

        // Create the Qdrant vector store using rig-qdrant
        let qdrant_store =
            QdrantVectorStore::new(qdrant_client, embedding_model.clone(), query_params);

        info!("Qdrant vector store initialized successfully");

        Ok(Self {
            store_name: config.name.clone(),
            store_type: "qdrant".to_string(),
            qdrant_url: Some(url.clone()),
            collection_name: Some(collection_name.clone()),
            qdrant_store: Some(Arc::new(qdrant_store)),
            in_memory_store: None,
            embedding_model,
            context_prefix: config.context_prefix.clone(),
        })
    }

    /// Add documents to the vector store
    pub async fn add_documents(&self, documents: Vec<String>) -> Result<(), BuilderError> {
        info!("Adding {} documents to vector store", documents.len());

        // TODO: Implement document addition
        // For now, we'll just log that documents would be added
        for (i, doc) in documents.iter().enumerate() {
            debug!("Would add document {}: {} chars", i, doc.len());
        }

        info!("Documents added successfully (placeholder implementation)");
        Ok(())
    }

    /// Search for similar documents
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, BuilderError> {
        info!("Searching vector store for: '{}' (limit: {})", query, limit);

        match self.store_type.as_str() {
            "qdrant" => {
                if let Some(qdrant_store) = &self.qdrant_store {
                    // Create vector search request using the builder pattern
                    let search_request = VectorSearchRequest::builder()
                        .query(query)
                        .samples(limit as u64)
                        .build()
                        .map_err(|e| {
                            BuilderError::VectorStoreError(format!(
                                "Failed to build search request: {e}"
                            ))
                        })?;

                    // Perform vector search using the VectorStoreIndex trait
                    debug!("Performing vector search in Qdrant for: '{}'", query);
                    let search_results = qdrant_store
                        .top_n::<serde_json::Value>(search_request)
                        .await
                        .map_err(|e| {
                            BuilderError::VectorStoreError(format!("Qdrant search failed: {e}"))
                        })?;

                    // Convert results to our SearchResult format
                    let results: Vec<SearchResult> = search_results
                        .into_iter()
                        .map(|(score, id, document)| SearchResult {
                            content: document.to_string(),
                            score: score as f32,
                            metadata: Some(serde_json::json!({"id": id})),
                        })
                        .collect();

                    info!("Found {} results from Qdrant search", results.len());
                    Ok(results)
                } else {
                    Err(BuilderError::VectorStoreError(
                        "Qdrant store not initialized".to_string(),
                    ))
                }
            }
            "in_memory" => {
                // For in-memory, we still need to implement search
                debug!("In-memory search not yet implemented, returning empty results");
                Ok(Vec::new())
            }
            _ => Err(BuilderError::VectorStoreError(format!(
                "Unsupported store type: {}",
                self.store_type
            ))),
        }
    }

    /// Get vector store statistics
    pub fn get_stats(&self) -> VectorStoreStats {
        VectorStoreStats {
            store_type: self.store_type.clone(),
            embedding_provider: "openai".to_string(), // Only OpenAI supported for now
            embedding_model: "text-embedding-3-small".to_string(), // From config
            document_count: 0,                        // TODO: Get actual count from store
            index_size: 0,                            // TODO: Get actual size from store
        }
    }

    /// Get the context prefix for formatting search results
    pub fn get_store_name(&self) -> Option<&str> {
        Some(&self.store_name)
    }

    pub fn get_context_prefix(&self) -> Option<&str> {
        self.context_prefix.as_deref()
    }

    /// Search for similar documents with exact-match payload filters (Qdrant only)
    pub async fn search_with_filter(
        &self,
        query: &str,
        limit: usize,
        payload_equals: std::collections::HashMap<String, Value>,
    ) -> Result<Vec<SearchResult>, BuilderError> {
        match self.store_type.as_str() {
            "qdrant" => {
                let qdrant_store = self.qdrant_store.as_ref().ok_or_else(|| {
                    BuilderError::VectorStoreError("Qdrant store not initialized".to_string())
                })?;

                let search_request = VectorSearchRequest::builder()
                    .query(query)
                    .samples(limit as u64)
                    .build()
                    .map_err(|e| {
                        BuilderError::VectorStoreError(format!(
                            "Failed to build search request: {e}"
                        ))
                    })?;

                let mut candidates =
                    qdrant_store
                        .top_n::<Value>(search_request)
                        .await
                        .map_err(|e| {
                            BuilderError::VectorStoreError(format!("Qdrant search failed: {e}"))
                        })?;

                // Client-side filter by payload equality
                candidates.retain(|(_score, _id, payload)| {
                    payload
                        .as_object()
                        .map(|map| payload_equals.iter().all(|(k, v)| map.get(k) == Some(v)))
                        .unwrap_or(false)
                });

                Ok(candidates
                    .into_iter()
                    .map(|(score, id, document)| SearchResult {
                        content: document.to_string(),
                        score: score as f32,
                        metadata: Some(serde_json::json!({"id": id})),
                    })
                    .collect())
            }
            "in_memory" => {
                // No filter support for in-memory; fall back to unfiltered search
                self.search(query, limit).await
            }
            _ => Err(BuilderError::VectorStoreError(format!(
                "Unsupported store type: {}",
                self.store_type
            ))),
        }
    }

    /// Format search results with optional context prefix
    pub fn format_search_results(&self, results: &[SearchResult], query: &str) -> String {
        if results.is_empty() {
            return format!("No results found for query: '{query}'");
        }

        let mut formatted = String::new();

        // Add context prefix if configured
        if let Some(context_prefix) = &self.context_prefix {
            formatted.push_str(context_prefix);
            formatted.push_str("\n\n");
        }

        // Add results
        for (i, result) in results.iter().enumerate() {
            formatted.push_str(&format!(
                "Result {} (score: {:.3}):\n{}\n",
                i + 1,
                result.score,
                result.content
            ));

            if i < results.len() - 1 {
                formatted.push_str("\n---\n\n");
            }
        }

        formatted
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub content: String,
    pub score: f32,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct VectorStoreStats {
    pub store_type: String,
    pub embedding_provider: String,
    pub embedding_model: String,
    pub document_count: usize,
    pub index_size: usize,
}

/// Document for ingestion into vector store
#[derive(Debug, Clone)]
pub struct Document {
    pub id: Option<String>,
    pub content: String,
    pub metadata: Option<Value>,
}

impl Document {
    pub fn new(content: String) -> Self {
        Self {
            id: None,
            content,
            metadata: None,
        }
    }

    pub fn with_id(mut self, id: String) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}
