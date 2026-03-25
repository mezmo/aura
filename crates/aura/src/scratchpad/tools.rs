//! Scratchpad exploration tools.
//!
//! Six read-only tools that let the LLM selectively explore large
//! tool outputs stored on disk, rather than loading everything into context.

use super::context_budget::ContextBudget;
use super::schema::{analyze_json_structure, format_schema};
use super::storage::ScratchpadStorage;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

// ============================================================================
// Shared Error Type
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum ScratchpadToolError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Path error: {0}")]
    Path(String),
    #[error("Invalid argument: {0}")]
    InvalidArg(String),
    #[error("Not JSON: file is not valid JSON")]
    NotJson,
    #[error("Key path not found: {0}")]
    KeyNotFound(String),
}

// ============================================================================
// Shared Helpers
// ============================================================================

/// Add 1-indexed line numbers to content.
fn add_line_numbers(content: &str) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build metadata JSON for tool responses.
fn build_metadata(lines: usize, content: &str, budget: &ContextBudget) -> serde_json::Value {
    json!({
        "lines": lines,
        "estimated_tokens": ContextBudget::estimate_tokens(content),
        "window_hint": budget.window_hint()
    })
}

/// Read a scratchpad file, validating the path.
async fn read_scratchpad_file(
    storage: &ScratchpadStorage,
    file: &str,
) -> Result<String, ScratchpadToolError> {
    let path = storage
        .validate_path(file)
        .map_err(|e| ScratchpadToolError::Path(e.to_string()))?;
    tokio::fs::read_to_string(&path)
        .await
        .map_err(ScratchpadToolError::Io)
}

