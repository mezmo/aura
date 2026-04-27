//! Scratchpad exploration tools.
//!
//! Tools that let the LLM selectively explore large tool outputs stored on disk, rather
//! than loading everything into context.

use super::context_budget::ContextBudget;
use super::schema::{
    analyze_json_structure, analyze_markdown_structure, format_markdown_schema, format_schema,
};
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

/// Max lines of a string value to include inline in `iterate_over` results.
/// Strings longer than this are truncated with a preview and a `get_in` hint.
const ITERATE_OVER_STRING_PREVIEW_LINES: usize = 5;

/// Hard cap on the length of a regex pattern accepted by `grep`. Catastrophic
/// regex growth is most likely to come from a pathologically long or nested
/// pattern — this cap stops them at the door.
const GREP_MAX_PATTERN_LEN: usize = 1024;

/// Hard cap on the token count of `grep` results. Stops the scan early so a
/// pathological pattern on a huge file can't force us to allocate unbounded
/// output before `check_and_record_budget` rejects it. Sized with ~3× headroom
/// over the default `max_extraction_tokens` (10k), so the extraction-budget
/// check normally triggers first on typical configs and this cap only kicks
/// in on truly pathological cases.
const GREP_MAX_OUTPUT_TOKENS: usize = 32_768;

/// Truncate a large string value for `iterate_over`, keeping a preview of the
/// first few lines and appending a hint to use `get_in` for the full content.
/// Non-string values are returned as-is.
fn truncate_large_string(
    val: &serde_json::Value,
    max_lines: usize,
    array_path: &str,
    item_index: usize,
    field: &str,
) -> serde_json::Value {
    let Some(s) = val.as_str() else {
        return val.clone();
    };
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return val.clone();
    }
    let preview = lines[..max_lines].join("\n");
    let hint = format!(
        "{preview}\n...[truncated, {} total lines — use get_in(file, '{array_path}.{item_index}.{field}') for full content]",
        lines.len(),
    );
    serde_json::Value::String(hint)
}

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
        "estimated_tokens": budget.count_tokens(content),
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

/// Format a BudgetCheckError into a JSON string suitable for tool output.
fn format_budget_error(
    error: BudgetCheckError,
    error_code: &str,
    extra_fields: serde_json::Value,
) -> String {
    let mut obj = match error {
        BudgetCheckError::ExtractionLimit(e) => json!({
            "error": error_code,
            "message": e.to_string(),
            "estimated_tokens": e.estimated_tokens,
            "per_call_limit": e.limit,
        }),
        BudgetCheckError::BudgetExceeded(e) => json!({
            "error": error_code,
            "message": format!(
                "Too large for remaining context (~{} tokens requested, ~{} remaining). Narrow your request.",
                e.requested_tokens, e.remaining_tokens
            ),
            "estimated_tokens": e.requested_tokens,
            "remaining_budget_tokens": e.remaining_tokens,
        }),
    };
    // Merge extra fields
    if let (Some(obj_map), Some(extra_map)) = (obj.as_object_mut(), extra_fields.as_object()) {
        for (k, v) in extra_map {
            obj_map.insert(k.clone(), v.clone());
        }
    }
    obj.to_string()
}

/// Result of a budget check — either Ok or one of two exceeded variants.
enum BudgetCheckError {
    ExtractionLimit(super::context_budget::ExtractionLimitExceeded),
    BudgetExceeded(super::context_budget::BudgetExceeded),
}

