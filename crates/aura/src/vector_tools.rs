use qdrant_client::{
    qdrant::{
        point_id::PointIdOptions, Condition, Filter, PointId, Query, QueryPointsBuilder,
        ScoredPoint, Value as QdrantValue,
    },
    Qdrant,
};
use rig::client::EmbeddingsClient;
use rig::completion::ToolDefinition;
use rig::embeddings::EmbeddingModel;
use rig::providers::openai::{Client, EmbeddingModel as OpenAIEmbeddingModel};
use rig::tool::Tool as RigTool;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::config::VectorStoreConfig;
use crate::error::BuilderError;

// ============================================================================
// Tool Definition Schema (extracted for readability)
// ============================================================================

fn tool_parameters_schema(context: &str) -> JsonValue {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": format!(
                    "Natural language question about {context}. \
                    Examples: 'pipeline configuration', 'how to set up alerting'"
                )
            },
            "limit": {
                "type": "integer",
                "description": "Max documents to retrieve",
                "default": SearchParams::DEFAULT_LIMIT,
                "minimum": SearchParams::MIN_LIMIT,
                "maximum": SearchParams::MAX_LIMIT
            },
            "min_score": {
                "type": "number",
                "description": "Minimum similarity threshold 0.0-1.0. Use 0.7+ for precise, 0.3-0.5 for broader.",
                "default": SearchParams::DEFAULT_MIN_SCORE,
                "minimum": SearchParams::MIN_SCORE,
                "maximum": SearchParams::MAX_SCORE
            },
            "label_filters": {
                "type": "array",
                "description": "Optional exact-match payload filters. Only use if explicitly mentioned.",
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
    })
}

fn tool_description(context: &str) -> String {
    format!(
        "Retrieve documents from a PRE-INDEXED knowledge base containing: {context}.\n\n\
        HOW IT WORKS: Uses semantic similarity to find relevant pre-indexed documents. \
        This is NOT a web search - it cannot access the internet or external content.\n\n\
        WHEN TO USE:\n\
        - Questions about {context}\n\
        - 'How do I...' questions about documented features\n\
        - Looking up internal guides or documentation\n\n\
        WHEN NOT TO USE:\n\
        - General knowledge (use your training)\n\
        - Current events or news\n\
        - Web searches or external sites\n\
        - Topics not in this knowledge base"
    )
}

// ============================================================================
// Search Parameters (validated, strongly-typed)
// ============================================================================

struct SearchParams {
    limit: u32,
    min_score: f32,
    filters: Vec<PayloadFilter>,
}

impl SearchParams {
    /// Default result limit: balances embedding API cost against result comprehensiveness.
    /// Typical RAG use cases need 3-7 results for context without overwhelming the LLM.
    const DEFAULT_LIMIT: u32 = 5;
    const MIN_LIMIT: u32 = 1;
    /// Maximum results: Qdrant can return more, but >20 results typically degrades
    /// LLM response quality (context overload) without improving answer relevance.
    const MAX_LIMIT: u32 = 20;
    /// Default similarity threshold: 0.5 provides good precision for most embeddings.
    /// Use 0.7+ for strict matching, 0.3-0.5 for exploratory/broad searches.
    const DEFAULT_MIN_SCORE: f32 = 0.5;
    const MIN_SCORE: f32 = 0.0;
    const MAX_SCORE: f32 = 1.0;

    fn new(limit: u32, min_score: f32, filters: Vec<PayloadFilter>) -> Self {
        Self {
            limit: limit.clamp(Self::MIN_LIMIT, Self::MAX_LIMIT),
            min_score: min_score.clamp(Self::MIN_SCORE, Self::MAX_SCORE),
            filters,
        }
    }

    fn qdrant_filter(&self) -> Option<Filter> {
        (!self.filters.is_empty()).then(|| Filter {
            must: self.filters.iter().map(Condition::from).collect(),
            ..Default::default()
        })
    }
}

// ============================================================================
// Payload Filter (typed filter value)
// ============================================================================

#[derive(Debug, Clone)]
struct PayloadFilter {
    key: String,
    value: FilterValue,
}