/// Check budget and record usage. Returns `Ok(())` if within budget,
/// or `Err(BudgetExceeded)` with token details for the caller to format.
fn check_and_record_budget(
    budget: &ContextBudget,
    content: &str,
) -> Result<(), super::context_budget::BudgetExceeded> {
    match budget.check_fits(content) {
        Ok(tokens) => {
            budget.record_usage(tokens);
            budget.record_extracted(content.len());
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ============================================================================
// head — First N lines
// ============================================================================

#[derive(Clone)]
pub struct HeadTool {
    storage: Arc<ScratchpadStorage>,
    budget: ContextBudget,
}

impl HeadTool {
    pub fn new(storage: Arc<ScratchpadStorage>, budget: ContextBudget) -> Self {
        Self { storage, budget }
    }
}

#[derive(Deserialize, Serialize)]
pub struct HeadArgs {
    pub file: String,
    #[serde(default = "default_head_lines")]
    pub lines: usize,
}

fn default_head_lines() -> usize {
    50
}

impl Tool for HeadTool {
    const NAME: &'static str = "head";
    type Error = ScratchpadToolError;
    type Args = HeadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read the first N lines of a scratchpad file. Use this to preview \
                          large tool outputs before deciding what to extract."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename (e.g., 'call_abc123.json')"
                    },
                    "lines": {
                        "type": "integer",
                        "description": "Number of lines to read (default: 50)",
                        "default": 50
                    }
                },
                "required": ["file"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!("scratchpad head: file={}, lines={}", args.file, args.lines);
        let content = read_scratchpad_file(&self.storage, &args.file).await?;
        let selected: String = content
            .lines()
            .take(args.lines)
            .collect::<Vec<_>>()
            .join("\n");
        let total_lines = content.lines().count();

        if let Err(exceeded) = check_and_record_budget(&self.budget, &selected) {
            return Ok(json!({
                "error": "head_too_large",
                "message": format!(
                    "The head of this file is too large for your remaining context \
                     (estimated {} tokens, ~{} tokens remaining). Narrow your request.",
                    exceeded.requested_tokens, exceeded.remaining_tokens
                ),
                "requested_lines": args.lines,
                "estimated_tokens": exceeded.requested_tokens,
                "remaining_budget_tokens": exceeded.remaining_tokens,
                "suggestions": [
                    format!("Try a smaller number of lines: head(file, {})",
                        (exceeded.remaining_tokens * 4).min(args.lines / 2).max(10)),
                    "Use grep to find specific lines first",
                    "Use get_in to extract a specific key if it's structured data"
                ]
            })
            .to_string());
        }

        let numbered = add_line_numbers(&selected);
        let meta = build_metadata(selected.lines().count(), &selected, &self.budget);
        Ok(format!(
            "{}\n\n--- scratchpad head: showing {}/{} lines | {} ---",
            numbered,
            selected.lines().count(),
            total_lines,
            meta
        ))
    }
}

// ============================================================================
// slice — Line range extraction
// ============================================================================

#[derive(Clone)]
pub struct SliceTool {
    storage: Arc<ScratchpadStorage>,
    budget: ContextBudget,
}

impl SliceTool {
    pub fn new(storage: Arc<ScratchpadStorage>, budget: ContextBudget) -> Self {
        Self { storage, budget }
    }
}

#[derive(Deserialize, Serialize)]
pub struct SliceArgs {
    pub file: String,
    /// 1-indexed start line.
    pub start: usize,
    /// 1-indexed end line (inclusive).
    pub end: usize,
}

impl Tool for SliceTool {
    const NAME: &'static str = "slice";
    type Error = ScratchpadToolError;
    type Args = SliceArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Extract a range of lines (1-indexed, inclusive) from a scratchpad file."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename"
                    },
                    "start": {
                        "type": "integer",
                        "description": "Start line number (1-indexed)"
                    },
                    "end": {
                        "type": "integer",
                        "description": "End line number (1-indexed, inclusive)"
                    }
                },
                "required": ["file", "start", "end"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if args.start == 0 || args.end < args.start {
            return Err(ScratchpadToolError::InvalidArg(
                "start must be >= 1 and end >= start".to_string(),
            ));
        }
        tracing::debug!(
            "scratchpad slice: file={}, start={}, end={}",
            args.file,
            args.start,
            args.end
        );
        let content = read_scratchpad_file(&self.storage, &args.file).await?;
        let total_lines = content.lines().count();
        let selected: String = content
            .lines()
            .skip(args.start - 1)
            .take(args.end - args.start + 1)
            .collect::<Vec<_>>()
            .join("\n");

        if let Err(exceeded) = check_and_record_budget(&self.budget, &selected) {
            let suggested_end = args.start + (exceeded.remaining_tokens * 4 / 80).max(1);
            return Ok(json!({
                "error": "slice_too_large",
                "message": format!(
                    "This slice is too large for your remaining context \
                     (estimated {} tokens, ~{} tokens remaining). Narrow your request.",
                    exceeded.requested_tokens, exceeded.remaining_tokens
                ),
                "requested_lines": [args.start, args.end],
                "estimated_tokens": exceeded.requested_tokens,
                "remaining_budget_tokens": exceeded.remaining_tokens,
                "suggestions": [
                    format!("Try a smaller range: slice(file, {}, {})", args.start, suggested_end),
                    "Use grep to find specific lines first",
                    "Use get_in to extract a specific key if it's structured data"
                ]
            })
            .to_string());
        }

        // Add line numbers starting from args.start
        let numbered: String = content
            .lines()
            .skip(args.start - 1)
            .take(args.end - args.start + 1)
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}", args.start + i, line))
            .collect::<Vec<_>>()
            .join("\n");

        let actual_lines = selected.lines().count();
        let meta = build_metadata(actual_lines, &selected, &self.budget);
        Ok(format!(
            "{}\n\n--- scratchpad slice: lines {}-{} of {} | {} ---",
            numbered, args.start, args.end, total_lines, meta
        ))
    }
}

// ============================================================================
// grep — Regex search with context lines
// ============================================================================

#[derive(Clone)]
pub struct GrepTool {
    storage: Arc<ScratchpadStorage>,
    budget: ContextBudget,
}

impl GrepTool {
    pub fn new(storage: Arc<ScratchpadStorage>, budget: ContextBudget) -> Self {
        Self { storage, budget }
    }
}

#[derive(Deserialize, Serialize)]
pub struct GrepArgs {
    pub file: String,
    pub pattern: String,
    #[serde(default = "default_context_lines")]
    pub context: usize,
}

fn default_context_lines() -> usize {
    3
}

