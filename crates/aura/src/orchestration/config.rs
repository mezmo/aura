//! Configuration types for orchestration mode.

use crate::config::{LlmConfig, VectorStoreConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
// Tool Visibility Configuration
// ============================================================================

/// Controls how tool information is shown to the coordinator during planning.
///
/// This is **display only** — it does not affect which tools workers can execute.
/// Tool execution access is controlled by each worker's `mcp_filter`.
/// This setting only affects what the coordinator sees when deciding how to
/// assign tasks, balancing context length vs. precision.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolVisibility {
    /// No tool information in planning prompt (minimal context, display only).
    None,
    /// Tool names only, bucketed by worker (default — good balance, display only).
    #[default]
    Summary,
    /// Tool names with descriptions (maximum context, higher token usage, display only).
    Full,
}

/// Default for `tools_in_planning` config field.
fn default_tools_in_planning() -> ToolVisibility {
    ToolVisibility::default()
}

/// Default for `max_tools_per_worker` config field.
const fn default_max_tools_per_worker() -> usize {
    10
}

fn default_true() -> bool {
    true
}

/// Default maximum planning cycles for the plan-execute-synthesize loop.
fn default_max_planning_cycles() -> usize {
    3
}

/// Default quality threshold (0.0-1.0) for iteration termination.
fn default_quality_threshold() -> f32 {
    0.8
}

/// Default character threshold for artifact extraction.
fn default_result_artifact_threshold() -> usize {
    4000
}

/// Default summary length for artifact extraction.
fn default_result_summary_length() -> usize {
    2000
}

/// Default per-call timeout for coordinator/worker LLM calls (seconds).
/// Default: 0 (disabled). Opt-in by setting a positive value.
fn default_per_call_timeout_secs() -> u64 {
    0
}

/// Default maximum plan parse retries.
fn default_max_plan_parse_retries() -> usize {
    3
}

/// Hardcoded orchestrator preamble template loaded at compile time.
///
/// Contains the core orchestrator behavior with a `{{orchestration_system_prompt}}`
/// placeholder where `[agent].system_prompt` is injected. This creates the layered
/// system prompt: framework instructions → user domain context → (worker details
/// in user message).
pub const ORCHESTRATOR_PREAMBLE_TEMPLATE: &str =
    include_str!("../prompts/orchestrator_preamble.md");

/// Hardcoded worker preamble template loaded at compile time.
/// Contains the worker agent behavior with a `{{worker_system_prompt}}`
/// placeholder for user customization.
pub const WORKER_PREAMBLE_TEMPLATE: &str = include_str!("../prompts/worker_preamble.md");

/// Per-worker configuration for specialized workers.
///
/// Workers are specialized agents with custom preambles and filtered tool access.
/// Configure workers using TOML sections like `[orchestration.worker.operations]`.
///
/// # Example
///
/// ```toml
/// [orchestration.worker.operations]
/// description = "For logs, pipelines, metrics, and system analysis"
/// preamble = "You are an Operations Specialist..."
/// mcp_filter = ["mezmo_*"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Short description of this worker's purpose (for planning prompt).
    ///
    /// This is shown to the LLM during planning so it can assign tasks appropriately.
    /// Keep it concise (one line).
    pub description: String,

    /// System prompt for this worker (replaces generic worker preamble).
    ///
    /// This is the complete system prompt - it does NOT use the worker_preamble.md template.
    /// For specialized workers, provide domain-specific instructions here.
    pub preamble: String,

    /// Glob patterns for which MCP tools this worker gets access to.
    ///
    /// Examples:
    /// - `["mezmo_*"]` - all tools starting with "mezmo_"
    /// - `["ListKnowledgeBases", "QueryKnowledgeBases"]` - specific tools
    /// - `["*"]` or empty - all tools (default)
    ///
    /// Patterns are matched using glob syntax (supports `*`, `**`, `?`, `[abc]`).
    #[serde(default)]
    pub mcp_filter: Vec<String>,

    /// Vector stores this worker has access to.
    ///
    /// By default (empty), workers have NO vector store access. Workers must
    /// explicitly list the stores they need. This prevents unintended RAG
    /// access and keeps workers focused on their specialization.
    ///
    /// Values should match the `name` field of entries in `[[vector_stores]]`.
    ///
    /// # Example
    ///
    /// ```toml
    /// [orchestration.worker.knowledge]
    /// description = "Knowledge specialist"
    /// preamble = "You are a Knowledge Specialist..."
    /// vector_stores = ["mezmo_docs", "customer_kb"]
    /// ```
    #[serde(default)]
    pub vector_stores: Vec<String>,

    /// Max tool-calling turns for this worker.
    ///
    /// Controls how many Rig ReAct turns (tool calls) a worker can make
    /// per task execution. Overrides `[agent].turn_depth`. Falls back to
    /// `[agent].turn_depth` → `DEFAULT_MAX_DEPTH` (8) if not set.
    pub turn_depth: Option<usize>,

    /// Optional per-worker LLM override.
    ///
    /// When `Some`, the worker runs with this LLM config instead of inheriting
    /// `[agent.llm]`. The resolved `context_window` drives per-worker budget
    /// math (e.g. scratchpad sizing, LOG-23439).
    #[serde(default)]
    pub llm: Option<LlmConfig>,
}

