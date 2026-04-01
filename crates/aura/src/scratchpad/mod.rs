//! Scratchpad module — intercepts large MCP tool outputs and provides
//! read-only tools for the LLM to selectively explore the data.
//!
//! # Architecture
//!
//! 1. **ScratchpadWrapper** (ToolWrapper) — intercepts flagged tool outputs
//!    exceeding a size threshold, writes them to disk, returns a summary pointer.
//! 2. **Scratchpad tools** — eight Rig native tools that read scratchpad files:
//!    `head`, `slice`, `grep`, `schema`, `get_in`, `iterate_over`, `item_schema`, `read`.
//! 3. **ContextBudget** — tracks estimated token usage to prevent overflow.
//! 4. **ScratchpadStorage** — file I/O with path validation and cleanup.

pub mod context_budget;
pub mod schema;
pub mod storage;
pub mod tools;
pub mod wrapper;

pub use context_budget::{
    ContextBudget, ExtractionLimitExceeded, TiktokenCounter, TokenCounter,
    token_counter_for_provider,
};
pub use storage::ScratchpadStorage;
pub use tools::{
    GetInTool, GrepTool, HeadTool, ItemSchemaTool, IterateOverTool, ReadTool, SchemaTool,
    SliceTool, all_tool_definitions,
};
pub use wrapper::ScratchpadWrapper;

use std::collections::HashMap;
use std::sync::Arc;

/// Runtime configuration for scratchpad tools, passed via AgentConfig extension field.
#[derive(Debug, Clone)]
pub struct ScratchpadToolsConfig {
    /// Shared storage for this request's scratchpad files.
    pub storage: Arc<ScratchpadStorage>,
    /// Context budget tracker shared across all scratchpad tools.
    pub budget: ContextBudget,
    /// Map of tool name → min_tokens threshold for scratchpad interception.
    pub scratchpad_tools: HashMap<String, usize>,
}

/// Scratchpad usage instructions appended to worker preambles.
pub const SCRATCHPAD_PREAMBLE: &str = r#"
## Scratchpad Tools

Some tool outputs are too large for the context window and have been saved to scratchpad files.
When you see a `[scratchpad: ...]` message instead of direct output, use these tools to explore:

1. **schema** — See the structure with line ranges. Works on JSON (keys, types) and Markdown (sections, keys). Start here.
2. **item_schema** — See all unique keys across items in a JSON array (e.g., `item_schema(file, 'results')`).
3. **head** — Preview the first N lines.
4. **grep** — Search for specific content with regex.
5. **get_in** — Extract a value at a nested JSON path (e.g., `results.0.title`). For large string values, use `offset` and `limit` to paginate by line.
6. **iterate_over** — Extract selected fields from every item in a JSON array (e.g., `iterate_over(file, 'results', 'id,title')`).
7. **slice** — Extract a specific line range.
8. **read** — Read the entire file (WARNING: may be large, prefer targeted tools).

**Companion files**: Large structured string values inside JSON (escaped JSON → `.json`, markdown → `.md`) are automatically extracted to companion files. Use `schema` on the companion file to see its structure, then `slice` or `grep` to explore specific sections.

**Strategy**: Use `schema` first to understand structure. For JSON arrays, use `item_schema` to discover fields, then `iterate_over` to extract them. For companion `.md` files, use `schema` to see sections, then `slice` to extract a specific section by line range. Use `get_in` or `grep` for targeted lookups. Avoid `read` unless the file is small.
"#;
