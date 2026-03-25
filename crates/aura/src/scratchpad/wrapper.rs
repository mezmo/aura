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
    /// Map of tool name → min_bytes threshold for scratchpad interception.
    scratchpad_tools: HashMap<String, usize>,
    /// Storage backend for writing scratchpad files.
    storage: Arc<ScratchpadStorage>,
    /// Budget tracker for recording intercepted bytes.
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
        let min_bytes = match self.scratchpad_tools.get(&ctx.tool_name) {
            Some(&mb) => mb,
            None => return TransformOutputResult::new(output),
        };

        // Only intercept outputs exceeding the per-tool size threshold
        if output.len() < min_bytes {
            tracing::debug!(
                "Scratchpad: {} output ({} bytes) below threshold ({}), passing through",
                ctx.tool_name,
                output.len(),
                min_bytes
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
            Ok((path, format)) => {
                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| file_id.clone());

                let line_count = output.lines().count();
                let byte_count = output.len();

                let pointer = format!(
                    "[scratchpad: output saved to '{filename}' ({byte_count} bytes, \
                     {line_count} lines, format={format})]\n\n\
                     The full output is too large for the context window. \
                     Use these tools to explore it:\n\
                     - schema file=\"{filename}\" — view structure and line ranges\n\
                     - item_schema file=\"{filename}\" path=\"key\" — see all keys across array items\n\
                     - head file=\"{filename}\" lines=50 — preview first 50 lines\n\
                     - grep file=\"{filename}\" pattern=\"keyword\" — search for specific content\n\
                     - get_in file=\"{filename}\" path=\"key.subkey\" — extract nested values\n\
                     - iterate_over file=\"{filename}\" path=\"key\" fields=\"a,b\" — extract fields from array items\n\
                     - slice file=\"{filename}\" start=N end=M — extract line range",
                    format = format.as_str(),
                );

                self.budget.record_intercepted(byte_count);

                tracing::debug!(
                    "Scratchpad: intercepted {} output ({} bytes) → {}",
                    ctx.tool_name,
                    byte_count,
                    filename
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

        let tools = HashMap::from([("search_knowledge_base".to_string(), 100)]);

        let budget = ContextBudget::new(128_000, 0.20);
        let wrapper = ScratchpadWrapper::new(tools, storage.clone(), budget);

        let large_output = "x".repeat(500);
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

        let budget = ContextBudget::new(128_000, 0.20);
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

        let budget = ContextBudget::new(128_000, 0.20);
        let wrapper = ScratchpadWrapper::new(tools, storage, budget);

        let large_output = "x".repeat(500);
        let ctx = ToolCallContext::new("other_tool");

        let result = wrapper.transform_output(large_output.clone(), &ctx, None);
        assert_eq!(result.output, large_output);
    }
}