/// Check budget and record usage. Returns `Ok(())` if within budget,
/// or `Err` with details for the caller to format.
fn check_and_record_budget(budget: &ContextBudget, content: &str) -> Result<(), BudgetCheckError> {
    // Per-call extraction limit check first
    if let Some(limit) = budget.max_extraction_tokens() {
        let tokens = budget.count_tokens(content);
        if tokens > limit {
            return Err(BudgetCheckError::ExtractionLimit(
                super::context_budget::ExtractionLimitExceeded {
                    estimated_tokens: tokens,
                    limit,
                },
            ));
        }
    }
    // Atomic cumulative budget check + record (no TOCTOU race)
    match budget.try_record_usage(content) {
        Ok(tokens) => {
            budget.record_extracted(tokens);
            Ok(())
        }
        Err(e) => Err(BudgetCheckError::BudgetExceeded(e)),
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

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "head".to_string(),
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
        // Delegates to static method so callers can get the definition without a tool instance.
        Self::tool_definition()
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

        if let Err(e) = check_and_record_budget(&self.budget, &selected) {
            return Ok(format_budget_error(
                e,
                "head_too_large",
                json!({
                    "requested_lines": args.lines,
                    "suggestions": [
                        format!("Try a smaller number of lines: head(file, {})", args.lines / 2),
                        "Use grep to find specific lines first",
                        "Use get_in to extract a specific key if it's structured data"
                    ]
                }),
            ));
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

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "slice".to_string(),
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
        // Delegates to static method so callers can get the definition without a tool instance.
        Self::tool_definition()
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

        if let Err(e) = check_and_record_budget(&self.budget, &selected) {
            let suggested_end = args.start + (args.end - args.start) / 2;
            return Ok(format_budget_error(
                e,
                "slice_too_large",
                json!({
                    "requested_lines": [args.start, args.end],
                    "suggestions": [
                        format!("Try a smaller range: slice(file, {}, {})", args.start, suggested_end),
                        "Use grep to find specific lines first",
                        "Use get_in to extract a specific key if it's structured data"
                    ]
                }),
            ));
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

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
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
        // Delegates to static method so callers can get the definition without a tool instance.
        Self::tool_definition()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!(
            "scratchpad grep: file={}, pattern={}, context={}",
            args.file,
            args.pattern,
            args.context
        );
        if args.pattern.len() > GREP_MAX_PATTERN_LEN {
            return Err(ScratchpadToolError::InvalidArg(format!(
                "Regex pattern too long ({} bytes, max {}). Use a shorter/more specific pattern.",
                args.pattern.len(),
                GREP_MAX_PATTERN_LEN
            )));
        }
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

        // Merge overlapping ranges, then build output while watching the
        // token cap so a pathological pattern can't force us to allocate
        // megabytes before the budget check would have rejected it.
        let merged = merge_ranges(&match_ranges);
        let mut output_parts: Vec<String> = Vec::new();
        let mut match_count = 0;
        let mut accumulated_tokens = 0;
        let mut truncated = false;

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
            // Token-count per section and accumulate (O(output_size) total)
            // rather than re-tokenizing the growing result each iteration.
            let section_tokens = self.budget.count_tokens(&section);
            if accumulated_tokens + section_tokens > GREP_MAX_OUTPUT_TOKENS {
                truncated = true;
                break;
            }
            accumulated_tokens += section_tokens;
            output_parts.push(section);
        }

        let mut result = output_parts.join("\n---\n\n");
        if truncated {
            result.push_str(&format!(
                "\n---\n[grep: output truncated at ~{} tokens. Narrow the pattern or use head/slice to explore a smaller window.]\n",
                GREP_MAX_OUTPUT_TOKENS
            ));
        }

        if let Err(e) = check_and_record_budget(&self.budget, &result) {
            return Ok(format_budget_error(
                e,
                "grep_too_large",
                json!({
                    "requested_pattern": args.pattern,
                    "suggestions": [
                        format!("Try a more specific pattern: grep(file, '{}')", args.pattern),
                        "Use head or slice to limit the number of lines first",
                        "Use get_in to extract a specific key if it's structured data"
                    ]
                }),
            ));
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

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "schema".to_string(),
            description: "Show the structure of a scratchpad file with line ranges. \
                          Works on JSON (keys, types, arrays) and Markdown (sections, keys). \
                          Helps you decide which parts to extract with slice or get_in."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad filename (.json or .md)"
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
        // Delegates to static method so callers can get the definition without a tool instance.
        Self::tool_definition()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!(
            "scratchpad schema: file={}, max_depth={}",
            args.file,
            args.max_depth
        );
        let content = read_scratchpad_file(&self.storage, &args.file).await?;

        // Dispatch based on file extension
        let is_markdown = args.file.ends_with(".md");
        let schema = if is_markdown {
            let sections = analyze_markdown_structure(&content).ok_or_else(|| {
                ScratchpadToolError::InvalidArg(
                    "File has no markdown headers (expected # or ## or ### sections)".to_string(),
                )
            })?;
            format_markdown_schema(&sections, args.max_depth)
        } else {
            let node = analyze_json_structure(&content).ok_or(ScratchpadToolError::NotJson)?;
            format_schema(&node, args.max_depth)
        };

        if let Err(e) = check_and_record_budget(&self.budget, &schema) {
            let suggestions = if is_markdown {
                json!([
                    format!(
                        "Try a smaller depth: schema(file, max_depth={})",
                        (args.max_depth / 2).max(1)
                    ),
                    "Use head to preview the first sections",
                    "Use grep to search for a specific section header (e.g., grep(file, '### Groups'))",
                    "Use slice to extract a section by its line range"
                ])
            } else {
                json!([
                    format!(
                        "Try a smaller depth: schema(file, max_depth={})",
                        (args.max_depth / 2).max(1)
                    ),
                    "Use get_in to explore a specific JSON subtree",
                    "Use item_schema to see keys across array items",
                    "Use head to preview the first few lines"
                ])
            };
            return Ok(format_budget_error(
                e,
                "schema_too_large",
                json!({
                    "requested_file": args.file,
                    "format": if is_markdown { "markdown" } else { "json" },
                    "suggestions": suggestions,
                }),
            ));
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

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "get_in".to_string(),
            description: "Extract a value at a nested key path from a JSON scratchpad file. \
                          Use dot notation: 'results.0.metadata.title'. For large string values \
                          (e.g. embedded markdown), use offset/limit to paginate by line."
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
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line offset (0-indexed) for paginating large string values"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max lines to return when paginating a large string value"
                    }
                },
                "required": ["file", "path"],
                "additionalProperties": false
            }),
        }
    }

    /// Return a paginated slice of a string value's lines.
    fn get_in_paginated(
        &self,
        path: &str,
        lines: &[&str],
        total_lines: usize,
        offset: usize,
        limit: usize,
    ) -> Result<String, ScratchpadToolError> {
        if offset >= total_lines {
            return Ok(json!({
                "error": "offset_out_of_range",
                "message": format!("Offset {} exceeds total lines ({})", offset, total_lines),
                "total_lines": total_lines,
            })
            .to_string());
        }

        let end = total_lines.min(offset + limit);
        let chunk: String = lines[offset..end].join("\n");

        if let Err(e) = check_and_record_budget(&self.budget, &chunk) {
            return Ok(format_budget_error(
                e,
                "get_in_too_large",
                json!({
                    "requested_path": path,
                    "total_lines": total_lines,
                    "offset": offset,
                    "limit": limit,
                    "suggestions": ["Reduce the limit parameter to read fewer lines"]
                }),
            ));
        }

        let numbered = add_line_numbers(&chunk);
        let meta = build_metadata(end - offset, &chunk, &self.budget);
        Ok(format!(
            "{}\n\n--- scratchpad get_in: $.{} (string, lines {}-{} of {}) | {} ---",
            numbered,
            path,
            offset + 1,
            end,
            total_lines,
            meta
        ))
    }
}