// ============================================================================
// Timeout Sub-Config
// ============================================================================

/// Timeout configuration for orchestration LLM calls.
///
/// # Example
///
/// ```toml
/// [orchestration.timeouts]
/// per_call_timeout_secs = 120
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutsConfig {
    /// Per-call timeout (seconds) for coordinator and worker LLM calls.
    ///
    /// Each individual `.chat()` call (planning, synthesis, evaluation, worker task)
    /// is wrapped with this timeout. Prevents a single hung LLM call from blocking
    /// the request.
    ///
    /// Default: 0 (disabled). Set to a positive value to enable per-call timeouts.
    #[serde(default = "default_per_call_timeout_secs")]
    pub per_call_timeout_secs: u64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            per_call_timeout_secs: default_per_call_timeout_secs(),
        }
    }
}

// ============================================================================
// Artifacts Sub-Config
// ============================================================================

/// Artifact and persistence configuration for orchestration.
///
/// # Example
///
/// ```toml
/// [orchestration.artifacts]
/// memory_dir = "/tmp/aura-orchestration"
/// result_artifact_threshold = 4000
/// result_summary_length = 2000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactsConfig {
    /// Optional base directory for execution persistence and plan storage.
    ///
    /// When set, detailed execution artifacts are written for debugging and
    /// retry intelligence. Each run creates a subdirectory with:
    /// - Planning prompts and responses
    /// - Worker task prompts and responses
    /// - Tool call records with reasoning
    /// - Synthesis and result artifacts
    ///
    /// Structure: `<memory_dir>/<run_id>/iteration-{n}/...`
    /// A `latest` symlink points to the most recent run.
    ///
    /// If not set, execution persistence is disabled.
    #[serde(default, alias = "memory_path")]
    pub memory_dir: Option<String>,

    /// Character threshold for writing large results to artifact files.
    /// Results exceeding this length are written to disk with an inline summary.
    /// Default: 4000 characters.
    #[serde(default = "default_result_artifact_threshold")]
    pub result_artifact_threshold: usize,

    /// Maximum length of inline summary when a result is written to an artifact.
    /// Default: 2000 characters.
    #[serde(default = "default_result_summary_length")]
    pub result_summary_length: usize,

    /// Maximum number of prior run manifests auto-injected into the coordinator
    /// preamble as session context. Set to 0 to disable session history injection.
    /// Default: 3.
    #[serde(default = "default_session_history_turns")]
    pub session_history_turns: usize,
}

fn default_session_history_turns() -> usize {
    3
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            memory_dir: None,
            result_artifact_threshold: default_result_artifact_threshold(),
            result_summary_length: default_result_summary_length(),
            session_history_turns: default_session_history_turns(),
        }
    }
}

// ============================================================================
// Orchestration Config (main)
// ============================================================================

/// Configuration for orchestration mode.
///
/// When `enabled` is true, the agent operates in orchestrated mode where
/// a coordinator agent decomposes queries into tasks executed by worker agents.
/// The coordinator's system prompt comes from `[agent].system_prompt`.
///
/// # Turn Depth
///
/// Coordinator and worker turn depths are derived from `[agent].turn_depth`
/// (the universal default). Per-worker overrides via `[orchestration.worker.<name>].turn_depth`
/// take precedence. Synthesis (4) and evaluation (1) depths are hardcoded.
///
/// # Example
///
/// ```toml
/// [orchestration]
/// enabled = true
/// max_planning_cycles = 3
/// quality_threshold = 0.8
/// max_plan_parse_retries = 3
///
/// [orchestration.timeouts]
/// per_call_timeout_secs = 120
///
/// [orchestration.artifacts]
/// memory_dir = "/tmp/aura-orchestration"
/// ```
///
/// # Backward Compatibility
///
/// The following flat fields are still accepted at the `[orchestration]` level
/// and mapped into their sub-tables during deserialization:
/// - `memory_dir` / `memory_path` → `artifacts.memory_dir`
/// - `result_artifact_threshold` → `artifacts.result_artifact_threshold`
/// - `result_summary_length` → `artifacts.result_summary_length`
#[derive(Debug, Clone, Serialize)]
pub struct OrchestrationConfig {
    // --- Mode ---
    /// Whether orchestration mode is enabled.
    /// When false (default), standard single-agent streaming is used.
    pub enabled: bool,

