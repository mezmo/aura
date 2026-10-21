//! `ScratchpadMiddleware`: the layer that keeps large tool output out of
//! the context window by writing it to the scratchpad and handing the model
//! a pointer to explore instead.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;

use super::context_budget::Fit;
use super::storage::{ContentFormat, ScratchpadStorage};
use crate::orchestration::persistence_wrapper::strip_artifact_footer;
use crate::run_context::BoundRun;
use crate::tool_middleware::{CallFailure, Next, ToolMiddleware, ToolReply, ToolRequest};
use crate::tool_wrapper::ToolCallContext;

/// Build the primary "this lives in a file, explore it with these tools"
/// pointer. Shared by [`ScratchpadMiddleware`] (for intercepted MCP output)
/// and the orchestration `read_artifact` tool (for large artifacts read in
/// place), so both surfaces give the LLM identical exploration guidance.
///
/// `headline` is the caller-specific first line (e.g. "[scratchpad: output
/// saved to 'foo.json' (...)]" vs "[artifact 'result.txt' is large (...)]").
/// `file_ref` is the exact `file=` token the read tools accept — a bare
/// scratchpad filename, or a relative path under the read root for an in-place
/// artifact.
pub(crate) fn build_file_pointer(headline: &str, file_ref: &str) -> String {
    format!(
        "{headline}\n\n\
         The full output is too large for the context window. \
         Use these tools to explore it:\n\
         - schema file=\"{f}\" — view structure and line ranges\n\
         - item_schema file=\"{f}\" path=\"key\" — see all keys across array items\n\
         - head file=\"{f}\" lines=50 — preview first 50 lines\n\
         - grep file=\"{f}\" pattern=\"keyword\" — search for specific content\n\
         - get_in file=\"{f}\" path=\"key.subkey\" — extract nested values (supports offset/limit for large strings)\n\
         - iterate_over file=\"{f}\" path=\"key\" fields=\"a,b\" — extract fields from array items\n\
         - slice file=\"{f}\" start=N end=M — extract line range",
        f = file_ref,
    )
}

/// The scratchpad's [`ToolMiddleware`] layer: the flagged tools, their
/// thresholds, and the storage their output goes to.
#[derive(Clone)]
pub struct ScratchpadMiddleware {
    /// Map of bare tool name → `min_tokens` threshold.
    scratchpad_tools: HashMap<String, usize>,
    /// Storage backend for writing scratchpad files.
    storage: Arc<ScratchpadStorage>,
    /// The prepared agent's run slot.
    run: Arc<BoundRun>,
}

impl ScratchpadMiddleware {
    pub fn new(
        scratchpad_tools: HashMap<String, usize>,
        storage: Arc<ScratchpadStorage>,
        run: Arc<BoundRun>,
    ) -> Self {
        Self {
            scratchpad_tools,
            storage,
            run,
        }
    }

    /// Replace `output` with a pointer when it reaches `min_tokens`.
    ///
    /// Counts and records against the budget of the run bound when the call
    /// happens. Output under the threshold passes through untouched, footer
    /// and all; the scratchpad file holds only the raw tool output, since a
    /// trailing non-JSON footer breaks `get_in`, `schema` and `iterate_over`
    /// on otherwise-valid JSON. The file name carries a hash of that content
    /// rather than a UUID so identical output yields an identical pointer:
    /// the orchestration `DuplicateCallGuard` compares pointer strings, and a
    /// fresh name every call would defeat it on intercepted tools.
    async fn intercept(&self, ctx: &ToolCallContext, output: String, min_tokens: usize) -> String {
        let content = strip_artifact_footer(&output);

        // With no run bound there is no budget to count against, and a
        // flagged tool's output is presumed large enough to need one.
        // Withhold it rather than pass through the overflow scratchpad
        // exists to prevent — the same choice the read tools make with
        // `ScratchpadToolError::NoRun`.
        let Some(budget) = self.run.scratchpad_budget() else {
            tracing::warn!(
                "Scratchpad: {} output withheld — no run is bound",
                ctx.tool_name
            );
            return format!(
                "[scratchpad: {} output withheld: no run is bound to count it against. \
                 Retry the tool call.]",
                ctx.tool_name
            );
        };

        let output_tokens = match budget.fit(content, min_tokens.saturating_sub(1)) {
            Fit::Fits { tokens } => {
                tracing::debug!(
                    "Scratchpad: {} output (~{} tokens) below threshold ({}), passing through",
                    ctx.tool_name,
                    tokens,
                    min_tokens
                );
                return output;
            }
            Fit::Exceeds { tokens, .. } => tokens,
        };

        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
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

        // The write is awaited before returning the pointer.
        match self.storage.write_output(&file_id, content).await {
            Ok(result) => {
                let filename = result
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| file_id.clone());
                let line_count = result.line_count;
                let format = result.format;
                let token_count = output_tokens;

                let headline = format!(
                    "[scratchpad: output saved to '{filename}' (~{token_count} tokens, \
                     {line_count} lines, format={fmt})]",
                    fmt = format.as_str(),
                );
                let mut pointer = build_file_pointer(&headline, &filename);

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

                budget.record_intercepted(token_count);

                tracing::debug!(
                    "Scratchpad: intercepted {} output (~{} tokens) → {} ({} companions)",
                    ctx.tool_name,
                    token_count,
                    filename,
                    result.companions.len(),
                );

                pointer
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
                format!(
                    "[scratchpad: failed to save {} output (~{} tokens): {}. \
                     The output was too large for the context window and could not be \
                     persisted. Retry the tool call, narrow the query, or proceed without it.]",
                    ctx.tool_name, output_tokens, e
                )
            }
        }
    }
}

