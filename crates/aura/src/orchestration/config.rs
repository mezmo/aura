//! Runtime orchestration helpers.
//!
//! The pure, serializable orchestration config types live in
//! `aura_config::orchestration`. This module re-exports them and holds the
//! runtime-only helpers that depend on aura's prompt templates or the
//! `env_flags` escape-hatch toggle: coordinator/worker preamble building and
//! vector-store context strings.

pub use aura_config::orchestration::{
    ArtifactsConfig, OrchestrationConfig, TimeoutsConfig, ToolVisibility, WorkerConfig,
};

use aura_config::VectorStoreConfig;

// ============================================================================
// Vector Store Context Helpers
// ============================================================================

/// Build a formatted context string describing available vector stores.
///
/// This is injected into the agent's system prompt so it knows about its RAG
/// capabilities upfront, rather than discovering them via tool inspection.
///
/// # Example output
///
/// ```text
/// ## Available Knowledge Bases
///
/// You have access to the following knowledge bases for retrieval:
///
/// - **mezmo_docs**: Mezmo documentation and knowledge base articles...
///   Tool: `vector_search_mezmo_docs`
/// ```
pub fn build_vector_store_context(stores: &[VectorStoreConfig]) -> String {
    if stores.is_empty() {
        return String::new();
    }

    let mut context = String::from("\n## Available Knowledge Bases\n\n");
    context.push_str("You have access to the following knowledge bases for retrieval:\n\n");

    for store in stores {
        let description = store
            .context_prefix
            .as_deref()
            .unwrap_or("No description provided");
        context.push_str(&format!(
            "- **{}**: {}\n  Tool: `vector_search_{}`\n\n",
            store.name, description, store.name
        ));
    }

    context
}

// ============================================================================
// Preamble Builders
// ============================================================================

/// Build the coordinator's system prompt by composing the orchestrator
/// framework template with the user's domain-specific system prompt.
///
/// Layering: orchestration instructions → user system prompt → (worker
/// details injected into user message by the planning prompt).
///
/// The `agent_system_prompt` parameter is `[agent].system_prompt` from config.
pub fn build_coordinator_preamble(
    agent_system_prompt: &str,
    include_recon_tools: bool,
    include_history_tools: bool,
) -> String {
    let artifact_tools = if include_history_tools {
        "two **artifact/history tools** (`read_artifact`, `list_prior_runs`)"
    } else {
        "one **artifact tool** (`read_artifact`)"
    };

    let tools_section = if include_recon_tools {
        format!(
            "You have three **routing tools** (`respond_directly`, `create_plan`, `request_clarification`), \
             two **reconnaissance tools** (`list_tools`, `inspect_tool_params`), and {artifact_tools}. \
             Call exactly one routing tool per query."
        )
    } else {
        format!(
            "You have three **routing tools** (`respond_directly`, `create_plan`, `request_clarification`) \
             and {artifact_tools}. Call exactly one routing tool per query."
        )
    };

    let recon_guidance = if include_recon_tools {
        "## Reconnaissance Guidance\n\n\
         Tool names and worker capabilities are already listed in the planning context below. \
         You do NOT need to call `list_tools` or `inspect_tool_params` to discover what's available \
         — that information is already provided to you.\n\n\
         Only call `inspect_tool_params` when you need the **exact parameter schema** for a tool \
         (e.g., to decide between two similar tools based on their parameters). In most cases, \
         the tool name and worker description are sufficient for planning.\n\n\
         **Budget awareness**: Each planning attempt has a limited number of tool calls. \
         Prioritize calling a routing tool (`respond_directly`, `create_plan`, or \
         `request_clarification`) over reconnaissance. Do not spend multiple turns inspecting tools.\n\n\
         **Worker names vs tool names**: The worker names listed below (e.g., \"arithmetic\", \
         \"statistics\") are role assignments for task routing — they are NOT callable tools. \
         Only the tools listed under each worker (e.g., \"add\", \"mean\", \"sin\") are MCP tools \
         that workers can execute."
    } else {
        "**Worker names vs tool names**: The worker names listed below (e.g., \"arithmetic\", \
         \"statistics\") are role assignments for task routing — they are NOT callable tools. \
         Only the tools listed under each worker (e.g., \"add\", \"mean\", \"sin\") are MCP tools \
         that workers can execute."
    };

    let mut preamble =
        super::templates::render_coordinator_preamble(&super::templates::CoordinatorPreambleVars {
            orchestration_system_prompt: agent_system_prompt,
            tools_section: &tools_section,
            recon_guidance,
        });

    // AURA_ESCAPE_HATCH=false strips the "Resolve tool gaps" directive for A/B testing
    if !crate::env_flags::bool_env("AURA_ESCAPE_HATCH", true) {
        tracing::info!("AURA_ESCAPE_HATCH=false — stripping escape hatch directive");
        preamble = preamble.replace(
            "6. **Resolve tool gaps pragmatically**: If a user requests an operation with no matching tool, create a plan using the available tools and note the gap in `planning_summary`. Do NOT deliberate at length about missing capabilities — route what you can, report what you cannot.\n",
            "",
        );
    }

    preamble
}