impl Tool for GrepTool {
    const NAME: &'static str = "grep";
    type Error = ScratchpadToolError;
    type Args = GrepArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Search a scratchpad file with a regex pattern. Returns matching lines \
                          with surrounding context."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "context": {
                        "type": "integer",
                        "description": "Number of context lines before and after each match (default: 3)",
                        "default": 3
                    }
                },
                "required": ["file", "pattern"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!(
            "scratchpad grep: file={}, pattern={}, context={}",
            args.file,
            args.pattern,
            args.context
        );
        let content = read_scratchpad_file(&self.storage, &args.file).await?;
        let regex = regex::Regex::new(&args.pattern)
            .map_err(|e| ScratchpadToolError::InvalidArg(format!("Invalid regex: {e}")))?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let mut match_ranges: Vec<(usize, usize)> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            if regex.is_match(line) {
                let start = i.saturating_sub(args.context);
                let end = (i + args.context).min(total_lines - 1);
                match_ranges.push((start, end));
            }
        }

        if match_ranges.is_empty() {
            return Ok(format!(
                "No matches for '{}' in {} ({} lines)",
                args.pattern, args.file, total_lines
            ));
        }

        // Merge overlapping ranges
        let merged = merge_ranges(&match_ranges);
        let mut output_parts = Vec::new();
        let mut match_count = 0;

        for (start, end) in &merged {
            let mut section = String::new();
            for (i, line) in lines.iter().enumerate().take(*end + 1).skip(*start) {
                let marker = if regex.is_match(line) {
                    match_count += 1;
                    ">"
                } else {
                    " "
                };
                section.push_str(&format!("{}{:>5}\t{}\n", marker, i + 1, line));
            }
            output_parts.push(section);
        }

        let result = output_parts.join("\n---\n\n");

        if let Err(exceeded) = check_and_record_budget(&self.budget, &result) {
            return Ok(json!({
                "error": "grep_too_large",
                "message": format!(
                    "This grep result is too large for your remaining context \
                     (estimated {} tokens, ~{} tokens remaining). Narrow your request.",
                    exceeded.requested_tokens, exceeded.remaining_tokens
                ),
                "requested_pattern": args.pattern,
                "estimated_tokens": exceeded.requested_tokens,
                "remaining_budget_tokens": exceeded.remaining_tokens,
                "suggestions": [
                    format!("Try a more specific pattern: grep(file, '{}')", args.pattern),
                    "Use head or slice to limit the number of lines first",
                    "Use get_in to extract a specific key if it's structured data"
                ]
            })
            .to_string());
        }

        let meta = build_metadata(result.lines().count(), &result, &self.budget);
        Ok(format!(
            "{}\n--- scratchpad grep: {} matches in {} regions of {} | {} ---",
            result,
            match_count,
            merged.len(),
            args.file,
            meta
        ))
    }
}

fn merge_ranges(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return vec![];
    }
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|r| r.0);

    let mut merged = vec![sorted[0]];
    for &(start, end) in &sorted[1..] {
        let last = merged.last_mut().unwrap();
        if start <= last.1 + 1 {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

// ============================================================================
// schema — JSON structure with line ranges
// ============================================================================

#[derive(Clone)]
pub struct SchemaTool {
    storage: Arc<ScratchpadStorage>,
    budget: ContextBudget,
}

impl SchemaTool {
    pub fn new(storage: Arc<ScratchpadStorage>, budget: ContextBudget) -> Self {
        Self { storage, budget }
    }
}

#[derive(Deserialize, Serialize)]
pub struct SchemaArgs {
    pub file: String,
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
}

fn default_max_depth() -> usize {
    4
}

impl Tool for SchemaTool {
    const NAME: &'static str = "schema";
    type Error = ScratchpadToolError;
    type Args = SchemaArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Show the JSON structure of a scratchpad file: keys, types, array \
                          lengths, and line ranges. Helps you decide which parts to extract."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename (must be JSON)"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Maximum depth to show (default: 4)",
                        "default": 4
                    }
                },
                "required": ["file"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!(
            "scratchpad schema: file={}, max_depth={}",
            args.file,
            args.max_depth
        );
        let content = read_scratchpad_file(&self.storage, &args.file).await?;
        let node = analyze_json_structure(&content).ok_or(ScratchpadToolError::NotJson)?;
        let schema = format_schema(&node, args.max_depth);

        if let Err(exceeded) = check_and_record_budget(&self.budget, &schema) {
            return Ok(json!({
                "error": "schema_too_large",
                "message": format!(
                    "The schema output is too large for your remaining context \
                     (estimated {} tokens, ~{} tokens remaining). Narrow your request.",
                    exceeded.requested_tokens, exceeded.remaining_tokens
                ),
                "requested_file": args.file,
                "estimated_tokens": exceeded.requested_tokens,
                "remaining_budget_tokens": exceeded.remaining_tokens,
                "suggestions": [
                    format!("Try a smaller depth: schema(file, max_depth={})", (args.max_depth / 2).max(1)),
                    "Use get_in to explore a specific subtree",
                    "Use head to preview the first few lines instead"
                ]
            }).to_string());
        }

        let meta = build_metadata(schema.lines().count(), &schema, &self.budget);
        Ok(format!(
            "{}\n--- scratchpad schema: {} | {} ---",
            schema, args.file, meta
        ))
    }
}