#[async_trait]
impl ToolMiddleware for ScratchpadMiddleware {
    /// A successful reply from a flagged tool that reaches its threshold is
    /// written to a scratchpad file and replaced with a compact pointer to
    /// it (see `intercept`). Tool
    /// errors must reach the LLM inline so it can react, and oversized
    /// errors are bounded upstream at the tool-result source
    /// (`crate::mcp::response::bound_error_content`), so passing them through here
    /// cannot flood the context. Exact-name lookup against
    /// `scratchpad_tools`, which `scratchpad::scratchpad_tool_map` resolves
    /// from the per-server glob patterns when the agent is prepared.
    async fn call(&self, req: ToolRequest, next: Next<'_>) -> Result<ToolReply, CallFailure> {
        let ctx = Arc::clone(&req.ctx);
        match next.run(req).await? {
            ToolReply::Tool {
                output,
                is_error: false,
            } => {
                let output = match self.scratchpad_tools.get(&ctx.tool_name) {
                    Some(&min_tokens) => self.intercept(&ctx, output, min_tokens).await,
                    None => output,
                };
                Ok(ToolReply::Tool {
                    output,
                    is_error: false,
                })
            }
            reply => Ok(reply),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratchpad::ContextBudget;
    use crate::scratchpad::context_budget::{TiktokenCounter, TokenCounter};
    use crate::tool_middleware::{MiddlewareStack, MiddlewareTool};
    use crate::tool_wrapper::ToolCallContext;
    use rig::tool::{Tool as _, ToolError};
    use tempfile::TempDir;

    /// A tool with a fixed output, so a test can push any payload through
    /// the layer.
    #[derive(Clone)]
    struct Echo(String);

    impl rig::tool::Tool for Echo {
        const NAME: &'static str = "echo";
        type Error = ToolError;
        type Args = serde_json::Value;
        type Output = String;

        async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
            rig::completion::ToolDefinition {
                name: Self::NAME.to_string(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }

        async fn call(&self, _args: serde_json::Value) -> Result<String, ToolError> {
            Ok(self.0.clone())
        }
    }

    /// Run `output` from the tool `ctx` names up through `layer` alone.
    async fn run(
        layer: &ScratchpadMiddleware,
        ctx: &ToolCallContext,
        output: impl Into<String>,
    ) -> String {
        let ctx = ctx.clone();
        MiddlewareTool::new(
            Echo(output.into()),
            MiddlewareStack::new(vec![Arc::new(layer.clone())]),
        )
        .with_context_factory(move |_| ctx.clone())
        .call(serde_json::json!({}))
        .await
        .unwrap()
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
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage.clone(),
            Arc::new(BoundRun::pinned_budget(budget)),
        );

        let large_output = (0..500)
            .map(|i| format!("entry_{} ", i))
            .collect::<String>();
        let mut ctx = ToolCallContext::new("list_pipelines");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "incident".to_string();
        ctx.attempt = Some(0);

        let result = run(&wrapper, &ctx, large_output).await;
        assert!(
            result.contains("[scratchpad:"),
            "exact-name lookup must intercept; got: {}",
            &result[..result.len().min(200)]
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
        let wrapper =
            ScratchpadMiddleware::new(tools, storage, Arc::new(BoundRun::pinned_budget(budget)));

        let large_output = (0..200).map(|i| format!("entry_{i} ")).collect::<String>();
        let mut ctx = ToolCallContext::new("echo_large");
        ctx.task_id = Some(3);
        ctx.tool_initiator_id = "worker_loop".to_string();
        ctx.attempt = Some(0);

        let r1 = run(&wrapper, &ctx, large_output.clone()).await;
        let r2 = run(&wrapper, &ctx, large_output).await;
        assert_eq!(
            r1, r2,
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
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage.clone(),
            Arc::new(BoundRun::pinned_budget(budget)),
        );

        // Use varied content to avoid tokenizer compression of repeated chars
        let large_output = (0..500).map(|i| format!("item_{} ", i)).collect::<String>();
        let mut ctx = ToolCallContext::new("search_knowledge_base");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker_abc".to_string();
        ctx.attempt = Some(0);

        let result = run(&wrapper, &ctx, large_output).await;
        let output = result;

        assert!(output.contains("[scratchpad:"));
        assert!(output.contains("schema"));
        assert!(output.contains("task_1-worker_abc-search_knowledge_base-0"));

        // Verify file was written
        let files = storage.list_files().await.unwrap();
        assert!(!files.is_empty());
    }

    #[test]
    fn test_strip_artifact_footer() {
        let body = "{\"a\":1}";
        let with_footer =
            format!("{body}\n\n[Tool output saved to artifact: task-0-w-iter-1-t-0-output.txt]");
        assert_eq!(strip_artifact_footer(&with_footer), body);
        // No footer → returned unchanged.
        assert_eq!(strip_artifact_footer(body), body);
    }

    /// A large JSON output that arrives carrying a trailing artifact footer
    /// must be written to the scratchpad file WITHOUT the footer, so the
    /// exploration tools can still parse it as JSON. Regression test for the
    /// footer leaking into the file and breaking get_in/schema/iterate_over
    /// with "not valid JSON".
    #[tokio::test]
    async fn test_wrapper_strips_persistence_footer_before_write() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-footer")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("execute_range_query".to_string(), 10)]);
        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage.clone(),
            Arc::new(BoundRun::pinned_budget(budget)),
        );

        // Valid JSON payload, large enough to be intercepted...
        let items: String = (0..200)
            .map(|i| format!("{{\"metric\":{{\"id\":{i}}},\"value\":{i}}},"))
            .collect();
        let json = format!("{{\"result\":[{}]}}", items.trim_end_matches(','));
        // ...with the persistence footer appended, exactly as persistence does.
        let with_footer = format!(
            "{json}\n\n[Tool output saved to artifact: \
             task-0-metrics-iter-1-execute_range_query-0-output.txt]"
        );

        let mut ctx = ToolCallContext::new("execute_range_query");
        ctx.task_id = Some(0);
        ctx.tool_initiator_id = "metrics".to_string();
        ctx.attempt = Some(0);

        let result = run(&wrapper, &ctx, with_footer).await;
        assert!(result.contains("[scratchpad:"), "must intercept");

        let files = storage.list_files().await.unwrap();
        let name = files.first().expect("a scratchpad file must be written");
        let written = tokio::fs::read_to_string(storage.dir().join(name))
            .await
            .unwrap();

        assert!(
            !written.contains("[Tool output saved to artifact:"),
            "persistence footer must be stripped from the scratchpad file; tail: {}",
            &written[written.len().saturating_sub(120)..]
        );
        // The file (pretty-printed on write) must parse as JSON and match the
        // original payload semantically — the whole point of the strip.
        let written_value: serde_json::Value = serde_json::from_str(&written)
            .expect("written scratchpad file must be valid JSON after footer strip");
        let expected: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(written_value, expected);
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
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage.clone(),
            Arc::new(BoundRun::pinned_budget(budget.clone())),
        );

        let small_output = "small result".to_string();
        let ctx = ToolCallContext::new("search_knowledge_base");

        let result = run(&wrapper, &ctx, small_output.clone()).await;
        assert_eq!(
            result, small_output,
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
        let wrapper =
            ScratchpadMiddleware::new(tools, storage, Arc::new(BoundRun::pinned_budget(budget)));

        let large_output = "x".repeat(500);
        let ctx = ToolCallContext::new("other_tool");

        let result = run(&wrapper, &ctx, large_output.clone()).await;
        assert_eq!(result, large_output);
    }

    /// Skill tool outputs must never be diverted to scratchpad: skills are
    /// loaded specifically to be read in context. The map is built from MCP
    /// server configs only, so `load_skill` / `read_skill_file` are never
    /// keys — this pins the passthrough.
    #[tokio::test]
    async fn test_wrapper_passes_through_skill_tool_outputs() {
        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-wrap-skills")
                .await
                .unwrap(),
        );

        let tools = HashMap::from([("search_knowledge_base".to_string(), 10)]);

        let counter = TiktokenCounter::default_counter();
        let budget = ContextBudget::new(128_000, 0.20, 0, std::sync::Arc::new(counter));
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage.clone(),
            Arc::new(BoundRun::pinned_budget(budget)),
        );

