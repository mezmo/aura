use crate::{
    bedrock_embedding::AuraBedrockEmbeddingModel as BedrockEmbeddingModel,
    config::{EmbeddingModelConfig, VectorStoreConfig, VectorStoreType},
    error::BuilderError,
};
use qdrant_client::{Qdrant, qdrant::QueryPoints};
use rig::{
    OneOrMany,
    client::EmbeddingsClient,
    embeddings::Embedding,
    providers::openai::{Client, EmbeddingModel as OpenAIEmbeddingModel},
    vector_store::{
        VectorStoreIndex, in_memory_store::InMemoryVectorStore, request::VectorSearchRequest,
    },
};
use rig_qdrant::QdrantVectorStore;
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, info};

/// Qdrant store variant parameterized by embedding model type
enum QdrantStoreKind {
    OpenAI(QdrantVectorStore<OpenAIEmbeddingModel>),
    Bedrock(QdrantVectorStore<BedrockEmbeddingModel>),
}

/// Vector store manager for handling document retrieval via vector search
pub struct VectorStoreManager {
    pub store_name: String,
    pub store_type: String,
    pub qdrant_url: Option<String>,
    pub collection_name: Option<String>,
    pub in_memory_store: Option<Arc<InMemoryVectorStore<String>>>,
    qdrant_store: Option<Arc<QdrantStoreKind>>,
    bedrock_kb_client: Option<Arc<aws_sdk_bedrockagentruntime::Client>>,
    bedrock_kb_id: Option<String>,
    embedding_provider: String,
    embedding_model_name: String,
    pub context_prefix: Option<String>,
}

impl VectorStoreManager {
    /// Create a new vector store from configuration
    pub async fn from_config(config: &VectorStoreConfig) -> Result<Self, BuilderError> {
        match &config.store {
            VectorStoreType::InMemory { embedding_model } => {
                info!("Initializing vector store: in_memory");
                Self::create_in_memory_store(config, embedding_model).await
            }
            VectorStoreType::Qdrant {
                embedding_model,
                url,
                collection_name,
            } => {
                info!("Initializing vector store: qdrant");
                Self::create_qdrant_store(config, embedding_model, url, collection_name).await
            }
            VectorStoreType::BedrockKb {
                knowledge_base_id,
                region,
                profile,
            } => {
                info!("Initializing vector store: bedrock_kb");
                Self::create_bedrock_kb_store(config, knowledge_base_id, region, profile.as_deref())
                    .await
            }
        }
    }

    fn create_openai_embedding_model(
        api_key: &str,
        model: &str,
    ) -> Result<OpenAIEmbeddingModel, BuilderError> {
        let client = Client::new(api_key).map_err(|e| {
            BuilderError::VectorStoreError(format!("Failed to create OpenAI client: {e}"))
        })?;
        Ok(client.embedding_model(model))
    }