// ============================================================================
// get_in — Nested key path extraction
// ============================================================================

#[derive(Clone)]
pub struct GetInTool {
    storage: Arc<ScratchpadStorage>,
    budget: ContextBudget,
}

impl GetInTool {
    pub fn new(storage: Arc<ScratchpadStorage>, budget: ContextBudget) -> Self {
        Self { storage, budget }
    }
}

#[derive(Deserialize, Serialize)]
pub struct GetInArgs {
    pub file: String,
    /// Dot-separated key path, e.g. "results.0.metadata" or "data.items".
    pub path: String,
}

impl Tool for GetInTool {
    const NAME: &'static str = "get_in";
    type Error = ScratchpadToolError;
    type Args = GetInArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Extract a value at a nested key path from a JSON scratchpad file. \
                          Use dot notation: 'results.0.metadata.title'"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename (must be JSON)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Dot-separated key path (e.g., 'results.0.name')"
                    }
                },
                "required": ["file", "path"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!("scratchpad get_in: file={}, path={}", args.file, args.path);
        let content = read_scratchpad_file(&self.storage, &args.file).await?;
        let root: serde_json::Value =
            serde_json::from_str(&content).map_err(|_| ScratchpadToolError::NotJson)?;

        let current = navigate_path(&root, &args.path)?;

        let result = serde_json::to_string_pretty(current).unwrap_or_else(|_| current.to_string());

        if let Err(exceeded) = check_and_record_budget(&self.budget, &result) {
            return Ok(json!({
                "error": "get_in_too_large",
                "message": format!(
                    "The value at this path is too large for your remaining context \
                     (estimated {} tokens, ~{} tokens remaining). Narrow your request.",
                    exceeded.requested_tokens, exceeded.remaining_tokens
                ),
                "requested_path": args.path,
                "estimated_tokens": exceeded.requested_tokens,
                "remaining_budget_tokens": exceeded.remaining_tokens,
                "suggestions": [
                    format!("Try a deeper path: get_in(file, '{}.0')", args.path),
                    "Use schema to see the structure and pick a smaller subtree",
                    "Use grep to find specific content within the value"
                ]
            })
            .to_string());
        }

        let numbered = add_line_numbers(&result);
        let meta = build_metadata(result.lines().count(), &result, &self.budget);
        Ok(format!(
            "{}\n\n--- scratchpad get_in: $.{} | {} ---",
            numbered, args.path, meta
        ))
    }
}

// ============================================================================
// iterate_over — Extract fields from array items
// ============================================================================

#[derive(Clone)]
pub struct IterateOverTool {
    storage: Arc<ScratchpadStorage>,
    budget: ContextBudget,
}

impl IterateOverTool {
    pub fn new(storage: Arc<ScratchpadStorage>, budget: ContextBudget) -> Self {
        Self { storage, budget }
    }
}

#[derive(Deserialize, Serialize)]
pub struct IterateOverArgs {
    pub file: String,
    /// Dot-separated path to an array (e.g., "results" or "data.items").
    pub path: String,
    /// Comma-separated field names to extract from each item (e.g., "id,title,score").
    pub fields: String,
}

