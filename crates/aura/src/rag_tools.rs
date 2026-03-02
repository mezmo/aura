use crate::{error::BuilderError, vector_store::VectorStoreManager};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Macro to create unique vector search tool structs for each vector store
macro_rules! create_vector_search_tool {
    ($struct_name:ident, $tool_name:expr) => {
        #[derive(Clone)]
        pub struct $struct_name {
            vector_store: Arc<VectorStoreManager>,
        }

        impl $struct_name {
            pub fn new(vector_store: Arc<VectorStoreManager>) -> Self {
                Self { vector_store }
            }
        }

        impl Tool for $struct_name {
            const NAME: &'static str = $tool_name;

            type Error = BuilderError;
            type Args = VectorSearchArgs;
            type Output = VectorSearchResponse;

            async fn definition(&self, _prompt: String) -> ToolDefinition {
                // Build description based on context prefix
                let base_description = "Search the vector store for documents semantically similar to a query. Returns relevant documents with similarity scores.";

                let description = if let Some(context) = self.vector_store.get_context_prefix() {
                    format!("{} This vector store contains: {}. Use this tool when you need to find information related to that domain.", base_description, context.replace("Based on the following information from the ", "").replace(":", ""))
                } else {
                    base_description.to_string()
                };

                ToolDefinition {
                    name: Self::NAME.to_string(),
                    description,
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "The text query to search for similar documents"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results to return (default: 5)",
                                "default": 5,
                                "minimum": 1,
                                "maximum": 20
                            },
                            "min_score": {
                                "type": "number",
                                "description": "Minimum similarity score threshold from 0.0 to 1.0 (default: 0.0)",
                                "default": 0.0,
                                "minimum": 0.0,
                                "maximum": 1.0
                            }
                        },
                        "required": ["query", "limit", "min_score"]
                    }),
                }
            }

            async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
                info!(
                    "🔍 Searching vector store: '{}' (limit: {}, min_score: {})",
                    args.query, args.limit, args.min_score
                );

                let search_results = self
                    .vector_store
                    .search(&args.query, args.limit)
                    .await?;

                // Filter by minimum score if specified
                let filtered_results: Vec<_> = search_results
                    .into_iter()
                    .filter(|result| result.score >= args.min_score)
                    .collect();

                let results_count = filtered_results.len();

                info!(
                    "✅ Vector search completed: {} results found",
                    results_count
                );

                if filtered_results.is_empty() {
                    debug!(
                        "No results found above minimum score threshold: {}",
                        args.min_score
                    );
                }

                let vector_results: Vec<VectorSearchResult> = filtered_results
                    .iter()
                    .map(|result| VectorSearchResult {
                        content: result.content.clone(),
                        score: result.score,
                        metadata: result.metadata.clone(),
                    })
                    .collect();

                // Format results with optional context prefix
                let formatted_results = self.vector_store.format_search_results(&filtered_results, &args.query);

                Ok(VectorSearchResponse {
                    results: vector_results,
                    query: args.query,
                    total_found: results_count,
                    formatted_results,
                })
            }
        }
    };
}

// Create unique tool structs for each vector store based on config names
create_vector_search_tool!(VectorSearchMezmoKbTool, "vector_search_mezmo_kb");
create_vector_search_tool!(
    VectorSearchMezmoRunbooksTool,
    "vector_search_mezmo_runbooks"
);

#[derive(Debug, Deserialize, Serialize)]
pub struct VectorSearchArgs {
    /// Query text to search for semantically similar documents
    pub query: String,
    /// Maximum number of results to return (default: 5)
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Minimum similarity score threshold (0.0-1.0, default: 0.0)
    #[serde(default)]
    pub min_score: f32,
}

fn default_limit() -> usize {
    5
}

#[derive(Debug, Serialize)]
pub struct VectorSearchResponse {
    pub results: Vec<VectorSearchResult>,
    pub query: String,
    pub total_found: usize,
    /// Formatted results with context prefix (ready for RAG integration)
    pub formatted_results: String,
}