        let large_output = (0..500).map(|i| format!("line_{} ", i)).collect::<String>();
        for tool_name in ["load_skill", "read_skill_file"] {
            let ctx = ToolCallContext::new(tool_name);
            let result = run(&wrapper, &ctx, large_output.clone()).await;
            assert_eq!(
                result, large_output,
                "{tool_name} output must pass through unintercepted"
            );
        }
        assert!(
            storage.list_files().await.unwrap().is_empty(),
            "skill tool outputs must not produce scratchpad files"
        );
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
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage.clone(),
            Arc::new(BoundRun::pinned_budget(budget)),
        );

        let mut ctx = ToolCallContext::new("tool_at_boundary");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker".to_string();
        ctx.attempt = Some(0);

        let result = run(&wrapper, &ctx, content.clone()).await;
        assert!(
            result.contains("[scratchpad:"),
            "Output at exact threshold should be intercepted, got: {}",
            &result[..result.len().min(200)]
        );

        // Now test one token below threshold — should pass through
        let tools_above = HashMap::from([("tool_at_boundary".to_string(), exact_tokens + 1)]);
        let counter2 = TiktokenCounter::default_counter();
        let budget2 = ContextBudget::new(128_000, 0.20, 0, Arc::new(counter2));
        let wrapper2 = ScratchpadMiddleware::new(
            tools_above,
            storage,
            Arc::new(BoundRun::pinned_budget(budget2)),
        );

        let result2 = run(&wrapper2, &ctx, content.clone()).await;
        assert_eq!(
            result2, content,
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

        let tools = HashMap::from([("counted_tool".to_string(), 100)]);
        let counter = TiktokenCounter::default_counter();
        let content = (0..200)
            .map(|i| format!("entry_{} ", i))
            .collect::<String>();
        let expected_tokens = counter.count_tokens(&content);

        let budget = ContextBudget::new(128_000, 0.20, 0, Arc::new(counter));
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage,
            Arc::new(BoundRun::pinned_budget(budget.clone())),
        );

        let mut ctx = ToolCallContext::new("counted_tool");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker".to_string();
        ctx.attempt = Some(0);

        assert_eq!(budget.scratchpad_usage(), (0, 0));

        let result = run(&wrapper, &ctx, content).await;
        assert!(result.contains("[scratchpad:"));

        let (intercepted, _) = budget.scratchpad_usage();
        assert_eq!(
            intercepted, expected_tokens,
            "tokens_intercepted should match the token count of the intercepted output"
        );
    }

    #[tokio::test]
    async fn test_wrapper_falls_back_on_write_failure() {
        // Point storage at a non-existent directory that we then remove,
        // so the write will fail with an I/O error.
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
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage,
            Arc::new(BoundRun::pinned_budget(budget.clone())),
        );

        let large_output = (0..200).map(|i| format!("item_{} ", i)).collect::<String>();
        let mut ctx = ToolCallContext::new("failing_tool");
        ctx.task_id = Some(1);
        ctx.tool_initiator_id = "worker".to_string();
        ctx.attempt = Some(0);

        let result = run(&wrapper, &ctx, large_output.clone()).await;

        // Returning the raw payload would re-introduce the overflow scratchpad
        // exists to prevent. The wrapper must instead surface a compact error
        // pointer so the LLM sees something small and actionable.
        assert!(
            result.contains("[scratchpad: failed"),
            "On write failure, output should be a compact error pointer; got: {}",
            &result[..result.len().min(200)]
        );
        assert!(
            result.len() < large_output.len(),
            "Error pointer must be smaller than the raw payload it replaces"
        );
        assert!(
            result.contains("failing_tool"),
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
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage.clone(),
            Arc::new(BoundRun::pinned_budget(budget)),
        );

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

        let result = run(&wrapper, &ctx, outer.to_string()).await;
        let output = result;

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
        let wrapper = ScratchpadMiddleware::new(
            tools,
            storage.clone(),
            Arc::new(BoundRun::pinned_budget(budget)),
        );

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

        let result = run(&wrapper, &ctx, large_output).await;
        let output = result;

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
