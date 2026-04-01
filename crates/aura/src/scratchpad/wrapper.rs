//! ScratchpadWrapper — intercepts large MCP tool outputs and writes them
//! to the scratchpad directory, returning a summary pointer to the LLM.

use super::context_budget::ContextBudget;
use super::storage::ScratchpadStorage;
use crate::tool_wrapper::{ToolCallContext, ToolWrapper, TransformOutputResult};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// ToolWrapper that intercepts large outputs from flagged tools and writes
/// them to scratchpad files, replacing the output with a compact pointer.
pub struct ScratchpadWrapper {
    /// Map of tool name → min_tokens threshold for scratchpad interception.
    scratchpad_tools: HashMap<String, usize>,
    /// Storage backend for writing scratchpad files.
    storage: Arc<ScratchpadStorage>,
    /// Budget tracker for recording intercepted tokens.
    budget: ContextBudget,
}

impl ScratchpadWrapper {
    pub fn new(
        scratchpad_tools: HashMap<String, usize>,
        storage: Arc<ScratchpadStorage>,
        budget: ContextBudget,
    ) -> Self {
        Self {
            scratchpad_tools,
            storage,
            budget,
        }
    }
}

#[async_trait]
impl ToolWrapper for ScratchpadWrapper {
    fn transform_output(
        &self,
        output: String,
        ctx: &ToolCallContext,
        _extracted: Option<&serde_json::Value>,
    ) -> TransformOutputResult {
        // Only intercept tools in the scratchpad map
        let min_tokens = match self.scratchpad_tools.get(&ctx.tool_name) {
            Some(&mt) => mt,
            None => return TransformOutputResult::new(output),
        };

        // Only intercept outputs exceeding the per-tool token threshold
        let output_tokens = self.budget.count_tokens(&output);
        if output_tokens < min_tokens {
            tracing::debug!(
                "Scratchpad: {} output (~{} tokens) below threshold ({}), passing through",
                ctx.tool_name,
                output_tokens,
                min_tokens
            );
            return TransformOutputResult::new(output);
        }

        // Generate a unique file ID from context fields + random suffix
        let file_id = format!(
            "task_{}-{}-{}-{}-{}",
            ctx.task_id.unwrap_or(0),
            ctx.tool_initiator_id,
            ctx.tool_name,
            ctx.attempt.unwrap_or(0),
            &uuid::Uuid::new_v4().to_string()[..8],
        )
        .replace(['/', '\\', ':', ' '], "_");

        match self.storage.write_output_sync(&file_id, &output) {
            Ok(result) => {
                let filename = result
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| file_id.clone());
                let line_count = result.line_count;
                let format = result.format;

                let token_count = output_tokens; // counted on raw content above

                let mut pointer = format!(
                    "[scratchpad: output saved to '{filename}' (~{token_count} tokens, \
                     {line_count} lines, format={fmt})]\n\n\
                     The full output is too large for the context window. \
                     Use these tools to explore it:\n\
                     - schema file=\"{filename}\" — view structure and line ranges\n\
                     - item_schema file=\"{filename}\" path=\"key\" — see all keys across array items\n\
                     - head file=\"{filename}\" lines=50 — preview first 50 lines\n\
                     - grep file=\"{filename}\" pattern=\"keyword\" — search for specific content\n\
                     - get_in file=\"{filename}\" path=\"key.subkey\" — extract nested values (supports offset/limit for large strings)\n\
                     - iterate_over file=\"{filename}\" path=\"key\" fields=\"a,b\" — extract fields from array items\n\
                     - slice file=\"{filename}\" start=N end=M — extract line range",
                    fmt = format.as_str(),
                );

                // Append companion file hints when structured content was extracted
                for companion in &result.companions {
                    pointer.push_str(&format!(
                        "\n\n[companion: '{name}' extracted from key '{key}' ({lines} lines, {fmt})]\n\
                         This file contains the raw content from $.{key} — use line-based tools directly:\n\
                         - schema file=\"{name}\" — view section structure and line ranges\n\
                         - head file=\"{name}\" lines=30 — preview first sections\n\
                         - grep file=\"{name}\" pattern=\"keyword\" — search within content\n\
                         - slice file=\"{name}\" start=N end=M — extract a section by line range",
                        name = companion.filename,
                        key = companion.source_key,
                        lines = companion.line_count,
                        fmt = companion.format.as_str(),
                    ));
                }

                self.budget.record_intercepted(token_count);

                tracing::debug!(
                    "Scratchpad: intercepted {} output (~{} tokens) → {} ({} companions)",
                    ctx.tool_name,
                    token_count,
                    filename,
                    result.companions.len(),
                );

                TransformOutputResult::new(pointer)
            }
            Err(e) => {
                // If write fails, pass through the original output
                tracing::warn!(
                    "Scratchpad: failed to write output for {}: {}",
                    ctx.tool_name,
                    e
                );
                TransformOutputResult::new(output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_wrapper::ToolCallContext;
    use tempfile::TempDir;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_wrapper_intercepts_large_output() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-1")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("search_knowledge_base".to_string(), 10)]);

        let counter = crate::scratchpad::context_budget::TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage.clone(), budget);

        // Use varied content to avoid tokenizer compression of repeated chars
        let large_output = (0..500).map(|i| format!("item_{} ", i)).collect::<String>();
        let mut ctx = ToolCallContext::new("search_knowledge_base");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker_abc".to_string();
        ctx.attempt = Some(0);

        let result = wrapper.transform_output(large_output, &ctx, None);
        let output = result.output;

        assert!(output.contains("[scratchpad:"));
        assert!(output.contains("schema"));
        assert!(output.contains("task_1-worker_abc-search_knowledge_base-0"));

        // Verify file was written
        let files = storage.list_files().await.unwrap();
        assert!(!files.is_empty());
    }

    #[tokio::test]
    async fn test_wrapper_passes_through_small_output() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-2")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("search_knowledge_base".to_string(), 1000)]);

        let counter = crate::scratchpad::context_budget::TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage, budget);