impl Tool for IterateOverTool {
    const NAME: &'static str = "iterate_over";
    type Error = ScratchpadToolError;
    type Args = IterateOverArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description:
                "Iterate over items in a JSON array and extract selected fields from each. \
                          Use dot-notation for the array path and comma-separated field names. \
                          Fields can use dot-notation for nested access (e.g., 'metadata.score')."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename (must be JSON)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Dot-separated path to the array (e.g., 'results' or 'data.items')"
                    },
                    "fields": {
                        "type": "string",
                        "description": "Comma-separated field names to extract (e.g., 'id,title,metadata.score')"
                    }
                },
                "required": ["file", "path", "fields"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!(
            "scratchpad iterate_over: file={}, path={}, fields={}",
            args.file,
            args.path,
            args.fields
        );
        let content = read_scratchpad_file(&self.storage, &args.file).await?;
        let root: serde_json::Value =
            serde_json::from_str(&content).map_err(|_| ScratchpadToolError::NotJson)?;

        let array_value = navigate_path(&root, &args.path)?;
        let items = array_value.as_array().ok_or_else(|| {
            ScratchpadToolError::InvalidArg(format!("Value at '{}' is not an array", args.path))
        })?;

        let field_names: Vec<&str> = args.fields.split(',').map(|s| s.trim()).collect();

        let mut rows: Vec<serde_json::Value> = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let mut row = serde_json::Map::new();
            row.insert("_index".to_string(), json!(i));
            for &field in &field_names {
                let val = navigate_path_value(item, field).unwrap_or(&serde_json::Value::Null);
                row.insert(field.to_string(), val.clone());
            }
            rows.push(serde_json::Value::Object(row));
        }

        let result = serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string());

        if let Err(exceeded) = check_and_record_budget(&self.budget, &result) {
            return Ok(json!({
                "error": "iterate_over_too_large",
                "message": format!(
                    "The iteration result is too large for your remaining context \
                     (estimated {} tokens, ~{} tokens remaining). Narrow your request.",
                    exceeded.requested_tokens, exceeded.remaining_tokens
                ),
                "item_count": items.len(),
                "requested_fields": field_names,
                "estimated_tokens": exceeded.requested_tokens,
                "remaining_budget_tokens": exceeded.remaining_tokens,
                "suggestions": [
                    "Request fewer fields",
                    "Use get_in to access specific items by index (e.g., 'results.0.title')",
                    "Use grep to find specific items first"
                ]
            })
            .to_string());
        }

        let meta = build_metadata(result.lines().count(), &result, &self.budget);
        Ok(format!(
            "{}\n\n--- scratchpad iterate_over: $.{} ({} items, fields: [{}]) | {} ---",
            result,
            args.path,
            items.len(),
            args.fields,
            meta
        ))
    }
}

// ============================================================================
// item_schema — Union of all keys across array items
// ============================================================================

#[derive(Clone)]
pub struct ItemSchemaTool {
    storage: Arc<ScratchpadStorage>,
    budget: ContextBudget,
}

impl ItemSchemaTool {
    pub fn new(storage: Arc<ScratchpadStorage>, budget: ContextBudget) -> Self {
        Self { storage, budget }
    }
}

#[derive(Deserialize, Serialize)]
pub struct ItemSchemaArgs {
    pub file: String,
    /// Dot-separated path to an array (e.g., "results" or "data.items").
    pub path: String,
}