#[derive(Debug, Clone)]
enum FilterValue {
    String(String),
    Integer(i64),
    Bool(bool),
    StringArray(Vec<String>),
    IntegerArray(Vec<i64>),
}

impl From<&LabelFilter> for PayloadFilter {
    fn from(lf: &LabelFilter) -> Self {
        Self {
            key: lf.key.clone(),
            value: FilterValue::from(lf.value.as_str()),
        }
    }
}

impl From<&PayloadFilter> for Condition {
    fn from(pf: &PayloadFilter) -> Self {
        let key = &pf.key;
        match &pf.value {
            FilterValue::String(s) => Condition::matches(key, s.clone()),
            FilterValue::Integer(i) => Condition::matches(key, *i),
            FilterValue::Bool(b) => Condition::matches(key, *b),
            FilterValue::StringArray(arr) => Condition::matches(key, arr.clone()),
            FilterValue::IntegerArray(arr) => Condition::matches(key, arr.clone()),
        }
    }
}

impl From<&str> for FilterValue {
    /// Converts a string to a FilterValue, attempting JSON parsing first.
    ///
    /// If JSON parsing fails, treats the input as a raw string. This means
    /// "42" (as a string value) becomes String("42"), not Integer(42).
    /// This is intentional because LabelFilter values are already String-typed
    /// from the API schema, so literal strings should be preserved as strings.
    fn from(s: &str) -> Self {
        let json: JsonValue = serde_json::from_str(s).unwrap_or_else(|e| {
            tracing::debug!("Filter value '{}' not valid JSON ({}), treating as string", s, e);
            JsonValue::String(s.into())
        });

        match json {
            JsonValue::String(s) => Self::String(s),
            JsonValue::Number(n) => n
                .as_i64()
                .map(Self::Integer)
                .unwrap_or(Self::String(n.to_string())),
            JsonValue::Bool(b) => Self::Bool(b),
            JsonValue::Array(arr) => Self::parse_array(arr),
            _ => Self::String(s.into()),
        }
    }
}

impl FilterValue {
    fn parse_array(arr: Vec<JsonValue>) -> Self {
        // Try as string array
        let strings: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(Into::into))
            .collect();
        if strings.len() == arr.len() {
            return Self::StringArray(strings);
        }

        // Try as integer array
        let ints: Vec<i64> = arr.iter().filter_map(|v| v.as_i64()).collect();
        if ints.len() == arr.len() {
            return Self::IntegerArray(ints);
        }

