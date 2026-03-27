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

pub use context_budget::ContextBudget;
pub use storage::ScratchpadStorage;
pub use tools::{
    GetInTool, GrepTool, HeadTool, ItemSchemaTool, IterateOverTool, ReadTool, SchemaTool, SliceTool,
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
    /// Map of tool name → min_bytes threshold for scratchpad interception.
    pub scratchpad_tools: HashMap<String, usize>,
}

/// Scratchpad usage instructions appended to worker preambles.
pub const SCRATCHPAD_PREAMBLE: &str = r#"
## Scratchpad Tools

Some tool outputs are too large for the context window and have been saved to scratchpad files.
When you see a `[scratchpad: ...]` message instead of direct output, use these tools to explore:

1. **schema** — See the JSON structure (keys, types, line ranges). Start here.
2. **item_schema** — See all unique keys across items in a JSON array (e.g., `item_schema(file, 'results')`).
3. **head** — Preview the first N lines.
4. **grep** — Search for specific content with regex.
5. **get_in** — Extract a value at a nested JSON path (e.g., `results.0.title`).
6. **iterate_over** — Extract selected fields from every item in a JSON array (e.g., `iterate_over(file, 'results', 'id,title')`).
7. **slice** — Extract a specific line range.
8. **read** — Read the entire file (WARNING: may be large, prefer targeted tools).

**Strategy**: Use `schema` first to understand structure. For arrays, use `item_schema` to discover fields, then `iterate_over` to extract them. Use `get_in` or `grep` for targeted lookups. Avoid `read` unless the file is small.
"#;