    async fn create_bedrock_embedding_model(
        model: &str,
        region: &str,
        profile: Option<&str>,
    ) -> Result<BedrockEmbeddingModel, BuilderError> {
        use aws_config::{BehaviorVersion, Region};

        let sdk_config = if let Some(profile_name) = profile {
            info!(
                "Loading AWS config with profile '{}' for Bedrock embeddings",
                profile_name
            );
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(region.to_string()))
                .profile_name(profile_name)
                .load()
                .await
        } else {
            info!("Loading AWS config from environment for Bedrock embeddings");
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(region.to_string()))
                .load()
                .await
        };

        let aws_client = aws_sdk_bedrockruntime::Client::new(&sdk_config);
        info!("Bedrock embedding client initialized successfully");

        Ok(BedrockEmbeddingModel::new(aws_client, model, None))
    }

    /// Load AWS SDK config with optional region and profile
    async fn load_aws_config(
        region: &str,
        profile: Option<&str>,
    ) -> aws_config::SdkConfig {
        use aws_config::{BehaviorVersion, Region};

        if let Some(profile_name) = profile {
            info!(
                "Loading AWS config with profile '{}'",
                profile_name
            );
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(region.to_string()))
                .profile_name(profile_name)
                .load()
                .await
        } else {
            info!("Loading AWS config from environment");
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(region.to_string()))
                .load()
                .await
        }
    }

    /// Create an in-memory vector store
    async fn create_in_memory_store(
        config: &VectorStoreConfig,
        embedding: &EmbeddingModelConfig,
    ) -> Result<Self, BuilderError> {
        info!(
            "Creating in-memory vector store with {} embeddings",
            embedding.model()
        );

        // Validate the embedding provider can be initialized
        match embedding {
            EmbeddingModelConfig::OpenAI { api_key, model, .. } => {
                Self::create_openai_embedding_model(api_key, model)?;
            }
            EmbeddingModelConfig::Bedrock {
                model,
                region,
                profile,
            } => {
                Self::create_bedrock_embedding_model(model, region, profile.as_deref()).await?;
            }
        }

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
            bedrock_kb_client: None,
            bedrock_kb_id: None,
            embedding_provider: embedding.provider().to_string(),
            embedding_model_name: embedding.model().to_string(),
            context_prefix: config.context_prefix.clone(),
        })
    }

    /// Create a Qdrant vector store
    async fn create_qdrant_store(
        config: &VectorStoreConfig,
        embedding: &EmbeddingModelConfig,
        url: &str,
        collection_name: &str,
    ) -> Result<Self, BuilderError> {
        info!(
            "Creating Qdrant vector store with {} embeddings",
            embedding.model()
        );

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

        // Create default query parameters for the collection
        let query_params = QueryPoints {
            collection_name: collection_name.to_string(),
            query: None,
            limit: Some(5),
            offset: None,
            with_payload: Some(true.into()),
            with_vectors: Some(false.into()),
            score_threshold: None,
            ..Default::default()
        };

        // Create embedding model and Qdrant store based on provider
        let qdrant_store = match embedding {
            EmbeddingModelConfig::OpenAI { api_key, model, .. } => {
                let embedding_model = Self::create_openai_embedding_model(api_key, model)?;
                let store =
                    QdrantVectorStore::new(qdrant_client, embedding_model, query_params);
                QdrantStoreKind::OpenAI(store)
            }
            EmbeddingModelConfig::Bedrock {
                model,
                region,
                profile,
            } => {
                let embedding_model =
                    Self::create_bedrock_embedding_model(model, region, profile.as_deref())
                        .await?;
                let store =
                    QdrantVectorStore::new(qdrant_client, embedding_model, query_params);
                QdrantStoreKind::Bedrock(store)
            }
        };

        info!("Qdrant vector store initialized successfully");

        Ok(Self {
            store_name: config.name.clone(),
            store_type: "qdrant".to_string(),
            qdrant_url: Some(url.to_string()),
            collection_name: Some(collection_name.to_string()),
            qdrant_store: Some(Arc::new(qdrant_store)),
            in_memory_store: None,
            bedrock_kb_client: None,
            bedrock_kb_id: None,
            embedding_provider: embedding.provider().to_string(),
            embedding_model_name: embedding.model().to_string(),
            context_prefix: config.context_prefix.clone(),
        })
    }

    /// Create a Bedrock Knowledge Base vector store
    async fn create_bedrock_kb_store(
        config: &VectorStoreConfig,
        knowledge_base_id: &str,
        region: &str,
        profile: Option<&str>,
    ) -> Result<Self, BuilderError> {
        info!("Creating Bedrock Knowledge Base store");
        info!("  Knowledge Base ID: {}", knowledge_base_id);
        info!("  Region: {}", region);

        let sdk_config = Self::load_aws_config(
            region,
            profile,
        )
        .await;

        let client = aws_sdk_bedrockagentruntime::Client::new(&sdk_config);
        info!("Bedrock Knowledge Base client initialized");

        Ok(Self {
            store_name: config.name.clone(),
            store_type: "bedrock_kb".to_string(),
            qdrant_url: None,
            collection_name: None,
            qdrant_store: None,
            in_memory_store: None,
            bedrock_kb_client: Some(Arc::new(client)),
            bedrock_kb_id: Some(knowledge_base_id.to_string()),
            embedding_provider: "bedrock_kb".to_string(),
            embedding_model_name: "managed".to_string(),
            context_prefix: config.context_prefix.clone(),
        })
    }

    /// Add documents to the vector store
    pub async fn add_documents(&self, documents: Vec<String>) -> Result<(), BuilderError> {
        if self.store_type == "bedrock_kb" {
            return Err(BuilderError::VectorStoreError(
                "Bedrock Knowledge Base is read-only; manage documents via the AWS console".to_string(),
            ));
        }

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
                    let search_results = match qdrant_store.as_ref() {
                        QdrantStoreKind::OpenAI(store) => {
                            store.top_n::<serde_json::Value>(search_request).await
                        }
                        QdrantStoreKind::Bedrock(store) => {
                            store.top_n::<serde_json::Value>(search_request).await
                        }
                    }
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
            "bedrock_kb" => self.search_bedrock_kb(query, limit).await,
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

    /// Search Bedrock Knowledge Base using the Retrieve API
    async fn search_bedrock_kb(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, BuilderError> {
        let client = self.bedrock_kb_client.as_ref().ok_or_else(|| {
            BuilderError::VectorStoreError(
                "Bedrock KB client not initialized".to_string(),
            )
        })?;
        let kb_id = self.bedrock_kb_id.as_ref().ok_or_else(|| {
            BuilderError::VectorStoreError(
                "Bedrock KB ID not set".to_string(),
            )
        })?;

        debug!("Performing Bedrock KB retrieve for: '{}'", query);

        let retrieval_query =
            aws_sdk_bedrockagentruntime::types::KnowledgeBaseQuery::builder()
                .text(query)
                .build();

        let retrieval_config =
            aws_sdk_bedrockagentruntime::types::KnowledgeBaseRetrievalConfiguration::builder()
                .vector_search_configuration(
                    aws_sdk_bedrockagentruntime::types::KnowledgeBaseVectorSearchConfiguration::builder()
                        .number_of_results(limit as i32)
                        .build(),
                )
                .build();

        let response = client
            .retrieve()
            .knowledge_base_id(kb_id)
            .retrieval_query(retrieval_query)
            .retrieval_configuration(retrieval_config)
            .send()
            .await
            .map_err(|e| {
                BuilderError::VectorStoreError(format!(
                    "Bedrock KB retrieve failed: {e}"
                ))
            })?;

        let results: Vec<SearchResult> = response
            .retrieval_results
            .into_iter()
            .filter_map(|r| {
                let content = r.content?.text;
                let score = r.score.unwrap_or(0.0) as f32;
                let metadata = r.location.map(|loc| {
                    serde_json::json!({
                        "type": format!("{:?}", loc.r#type),
                    })
                });
                Some(SearchResult {
                    content,
                    score,
                    metadata,
                })
            })
            .collect();

        info!("Found {} results from Bedrock KB", results.len());
        Ok(results)
    }

    /// Get vector store statistics
    pub fn get_stats(&self) -> VectorStoreStats {
        VectorStoreStats {
            store_type: self.store_type.clone(),
            embedding_provider: self.embedding_provider.clone(),
            embedding_model: self.embedding_model_name.clone(),
            document_count: 0, // TODO: Get actual count from store
            index_size: 0,     // TODO: Get actual size from store
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

                let mut candidates = match qdrant_store.as_ref() {
                    QdrantStoreKind::OpenAI(store) => {
                        store.top_n::<Value>(search_request).await
                    }
                    QdrantStoreKind::Bedrock(store) => {
                        store.top_n::<Value>(search_request).await
                    }
                }
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
            "bedrock_kb" | "in_memory" => {
                // No filter support; fall back to unfiltered search
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
