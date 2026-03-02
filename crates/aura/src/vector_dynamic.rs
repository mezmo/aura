use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool as RigTool;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info};

use crate::{error::BuilderError, vector_store::VectorStoreManager};

/// Dynamic Vector Store Tool Adaptor
/// This solves the "static const NAME" limitation by using name() override
#[derive(Clone)]
pub struct DynamicVectorSearchTool {
    vector_store: Arc<VectorStoreManager>,
    tool_name: String,
    store_name: String,
}

impl DynamicVectorSearchTool {
    pub fn new(vector_store: Arc<VectorStoreManager>, store_name: String) -> Self {
        let tool_name = format!("vector_search_{store_name}");
        Self {
            vector_store,
            tool_name,
            store_name,
        }
    }
}

impl RigTool for DynamicVectorSearchTool {
    type Error = BuilderError;
    type Args = VectorSearchArgs;
    type Output = VectorSearchResponse;

    // Static name required by trait - we override with name() method below
    const NAME: &'static str = "dynamic_vector_search_tool";

    // This is the key - overrides the static NAME with dynamic name
    fn name(&self) -> String {
        self.tool_name.clone()
    }

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let tool_name = self.name();
        let store_name = self.store_name.clone();

        // Build description based on context prefix
        let base_description = format!(
            "Search the '{store_name}' vector store for documents semantically similar to a query. \
            Returns relevant documents with similarity scores. \
            IMPORTANT: Only use label_filters if the user's request explicitly mentions filtering by labels/metadata - \
            DO NOT guess or speculatively add label filters."
        );

        let description = if let Some(context) = self.vector_store.get_context_prefix() {
            format!("{} This vector store contains: {}. Use this tool when you need to find information related to that domain.", base_description, context.replace("Based on the following information from the ", "").replace(":", ""))
        } else {
            base_description
        };

        ToolDefinition {
            name: tool_name,
            description,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Required natural-language query to search for similar documents"
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
                        "description": "Minimum similarity score threshold from 0.0 to 1.0 (default: 0.5)",
                        "default": 0.5,
                        "minimum": 0.1,
                        "maximum": 1.0
                    },
                    "label_filters": {
                        "type": "array",
                        "description": "Optional exact-match label filters. Only use if explicitly mentioned in the user's request. Array of {key, value} pairs where value is a string (numbers/booleans as strings like 'true' or '42').",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "key": { "type": "string" },
                                "value": { "type": "string" }
                            },
                            "required": ["key", "value"]
                        },
                        "default": []
                    }
                },
                "required": ["query", "limit", "min_score", "label_filters"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store_name = self.store_name.clone();
        let vector_store = self.vector_store.clone();

        info!(
            "🔍 Searching vector store '{}' (query: '{}', limit: {}, min_score: {}, filters: {})",
            store_name,
            args.query,
            args.limit,
            args.min_score,
            args.label_filters.len()
        );

        let search_results = if !args.label_filters.is_empty() {
            info!(
                "   Applying {} label filter(s): {:?}",
                args.label_filters.len(),
                args.label_filters
            );
            let filter_map = args
                .label_filters
                .iter()
                .map(|kv| (kv.key.clone(), parse_value_str(&kv.value)))
                .collect();
            vector_store
                .search_with_filter(&args.query, args.limit, filter_map)
                .await?
        } else {
            vector_store.search(&args.query, args.limit).await?
        };

        // Filter by minimum score if specified
        let filtered_results: Vec<_> = search_results
            .into_iter()
            .filter(|result| result.score >= args.min_score)
            .collect();

        let results_count = filtered_results.len();

        info!(
            "Vector search '{}' completed: {} results found",
            store_name, results_count
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
        let formatted_results = vector_store.format_search_results(&filtered_results, &args.query);

        Ok(VectorSearchResponse {
            results: vector_results,
            query: args.query,
            total_found: results_count,
            formatted_results,
        })
    }
}

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
    /// Exact-match label filters: array of { key, value } pairs
    #[serde(default)]
    pub label_filters: Vec<FilterKV>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FilterKV {
    pub key: String,
    pub value: String,
}

/// Parse a string value as JSON (number, boolean, null) or fallback to string
fn parse_value_str(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
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