        let small_output = "small result".to_string();
        let ctx = ToolCallContext::new("search_knowledge_base");

        let result = wrapper.transform_output(small_output.clone(), &ctx, None);
        assert_eq!(result.output, small_output);
    }

    #[tokio::test]
    async fn test_wrapper_ignores_non_scratchpad_tools() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-3")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("search_knowledge_base".to_string(), 100)]);

        let counter = crate::scratchpad::context_budget::TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage, budget);

        let large_output = "x".repeat(500);
        let ctx = ToolCallContext::new("other_tool");

        let result = wrapper.transform_output(large_output.clone(), &ctx, None);
        assert_eq!(result.output, large_output);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_wrapper_companion_files_in_pointer() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-comp")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("analyze_logs".to_string(), 10)]);

        let counter = crate::scratchpad::context_budget::TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage.clone(), budget);

        // JSON with a large markdown string value that will be extracted as a companion
        let md_lines = (0..15)
            .map(|i| format!("### Section {i}\n- key: value {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let json = serde_json::json!({ "status": "ok", "kv_markdown": md_lines });
        let large_output = json.to_string();

        let mut ctx = ToolCallContext::new("analyze_logs");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker_rca".to_string();
        ctx.attempt = Some(0);

        let result = wrapper.transform_output(large_output, &ctx, None);
        let output = result.output;

        // Primary file pointer
        assert!(output.contains("[scratchpad:"));
        // Companion file hint
        assert!(output.contains("[companion:"));
        assert!(output.contains("kv_markdown"));
        assert!(output.contains(".md"));
        // Companion-specific tool suggestions
        assert!(output.contains("grep"));
        assert!(output.contains("slice"));

        // Verify both files exist on disk
        let files = storage.list_files().await.unwrap();
        assert!(files.iter().any(|f| f.ends_with(".json")));
        assert!(files.iter().any(|f| f.ends_with(".md")));
    }
}