/// Build the complete worker preamble by injecting the custom system prompt
/// into the worker template.
///
/// The template contains `%%WORKER_SYSTEM_PROMPT%%` which is replaced with
/// the user's custom prompt, or a default message if none is provided.
pub fn build_worker_preamble(config: &OrchestrationConfig) -> String {
    let custom_prompt = config
        .worker_system_prompt
        .as_deref()
        .unwrap_or("(No custom instructions provided)");

    super::templates::render_worker_preamble(&super::templates::WorkerPreambleVars {
        worker_system_prompt: custom_prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::templates::{
        ORCHESTRATOR_PREAMBLE_TEMPLATE, WORKER_PREAMBLE_TEMPLATE,
    };
    use aura_config::{EmbeddingConfig, VectorStoreConfig, VectorStoreType};

    #[test]
    fn test_build_worker_preamble_with_custom_prompt() {
        let config = OrchestrationConfig {
            worker_system_prompt: Some("Focus on data analysis.".to_string()),
            ..Default::default()
        };

        let preamble = build_worker_preamble(&config);
        assert!(preamble.contains("Focus on data analysis."));
        assert!(preamble.contains("Worker Agent"));
        assert!(!preamble.contains("%%WORKER_SYSTEM_PROMPT%%"));
    }

    #[test]
    fn test_build_worker_preamble_without_custom_prompt() {
        let config = OrchestrationConfig::default();
        let preamble = build_worker_preamble(&config);

        assert!(preamble.contains("(No custom instructions provided)"));
        assert!(preamble.contains("Worker Agent"));
        assert!(!preamble.contains("%%WORKER_SYSTEM_PROMPT%%"));
    }

    #[test]
    fn test_worker_preamble_template_loaded() {
        assert!(!WORKER_PREAMBLE_TEMPLATE.is_empty());
        assert!(WORKER_PREAMBLE_TEMPLATE.contains("%%WORKER_SYSTEM_PROMPT%%"));
        assert!(WORKER_PREAMBLE_TEMPLATE.contains("Worker Agent"));
    }

    #[test]
    fn test_coordinator_preamble_injects_agent_system_prompt() {
        let preamble = build_coordinator_preamble("Focus on thorough testing.", true, false);

        // Framework instructions present
        assert!(preamble.contains("respond_directly"));
        assert!(preamble.contains("create_plan"));
        // User's system prompt injected
        assert!(preamble.contains("Focus on thorough testing."));
        // Placeholder replaced
        assert!(!preamble.contains("%%ORCHESTRATION_SYSTEM_PROMPT%%"));
    }

    #[test]
    fn test_coordinator_preamble_with_empty_system_prompt() {
        let preamble = build_coordinator_preamble("", true, false);

        // Framework instructions still present
        assert!(preamble.contains("create_plan"));
        assert!(preamble.contains("Orchestration Coordinator"));
        assert!(!preamble.contains("%%ORCHESTRATION_SYSTEM_PROMPT%%"));
    }

    #[test]
    fn test_orchestrator_preamble_template_loaded() {
        assert!(!ORCHESTRATOR_PREAMBLE_TEMPLATE.is_empty());
        assert!(ORCHESTRATOR_PREAMBLE_TEMPLATE.contains("%%ORCHESTRATION_SYSTEM_PROMPT%%"));
        assert!(ORCHESTRATOR_PREAMBLE_TEMPLATE.contains("%%TOOLS_SECTION%%"));
        assert!(ORCHESTRATOR_PREAMBLE_TEMPLATE.contains("%%RECON_GUIDANCE%%"));
    }

    #[test]
    fn test_coordinator_preamble_without_recon_tools() {
        let preamble = build_coordinator_preamble("Test prompt.", false, false);

        // Should have routing tools but NOT recon tools
        assert!(preamble.contains("routing tools"));
        assert!(preamble.contains("artifact tool"));
        assert!(!preamble.contains("reconnaissance tools"));
        assert!(!preamble.contains("## Reconnaissance Guidance"));
        assert!(!preamble.contains("inspect_tool_params"));
        // Worker names clarification should always be present
        assert!(preamble.contains("Worker names vs tool names"));
    }

    #[test]
    fn test_coordinator_preamble_with_recon_tools() {
        let preamble = build_coordinator_preamble("Test prompt.", true, false);

        assert!(preamble.contains("reconnaissance tools"));
        assert!(preamble.contains("## Reconnaissance Guidance"));
        assert!(preamble.contains("inspect_tool_params"));
    }

    #[test]
    fn test_coordinator_preamble_with_history_tools() {
        let preamble = build_coordinator_preamble("Test prompt.", true, true);

        assert!(preamble.contains("artifact/history tools"));
        assert!(preamble.contains("list_prior_runs"));
        assert!(preamble.contains("read_artifact"));
    }

    #[test]
    fn test_coordinator_preamble_without_history_tools() {
        let preamble = build_coordinator_preamble("Test prompt.", false, false);

        assert!(preamble.contains("artifact tool"));
        assert!(!preamble.contains("list_prior_runs"));
        assert!(preamble.contains("read_artifact"));
    }

    #[test]
    fn test_build_vector_store_context_empty() {
        let stores: Vec<VectorStoreConfig> = vec![];
        let context = build_vector_store_context(&stores);
        assert!(context.is_empty());
    }

    #[test]
    fn test_build_vector_store_context_single() {
        let stores = vec![VectorStoreConfig {
            name: "mezmo_docs".to_string(),
            context_prefix: Some("Mezmo documentation and procedures".to_string()),
            store: VectorStoreType::Qdrant {
                embedding_model: EmbeddingConfig::OpenAI {
                    api_key: "test".to_string(),
                    model: "text-embedding-3-small".to_string(),
                },
                url: "http://localhost:6334".to_string(),
                collection_name: "mezmo_docs".to_string(),
            },
        }];

        let context = build_vector_store_context(&stores);
        assert!(context.contains("## Available Knowledge Bases"));
        assert!(context.contains("**mezmo_docs**"));
        assert!(context.contains("Mezmo documentation and procedures"));
        assert!(context.contains("`vector_search_mezmo_docs`"));
    }

    #[test]
    fn test_build_vector_store_context_no_description() {
        let stores = vec![VectorStoreConfig {
            name: "test_kb".to_string(),
            context_prefix: None,
            store: VectorStoreType::Qdrant {
                embedding_model: EmbeddingConfig::OpenAI {
                    api_key: "test".to_string(),
                    model: "test".to_string(),
                },
                url: "http://localhost:6334".to_string(),
                collection_name: "test_kb".to_string(),
            },
        }];

        let context = build_vector_store_context(&stores);
        assert!(context.contains("**test_kb**"));
        assert!(context.contains("No description provided"));
    }
}