    // --- Planning loop ---
    /// Maximum number of plan-execute-synthesize cycles.
    pub max_planning_cycles: usize,

    /// Quality threshold (0.0-1.0) for early termination.
    pub quality_threshold: f32,

    /// Maximum number of plan parse retries before falling back to single-task.
    pub max_plan_parse_retries: usize,

    // --- Routing ---
    /// Allow the coordinator to answer simple queries directly without orchestration.
    pub allow_direct_answers: bool,

    /// Allow the coordinator to request clarification for ambiguous queries.
    pub allow_clarification: bool,

    // --- Planning display ---
    /// Controls how tool information is shown to the coordinator during planning.
    pub tools_in_planning: ToolVisibility,

    /// Truncates tool list per worker with "(+N more)" suffix. Default: 10.
    pub max_tools_per_worker: usize,

    // --- Worker defaults ---
    /// Custom system prompt to inject into worker agents.
    pub worker_system_prompt: Option<String>,

    /// Specialized worker configurations.
    pub workers: HashMap<String, WorkerConfig>,

    // --- Coordinator ---
    /// Vector stores available to the coordinator agent.
    pub coordinator_vector_stores: Vec<String>,

    // --- Safety ---
    /// Maximum consecutive duplicate tool calls before rejection.
    ///
    /// When a worker calls the same tool with identical arguments and receives
    /// the same result this many times in a row, subsequent calls are rejected
    /// with a nudge to emit the final answer. Prevents infinite ReAct loops
    /// in smaller/quantized models.
    ///
    /// Default: 2. Set to `None` / omit to use default. Set to a high value
    /// (e.g., 999) to effectively disable.
    pub max_consecutive_duplicate_tool_calls: Option<usize>,

    // --- Sub-configs ---
    /// Timeout settings for LLM calls.
    pub timeouts: TimeoutsConfig,

    /// Artifact and persistence settings.
    pub artifacts: ArtifactsConfig,
}

impl OrchestrationConfig {
    /// Per-call timeout (seconds) for coordinator and worker LLM calls.
    pub fn per_call_timeout_secs(&self) -> u64 {
        self.timeouts.per_call_timeout_secs
    }

    /// Optional memory/persistence directory.
    pub fn memory_dir(&self) -> Option<&str> {
        self.artifacts.memory_dir.as_deref()
    }

    /// Character threshold for artifact extraction.
    pub fn result_artifact_threshold(&self) -> usize {
        self.artifacts.result_artifact_threshold
    }

    /// Maximum summary length for artifact extraction.
    pub fn result_summary_length(&self) -> usize {
        self.artifacts.result_summary_length
    }

    /// Maximum prior run manifests to inject as session context.
    pub fn session_history_turns(&self) -> usize {
        self.artifacts.session_history_turns
    }
}

