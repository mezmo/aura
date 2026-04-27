//! Shared scratchpad wire-up used by both the single-agent builder and the
//! orchestration worker factory. Callers handle their own gating, path
//! selection, preamble mutation, and wrapper composition; this module owns
//! the construction of the budget, storage, wrapper, and tools config.

use super::{
    ContextBudget, SCRATCHPAD_PREAMBLE, ScratchpadConfig, ScratchpadStorage, ScratchpadToolsConfig,
    ScratchpadWrapper, TokenCounter, scratchpad_tool_schema_tokens,
};
use crate::config::glob_match;
use crate::tool_wrapper::ToolWrapper;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Inputs required to wire up scratchpad for one agent (single-agent or worker).
/// Callers resolve `context_window` and `initial_used` upstream so the helper
/// can't silently paper over a missing context_window.
pub struct ScratchpadBuildInputs<'a> {
    pub sp_cfg: &'a ScratchpadConfig,
    pub storage_dir: &'a Path,
    pub scratchpad_tool_map: HashMap<String, usize>,
    pub context_window: usize,
    pub initial_used: usize,
    pub token_counter: Arc<dyn TokenCounter>,
}

/// Output of `build_scratchpad`: the budget the caller records on its `Agent`
/// struct, the wrapper it composes into its tool pipeline, the storage handle,
/// and a ready-to-assign `ScratchpadToolsConfig` for `AgentConfig`.
pub struct ScratchpadBuild {
    pub budget: ContextBudget,
    pub storage: Arc<ScratchpadStorage>,
    pub wrapper: Arc<dyn ToolWrapper>,
    pub tools_config: ScratchpadToolsConfig,
}

/// Build the `ContextBudget`, `ScratchpadStorage`, `ScratchpadWrapper`, and
/// `ScratchpadToolsConfig`. Caller is responsible for gating (enabled check,
/// accessibility check, context_window resolution), preamble injection, and
/// composing the returned wrapper into its tool pipeline.
pub async fn build_scratchpad(
    inputs: ScratchpadBuildInputs<'_>,
) -> std::io::Result<ScratchpadBuild> {
    let budget = ContextBudget::new(
        inputs.context_window,
        inputs.sp_cfg.context_safety_margin,
        inputs.initial_used,
        inputs.token_counter,
    )
    .with_max_extraction_tokens(inputs.sp_cfg.max_extraction_tokens);

    let storage = Arc::new(ScratchpadStorage::in_dir(inputs.storage_dir).await?);
    tracing::info!(
        "Scratchpad active (dir={}, context_window={}, max_extraction_tokens={}, tool_patterns={})",
        inputs.storage_dir.display(),
        inputs.context_window,
        inputs.sp_cfg.max_extraction_tokens,
        inputs.scratchpad_tool_map.len(),
    );

    let wrapper: Arc<dyn ToolWrapper> = Arc::new(ScratchpadWrapper::new(
        inputs.scratchpad_tool_map.clone(),
        storage.clone(),
        budget.clone(),
    ));

    let tools_config = ScratchpadToolsConfig {
        storage: storage.clone(),
        budget: budget.clone(),
        scratchpad_tools: inputs.scratchpad_tool_map,
    };

    Ok(ScratchpadBuild {
        budget,
        storage,
        wrapper,
        tools_config,
    })
}

/// Estimate tokens consumed by the scratchpad preamble, the 8 scratchpad tool
/// schemas, and any caller-supplied preamble text (e.g.
/// `WORKER_PREAMBLE_TEMPLATE` + worker.preamble for orchestration, the
/// agent's effective preamble for single-agent).
///
/// MCP tool schemas, user query, and chat history are NOT included — callers
/// add them via [`count_mcp_tool_schema_tokens`] and direct
/// `counter.count_tokens(...)` calls so each piece stays exact and
/// caller-controlled.
pub fn estimate_scratchpad_overhead(
    counter: &dyn TokenCounter,
    extra_preamble_text: &[&str],
) -> usize {
    let scratchpad_preamble_tokens = counter.count_tokens(SCRATCHPAD_PREAMBLE);
    let scratchpad_tools_tokens = scratchpad_tool_schema_tokens(counter);
    let extra_tokens: usize = extra_preamble_text
        .iter()
        .map(|t| counter.count_tokens(t))
        .sum();

    scratchpad_preamble_tokens + scratchpad_tools_tokens + extra_tokens
}