impl Tool for ItemSchemaTool {
    const NAME: &'static str = "item_schema";
    type Error = ScratchpadToolError;
    type Args = ItemSchemaArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Show all unique keys found across all items in a JSON array, with their \
                          types and how many items contain each key. Use this to discover the full \
                          schema of array items before using iterate_over to extract fields."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename (must be JSON)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Dot-separated path to the array (e.g., 'results' or 'data.items')"
                    }
                },
                "required": ["file", "path"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!(
            "scratchpad item_schema: file={}, path={}",
            args.file,
            args.path
        );
        let content = read_scratchpad_file(&self.storage, &args.file).await?;
        let root: serde_json::Value =
            serde_json::from_str(&content).map_err(|_| ScratchpadToolError::NotJson)?;

        let array_value = navigate_path(&root, &args.path)?;
        let items = array_value.as_array().ok_or_else(|| {
            ScratchpadToolError::InvalidArg(format!("Value at '{}' is not an array", args.path))
        })?;

        // Collect all keys, their types, and occurrence counts
        let mut key_info: std::collections::BTreeMap<String, KeyInfo> =
            std::collections::BTreeMap::new();

        for item in items {
            if let Some(obj) = item.as_object() {
                collect_keys(obj, "", &mut key_info);
            }
        }

        let total_items = items.len();
        let mut result = format!(
            "Item schema for $.{} ({} items):\n\n",
            args.path, total_items
        );
        result.push_str(&format!(
            "{:<40} {:<20} {}\n",
            "KEY", "TYPE(S)", "PRESENT IN"
        ));
        result.push_str(&format!("{}\n", "-".repeat(72)));

        for (key, info) in &key_info {
            let types: Vec<&str> = info.types.iter().map(|s| s.as_str()).collect();
            let types_str = types.join("|");
            let presence = format!("{}/{} items", info.count, total_items);
            result.push_str(&format!("{:<40} {:<20} {}\n", key, types_str, presence));
        }

        if let Err(exceeded) = check_and_record_budget(&self.budget, &result) {
            return Ok(json!({
                "error": "item_schema_too_large",
                "message": format!(
                    "The item schema is too large for your remaining context \
                     (estimated {} tokens, ~{} tokens remaining).",
                    exceeded.requested_tokens, exceeded.remaining_tokens
                ),
                "estimated_tokens": exceeded.requested_tokens,
                "remaining_budget_tokens": exceeded.remaining_tokens,
            })
            .to_string());
        }

        let meta = build_metadata(result.lines().count(), &result, &self.budget);
        Ok(format!(
            "{}\n--- scratchpad item_schema: $.{} | {} ---",
            result, args.path, meta
        ))
    }
}

/// Info collected about a key across array items.
struct KeyInfo {
    types: std::collections::BTreeSet<String>,
    count: usize,
}

/// Recursively collect keys from an object, tracking types and counts.
fn collect_keys(
    obj: &serde_json::Map<String, serde_json::Value>,
    prefix: &str,
    info: &mut std::collections::BTreeMap<String, KeyInfo>,
) {
    for (key, value) in obj {
        let full_key = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{}.{}", prefix, key)
        };

        let type_name = match value {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "bool",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        };

        let entry = info.entry(full_key.clone()).or_insert_with(|| KeyInfo {
            types: std::collections::BTreeSet::new(),
            count: 0,
        });
        entry.types.insert(type_name.to_string());
        entry.count += 1;

        // Recurse into nested objects
        if let serde_json::Value::Object(nested) = value {
            collect_keys(nested, &full_key, info);
        }
    }
}

// ============================================================================
// Shared path navigation helpers
// ============================================================================

/// Navigate a dot-separated path into a JSON value. Returns an error if any segment is not found.
fn navigate_path<'a>(
    root: &'a serde_json::Value,
    path: &str,
) -> Result<&'a serde_json::Value, ScratchpadToolError> {
    let mut current = root;
    for key in path.split('.') {
        current = if let Ok(index) = key.parse::<usize>() {
            current
                .get(index)
                .ok_or_else(|| ScratchpadToolError::KeyNotFound(path.to_string()))?
        } else {
            current
                .get(key)
                .ok_or_else(|| ScratchpadToolError::KeyNotFound(path.to_string()))?
        };
    }
    Ok(current)
}

/// Navigate a dot-separated path, returning None instead of error on missing keys.
fn navigate_path_value<'a>(
    root: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = root;
    for key in path.split('.') {
        current = if let Ok(index) = key.parse::<usize>() {
            current.get(index)?
        } else {
            current.get(key)?
        };
    }
    Some(current)
}

// ============================================================================
// read — Full file (escape hatch)
// ============================================================================

#[derive(Clone)]
pub struct ReadTool {
    storage: Arc<ScratchpadStorage>,
    budget: ContextBudget,
}

impl ReadTool {
    pub fn new(storage: Arc<ScratchpadStorage>, budget: ContextBudget) -> Self {
        Self { storage, budget }
    }
}

#[derive(Deserialize, Serialize)]
pub struct ReadArgs {
    pub file: String,
}