impl OrchestrationConfig {
    /// Build the coordinator's system prompt by composing the orchestrator
    /// framework template with the user's domain-specific system prompt.
    ///
    /// Layering: orchestration instructions → user system prompt → (worker
    /// details injected into user message by the planning prompt).
    ///
    /// The `agent_system_prompt` parameter is `[agent].system_prompt` from config.
    pub fn build_coordinator_preamble(
        &self,
        agent_system_prompt: &str,
        include_recon_tools: bool,
    ) -> String {
        let tools_section = if include_recon_tools {
            "You have three **routing tools** (`respond_directly`, `create_plan`, `request_clarification`), \
             two **reconnaissance tools** (`list_tools`, `inspect_tool_params`), and one **artifact tool** \
             (`read_artifact`). Call exactly one routing tool per query."
        } else {
            "You have three **routing tools** (`respond_directly`, `create_plan`, `request_clarification`) \
             and one **artifact tool** (`read_artifact`). Call exactly one routing tool per query."
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

        let mut preamble = ORCHESTRATOR_PREAMBLE_TEMPLATE
            .replace("{{orchestration_system_prompt}}", agent_system_prompt)
            .replace("{{tools_section}}", tools_section)
            .replace("{{recon_guidance}}", recon_guidance);

        // AURA_ESCAPE_HATCH=false strips the "Resolve tool gaps" directive for A/B testing
        if std::env::var("AURA_ESCAPE_HATCH")
            .map(|v| v == "false" || v == "0")
            .unwrap_or(false)
        {
            tracing::info!("AURA_ESCAPE_HATCH=false — stripping escape hatch directive");
            preamble = preamble.replace(
                "5. **Resolve tool gaps pragmatically**: If a user requests an operation with no matching tool, create a plan using the available tools and note the gap in `planning_summary`. Do NOT deliberate at length about missing capabilities — route what you can, report what you cannot.\n",
                "",
            );
        }

        preamble
    }

    /// Build the complete worker preamble by injecting the custom system prompt
    /// into the worker template.
    ///
    /// The template contains `{{worker_system_prompt}}` which is replaced with
    /// the user's custom prompt, or a default message if none is provided.
    pub fn build_worker_preamble(&self) -> String {
        let custom_prompt = self
            .worker_system_prompt
            .as_deref()
            .unwrap_or("(No custom instructions provided)");

        WORKER_PREAMBLE_TEMPLATE.replace("{{worker_system_prompt}}", custom_prompt)
    }

    /// Check if specialized workers are configured.
    ///
    /// When true, tasks should be assigned to specific workers during planning.
    /// When false, all tasks use the generic worker preamble.
    pub fn has_workers(&self) -> bool {
        !self.workers.is_empty()
    }

    /// Get a worker configuration by name.
    ///
    /// Returns `None` if the worker doesn't exist.
    pub fn get_worker(&self, name: &str) -> Option<&WorkerConfig> {
        self.workers.get(name)
    }

    /// Get the names of all configured workers.
    ///
    /// Used to include available workers in the planning prompt.
    pub fn available_worker_names(&self) -> Vec<&str> {
        self.workers.keys().map(|s| s.as_str()).collect()
    }

    /// Format worker descriptions for the planning prompt.
    ///
    /// Returns a formatted string listing all workers with their descriptions.
    /// Example output:
    /// ```text
    /// - operations: For logs, pipelines, metrics, and system analysis
    /// - knowledge: For documentation, procedures, and best practices
    /// ```
    pub fn format_workers_for_prompt(&self) -> String {
        if self.workers.is_empty() {
            return String::new();
        }

        self.workers
            .iter()
            .map(|(name, config)| format!("- {}: {}", name, config.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_planning_cycles: default_max_planning_cycles(),
            quality_threshold: default_quality_threshold(),
            max_plan_parse_retries: default_max_plan_parse_retries(),
            allow_direct_answers: true,
            allow_clarification: true,
            tools_in_planning: default_tools_in_planning(),
            max_tools_per_worker: default_max_tools_per_worker(),
            worker_system_prompt: None,
            workers: HashMap::new(),
            coordinator_vector_stores: Vec::new(),
            max_consecutive_duplicate_tool_calls: None,
            timeouts: TimeoutsConfig::default(),
            artifacts: ArtifactsConfig::default(),
        }
    }
}

// ============================================================================
// Custom Deserialization (backward compatibility)
// ============================================================================

/// Intermediate struct for deserializing both flat and sub-table format.
///
/// Accepts:
/// - New format: `[orchestration.timeouts]` and `[orchestration.artifacts]` sub-tables
/// - Old format: `memory_dir`, `memory_path`, `result_artifact_threshold`,
///   `result_summary_length` at root level
///
/// Flat fields take precedence over sub-table values when both are present
/// (shouldn't happen in practice, but provides predictable behavior).
#[derive(Deserialize)]
struct RawOrchestrationConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_max_planning_cycles")]
    max_planning_cycles: usize,
    #[serde(default = "default_quality_threshold")]
    quality_threshold: f32,
    #[serde(default = "default_max_plan_parse_retries")]
    max_plan_parse_retries: usize,
    #[serde(default = "default_true")]
    allow_direct_answers: bool,
    #[serde(default = "default_true")]
    allow_clarification: bool,
    #[serde(default = "default_tools_in_planning")]
    tools_in_planning: ToolVisibility,
    #[serde(default = "default_max_tools_per_worker")]
    max_tools_per_worker: usize,
    #[serde(default)]
    worker_system_prompt: Option<String>,
    #[serde(default, rename = "worker")]
    workers: HashMap<String, WorkerConfig>,
    #[serde(default)]
    coordinator_vector_stores: Vec<String>,
    #[serde(default)]
    max_consecutive_duplicate_tool_calls: Option<usize>,

    // --- Sub-tables ---
    #[serde(default)]
    timeouts: Option<TimeoutsConfig>,
    #[serde(default)]
    artifacts: Option<ArtifactsConfig>,

    // --- Flat artifact fields (backward compat) ---
    #[serde(default, alias = "memory_path")]
    memory_dir: Option<String>,
    #[serde(default)]
    result_artifact_threshold: Option<usize>,
    #[serde(default)]
    result_summary_length: Option<usize>,
    #[serde(default)]
    session_history_turns: Option<usize>,
}

impl<'de> Deserialize<'de> for OrchestrationConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawOrchestrationConfig::deserialize(deserializer)?;