#[derive(Debug, Serialize)]
pub struct VectorSearchResult {
    pub content: String,
    pub score: f32,
    pub metadata: Option<Value>,
}

/// Tool for ingesting documents into the vector store
#[derive(Clone)]
pub struct VectorIngestTool {
    vector_store: Arc<VectorStoreManager>,
}

impl VectorIngestTool {
    pub fn new(vector_store: Arc<VectorStoreManager>) -> Self {
        Self { vector_store }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VectorIngestArgs {
    /// Documents to ingest into the vector store
    pub documents: Vec<IngestDocument>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct IngestDocument {
    /// Unique identifier for the document (optional)
    pub id: Option<String>,
    /// Document content to be embedded
    pub content: String,
    /// Additional metadata for the document (optional)
    pub metadata: Option<Value>,
}

impl Tool for VectorIngestTool {
    const NAME: &'static str = "vector_ingest";

    type Error = BuilderError;
    type Args = VectorIngestArgs;
    type Output = VectorIngestResponse;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Ingest documents into the vector store for semantic search. Documents will be embedded and indexed.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "documents": {
                        "type": "array",
                        "description": "Array of documents to ingest",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Optional unique identifier for the document"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "The document content to be embedded and indexed"
                                },
                                "metadata": {
                                    "type": "object",
                                    "description": "Optional metadata to associate with the document"
                                }
                            },
                            "required": ["content"]
                        }
                    }
                },
                "required": ["documents"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        info!(
            "📥 Ingesting {} documents into vector store",
            args.documents.len()
        );

        // Convert IngestDocument to simple strings for now
        // TODO: Enhanced ingestion with metadata support
        let documents: Vec<String> = args
            .documents
            .iter()
            .map(|doc| doc.content.clone())
            .collect();

        self.vector_store.add_documents(documents).await?;

        info!(
            "✅ Document ingestion completed: {} documents processed",
            args.documents.len()
        );

        Ok(VectorIngestResponse {
            ingested_count: args.documents.len(),
            success: true,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct VectorIngestResponse {
    pub ingested_count: usize,
    pub success: bool,
}

/// Auto-ingestion manager for loading documents on startup
pub struct AutoIngest {
    vector_store: Arc<VectorStoreManager>,
}

impl AutoIngest {
    pub fn new(vector_store: Arc<VectorStoreManager>) -> Self {
        Self { vector_store }
    }

    /// Load documents from a JSON file
    pub async fn load_from_json(&self, file_path: &str) -> Result<usize, BuilderError> {
        info!("📄 Loading documents from JSON file: {}", file_path);

        let content = std::fs::read_to_string(file_path).map_err(|e| {
            BuilderError::VectorStoreError(format!("Failed to read file {file_path}: {e}"))
        })?;

        let documents: Vec<serde_json::Value> = serde_json::from_str(&content).map_err(|e| {
            BuilderError::VectorStoreError(format!("Failed to parse JSON file {file_path}: {e}"))
        })?;

        let doc_strings: Vec<String> = documents
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                if let Some(content) = doc.get("content") {
                    content.as_str().unwrap_or("").to_string()
                } else {
                    warn!("Document {} missing 'content' field, using full JSON", i);
                    doc.to_string()
                }
            })
            .collect();

        let count = doc_strings.len();
        self.vector_store.add_documents(doc_strings).await?;

        info!("✅ Auto-ingested {} documents from {}", count, file_path);
        Ok(count)
    }

    /// Load documents from multiple sources based on configuration
    pub async fn auto_load(&self, sources: &[String]) -> Result<usize, BuilderError> {
        let mut total_loaded = 0;

        for source in sources {
            if source.ends_with(".json") {
                match self.load_from_json(source).await {
                    Ok(count) => total_loaded += count,
                    Err(e) => {
                        warn!("Failed to load from {}: {}", source, e);
                    }
                }
            } else {
                warn!("Unsupported document source format: {}", source);
            }
        }

        info!(
            "🎉 Auto-ingestion completed: {} total documents loaded",
            total_loaded
        );
        Ok(total_loaded)
    }
}