impl Tool for ReadTool {
    const NAME: &'static str = "read";
    type Error = ScratchpadToolError;
    type Args = ReadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read an entire scratchpad file. WARNING: may be large. Prefer \
                          head, slice, or grep for targeted reading."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename"
                    }
                },
                "required": ["file"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!("scratchpad read: file={}", args.file);
        let content = read_scratchpad_file(&self.storage, &args.file).await?;

        if let Err(exceeded) = check_and_record_budget(&self.budget, &content) {
            return Ok(json!({
                "error": "read_too_large",
                "message": format!(
                    "This file is too large for your remaining context \
                     (estimated {} tokens, ~{} tokens remaining). Narrow your request.",
                    exceeded.requested_tokens, exceeded.remaining_tokens
                ),
                "requested_file": args.file,
                "estimated_tokens": exceeded.requested_tokens,
                "remaining_budget_tokens": exceeded.remaining_tokens,
                "suggestions": [
                    "Use head to read just the beginning of the file: head(file, 50)",
                    "Use slice to read a specific range of lines: slice(file, 100, 150)",
                    "Use grep to find specific lines first",
                    "Use get_in to extract a specific key if it's structured data"
                ]
            })
            .to_string());
        }

        let numbered = add_line_numbers(&content);
        let meta = build_metadata(content.lines().count(), &content, &self.budget);
        Ok(format!(
            "{}\n\n--- scratchpad read: {} ({} lines) | {} ---",
            numbered,
            args.file,
            content.lines().count(),
            meta
        ))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, Arc<ScratchpadStorage>, ContextBudget) {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "test-req")
                .await
                .unwrap(),
        );
        let budget = ContextBudget::new(100_000, 0.20);
        (tmp, storage, budget)
    }

    fn sample_json() -> &'static str {
        r#"{
  "results": [
    {
      "id": 1,
      "title": "First Result",
      "metadata": {
        "score": 0.95,
        "source": "database"
      }
    },
    {
      "id": 2,
      "title": "Second Result",
      "metadata": {
        "score": 0.87,
        "source": "cache"
      }
    }
  ],
  "total": 2,
  "query": "test search"
}"#
    }

    #[tokio::test]
    async fn test_head() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = HeadTool::new(storage, budget);
        let result = tool
            .call(HeadArgs {
                file: "test.json".to_string(),
                lines: 5,
            })
            .await
            .unwrap();
        assert!(result.contains("results"));
        assert!(result.contains("head"));
    }

    #[tokio::test]
    async fn test_slice() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = SliceTool::new(storage, budget);
        let result = tool
            .call(SliceArgs {
                file: "test.json".to_string(),
                start: 3,
                end: 8,
            })
            .await
            .unwrap();
        assert!(result.contains("slice"));
        assert!(result.contains("id"));
    }

    #[tokio::test]
    async fn test_grep() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = GrepTool::new(storage, budget);
        let result = tool
            .call(GrepArgs {
                file: "test.json".to_string(),
                pattern: "score".to_string(),
                context: 1,
            })
            .await
            .unwrap();
        assert!(result.contains("score"));
        assert!(result.contains("grep"));
        assert!(result.contains("2 matches"));
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = GrepTool::new(storage, budget);
        let result = tool
            .call(GrepArgs {
                file: "test.json".to_string(),
                pattern: "nonexistent_xyz".to_string(),
                context: 1,
            })
            .await
            .unwrap();
        assert!(result.contains("No matches"));
    }

    #[tokio::test]
    async fn test_schema() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = SchemaTool::new(storage, budget);
        let result = tool
            .call(SchemaArgs {
                file: "test.json".to_string(),
                max_depth: 4,
            })
            .await
            .unwrap();
        assert!(result.contains("$.results"));
        assert!(result.contains("schema"));
    }

    #[tokio::test]
    async fn test_get_in() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = GetInTool::new(storage, budget);
        let result = tool
            .call(GetInArgs {
                file: "test.json".to_string(),
                path: "results.0.title".to_string(),
            })
            .await
            .unwrap();
        assert!(result.contains("First Result"));
    }

    #[tokio::test]
    async fn test_get_in_not_found() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = GetInTool::new(storage, budget);
        let result = tool
            .call(GetInArgs {
                file: "test.json".to_string(),
                path: "results.99.title".to_string(),
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_iterate_over() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = IterateOverTool::new(storage, budget);
        let result = tool
            .call(IterateOverArgs {
                file: "test.json".to_string(),
                path: "results".to_string(),
                fields: "id,title".to_string(),
            })
            .await
            .unwrap();
        assert!(result.contains("First Result"));
        assert!(result.contains("Second Result"));
        assert!(result.contains("iterate_over"));
        assert!(result.contains("2 items"));
    }

    #[tokio::test]
    async fn test_iterate_over_nested_fields() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = IterateOverTool::new(storage, budget);
        let result = tool
            .call(IterateOverArgs {
                file: "test.json".to_string(),
                path: "results".to_string(),
                fields: "id,metadata.score".to_string(),
            })
            .await
            .unwrap();
        assert!(result.contains("0.95"));
        assert!(result.contains("0.87"));
    }

    #[tokio::test]
    async fn test_iterate_over_not_array() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = IterateOverTool::new(storage, budget);
        let result = tool
            .call(IterateOverArgs {
                file: "test.json".to_string(),
                path: "total".to_string(),
                fields: "id".to_string(),
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_item_schema() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = ItemSchemaTool::new(storage, budget);
        let result = tool
            .call(ItemSchemaArgs {
                file: "test.json".to_string(),
                path: "results".to_string(),
            })
            .await
            .unwrap();
        assert!(result.contains("id"));
        assert!(result.contains("title"));
        assert!(result.contains("metadata"));
        assert!(result.contains("metadata.score"));
        assert!(result.contains("metadata.source"));
        assert!(result.contains("2/2 items"));
    }

    #[tokio::test]
    async fn test_item_schema_heterogeneous() {
        let (_tmp, storage, budget) = setup().await;
        let json = r#"{
  "items": [
    {"id": 1, "name": "alpha"},
    {"id": 2, "tags": ["a", "b"]},
    {"id": 3, "name": "gamma", "extra": true}
  ]
}"#;
        storage.write_output("hetero", json).await.unwrap();

        let tool = ItemSchemaTool::new(storage, budget);
        let result = tool
            .call(ItemSchemaArgs {
                file: "hetero.json".to_string(),
                path: "items".to_string(),
            })
            .await
            .unwrap();
        assert!(result.contains("id"));
        assert!(result.contains("3/3 items")); // id present in all
        assert!(result.contains("name"));
        assert!(result.contains("2/3 items")); // name present in 2 of 3
        assert!(result.contains("tags"));
        assert!(result.contains("1/3 items")); // tags present in 1 of 3
        assert!(result.contains("extra"));
    }

    #[tokio::test]
    async fn test_read() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = ReadTool::new(storage, budget);
        let result = tool
            .call(ReadArgs {
                file: "test.json".to_string(),
            })
            .await
            .unwrap();
        assert!(result.contains("results"));
        assert!(result.contains("read"));
    }

    #[tokio::test]
    async fn test_budget_exceeded() {
        let (_tmp, storage, _budget) = setup().await;
        // Create a small budget that will be exceeded
        let tiny_budget = ContextBudget::new(100, 0.20); // 80 tokens usable

        // Write a large file
        let large_content = "x".repeat(1000);
        storage.write_output("large", &large_content).await.unwrap();

        let tool = ReadTool::new(storage, tiny_budget);
        let result = tool
            .call(ReadArgs {
                file: "large.txt".to_string(),
            })
            .await
            .unwrap();
        // Budget exceeded now returns Ok with JSON error instead of Err
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["error"], "read_too_large");
        assert!(parsed["estimated_tokens"].as_u64().unwrap() > 0);
        assert!(
            parsed["remaining_budget_tokens"].as_u64().unwrap()
                < parsed["estimated_tokens"].as_u64().unwrap()
        );
        assert!(parsed["suggestions"].as_array().unwrap().len() >= 3);
    }

    #[tokio::test]
    async fn test_path_traversal_rejected() {
        let (_tmp, storage, budget) = setup().await;
        let tool = HeadTool::new(storage, budget);
        let result = tool
            .call(HeadArgs {
                file: "../../etc/passwd".to_string(),
                lines: 10,
            })
            .await;
        assert!(result.is_err());
    }
}