        let timeouts = raw.timeouts.unwrap_or_default();

        // Build artifacts: flat fields override sub-table defaults
        let mut artifacts = raw.artifacts.unwrap_or_default();
        if let Some(v) = raw.memory_dir {
            artifacts.memory_dir = Some(v);
        }
        if let Some(v) = raw.result_artifact_threshold {
            artifacts.result_artifact_threshold = v;
        }
        if let Some(v) = raw.result_summary_length {
            artifacts.result_summary_length = v;
        }
        if let Some(v) = raw.session_history_turns {
            artifacts.session_history_turns = v;
        }

        Ok(OrchestrationConfig {
            enabled: raw.enabled,
            max_planning_cycles: raw.max_planning_cycles,
            quality_threshold: raw.quality_threshold,
            max_plan_parse_retries: raw.max_plan_parse_retries,
            allow_direct_answers: raw.allow_direct_answers,
            allow_clarification: raw.allow_clarification,
            tools_in_planning: raw.tools_in_planning,
            max_tools_per_worker: raw.max_tools_per_worker,
            worker_system_prompt: raw.worker_system_prompt,
            workers: raw.workers,
            coordinator_vector_stores: raw.coordinator_vector_stores,
            max_consecutive_duplicate_tool_calls: raw.max_consecutive_duplicate_tool_calls,
            timeouts,
            artifacts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let config = OrchestrationConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_planning_cycles, 3);
        assert!((config.quality_threshold - 0.8).abs() < f32::EPSILON);
        assert_eq!(config.max_plan_parse_retries, 3);
        assert!(config.worker_system_prompt.is_none());
        assert!(config.workers.is_empty());
        assert!(!config.has_workers());
        // Capability-aware planning defaults
        assert_eq!(config.tools_in_planning, ToolVisibility::Summary);
        assert_eq!(config.max_tools_per_worker, 10);
        // Vector store defaults (none)
        assert!(config.coordinator_vector_stores.is_empty());
        // Timeout defaults (sub-table) — per_call disabled by default
        assert_eq!(config.per_call_timeout_secs(), 0);
        // Artifact threshold defaults (sub-table)
        assert_eq!(config.result_artifact_threshold(), 4000);
        assert_eq!(config.result_summary_length(), 2000);
        assert!(config.memory_dir().is_none());
        assert_eq!(config.session_history_turns(), 3);
    }

