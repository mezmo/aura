//! ScratchpadWrapper — intercepts large MCP tool outputs and writes them
//! to the scratchpad directory, returning a summary pointer to the LLM.

use super::context_budget::ContextBudget;
use super::storage::ScratchpadStorage;
use crate::mcp_response::CallOutcome;
use crate::tool_wrapper::{ToolCallContext, ToolWrapper, TransformOutputResult};
use async_trait::async_trait;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// ToolWrapper that intercepts large outputs from flagged tools and writes
/// them to scratchpad files, replacing the output with a compact pointer.
pub struct ScratchpadWrapper {
    /// Map of bare tool name → `min_tokens` threshold. Resolved per-request
    /// during `Agent::new` (single-agent) or `Orchestrator::create_worker`
    /// (orchestration) by `scratchpad::scratchpad_tool_map` — server-aware,
    /// glob patterns expanded against each server's tool list. Runtime
    /// lookup is an exact `HashMap::get`.
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
        outcome: &CallOutcome,
        ctx: &ToolCallContext,
        _extracted: Option<&serde_json::Value>,
    ) -> TransformOutputResult {
        // Tool errors should never be diverted to scratchpad — the LLM needs
        // to see the error inline so it can react. Only intercept successful
        // tool outputs that exceed the per-tool token threshold.
        if outcome.is_error() {
            return TransformOutputResult::new(output);
        }

        let min_tokens = match self.scratchpad_tools.get(&ctx.tool_name) {
            Some(&mt) => mt,
            None => return TransformOutputResult::new(output),
        };

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

        // Suffix is a content hash (not a UUID) so identical raw output yields
        // an identical pointer string. The orchestration `DuplicateCallGuard`
        // runs after this wrapper and compares pointer strings; a fresh UUID
        // every call would defeat duplicate detection on intercepted tools.
        let mut hasher = DefaultHasher::new();
        output.hash(&mut hasher);
        let content_hash = format!("{:016x}", hasher.finish());

        let file_id = format!(
            "task_{}-{}-{}-{}-{}",
            ctx.task_id.unwrap_or(0),
            ctx.tool_initiator_id,
            ctx.tool_name,
            ctx.attempt.unwrap_or(0),
            &content_hash[..8],
        )
        .replace(['/', '\\', ':', ' '], "_");

        // Sync write: actix-web spawns one current_thread tokio runtime per
        // worker, so `block_in_place` would panic ("can call blocking only
        // when running on the multi-threaded runtime").
        //
        // Going async here is a larger refactor than it looks: `#[async_trait]`
        // on `ToolWrapper` produces `Send`-only futures, but `WrappedTool::call`
        // (and `definition`) self-imposes `+ Send + Sync` on its return type.
        // Making `transform_output` async would force dropping that `+ Sync`
        // bound on the wrapper plumbing, async-ifying the trait method (a
        // breaking change for ToolWrapper impls), updating ComposedWrapper to
        // chain async transforms, and updating all four wrapper impls
        // (Scratchpad/Persistence/Observer/DuplicateCallGuard).
        //
        // KB-sized writes complete in sub-millisecond time — far cheaper than
        // the LLM round-trip that follows — so briefly blocking the actix
        // worker is acceptable (for now). Revisit if scratchpad payloads grow into the
        // multi-MB range or if profiling shows the sync write on the hot path.
        let write = self.storage.write_output_sync(&file_id, &output);
        match write {
            Ok(result) => {
                let filename = result
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| file_id.clone());
                let line_count = result.line_count;
                let format = result.format;
                let token_count = output_tokens;

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

                for companion in &result.companions {
                    // Header is the same for both formats: the content moved
                    // out of the parent JSON, so calling get_in on the parent
                    // would just return the raw escaped string.
                    pointer.push_str(&format!(
                        "\n\n[companion: '{name}' extracted from key '{key}' ({lines} lines, {fmt})]\n\
                         IMPORTANT: the content of $.{key} now lives in the companion file '{name}', \
                         NOT inside the parent JSON. Calling get_in on the parent for '{key}' will \
                         only return the raw string. Read the companion file directly:",
                        name = companion.filename,
                        key = companion.source_key,
                        lines = companion.line_count,
                        fmt = companion.format.as_str(),
                    ));

                    // Tool list per format. JSON companions are a parsed
                    // JSON tree — get_in / item_schema / iterate_over apply.
                    // Markdown companions are a flat string — line-based
                    // tools (slice) only.
                    use super::storage::ContentFormat;
                    let tool_list = match companion.format {
                        ContentFormat::Json => format!(
                            "\n\
                             - schema file=\"{name}\" — view structure and line ranges\n\
                             - get_in file=\"{name}\" path=\"key.subkey\" — extract nested values\n\
                             - item_schema file=\"{name}\" path=\"key\" — see keys across array items\n\
                             - iterate_over file=\"{name}\" path=\"key\" fields=\"a,b\" — extract fields from array items\n\
                             - head file=\"{name}\" lines=30 — preview\n\
                             - grep file=\"{name}\" pattern=\"keyword\" — search content",
                            name = companion.filename,
                        ),
                        ContentFormat::Markdown | ContentFormat::Text => format!(
                            "\n\
                             - schema file=\"{name}\" — view section structure and line ranges\n\
                             - head file=\"{name}\" lines=30 — preview first sections\n\
                             - grep file=\"{name}\" pattern=\"keyword\" — search within content\n\
                             - slice file=\"{name}\" start=N end=M — extract a section by line range",
                            name = companion.filename,
                        ),
                    };
                    pointer.push_str(&tool_list);
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
                tracing::warn!(
                    "Scratchpad: failed to write output for {}: {}",
                    ctx.tool_name,
                    e
                );
                // Returning the full payload here would re-introduce the exact
                // overflow scratchpad exists to prevent — the output already
                // exceeded the per-tool threshold by token count. Surface a
                // compact error pointer instead so the LLM can react and retry.
                let error_pointer = format!(
                    "[scratchpad: failed to save {} output (~{} tokens): {}. \
                     The output was too large for the context window and could not be \
                     persisted. Retry the tool call, narrow the query, or proceed without it.]",
                    ctx.tool_name, output_tokens, e
                );
                TransformOutputResult::new(error_pointer)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratchpad::context_budget::{TiktokenCounter, TokenCounter};
    use crate::tool_wrapper::ToolCallContext;
    use tempfile::TempDir;

    /// Build a `CallOutcome::Success` for tests that don't care about the
    /// content (the wrapper only reads `is_error()`, not the inner string).
    fn ok() -> CallOutcome {
        CallOutcome::Success(String::new())
    }

    // Glob pattern matching + per-server tie-break semantics now live in
    // `scratchpad::scratchpad_tool_map`, which runs once per request when the
    // agent (or orchestration worker) is constructed. See the
    // `scratchpad_tool_map_*` tests in `mod.rs`. The wrapper itself just
    // does an exact `HashMap::get(tool_name)` per tool call.

    /// End-to-end coverage of the wrapper's exact-name lookup. The map is
    /// keyed by bare tool name (resolved per-request during agent/worker
    /// construction by `scratchpad::scratchpad_tool_map` from per-server glob
    /// patterns). This guards against the regression where the wrapper tried
    /// to do glob matching at tool-call time against pattern keys.
    #[tokio::test]
    async fn test_wrapper_intercepts_via_resolved_tool_name() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-glob")
                .await
                .unwrap(),
        );

        // The map post-resolution lists the bare tool name → threshold.
        // Glob expansion happens at boot; the wrapper just does HashMap::get.
        let tools = HashMap::from([("list_pipelines".to_string(), 10)]);

        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage.clone(), budget);

        let large_output = (0..500)
            .map(|i| format!("entry_{} ", i))
            .collect::<String>();
        let mut ctx = ToolCallContext::new("list_pipelines");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "incident".to_string();
        ctx.attempt = Some(0);

        let result = wrapper.transform_output(large_output, &ok(), &ctx, None);
        assert!(
            result.output.contains("[scratchpad:"),
            "exact-name lookup must intercept; got: {}",
            &result.output[..result.output.len().min(200)]
        );

        let files = storage.list_files().await.unwrap();
        assert!(
            !files.is_empty(),
            "wildcard glob must produce a scratchpad file on disk"
        );
    }

    /// Identical raw output through identical context must yield identical
    /// pointer strings. The orchestration `DuplicateCallGuard` runs after
    /// this wrapper and compares pointer strings to detect pathological
    /// looping; a non-deterministic suffix here would defeat that.
    #[tokio::test]
    async fn test_wrapper_pointer_is_deterministic_for_identical_output() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-deterministic")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("echo_large".to_string(), 10)]);
        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage, budget);

        let large_output = (0..200).map(|i| format!("entry_{i} ")).collect::<String>();
        let mut ctx = ToolCallContext::new("echo_large");
        ctx.task_id = Some(3);
        ctx.tool_initiator_id = "worker_loop".to_string();
        ctx.attempt = Some(0);

        let r1 = wrapper.transform_output(large_output.clone(), &ok(), &ctx, None);
        let r2 = wrapper.transform_output(large_output, &ok(), &ctx, None);
        assert_eq!(
            r1.output, r2.output,
            "identical raw output must produce identical pointer strings so DuplicateCallGuard can detect loops"
        );
    }

    #[tokio::test]
    async fn test_wrapper_intercepts_large_output() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-1")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("search_knowledge_base".to_string(), 10)]);

        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage.clone(), budget);

        // Use varied content to avoid tokenizer compression of repeated chars
        let large_output = (0..500).map(|i| format!("item_{} ", i)).collect::<String>();
        let mut ctx = ToolCallContext::new("search_knowledge_base");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker_abc".to_string();
        ctx.attempt = Some(0);

        let result = wrapper.transform_output(large_output, &ok(), &ctx, None);
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

        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage.clone(), budget.clone());

        let small_output = "small result".to_string();
        let ctx = ToolCallContext::new("search_knowledge_base");

        let result = wrapper.transform_output(small_output.clone(), &ok(), &ctx, None);
        assert_eq!(
            result.output, small_output,
            "small output should pass through unchanged"
        );
        // Verify no interception occurred: budget should show zero intercepted tokens
        // and no files should have been written to storage
        let (intercepted, _) = budget.scratchpad_usage();
        assert_eq!(
            intercepted, 0,
            "no tokens should be intercepted for small output"
        );
        assert!(
            storage.list_files().await.unwrap().is_empty(),
            "no scratchpad files should be created for small output"
        );
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

        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage, budget);

        let large_output = "x".repeat(500);
        let ctx = ToolCallContext::new("other_tool");

        let result = wrapper.transform_output(large_output.clone(), &ok(), &ctx, None);
        assert_eq!(result.output, large_output);
    }

    #[tokio::test]
    async fn test_wrapper_intercepts_at_exact_threshold() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-boundary")
                .await
                .unwrap(),
        );

        let counter = TiktokenCounter::default_counter();

        // Build content and measure its exact token count
        let content = (0..100).map(|i| format!("item_{} ", i)).collect::<String>();
        let exact_tokens = counter.count_tokens(&content);
        assert!(exact_tokens > 0, "Content should have nonzero tokens");

        // Set threshold to exactly the token count — should be intercepted (>=)
        let tools = HashMap::from([("tool_at_boundary".to_string(), exact_tokens)]);
        let budget = ContextBudget::new(128_000, 0.20, 0, Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage.clone(), budget);

        let mut ctx = ToolCallContext::new("tool_at_boundary");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker".to_string();
        ctx.attempt = Some(0);

        let result = wrapper.transform_output(content.clone(), &ok(), &ctx, None);
        assert!(
            result.output.contains("[scratchpad:"),
            "Output at exact threshold should be intercepted, got: {}",
            &result.output[..result.output.len().min(200)]
        );

        // Now test one token below threshold — should pass through
        let tools_above = HashMap::from([("tool_at_boundary".to_string(), exact_tokens + 1)]);
        let counter2 = TiktokenCounter::default_counter();
        let budget2 = ContextBudget::new(128_000, 0.20, 0, Arc::new(counter2));
        let wrapper2 = ScratchpadWrapper::new(tools_above, storage, budget2);

        let result2 = wrapper2.transform_output(content.clone(), &ok(), &ctx, None);
        assert_eq!(
            result2.output, content,
            "Output below threshold should pass through unchanged"
        );
    }

    #[tokio::test]
    async fn test_wrapper_records_intercepted_tokens() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-counter")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("counted_tool".to_string(), 10)]);
        let counter = TiktokenCounter::default_counter();
        let content = (0..200)
            .map(|i| format!("entry_{} ", i))
            .collect::<String>();
        let expected_tokens = counter.count_tokens(&content);

        let budget = ContextBudget::new(128_000, 0.20, 0, Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage, budget.clone());

        let mut ctx = ToolCallContext::new("counted_tool");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker".to_string();
        ctx.attempt = Some(0);

        assert_eq!(budget.scratchpad_usage(), (0, 0));

        let result = wrapper.transform_output(content, &ok(), &ctx, None);
        assert!(result.output.contains("[scratchpad:"));

        let (intercepted, _) = budget.scratchpad_usage();
        assert_eq!(
            intercepted, expected_tokens,
            "tokens_intercepted should match the token count of the intercepted output"
        );
    }

    #[tokio::test]
    async fn test_wrapper_falls_back_on_write_failure() {
        // Point storage at a non-existent directory that we then remove,
        // so write_output_sync will fail with an I/O error.
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-fail")
                .await
                .unwrap(),
        );

        // Remove the directory so writes fail
        std::fs::remove_dir_all(storage.dir()).unwrap();

        let tools = HashMap::from([("failing_tool".to_string(), 10)]);
        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage, budget.clone());

        let large_output = (0..200).map(|i| format!("item_{} ", i)).collect::<String>();
        let mut ctx = ToolCallContext::new("failing_tool");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker".to_string();
        ctx.attempt = Some(0);

        let result = wrapper.transform_output(large_output.clone(), &ok(), &ctx, None);

        // Returning the raw payload would re-introduce the overflow scratchpad
        // exists to prevent. The wrapper must instead surface a compact error
        // pointer so the LLM sees something small and actionable.
        assert!(
            result.output.contains("[scratchpad: failed"),
            "On write failure, output should be a compact error pointer; got: {}",
            &result.output[..result.output.len().min(200)]
        );
        assert!(
            result.output.len() < large_output.len(),
            "Error pointer must be smaller than the raw payload it replaces"
        );
        assert!(
            result.output.contains("failing_tool"),
            "Error pointer should name the tool"
        );

        // tokens_intercepted should NOT have been incremented
        let (intercepted, _) = budget.scratchpad_usage();
        assert_eq!(
            intercepted, 0,
            "tokens_intercepted should be 0 when write fails"
        );
    }

    /// JSON companions (escaped JSON extracted from a string value) are a
    /// parsed JSON tree, so the pointer must steer the LLM at JSON-aware
    /// tools (`get_in`, `item_schema`, `iterate_over`). The earlier wording
    /// blanket-recommended "line-based tools only" which was wrong for
    /// `.json` companions and forced the LLM down the wrong tool path.
    #[tokio::test]
    async fn test_wrapper_json_companion_pointer_recommends_get_in() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-json-comp")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("nested_call".to_string(), 10)]);
        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadWrapper::new(tools, storage.clone(), budget);

        // Outer JSON whose `payload` value is itself an escaped JSON string
        // long enough to trigger companion extraction (>= COMPANION_MIN_LINES
        // pretty-printed).
        let inner = serde_json::json!({
            "items": (0..20).map(|i| serde_json::json!({ "id": i, "name": format!("item-{i}") })).collect::<Vec<_>>()
        });
        let payload_str = serde_json::to_string(&inner).unwrap();
        let outer = serde_json::json!({ "status": "ok", "payload": payload_str });

        let mut ctx = ToolCallContext::new("nested_call");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker_a".to_string();
        ctx.attempt = Some(0);

        let result = wrapper.transform_output(outer.to_string(), &ok(), &ctx, None);
        let output = result.output;

        // The companion was a JSON tree, so the pointer must list JSON-aware
        // tools alongside the line-based ones.
        assert!(output.contains("[companion:"), "missing companion section");
        // Find the companion section to assert against (the parent block also
        // mentions get_in).
        let companion_block = output
            .split("[companion:")
            .nth(1)
            .expect("expected a companion section");
        assert!(
            companion_block.contains("get_in"),
            "JSON companion pointer must mention get_in; got: {companion_block}"
        );
        assert!(
            companion_block.contains("item_schema"),
            "JSON companion pointer must mention item_schema"
        );
        assert!(
            companion_block.contains("iterate_over"),
            "JSON companion pointer must mention iterate_over"
        );

        // The companion file should be the .json variant
        let files = storage.list_files().await.unwrap();
        assert!(
            files.iter().any(|f| f.contains(".payload.json")),
            "expected a .payload.json companion; got {files:?}"
        );
    }

    #[tokio::test]
    async fn test_wrapper_companion_files_in_pointer() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-comp")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("analyze_logs".to_string(), 10)]);

        let counter = TiktokenCounter::default_counter();
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

        let result = wrapper.transform_output(large_output, &ok(), &ctx, None);
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