#[derive(Deserialize, Serialize)]
pub struct GetInArgs {
    pub file: String,
    /// Dot-separated key path, e.g. "results.0.metadata" or "data.items".
    pub path: String,
    /// Line offset (0-indexed) for paginating large string values.
    /// When the value at `path` is a string with embedded newlines, use
    /// offset/limit to read a chunk instead of the entire value.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to return when paginating a large string value.
    #[serde(default)]
    pub limit: Option<usize>,
}

impl Tool for GetInTool {
    const NAME: &'static str = "get_in";
    type Error = ScratchpadToolError;
    type Args = GetInArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        // Delegates to static method so callers can get the definition without a tool instance.
        Self::tool_definition()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!("scratchpad get_in: file={}, path={}", args.file, args.path);
        let content = read_scratchpad_file(&self.storage, &args.file).await?;
        let root: serde_json::Value =
            serde_json::from_str(&content).map_err(|_| ScratchpadToolError::NotJson)?;

        let current = navigate_path(&root, &args.path)?;

        // Non-string values: pretty-print, check budget, return
        let Some(raw_str) = current.as_str() else {
            let result =
                serde_json::to_string_pretty(current).unwrap_or_else(|_| current.to_string());

            if let Err(e) = check_and_record_budget(&self.budget, &result) {
                return Ok(format_budget_error(
                    e,
                    "get_in_too_large",
                    json!({
                        "requested_path": args.path,
                        "suggestions": [
                            format!("Try a deeper path: get_in(file, '{}.0')", args.path),
                            "Use schema to see the structure and pick a smaller subtree",
                            "Use grep to find specific content within the value"
                        ]
                    }),
                ));
            }

            let numbered = add_line_numbers(&result);
            let meta = build_metadata(result.lines().count(), &result, &self.budget);
            return Ok(format!(
                "{}\n\n--- scratchpad get_in: $.{} | {} ---",
                numbered, args.path, meta
            ));
        };