    #[test]
    fn test_deserialize_minimal() {
        let toml = r#"
            enabled = true
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.max_planning_cycles, 3);
        assert!((config.quality_threshold - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn test_deserialize_full() {
        let toml = r#"
            enabled = true
            max_planning_cycles = 5
            quality_threshold = 0.9
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.max_planning_cycles, 5);
        assert!((config.quality_threshold - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn test_build_worker_preamble_with_custom_prompt() {
        let config = OrchestrationConfig {
            worker_system_prompt: Some("Focus on data analysis.".to_string()),
            ..Default::default()
        };

        let preamble = config.build_worker_preamble();
        assert!(preamble.contains("Focus on data analysis."));
        assert!(preamble.contains("Worker Agent"));
        assert!(!preamble.contains("{{worker_system_prompt}}"));
    }

    #[test]
    fn test_build_worker_preamble_without_custom_prompt() {
        let config = OrchestrationConfig::default();
        let preamble = config.build_worker_preamble();

        assert!(preamble.contains("(No custom instructions provided)"));
        assert!(preamble.contains("Worker Agent"));
        assert!(!preamble.contains("{{worker_system_prompt}}"));
    }

    #[test]
    fn test_worker_preamble_template_loaded() {
        // Verify compile-time include works
        assert!(!WORKER_PREAMBLE_TEMPLATE.is_empty());
        assert!(WORKER_PREAMBLE_TEMPLATE.contains("{{worker_system_prompt}}"));
        assert!(WORKER_PREAMBLE_TEMPLATE.contains("Worker Agent"));
    }

    #[test]
    fn test_coordinator_preamble_injects_agent_system_prompt() {
        let config = OrchestrationConfig::default();
        let preamble = config.build_coordinator_preamble("Focus on thorough testing.", true);

        // Framework instructions present
        assert!(preamble.contains("respond_directly"));
        assert!(preamble.contains("create_plan"));
        // User's system prompt injected
        assert!(preamble.contains("Focus on thorough testing."));
        // Placeholder replaced
        assert!(!preamble.contains("{{orchestration_system_prompt}}"));
    }

    #[test]
    fn test_coordinator_preamble_with_empty_system_prompt() {
        let config = OrchestrationConfig::default();
        let preamble = config.build_coordinator_preamble("", true);

        // Framework instructions still present
        assert!(preamble.contains("create_plan"));
        assert!(preamble.contains("Orchestration Coordinator"));
        assert!(!preamble.contains("{{orchestration_system_prompt}}"));
    }

    #[test]
    fn test_orchestrator_preamble_template_loaded() {
        assert!(!ORCHESTRATOR_PREAMBLE_TEMPLATE.is_empty());
        assert!(ORCHESTRATOR_PREAMBLE_TEMPLATE.contains("{{orchestration_system_prompt}}"));
        assert!(ORCHESTRATOR_PREAMBLE_TEMPLATE.contains("{{tools_section}}"));
        assert!(ORCHESTRATOR_PREAMBLE_TEMPLATE.contains("{{recon_guidance}}"));
    }

    #[test]
    fn test_coordinator_preamble_without_recon_tools() {
        let config = OrchestrationConfig::default();
        let preamble = config.build_coordinator_preamble("Test prompt.", false);

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
        let config = OrchestrationConfig::default();
        let preamble = config.build_coordinator_preamble("Test prompt.", true);

        assert!(preamble.contains("reconnaissance tools"));
        assert!(preamble.contains("## Reconnaissance Guidance"));
        assert!(preamble.contains("inspect_tool_params"));
    }

    // ========================================================================
    // WorkerConfig Tests
    // ========================================================================

    #[test]
    fn test_worker_config_deserialize() {
        let toml = r#"
            description = "For operations"
            preamble = "You are an Operations Specialist."
            mcp_filter = ["mezmo_*"]
        "#;
        let config: WorkerConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.description, "For operations");
        assert_eq!(config.preamble, "You are an Operations Specialist.");
        assert_eq!(config.mcp_filter, vec!["mezmo_*"]);
    }

    #[test]
    fn test_worker_config_empty_filter() {
        let toml = r#"
            description = "Generic tasks"
            preamble = "Generic worker."
        "#;
        let config: WorkerConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.preamble, "Generic worker.");
        assert!(config.mcp_filter.is_empty());
    }

    #[test]
    fn test_worker_config_multiple_filters() {
        let toml = r#"
            description = "For knowledge retrieval"
            preamble = "Knowledge worker."
            mcp_filter = ["ListKnowledgeBases", "QueryKnowledgeBases"]
        "#;
        let config: WorkerConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.mcp_filter.len(), 2);
        assert!(
            config
                .mcp_filter
                .contains(&"ListKnowledgeBases".to_string())
        );
        assert!(
            config
                .mcp_filter
                .contains(&"QueryKnowledgeBases".to_string())
        );
    }

    #[test]
    fn test_orchestration_with_workers() {
        let toml = r#"
            enabled = true

            [worker.operations]
            description = "For logs and pipelines"
            preamble = "Operations specialist."
            mcp_filter = ["mezmo_*"]

            [worker.knowledge]
            description = "For documentation"
            preamble = "Knowledge specialist."
            mcp_filter = ["ListKnowledgeBases", "QueryKnowledgeBases"]
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        assert!(config.enabled);
        assert!(config.has_workers());
        assert_eq!(config.workers.len(), 2);

        // Check operations worker
        let ops = config.get_worker("operations").unwrap();
        assert_eq!(ops.description, "For logs and pipelines");
        assert_eq!(ops.preamble, "Operations specialist.");
        assert_eq!(ops.mcp_filter, vec!["mezmo_*"]);

        // Check knowledge worker
        let kb = config.get_worker("knowledge").unwrap();
        assert_eq!(kb.description, "For documentation");
        assert_eq!(kb.preamble, "Knowledge specialist.");
        assert_eq!(kb.mcp_filter.len(), 2);
    }

    #[test]
    fn test_available_worker_names() {
        let toml = r#"
            enabled = true

            [worker.alpha]
            description = "Alpha tasks"
            preamble = "Alpha worker."

            [worker.beta]
            description = "Beta tasks"
            preamble = "Beta worker."
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        let names = config.available_worker_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn test_get_worker_not_found() {
        let config = OrchestrationConfig::default();
        assert!(config.get_worker("nonexistent").is_none());
    }

    #[test]
    fn test_format_workers_for_prompt() {
        let toml = r#"
            enabled = true

            [worker.operations]
            description = "For logs and pipelines"
            preamble = "Operations."
            mcp_filter = ["mezmo_*"]

            [worker.general]
            description = "For general tasks"
            preamble = "General."
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        let prompt = config.format_workers_for_prompt();
        assert!(prompt.contains("operations"));
        assert!(prompt.contains("general"));
        assert!(prompt.contains("For logs and pipelines"));
        assert!(prompt.contains("For general tasks"));
    }

    #[test]
    fn test_format_workers_empty() {
        let config = OrchestrationConfig::default();
        let prompt = config.format_workers_for_prompt();
        assert!(prompt.is_empty());
    }

    // ========================================================================
    // ToolVisibility Tests
    // ========================================================================

    #[test]
    fn test_tool_visibility_default() {
        assert_eq!(ToolVisibility::default(), ToolVisibility::Summary);
    }

    #[test]
    fn test_tool_visibility_deserialize_none() {
        let toml = r#"tools_in_planning = "none""#;
        #[derive(Deserialize)]
        struct TestConfig {
            tools_in_planning: ToolVisibility,
        }
        let config: TestConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.tools_in_planning, ToolVisibility::None);
    }

    #[test]
    fn test_tool_visibility_deserialize_summary() {
        let toml = r#"tools_in_planning = "summary""#;
        #[derive(Deserialize)]
        struct TestConfig {
            tools_in_planning: ToolVisibility,
        }
        let config: TestConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.tools_in_planning, ToolVisibility::Summary);
    }