        // Fallback: stringify mixed-type arrays
        Self::String(Self::serialize_mixed_array(&arr))
    }

    /// Serialize a mixed-type array to a string, with fallback to simple formatting.
    /// Prefer readable format over empty string to preserve data for debugging.
    fn serialize_mixed_array(arr: &[JsonValue]) -> String {
        serde_json::to_string(arr).unwrap_or_else(|e| {
            tracing::debug!("Failed to serialize filter array: {e}");
            format!(
                "[{}]",
                arr.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }
}

// ============================================================================
// Tool Arguments (serde)
// ============================================================================

#[derive(Debug, Deserialize, Serialize)]
pub struct VectorSearchArgs {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default = "default_min_score")]
    pub min_score: f32,
    #[serde(default)]
    pub label_filters: Vec<LabelFilter>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LabelFilter {
    pub key: String,
    pub value: String,
}

const fn default_limit() -> u32 {
    SearchParams::DEFAULT_LIMIT
}

const fn default_min_score() -> f32 {
    SearchParams::DEFAULT_MIN_SCORE
}

impl From<VectorSearchArgs> for SearchParams {
    fn from(args: VectorSearchArgs) -> Self {
        let filters = args.label_filters.iter().map(PayloadFilter::from).collect();
        Self::new(args.limit, args.min_score, filters)
    }
}

// ============================================================================
// Content Extraction
// ============================================================================

/// Text content field names to search, in priority order.
/// Includes common frameworks: LangChain (page_content), LlamaIndex (text), Haystack (content).
const TEXT_FIELDS: &[&str] = &[
    // LangChain default
    "page_content",
    // Common/Haystack
    "content",
    "text",
    // Secondary
    "body",
    "chunk",
    "document",
    "passage",
    // Other
    "definition",
    "data",
    "raw_text",
    "description",
    "summary",
];

/// Source/URL field names to search for metadata
const URI_FIELDS: &[&str] = &["uri", "url", "source", "source_url", "link", "href"];

/// Title field names to search for metadata
const TITLE_FIELDS: &[&str] = &["title", "name", "heading", "filename", "doc_title"];

/// Nested struct field names that may contain metadata (e.g., LangChain's `metadata`)
const METADATA_STRUCT_FIELDS: &[&str] = &["metadata", "meta", "_metadata"];

/// Formats a Qdrant PointId into a human-readable string.
/// Handles both numeric IDs (u64) and UUID string IDs.
fn format_point_id(id: &PointId) -> String {
    match &id.point_id_options {
        Some(PointIdOptions::Num(n)) => n.to_string(),
        Some(PointIdOptions::Uuid(s)) => s.clone(),
        None => "unknown".to_string(),
    }
}

/// Extracts text content from a Qdrant point using a three-tier fallback strategy:
///
/// 1. **Top-level text fields** (fastest): Checks common field names like `page_content`,
///    `content`, `text` directly on the point payload. Most well-structured data hits here.
///
/// 2. **Nested metadata structures** (LangChain/LlamaIndex pattern): Checks fields like
///    `metadata.page_content` for frameworks that nest content inside metadata objects.
///
/// 3. **Full payload serialization** (last resort): If no text fields found, serializes
///    the entire payload as JSON. This may produce large/incomplete results but ensures
///    the agent receives something to work with.
///
/// Returns formatted content with title and source URL metadata when available.
fn extract_content(point: &ScoredPoint, max_payload_size: usize) -> String {
    // Strategy 1: Try top-level text fields (most common case)
    if let Some(text) = extract_text_from_fields(point, TEXT_FIELDS) {
        return format_with_metadata(point, &text);
    }

    // Strategy 2: Try nested metadata struct (LangChain pattern: metadata.page_content)
    if let Some(text) = extract_from_nested_metadata(point, TEXT_FIELDS) {
        return format_with_metadata(point, &text);
    }

    // Strategy 3: Fallback to full payload serialization with size cap
    let payload_json = serde_json::to_string_pretty(&point.payload).unwrap_or_else(|e| {
        tracing::warn!("Failed to serialize Qdrant payload: {e}");
        "[ERROR: Could not extract or format document content]".to_string()
    });

    // Cap payload size to prevent LLM context window overflow
    if payload_json.len() > max_payload_size {
        format!(
            "{}... [TRUNCATED: payload exceeded {} bytes]",
            &payload_json[..max_payload_size],
            max_payload_size
        )
    } else {
        payload_json
    }
}

/// Try to extract text from a list of field names at the top level
fn extract_text_from_fields(point: &ScoredPoint, fields: &[&str]) -> Option<String> {
    fields
        .iter()
        .find_map(|field| extract_text(point.get(field)).filter(|text| !text.is_empty()))
}

/// Try to extract text from nested metadata structs (e.g., `metadata.content`)
fn extract_from_nested_metadata(point: &ScoredPoint, text_fields: &[&str]) -> Option<String> {
    for meta_field in METADATA_STRUCT_FIELDS {
        if let Some(struct_val) = point.get(meta_field).as_struct() {
            for text_field in text_fields {
                if let Some(val) = struct_val.fields.get(*text_field) {
                    if let Some(text) = extract_text_from_value(val) {
                        if !text.is_empty() {
                            return Some(text);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Extract text from a Qdrant Value (handles strings, lists, and nested structs)
fn extract_text(value: &QdrantValue) -> Option<String> {
    extract_text_from_value(value)
}

/// Core text extraction logic supporting multiple value types.
/// Only extracts actual text content - does not stringify numeric IDs or other non-text fields.
fn extract_text_from_value(value: &QdrantValue) -> Option<String> {
    // String value - most common case
    if let Some(s) = value.as_str() {
        return Some(s.clone());
    }

    // List - extract strings or recurse into structs
    if let Some(list) = value.as_list() {
        let extracted: Vec<String> = list
            .iter()
            .filter_map(|v| {
                // Try string first
                if let Some(s) = v.as_str() {
                    return Some(s.clone());
                }
                // Try nested struct (handles list of chunks with text fields)
                if let Some(struct_val) = v.as_struct() {
                    for field in TEXT_FIELDS {
                        if let Some(nested) = struct_val.fields.get(*field) {
                            if let Some(text) = extract_text_from_value(nested) {
                                if !text.is_empty() {
                                    return Some(text);
                                }
                            }
                        }
                    }
                }
                None
            })
            .collect();
        if !extracted.is_empty() {
            return Some(extracted.join("\n\n"));
        }
    }

    // Nested struct - recursively check for text fields
    if let Some(struct_val) = value.as_struct() {
        for field in TEXT_FIELDS {
            if let Some(nested) = struct_val.fields.get(*field) {
                if let Some(text) = extract_text_from_value(nested) {
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
        }
    }

    // Note: Intentionally NOT extracting integer/double values as text content.
    // These are typically IDs or scores, not document content.

    None
}

fn format_with_metadata(point: &ScoredPoint, text: &str) -> String {
    let title = extract_first_match(point, TITLE_FIELDS);
    let uri = extract_first_match(point, URI_FIELDS);

    match (title, uri) {
        (Some(t), Some(u)) => format!("**{t}**\n\n{text}\n\nSource: {u}"),
        (Some(t), None) => format!("**{t}**\n\n{text}"),
        (None, Some(u)) => format!("{text}\n\nSource: {u}"),
        (None, None) => text.to_string(),
    }
}

/// Extract the first matching field from a list, checking both top-level and nested metadata
fn extract_first_match(point: &ScoredPoint, fields: &[&str]) -> Option<String> {
    // Check top-level fields
    for field in fields {
        if let Some(s) = point.get(field).as_str() {
            return Some(s.clone());
        }
    }

    // Check nested metadata structs (LangChain pattern: metadata.source)
    for meta_field in METADATA_STRUCT_FIELDS {
        if let Some(struct_val) = point.get(meta_field).as_struct() {
            for field in fields {
                if let Some(val) = struct_val.fields.get(*field) {
                    if let Some(s) = val.as_str() {
                        return Some(s.clone());
                    }
                }
            }
        }
    }

    None
}

// ============================================================================
// Vector Search Tool
// ============================================================================

/// Default timeout for Qdrant queries to prevent hanging tool execution.
/// 30 seconds is generous for production vector search. Rationale:
/// - Typical Qdrant queries complete in <1s for collections under 1M vectors
/// - Cloud-managed Qdrant (Qdrant Cloud) typically responds in <2s
/// - On-premise Qdrant with large collections may need 5-30s
/// - 30s provides buffer for network latency and cold-start scenarios
/// Override via `query_timeout_secs` in VectorStoreConfig.
const DEFAULT_QDRANT_QUERY_TIMEOUT_SECS: u64 = 30;

/// Default timeout for embedding API calls (OpenAI).
/// 15 seconds is generous; typical embedding calls complete in <1s.
/// Rationale: OpenAI embedding API is usually fast, but rate limiting or
/// network saturation can cause delays. 15s prevents indefinite blocking.
/// Override via `embedding_timeout_secs` in VectorStoreConfig.
const DEFAULT_EMBEDDING_TIMEOUT_SECS: u64 = 15;

/// Maximum size for fallback payload serialization to prevent context window overflow.
/// 50KB is sufficient for most document content while preventing LLM overload.
/// Override via `max_payload_size` in VectorStoreConfig.
const DEFAULT_MAX_PAYLOAD_SIZE: usize = 50_000;

/// Vector search tool that queries a Qdrant collection using semantic similarity.
///
/// Named "Dynamic" because Rig's Tool trait requires `const NAME`, but we need
/// runtime names like `retrieve_from_docs`. We override via `name()`.
#[derive(Clone)]
pub struct DynamicVectorSearchTool {
    qdrant: Qdrant,
    embeddings: OpenAIEmbeddingModel,
    collection: String,
    store_name: String,
    tool_name: String,
    description: Option<String>,
    /// Timeout for Qdrant queries (configurable, defaults to 30s)
    query_timeout: Duration,
    /// Timeout for embedding API calls (configurable, defaults to 15s)
    embedding_timeout: Duration,
    /// Maximum payload size for fallback extraction (configurable, defaults to 50KB)
    max_payload_size: usize,
}

impl DynamicVectorSearchTool {
    pub async fn from_config(config: &VectorStoreConfig) -> Result<Self, BuilderError> {
        // Validate configuration upfront for clear error messages
        if config.name.is_empty() {
            return Err(BuilderError::VectorStoreError(
                "Vector store name must not be empty".to_string(),
            ));
        }
        if config.collection_name.is_empty() {
            return Err(BuilderError::VectorStoreError(
                "Collection name must not be empty".to_string(),
            ));
        }
        if config.url.is_empty() {
            return Err(BuilderError::VectorStoreError(
                "Qdrant URL must not be empty".to_string(),
            ));
        }
        if !config.url.starts_with("http://") && !config.url.starts_with("https://") {
            return Err(BuilderError::VectorStoreError(format!(
                "Invalid Qdrant URL '{}': must start with http:// or https://",
                config.url
            )));
        }
        if config.embedding_model.provider != "openai" {
            return Err(BuilderError::VectorStoreError(format!(
                "Unsupported embedding provider: '{}'. Only 'openai' is currently supported.",
                config.embedding_model.provider
            )));
        }

        info!(
            "Initializing vector search tool '{}' (collection: {}, url: {})",
            config.name, config.collection_name, config.url
        );

        let qdrant = Qdrant::from_url(&config.url)
            .build()
            .map_err(|e| BuilderError::VectorStoreError(format!("Qdrant client failed: {e}")))?;

        let openai = Client::new(&config.embedding_model.api_key)
            .map_err(|e| BuilderError::VectorStoreError(format!("OpenAI client failed: {e}")))?;

        // Use config values with fallback to defaults
        let query_timeout = Duration::from_secs(
            config
                .query_timeout_secs
                .unwrap_or(DEFAULT_QDRANT_QUERY_TIMEOUT_SECS),
        );
        let embedding_timeout = Duration::from_secs(
            config
                .embedding_timeout_secs
                .unwrap_or(DEFAULT_EMBEDDING_TIMEOUT_SECS),
        );
        let max_payload_size = config
            .max_payload_size
            .unwrap_or(DEFAULT_MAX_PAYLOAD_SIZE);

        info!(
            "  Timeouts: query={}s, embedding={}s; max_payload={}KB",
            query_timeout.as_secs(),
            embedding_timeout.as_secs(),
            max_payload_size / 1000
        );

        Ok(Self {
            qdrant,
            embeddings: openai.embedding_model(&config.embedding_model.model),
            collection: config.collection_name.clone(),
            store_name: config.name.clone(),
            tool_name: format!("retrieve_from_{}", config.name),
            description: config.description.clone(),
            query_timeout,
            embedding_timeout,
            max_payload_size,
        })
    }

    async fn search(
        &self,
        query: &str,
        params: SearchParams,
    ) -> Result<Vec<SearchResult>, BuilderError> {
        // Wrap embedding API call with timeout to prevent indefinite blocking
        let embedding_future = self.embeddings.embed_text(query);
        let embedding = tokio::time::timeout(self.embedding_timeout, embedding_future)
            .await
            .map_err(|_| {
                warn!(
                    "Embedding API timeout after {:?} for query '{}'",
                    self.embedding_timeout,
                    &query[..query.len().min(50)]
                );
                BuilderError::VectorStoreError(format!(
                    "Embedding API timed out after {:?}",
                    self.embedding_timeout
                ))
            })?
            .map_err(|e| BuilderError::VectorStoreError(format!("Embedding failed: {e}")))?;

        // Rig's embedding API returns f64 for cross-provider compatibility.
        // Qdrant natively uses f32 (standard for most vector databases).
        // Precision loss (f64 → f32) is negligible for cosine similarity metrics
        // since embedding values are typically in [-1, 1] range where f32 is adequate.
        let vector: Vec<f32> = embedding.vec.iter().map(|&x| x as f32).collect();

        let mut builder = QueryPointsBuilder::new(&self.collection)
            .query(Query::new_nearest(vector))
            .limit(params.limit as u64)
            .with_payload(true)
            .with_vectors(false)
            .score_threshold(params.min_score);

        if let Some(filter) = params.qdrant_filter() {
            builder = builder.filter(filter);
        }

        debug!(
            "Qdrant query: collection={}, limit={}, min_score={}",
            self.collection, params.limit, params.min_score
        );

        // Wrap Qdrant query with timeout to prevent indefinite blocking
        let query_future = self.qdrant.query(builder.build());
        let response = tokio::time::timeout(self.query_timeout, query_future)
            .await
            .map_err(|_| {
                warn!(
                    "Qdrant query timeout after {:?} for collection '{}'",
                    self.query_timeout, self.collection
                );
                BuilderError::VectorStoreError(format!(
                    "Qdrant query timed out after {:?}",
                    self.query_timeout
                ))
            })?
            .map_err(|e| BuilderError::VectorStoreError(format!("Qdrant query failed: {e}")))?;

        // Log query execution time from Qdrant response
        debug!("Qdrant query completed in {:.3}s", response.time);

        let max_payload_size = self.max_payload_size;
        let results: Vec<SearchResult> = response
            .result
            .into_iter()
            .map(|p| {
                // Extract point ID in human-readable format (not Debug format)
                let point_id = p.id.as_ref().map(format_point_id).unwrap_or_else(|| "unknown".to_string());
                SearchResult {
                    point_id,
                    content: extract_content(&p, max_payload_size),
                    score: p.score,
                }
            })
            .collect();

        info!(
            "Vector search '{}': {} results for '{}' ({:.3}s)",
            self.store_name,
            results.len(),
            query,
            response.time
        );
        Ok(results)
    }

    fn format_results(&self, results: &[SearchResult], query: &str) -> String {
        if results.is_empty() {
            return format!("No results found for query: '{query}'");
        }

        let mut parts: Vec<String> = Vec::with_capacity(results.len() + 1);

        if let Some(desc) = &self.description {
            parts.push(desc.clone());
        }

        for (i, r) in results.iter().enumerate() {
            debug!("Result {} point_id={}", i + 1, r.point_id);
            parts.push(format!(
                "Result {} (score: {:.3}):\n{}",
                i + 1,
                r.score,
                r.content
            ));
        }

        parts.join("\n\n---\n\n")
    }
}

impl RigTool for DynamicVectorSearchTool {
    type Error = BuilderError;
    type Args = VectorSearchArgs;
    type Output = String;

    const NAME: &'static str = "dynamic_vector_search_tool";

    fn name(&self) -> String {
        self.tool_name.clone()
    }

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let context = self.description.as_deref().unwrap_or(&self.store_name);
        ToolDefinition {
            name: self.tool_name.clone(),
            description: tool_description(context),
            parameters: tool_parameters_schema(context),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Destructure to avoid unnecessary clone of query string
        let VectorSearchArgs {
            query,
            limit,
            min_score,
            label_filters,
        } = args;

        if !label_filters.is_empty() {
            debug!("Label filters: {:?}", label_filters);
        }

        let filters = label_filters.iter().map(PayloadFilter::from).collect();
        let params = SearchParams::new(limit, min_score, filters);
        let results = self.search(&query, params).await?;
        Ok(self.format_results(&results, &query))
    }
}

#[derive(Debug, Clone)]
struct SearchResult {
    point_id: String,
    content: String,
    score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // FilterValue Parsing Tests
    // =========================================================================

    #[test]
    fn test_filter_value_from_plain_string() {
        let fv = FilterValue::from("hello world");
        assert!(matches!(fv, FilterValue::String(s) if s == "hello world"));
    }

    #[test]
    fn test_filter_value_from_json_string() {
        // JSON string "hello" should be parsed as String
        let fv = FilterValue::from("\"hello\"");
        assert!(matches!(fv, FilterValue::String(s) if s == "hello"));
    }

    #[test]
    fn test_filter_value_from_integer() {
        let fv = FilterValue::from("42");
        assert!(matches!(fv, FilterValue::Integer(42)));
    }

    #[test]
    fn test_filter_value_from_negative_integer() {
        let fv = FilterValue::from("-123");
        assert!(matches!(fv, FilterValue::Integer(-123)));
    }

    #[test]
    fn test_filter_value_from_bool_true() {
        let fv = FilterValue::from("true");
        assert!(matches!(fv, FilterValue::Bool(true)));
    }

    #[test]
    fn test_filter_value_from_bool_false() {
        let fv = FilterValue::from("false");
        assert!(matches!(fv, FilterValue::Bool(false)));
    }

    #[test]
    fn test_filter_value_from_string_array() {
        let fv = FilterValue::from("[\"a\", \"b\", \"c\"]");
        match fv {
            FilterValue::StringArray(arr) => {
                assert_eq!(arr, vec!["a", "b", "c"]);
            }
            _ => panic!("Expected StringArray, got {:?}", fv),
        }
    }

    #[test]
    fn test_filter_value_from_integer_array() {
        let fv = FilterValue::from("[1, 2, 3]");
        match fv {
            FilterValue::IntegerArray(arr) => {
                assert_eq!(arr, vec![1, 2, 3]);
            }
            _ => panic!("Expected IntegerArray, got {:?}", fv),
        }
    }

    #[test]
    fn test_filter_value_from_mixed_array_falls_back_to_string() {
        // Mixed arrays cannot be typed, so they stringify
        let fv = FilterValue::from("[1, \"a\", true]");
        assert!(matches!(fv, FilterValue::String(_)));
    }

    #[test]
    fn test_filter_value_from_empty_array() {
        // Empty array - should become StringArray (first check passes with len 0 == 0)
        let fv = FilterValue::from("[]");
        assert!(matches!(fv, FilterValue::StringArray(arr) if arr.is_empty()));
    }

    // =========================================================================
    // SearchParams Validation Tests
    // =========================================================================

    #[test]
    fn test_search_params_clamps_limit_minimum() {
        let params = SearchParams::new(0, 0.5, vec![]);
        assert_eq!(params.limit, SearchParams::MIN_LIMIT);
    }

    #[test]
    fn test_search_params_clamps_limit_maximum() {
        let params = SearchParams::new(100, 0.5, vec![]);
        assert_eq!(params.limit, SearchParams::MAX_LIMIT);
    }

    #[test]
    fn test_search_params_clamps_min_score_minimum() {
        let params = SearchParams::new(5, -0.5, vec![]);
        assert_eq!(params.min_score, SearchParams::MIN_SCORE);
    }

    #[test]
    fn test_search_params_clamps_min_score_maximum() {
        let params = SearchParams::new(5, 1.5, vec![]);
        assert_eq!(params.min_score, SearchParams::MAX_SCORE);
    }

    #[test]
    fn test_search_params_valid_values_unchanged() {
        let params = SearchParams::new(10, 0.7, vec![]);
        assert_eq!(params.limit, 10);
        assert!((params.min_score - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn test_search_params_qdrant_filter_empty() {
        let params = SearchParams::new(5, 0.5, vec![]);
        assert!(params.qdrant_filter().is_none());
    }

    #[test]
    fn test_search_params_qdrant_filter_present() {
        let filters = vec![PayloadFilter {
            key: "status".to_string(),
            value: FilterValue::String("active".to_string()),
        }];
        let params = SearchParams::new(5, 0.5, filters);
        assert!(params.qdrant_filter().is_some());
    }

    // =========================================================================
    // VectorSearchArgs Conversion Tests
    // =========================================================================

    #[test]
    fn test_vector_search_args_default_values() {
        let args: VectorSearchArgs = serde_json::from_str(r#"{"query": "test"}"#).unwrap();
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, SearchParams::DEFAULT_LIMIT);
        assert!((args.min_score - SearchParams::DEFAULT_MIN_SCORE).abs() < f32::EPSILON);
        assert!(args.label_filters.is_empty());
    }

    #[test]
    fn test_vector_search_args_with_filters() {
        let args: VectorSearchArgs = serde_json::from_str(
            r#"{"query": "test", "limit": 10, "min_score": 0.8, "label_filters": [{"key": "type", "value": "doc"}]}"#,
        )
        .unwrap();
        assert_eq!(args.query, "test");
        assert_eq!(args.limit, 10);
        assert!((args.min_score - 0.8).abs() < f32::EPSILON);
        assert_eq!(args.label_filters.len(), 1);
        assert_eq!(args.label_filters[0].key, "type");
        assert_eq!(args.label_filters[0].value, "doc");
    }

    #[test]
    fn test_vector_search_args_to_search_params() {
        let args = VectorSearchArgs {
            query: "test".to_string(),
            limit: 100, // Will be clamped to MAX_LIMIT
            min_score: 2.0, // Will be clamped to MAX_SCORE
            label_filters: vec![],
        };
        let params: SearchParams = args.into();
        assert_eq!(params.limit, SearchParams::MAX_LIMIT);
        assert_eq!(params.min_score, SearchParams::MAX_SCORE);
    }

    // =========================================================================
    // PayloadFilter Conversion Tests
    // =========================================================================

    #[test]
    fn test_label_filter_to_payload_filter() {
        let lf = LabelFilter {
            key: "category".to_string(),
            value: "42".to_string(), // Should be parsed as Integer
        };
        let pf = PayloadFilter::from(&lf);
        assert_eq!(pf.key, "category");
        assert!(matches!(pf.value, FilterValue::Integer(42)));
    }

    #[test]
    fn test_label_filter_preserves_string_value() {
        let lf = LabelFilter {
            key: "name".to_string(),
            value: "hello".to_string(), // Plain string
        };
        let pf = PayloadFilter::from(&lf);
        assert_eq!(pf.key, "name");
        assert!(matches!(pf.value, FilterValue::String(s) if s == "hello"));
    }

    // =========================================================================
    // Tool Schema Tests
    // =========================================================================

    #[test]
    fn test_tool_description_contains_context() {
        let desc = tool_description("Mezmo documentation");
        assert!(desc.contains("Mezmo documentation"));
        assert!(desc.contains("WHEN TO USE"));
        assert!(desc.contains("WHEN NOT TO USE"));
    }

    #[test]
    fn test_tool_parameters_schema_structure() {
        let schema = tool_parameters_schema("docs");
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["limit"].is_object());
        assert!(schema["properties"]["min_score"].is_object());
        assert!(schema["properties"]["label_filters"].is_object());
        assert_eq!(schema["required"].as_array().unwrap().len(), 4);
    }

    // =========================================================================
    // Point ID Formatting Tests
    // =========================================================================

    #[test]
    fn test_format_point_id_numeric() {
        let id = PointId {
            point_id_options: Some(PointIdOptions::Num(12345)),
        };
        assert_eq!(format_point_id(&id), "12345");
    }

    #[test]
    fn test_format_point_id_uuid() {
        let id = PointId {
            point_id_options: Some(PointIdOptions::Uuid(
                "550e8400-e29b-41d4-a716-446655440000".to_string(),
            )),
        };
        assert_eq!(
            format_point_id(&id),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_format_point_id_none() {
        let id = PointId {
            point_id_options: None,
        };
        assert_eq!(format_point_id(&id), "unknown");
    }

    // =========================================================================
    // Constants Validation Tests
    // =========================================================================

    #[test]
    fn test_default_constants_are_reasonable() {
        // Qdrant timeout should be between 5s and 120s
        assert!(DEFAULT_QDRANT_QUERY_TIMEOUT_SECS >= 5);
        assert!(DEFAULT_QDRANT_QUERY_TIMEOUT_SECS <= 120);

        // Embedding timeout should be between 5s and 60s
        assert!(DEFAULT_EMBEDDING_TIMEOUT_SECS >= 5);
        assert!(DEFAULT_EMBEDDING_TIMEOUT_SECS <= 60);

        // Max payload fallback should be at least 10KB but not exceed 100KB
        assert!(DEFAULT_MAX_PAYLOAD_SIZE >= 10_000);
        assert!(DEFAULT_MAX_PAYLOAD_SIZE <= 100_000);
    }
}
