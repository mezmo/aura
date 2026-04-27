//! Shared scratchpad wire-up used by both the single-agent builder and the
//! orchestration worker factory. Callers handle their own gating, path
//! selection, preamble mutation, and wrapper composition; this module owns
//! the construction of the budget, storage, wrapper, and tools config.

use super::{
    ContextBudget, MCP_TOOL_SCHEMA_TOKEN_ESTIMATE, SCRATCHPAD_PREAMBLE, ScratchpadConfig,
    ScratchpadStorage, ScratchpadToolsConfig, ScratchpadWrapper, TokenCounter,
    scratchpad_tool_schema_tokens,
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

/// Estimate tokens consumed by the scratchpad preamble + the 8 scratchpad tool
/// schemas + the worker-accessible MCP tool schemas (glob-filtered by
/// `mcp_filter`; empty filter means all tools). Callers pass any additional
/// preamble text (e.g. `WORKER_PREAMBLE_TEMPLATE` + worker.preamble) through
/// `extra_preamble_text`.
pub fn estimate_scratchpad_overhead(
    counter: &dyn TokenCounter,
    accessible_tools: &[String],
    mcp_filter: &[String],
    extra_preamble_text: &[&str],
) -> usize {
    let scratchpad_preamble_tokens = counter.count_tokens(SCRATCHPAD_PREAMBLE);
    let scratchpad_tools_tokens = scratchpad_tool_schema_tokens(counter);
    let extra_tokens: usize = extra_preamble_text
        .iter()
        .map(|t| counter.count_tokens(t))
        .sum();

    let matching_mcp_tools = match mcp_filter {
        [] => accessible_tools.len(),
        filter => accessible_tools
            .iter()
            .filter(|name| filter.iter().any(|p| glob_match(p, name)))
            .count(),
    };

    scratchpad_preamble_tokens
        + scratchpad_tools_tokens
        + extra_tokens
        + matching_mcp_tools * MCP_TOOL_SCHEMA_TOKEN_ESTIMATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratchpad::context_budget::TiktokenCounter;

    fn counter() -> Arc<dyn TokenCounter> {
        Arc::new(TiktokenCounter::default_counter())
    }

    #[test]
    fn estimate_overhead_with_no_filter_counts_all_accessible_tools() {
        let c = counter();
        let tools: Vec<String> = (0..5).map(|i| format!("tool_{i}")).collect();
        let overhead = estimate_scratchpad_overhead(&*c, &tools, &[], &[]);
        // At least the 5 MCP tools worth of estimated schema tokens should be present.
        assert!(overhead >= 5 * MCP_TOOL_SCHEMA_TOKEN_ESTIMATE);
    }

    #[test]
    fn estimate_overhead_with_filter_counts_only_matching_tools() {
        let c = counter();
        let tools: Vec<String> = vec!["alpha_list".into(), "alpha_get".into(), "beta_get".into()];
        let filter: Vec<String> = vec!["alpha_*".into()];
        let all = estimate_scratchpad_overhead(&*c, &tools, &[], &[]);
        let filtered = estimate_scratchpad_overhead(&*c, &tools, &filter, &[]);
        // Filter drops beta_get → overhead should be smaller by one MCP tool estimate.
        assert_eq!(all - filtered, MCP_TOOL_SCHEMA_TOKEN_ESTIMATE);
    }

    #[test]
    fn estimate_overhead_includes_extra_preamble_text() {
        let c = counter();
        let baseline = estimate_scratchpad_overhead(&*c, &[], &[], &[]);
        let with_extra = estimate_scratchpad_overhead(&*c, &[], &[], &["hello world"]);
        assert!(with_extra > baseline);
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