    #[test]
    fn test_tool_visibility_deserialize_full() {
        let toml = r#"tools_in_planning = "full""#;
        #[derive(Deserialize)]
        struct TestConfig {
            tools_in_planning: ToolVisibility,
        }
        let config: TestConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.tools_in_planning, ToolVisibility::Full);
    }

    #[test]
    fn test_capability_aware_planning_config() {
        let toml = r#"
            enabled = true
            tools_in_planning = "full"
            max_tools_per_worker = 5
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        assert!(config.enabled);
        assert_eq!(config.tools_in_planning, ToolVisibility::Full);
        assert_eq!(config.max_tools_per_worker, 5);
    }

    #[test]
    fn test_capability_aware_planning_defaults_when_omitted() {
        let toml = r#"enabled = true"#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        // Should use defaults when not specified
        assert_eq!(config.tools_in_planning, ToolVisibility::Summary);
        assert_eq!(config.max_tools_per_worker, 10);
    }

    #[test]
    fn test_backward_compat_existing_config() {
        // Simulate an existing config without new fields
        let toml = r#"
            enabled = true
            max_planning_cycles = 5
            quality_threshold = 0.9

            [worker.operations]
            description = "For logs"
            preamble = "Operations."
            mcp_filter = ["mezmo_*"]
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        // Old fields work
        assert!(config.enabled);
        assert_eq!(config.max_planning_cycles, 5);
        assert!(config.has_workers());

        // New fields have defaults
        assert_eq!(config.tools_in_planning, ToolVisibility::Summary);
        assert_eq!(config.max_tools_per_worker, 10);
    }

    // ========================================================================
    // Vector Store Config Tests
    // ========================================================================

    #[test]
    fn test_coordinator_vector_stores() {
        let toml = r#"
            enabled = true
            coordinator_vector_stores = ["runbooks", "procedures"]
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        assert_eq!(config.coordinator_vector_stores.len(), 2);
        assert!(
            config
                .coordinator_vector_stores
                .contains(&"runbooks".to_string())
        );
        assert!(
            config
                .coordinator_vector_stores
                .contains(&"procedures".to_string())
        );
    }

    #[test]
    fn test_coordinator_vector_stores_empty_default() {
        let toml = r#"enabled = true"#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        // Default: coordinator has no vector stores
        assert!(config.coordinator_vector_stores.is_empty());
    }

    #[test]
    fn test_worker_vector_stores() {
        let toml = r#"
            enabled = true

            [worker.knowledge]
            description = "Knowledge specialist"
            preamble = "You are a Knowledge Specialist."
            vector_stores = ["mezmo_docs", "customer_kb"]
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        let worker = config.get_worker("knowledge").unwrap();
        assert_eq!(worker.vector_stores.len(), 2);
        assert!(worker.vector_stores.contains(&"mezmo_docs".to_string()));
        assert!(worker.vector_stores.contains(&"customer_kb".to_string()));
    }

    #[test]
    fn test_worker_vector_stores_empty_default() {
        let toml = r#"
            enabled = true

            [worker.operations]
            description = "Operations specialist"
            preamble = "You are an Operations Specialist."
            mcp_filter = ["mezmo_*"]
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        let worker = config.get_worker("operations").unwrap();
        // Default: worker has no vector stores (must explicitly opt-in)
        assert!(worker.vector_stores.is_empty());
    }

    #[test]
    fn test_mixed_worker_vector_stores() {
        let toml = r#"
            enabled = true

            [worker.knowledge]
            description = "Has RAG access"
            preamble = "Knowledge worker."
            vector_stores = ["docs"]

            [worker.operations]
            description = "No RAG access"
            preamble = "Operations worker."
            mcp_filter = ["mezmo_*"]
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        // Knowledge worker has vector stores
        let knowledge = config.get_worker("knowledge").unwrap();
        assert_eq!(knowledge.vector_stores, vec!["docs"]);

        // Operations worker has no vector stores (empty default)
        let operations = config.get_worker("operations").unwrap();
        assert!(operations.vector_stores.is_empty());
    }

    // ========================================================================
    // build_vector_store_context Tests
    // ========================================================================

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
            store_type: "qdrant".to_string(),
            embedding_model: crate::config::EmbeddingModelConfig {
                provider: "openai".to_string(),
                model: "text-embedding-3-small".to_string(),
                api_key: "test".to_string(),
                base_url: None,
            },
            connection_string: None,
            url: None,
            collection_name: None,
            context_prefix: Some("Mezmo documentation and procedures".to_string()),
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
            store_type: "qdrant".to_string(),
            embedding_model: crate::config::EmbeddingModelConfig {
                provider: "openai".to_string(),
                model: "test".to_string(),
                api_key: "test".to_string(),
                base_url: None,
            },
            connection_string: None,
            url: None,
            collection_name: None,
            context_prefix: None, // No description
        }];

        let context = build_vector_store_context(&stores);
        assert!(context.contains("**test_kb**"));
        assert!(context.contains("No description provided"));
    }

    // ========================================================================
    // Backward Compatibility + Sub-Table Tests
    // ========================================================================

    #[test]
    fn test_backward_compat_flat_artifacts() {
        let toml = r#"
            enabled = true
            memory_dir = "/tmp/test"
            result_artifact_threshold = 5000
            result_summary_length = 3000
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.memory_dir(), Some("/tmp/test"));
        assert_eq!(config.result_artifact_threshold(), 5000);
        assert_eq!(config.result_summary_length(), 3000);
    }

    #[test]
    fn test_new_sub_table_format() {
        let toml = r#"
            enabled = true

            [timeouts]
            per_call_timeout_secs = 45

            [artifacts]
            memory_dir = "/tmp/new-style"
            result_artifact_threshold = 8000
            result_summary_length = 1500
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.per_call_timeout_secs(), 45);
        assert_eq!(config.memory_dir(), Some("/tmp/new-style"));
        assert_eq!(config.result_artifact_threshold(), 8000);
        assert_eq!(config.result_summary_length(), 1500);
    }

    #[test]
    fn test_new_field_defaults() {
        let toml = r#"enabled = true"#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.max_plan_parse_retries, 3);
        assert_eq!(config.per_call_timeout_secs(), 0);
    }

    #[test]
    fn test_memory_path_alias_in_artifacts() {
        let toml = r#"
            enabled = true

            [artifacts]
            memory_path = "/tmp/alias-test"
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.memory_dir(), Some("/tmp/alias-test"));
    }

    #[test]
    fn test_flat_memory_path_alias() {
        let toml = r#"
            enabled = true
            memory_path = "/tmp/flat-alias"
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.memory_dir(), Some("/tmp/flat-alias"));
    }

    // ========================================================================
    // Session History Turns Tests
    // ========================================================================

    #[test]
    fn test_session_history_turns_default() {
        let config = OrchestrationConfig::default();
        assert_eq!(config.session_history_turns(), 3);
    }

    #[test]
    fn test_session_history_turns_in_artifacts_sub_table() {
        let toml = r#"
            enabled = true

            [artifacts]
            memory_dir = "/tmp/test"
            session_history_turns = 5
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.session_history_turns(), 5);
    }

    #[test]
    fn test_session_history_turns_flat_backward_compat() {
        let toml = r#"
            enabled = true
            session_history_turns = 0
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.session_history_turns(), 0);
    }

    #[test]
    fn test_session_history_turns_flat_overrides_sub_table() {
        let toml = r#"
            enabled = true
            session_history_turns = 7

            [artifacts]
            session_history_turns = 2
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        // Flat field takes precedence
        assert_eq!(config.session_history_turns(), 7);
    }

    #[test]
    fn test_session_history_turns_omitted_uses_default() {
        let toml = r#"
            enabled = true
            memory_dir = "/tmp/test"
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.session_history_turns(), 3);
    }
}