/// BPE-count the JSON-Schema bodies of every MCP tool the agent will see,
/// optionally filtered by glob patterns (empty filter = include all).
///
/// Each tool is serialized as `{name, description, input_schema}` — the same
/// shape the LLM sees in its tool list. Per-provider serialization differs
/// slightly (OpenAI vs. Anthropic envelope keys), so the BPE count is a
/// conservative approximation of the wire format. That over-counts a few
/// tokens per tool — the safer direction for budget gating, and accuracy is
/// recovered after turn 1 anyway via the LLM's reported `input_tokens`.
pub fn count_mcp_tool_schema_tokens<'a>(
    counter: &dyn TokenCounter,
    tools: impl IntoIterator<Item = &'a rmcp::model::Tool>,
    mcp_filter: &[String],
) -> usize {
    tools
        .into_iter()
        .filter(|t| {
            mcp_filter.is_empty() || mcp_filter.iter().any(|p| glob_match(p, t.name.as_ref()))
        })
        .map(|t| {
            let json = serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": &*t.input_schema,
            });
            counter.count_tokens(&json.to_string())
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratchpad::context_budget::TiktokenCounter;

    fn counter() -> Arc<dyn TokenCounter> {
        Arc::new(TiktokenCounter::default_counter())
    }

    fn synth_tool(
        name: &str,
        description: &str,
        input_schema: serde_json::Value,
    ) -> rmcp::model::Tool {
        let serde_json::Value::Object(map) = input_schema else {
            panic!("input_schema must be a JSON object");
        };
        rmcp::model::Tool {
            name: name.to_string().into(),
            title: None,
            description: Some(description.to_string().into()),
            input_schema: Arc::new(map),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        }
    }

    #[test]
    fn estimate_overhead_includes_extra_preamble_text() {
        let c = counter();
        let baseline = estimate_scratchpad_overhead(&*c, &[]);
        let with_extra = estimate_scratchpad_overhead(&*c, &["hello world"]);
        assert!(with_extra > baseline);
    }

    /// Concretely verifies the delta when extra_preamble_text is added: the
    /// added overhead must equal the BPE token count of the supplied text.
    /// This is the contract `builder.rs::setup_single_agent_scratchpad`
    /// relies on when seeding `initial_used` with the system prompt — under-
    /// counting here would mean early extraction budget checks pass when
    /// they shouldn't.
    #[test]
    fn estimate_overhead_extra_preamble_delta_equals_bpe_count() {
        let c = counter();
        let prompt = "You are an SRE assistant. Always cite the source pipeline.";
        let baseline = estimate_scratchpad_overhead(&*c, &[]);
        let with_prompt = estimate_scratchpad_overhead(&*c, &[prompt]);
        assert_eq!(
            with_prompt - baseline,
            c.count_tokens(prompt),
            "delta should match the BPE token count of the added preamble \
             so the budget seed is conservative",
        );
    }

    /// Two preamble strings should sum: lets the orchestration caller pass
    /// `[WORKER_PREAMBLE_TEMPLATE, &worker.preamble]` and have both counted.
    #[test]
    fn estimate_overhead_extra_preamble_text_sums_across_entries() {
        let c = counter();
        let a = "alpha alpha alpha";
        let b = "beta beta beta";
        let baseline = estimate_scratchpad_overhead(&*c, &[]);
        let combined = estimate_scratchpad_overhead(&*c, &[a, b]);
        assert_eq!(combined - baseline, c.count_tokens(a) + c.count_tokens(b),);
    }

    /// `count_mcp_tool_schema_tokens` must return the BPE count of the
    /// serialized `{name, description, input_schema}` envelope for each tool
    /// — this is what gets used to seed `initial_used` so under-counting
    /// would let early extraction budget checks pass spuriously.
    #[test]
    fn count_mcp_tool_schema_tokens_returns_serialized_bpe_count() {
        let c = counter();
        let tool = synth_tool(
            "search_logs",
            "Search log lines by regex",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["pattern"]
            }),
        );
        let actual = count_mcp_tool_schema_tokens(&*c, std::iter::once(&tool), &[]);
        let expected_json = serde_json::json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": &*tool.input_schema,
        });
        assert_eq!(actual, c.count_tokens(&expected_json.to_string()));
    }

    /// Glob filter must drop non-matching tools. With one matching and one
    /// non-matching tool, the filtered count should equal the matching
    /// tool's individual BPE count.
    #[test]
    fn count_mcp_tool_schema_tokens_filters_via_glob() {
        let c = counter();
        let alpha = synth_tool(
            "alpha_get",
            "alpha desc",
            serde_json::json!({"type": "object"}),
        );
        let beta = synth_tool(
            "beta_get",
            "beta desc",
            serde_json::json!({"type": "object"}),
        );
        let filter: Vec<String> = vec!["alpha_*".into()];
        let all = count_mcp_tool_schema_tokens(&*c, [&alpha, &beta], &[]);
        let filtered = count_mcp_tool_schema_tokens(&*c, [&alpha, &beta], &filter);
        let alpha_only = count_mcp_tool_schema_tokens(&*c, std::iter::once(&alpha), &[]);
        assert_eq!(filtered, alpha_only);
        assert!(all > filtered, "unfiltered must exceed filtered");
    }

    #[tokio::test]
    async fn build_scratchpad_wires_budget_storage_and_wrapper() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sp_cfg = ScratchpadConfig {
            enabled: true,
            context_safety_margin: 0.20,
            max_extraction_tokens: 5_000,
            turn_depth_bonus: 6,
        };
        let mut tool_map = HashMap::new();
        tool_map.insert("search_*".into(), 512);

        let build = build_scratchpad(ScratchpadBuildInputs {
            sp_cfg: &sp_cfg,
            storage_dir: tmp.path(),
            scratchpad_tool_map: tool_map,
            context_window: 128_000,
            initial_used: 1_000,
            token_counter: counter(),
        })
        .await
        .expect("build should succeed");

        assert_eq!(build.budget.max_extraction_tokens(), Some(5_000));
        assert_eq!(build.tools_config.scratchpad_tools.len(), 1);
        assert!(tmp.path().join("scratchpad").exists());
        // Budget in the returned struct and in tools_config share the same counters.
        build.budget.record_intercepted(42);
        assert_eq!(build.tools_config.budget.scratchpad_usage().0, 42);
    }
}