        // String values: use raw content so embedded newlines become real lines.
        // Supports offset/limit pagination for large values.
        let lines: Vec<&str> = raw_str.lines().collect();
        let total_lines = lines.len();

        if args.offset.is_some() || args.limit.is_some() {
            return self.get_in_paginated(
                &args.path,
                &lines,
                total_lines,
                args.offset.unwrap_or(0),
                args.limit.unwrap_or(100),
            );
        }

        // No pagination — return full string if it fits in budget
        if let Err(e) = check_and_record_budget(&self.budget, raw_str) {
            return Ok(format_budget_error(
                e,
                "get_in_too_large",
                json!({
                    "requested_path": args.path,
                    "value_type": "string",
                    "total_lines": total_lines,
                    "suggestions": [
                        format!("This is a large string value ({total_lines} lines). Use offset/limit to paginate:"),
                        format!("  get_in file=\"{}\" path=\"{}\" offset=0 limit=100", args.file, args.path),
                        "Use grep to search for specific content within the file"
                    ]
                }),
            ));
        }

        let numbered = add_line_numbers(raw_str);
        let meta = build_metadata(total_lines, raw_str, &self.budget);
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

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "iterate_over".to_string(),
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
        // Delegates to static method so callers can get the definition without a tool instance.
        Self::tool_definition()
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
                let val = truncate_large_string(
                    val,
                    ITERATE_OVER_STRING_PREVIEW_LINES,
                    &args.path,
                    i,
                    field,
                );
                row.insert(field.to_string(), val);
            }
            rows.push(serde_json::Value::Object(row));
        }

        let result = serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string());

        if let Err(e) = check_and_record_budget(&self.budget, &result) {
            return Ok(format_budget_error(
                e,
                "iterate_over_too_large",
                json!({
                    "item_count": items.len(),
                    "requested_fields": field_names,
                    "suggestions": [
                        "Request fewer fields",
                        "Use get_in to access specific items by index (e.g., 'results.0.title')",
                        "Use grep to find specific items first"
                    ]
                }),
            ));
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

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "item_schema".to_string(),
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
        // Delegates to static method so callers can get the definition without a tool instance.
        Self::tool_definition()
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

        if let Err(e) = check_and_record_budget(&self.budget, &result) {
            return Ok(format_budget_error(e, "item_schema_too_large", json!({})));
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

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "read".to_string(),
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
        // Delegates to static method so callers can get the definition without a tool instance.
        Self::tool_definition()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!("scratchpad read: file={}", args.file);

        // Pre-flight size check: skip loading a multi-MB file into memory
        // just to reject it on budget. Conservatively assume ~3 bytes/token
        // (BPE tokenizers average 3–5 on English/JSON), so file_bytes/3 is an
        // upper bound on token count. If that alone exceeds the per-call
        // limit, bail out before reading.
        if let Some(limit) = self.budget.max_extraction_tokens()
            && let Ok(path) = self.storage.validate_path(&args.file)
            && let Ok(meta) = tokio::fs::metadata(&path).await
        {
            let approx_tokens = (meta.len() as usize) / 3;
            if approx_tokens > limit {
                return Ok(format_budget_error(
                    BudgetCheckError::ExtractionLimit(
                        super::context_budget::ExtractionLimitExceeded {
                            estimated_tokens: approx_tokens,
                            limit,
                        },
                    ),
                    "read_too_large",
                    json!({
                        "requested_file": args.file,
                        "file_bytes": meta.len(),
                        "suggestions": [
                            "Use head to read just the beginning of the file: head(file, 50)",
                            "Use slice to read a specific range of lines: slice(file, 100, 150)",
                            "Use grep to find specific lines first",
                            "Use get_in to extract a specific key if it's structured data"
                        ]
                    }),
                ));
            }
        }

        let content = read_scratchpad_file(&self.storage, &args.file).await?;

        if let Err(e) = check_and_record_budget(&self.budget, &content) {
            return Ok(format_budget_error(
                e,
                "read_too_large",
                json!({
                    "requested_file": args.file,
                    "suggestions": [
                        "Use head to read just the beginning of the file: head(file, 50)",
                        "Use slice to read a specific range of lines: slice(file, 100, 150)",
                        "Use grep to find specific lines first",
                        "Use get_in to extract a specific key if it's structured data"
                    ]
                }),
            ));
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

/// All scratchpad tool definitions, for token counting without constructing tool instances.
pub fn all_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        HeadTool::tool_definition(),
        SliceTool::tool_definition(),
        GrepTool::tool_definition(),
        SchemaTool::tool_definition(),
        ItemSchemaTool::tool_definition(),
        GetInTool::tool_definition(),
        IterateOverTool::tool_definition(),
        ReadTool::tool_definition(),
    ]
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
        let counter = crate::scratchpad::context_budget::TiktokenCounter::default_counter();
        let budget = ContextBudget::new(100_000, 0.20, 0, std::sync::Arc::new(counter));
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
    async fn test_grep_rejects_overly_long_pattern() {
        let (_tmp, storage, budget) = setup().await;
        storage.write_output("test", sample_json()).await.unwrap();

        let tool = GrepTool::new(storage, budget);
        let long_pattern = "a".repeat(GREP_MAX_PATTERN_LEN + 1);
        let err = tool
            .call(GrepArgs {
                file: "test.json".to_string(),
                pattern: long_pattern,
                context: 1,
            })
            .await
            .expect_err("patterns longer than GREP_MAX_PATTERN_LEN must be rejected");
        assert!(
            matches!(err, ScratchpadToolError::InvalidArg(ref msg) if msg.contains("too long")),
            "error should surface the length violation: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_grep_truncates_huge_output() {
        let (_tmp, storage, budget) = setup().await;
        // Build a file where every line matches, then run a regex that matches
        // every line. Output tokens should exceed both the per-call extraction
        // budget and the grep-specific token cap.
        let huge_content = (0..100_000).map(|_| "aa").collect::<Vec<_>>().join("\n");
        storage.write_output("huge", &huge_content).await.unwrap();

        let tool = GrepTool::new(storage, budget);
        let result = tool
            .call(GrepArgs {
                file: "huge.txt".to_string(),
                pattern: "a".to_string(),
                context: 0,
            })
            .await
            .unwrap();
        // Either the grep-specific token cap truncated mid-scan, or the
        // extraction budget rejected the assembled output — either signals
        // the guardrail did its job.
        let saw_truncation = result.contains("output truncated");
        let saw_budget_error = result.contains("\"error\":\"grep_too_large\"");
        assert!(
            saw_truncation || saw_budget_error,
            "grep output should be capped or rejected, got first 200 chars: {}",
            &result[..result.len().min(200)]
        );
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
                offset: None,
                limit: None,
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
                offset: None,
                limit: None,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_in_string_pagination() {
        let (_tmp, storage, budget) = setup().await;
        // JSON with a large multi-line string value
        let json = r#"{"kv_markdown": "line1\nline2\nline3\nline4\nline5"}"#;
        storage.write_output("md", json).await.unwrap();

        let tool = GetInTool::new(storage, budget);

        // Without pagination, should return the raw string content
        let result = tool
            .call(GetInArgs {
                file: "md.json".to_string(),
                path: "kv_markdown".to_string(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        assert!(result.contains("line1"));
        assert!(result.contains("line5"));

        // With pagination, should return a slice
        let result = tool
            .call(GetInArgs {
                file: "md.json".to_string(),
                path: "kv_markdown".to_string(),
                offset: Some(1),
                limit: Some(2),
            })
            .await
            .unwrap();
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(!result.contains("line1"));
        assert!(!result.contains("line4"));
        assert!(result.contains("lines 2-3 of 5"));
    }

    #[tokio::test]
    async fn test_get_in_string_pagination_offset_out_of_range() {
        let (_tmp, storage, budget) = setup().await;
        let json = r#"{"data": "a\nb\nc"}"#;
        storage.write_output("small", json).await.unwrap();

        let tool = GetInTool::new(storage, budget);
        let result = tool
            .call(GetInArgs {
                file: "small.json".to_string(),
                path: "data".to_string(),
                offset: Some(100),
                limit: Some(10),
            })
            .await
            .unwrap();
        assert!(result.contains("offset_out_of_range"));
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
    async fn test_iterate_over_truncates_large_strings() {
        let (_tmp, storage, budget) = setup().await;
        // JSON array where each item has a large string field
        let big_str = (0..20)
            .map(|i| format!("### Section {i}\n- key: value"))
            .collect::<Vec<_>>()
            .join("\n");
        let json = serde_json::json!({
            "items": [
                { "id": 1, "content": big_str },
                { "id": 2, "content": "short value" },
            ]
        });
        storage
            .write_output("trunc", &json.to_string())
            .await
            .unwrap();

        let tool = IterateOverTool::new(storage, budget);
        let result = tool
            .call(IterateOverArgs {
                file: "trunc.json".to_string(),
                path: "items".to_string(),
                fields: "id,content".to_string(),
            })
            .await
            .unwrap();

        // Item 0's content should be truncated with a preview + get_in hint
        assert!(result.contains("truncated"));
        assert!(result.contains("get_in"));
        assert!(result.contains("items.0.content"));
        // Should show the first few lines as preview
        assert!(result.contains("### Section 0"));
        // Item 1's short content should be intact
        assert!(result.contains("short value"));
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
        let counter = crate::scratchpad::context_budget::TiktokenCounter::default_counter();
        let tiny_budget = ContextBudget::new(100, 0.20, 0, std::sync::Arc::new(counter)); // 80 tokens usable

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
        assert!(parsed["message"].as_str().unwrap().contains("remaining"));
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

    #[tokio::test]
    async fn test_schema_markdown_file() {
        let (_tmp, storage, budget) = setup().await;
        // Write a markdown file directly (simulating a companion file)
        let md = "### Summary\n- total: 5\n- status: ok\n\n### Details\n- item: 1\n  - name: foo\n- item: 2\n  - name: bar\n";
        let path = storage.dir().join("test.md");
        tokio::fs::write(&path, md).await.unwrap();

        let tool = SchemaTool::new(storage, budget);
        let result = tool
            .call(SchemaArgs {
                file: "test.md".to_string(),
                max_depth: 4,
            })
            .await
            .unwrap();
        assert!(result.contains("Summary"));
        assert!(result.contains("Details"));
        assert!(result.contains("total, status"));
        assert!(result.contains("[L"));
    }

    #[tokio::test]
    async fn test_schema_companion_json_file() {
        let (_tmp, storage, budget) = setup().await;
        // Simulate a companion .json file extracted from an escaped JSON string
        let inner = serde_json::json!({
            "data": [
                {"id": 1, "value": "alpha"},
                {"id": 2, "value": "beta"},
            ],
            "count": 2,
        });
        let pretty = serde_json::to_string_pretty(&inner).unwrap();
        let path = storage.dir().join("test.payload.json");
        tokio::fs::write(&path, &pretty).await.unwrap();

        let tool = SchemaTool::new(storage, budget);
        let result = tool
            .call(SchemaArgs {
                file: "test.payload.json".to_string(),
                max_depth: 4,
            })
            .await
            .unwrap();
        assert!(result.contains("$.data"));
        assert!(result.contains("$.count"));
        assert!(result.contains("array(2 items)"));
    }

    #[tokio::test]
    async fn test_get_in_raw_string_not_json_escaped() {
        let (_tmp, storage, budget) = setup().await;
        let json = serde_json::json!({"msg": "line one\nline two\nline three"});
        storage
            .write_output("raw", &json.to_string())
            .await
            .unwrap();

        let tool = GetInTool::new(storage, budget);
        let result = tool
            .call(GetInArgs {
                file: "raw.json".to_string(),
                path: "msg".to_string(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        // Raw string content, not JSON-quoted with escape sequences
        assert!(result.contains("line one"));
        assert!(result.contains("line two"));
        assert!(!result.contains(r#"\n"#)); // should NOT have escaped newlines
    }

    #[tokio::test]
    async fn test_get_in_pagination_budget_exceeded() {
        let (_tmp, storage, _budget) = setup().await;
        // Tiny budget with per-call extraction limit
        let counter = crate::scratchpad::context_budget::TiktokenCounter::default_counter();
        let tiny_budget = ContextBudget::new(1000, 0.20, 0, std::sync::Arc::new(counter))
            .with_max_extraction_tokens(5); // very low per-call limit

        // Write JSON with a large string value
        let big_str = (0..100)
            .map(|i| format!("line {i} with some content"))
            .collect::<Vec<_>>()
            .join("\n");
        let json = serde_json::json!({"content": big_str});
        storage
            .write_output("bigstr", &json.to_string())
            .await
            .unwrap();

        let tool = GetInTool::new(storage, tiny_budget);
        // With offset/limit, the chunk should still exceed the tiny per-call limit
        let result = tool
            .call(GetInArgs {
                file: "bigstr.json".to_string(),
                path: "content".to_string(),
                offset: Some(0),
                limit: Some(50),
            })
            .await
            .unwrap();
        // Should get a budget error, not a panic
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["error"], "get_in_too_large");
        assert!(parsed["suggestions"].is_array());
    }

    #[test]
    fn test_all_tool_definitions_count() {
        let defs = all_tool_definitions();
        assert_eq!(defs.len(), 8, "expected 8 scratchpad tool definitions");
    }

    #[tokio::test]
    async fn test_tool_definition_matches_trait_definition() {
        let (_tmp, storage, budget) = setup().await;

        let head = HeadTool::new(storage.clone(), budget.clone());
        assert_eq!(
            HeadTool::tool_definition(),
            head.definition(String::new()).await
        );

        let read = ReadTool::new(storage, budget);
        assert_eq!(
            ReadTool::tool_definition(),
            read.definition(String::new()).await
        );
    }
}
