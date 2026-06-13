//! Configuration types for orchestration mode.
//!
//! These are the pure, serializable orchestration knobs. The runtime helpers
//! that depend on aura's prompt templates (coordinator/worker preamble
//! building, vector-store context strings) live in the `aura` crate's
//! `orchestration::config` module.

use crate::config::LlmConfig;
use crate::scratchpad::ScratchpadConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

// ============================================================================
// Per-Worker Configuration
// ============================================================================

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
    #[serde(default)]
    pub vector_stores: Vec<String>,

    /// Max tool-calling turns for this worker.
    ///
    /// Controls how many Rig ReAct turns (tool calls) a worker can make
    /// per task execution. Overrides `[agent].turn_depth`. Falls back to
    /// `[agent].turn_depth` → `DEFAULT_MAX_DEPTH` if not set.
    #[serde(default)]
    pub turn_depth: Option<usize>,

    /// Optional per-worker LLM override.
    ///
    /// When `Some`, the worker runs with this LLM config instead of inheriting
    /// `[agent.llm]`. The resolved `context_window` drives per-worker budget
    /// math (e.g. scratchpad sizing).
    #[serde(default)]
    pub llm: Option<LlmConfig>,

    /// Per-worker override of `[agent.scratchpad]`. Parsed from
    /// `[orchestration.worker.<name>.scratchpad]`.
    #[serde(default)]
    pub scratchpad: Option<ScratchpadConfig>,
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
    /// Each individual `.chat()` call (planning, continuation, worker task)
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
    /// Structure: `<memory_dir>/<run_id>/iteration-{n}/...`
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

    /// Total byte budget for dependency context injected into a worker prompt.
    /// Direct dependencies always render in full; transitive ancestors render
    /// in full until this budget is reached, then degrade to compact previews.
    /// Default: 32000 (8x result_artifact_threshold, sized to the same
    /// continuation-prompt envelope that fits all 200K-window models).
    #[serde(default = "default_dependency_context_budget")]
    pub dependency_context_budget: usize,

    /// Maximum number of prior run manifests auto-injected into the coordinator
    /// preamble as session context. Set to 0 to disable session history injection.
    /// Default: 3.
    #[serde(default = "default_session_history_turns")]
    pub session_history_turns: usize,

    /// Timeout (ms) for draining in-flight persistence writes between execute()
    /// and write_plan(). Default: 2000ms.
    #[serde(default = "default_persistence_drain_timeout_ms")]
    pub persistence_drain_timeout_ms: u64,

    /// Character threshold for promoting tool outputs to artifact files.
    /// Outputs exceeding this size get written to artifacts/ with an inline
    /// footer referencing the file. Set to 0 to promote all tool outputs.
    /// Default: 500.
    #[serde(default = "default_tool_output_artifact_threshold")]
    pub tool_output_artifact_threshold: usize,

    /// Duration threshold (ms) for promoting tool outputs to artifact files.
    /// Tool calls exceeding this duration get promoted regardless of size.
    /// Set to 0 to disable duration-based promotion. Default: 5000ms.
    #[serde(default = "default_tool_output_duration_threshold_ms")]
    pub tool_output_duration_threshold_ms: u64,

    /// When true, include condensed tool reasoning traces in the continuation
    /// prompt so the coordinator can see why workers called specific tools.
    /// Default: false.
    #[serde(default)]
    pub show_tool_reasoning_in_continuation: bool,

    /// Maximum number of run directories retained per session. When a new run
    /// is created and the directory count exceeds this limit, the oldest runs
    /// are pruned. Set to 0 to disable pruning. Default: 20.
    #[serde(default = "default_max_session_runs")]
    pub max_session_runs: usize,
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            memory_dir: None,
            result_artifact_threshold: default_result_artifact_threshold(),
            result_summary_length: default_result_summary_length(),
            dependency_context_budget: default_dependency_context_budget(),
            session_history_turns: default_session_history_turns(),
            persistence_drain_timeout_ms: default_persistence_drain_timeout_ms(),
            tool_output_artifact_threshold: default_tool_output_artifact_threshold(),
            tool_output_duration_threshold_ms: default_tool_output_duration_threshold_ms(),
            show_tool_reasoning_in_continuation: false,
            max_session_runs: default_max_session_runs(),
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
/// Uses custom deserialization for backward compatibility with flat field
/// format (e.g. `memory_dir` at the `[orchestration]` level maps into
/// `artifacts.memory_dir`).
#[derive(Debug, Clone, Serialize)]
pub struct OrchestrationConfig {
    // --- Mode ---
    /// Whether orchestration mode is enabled.
    /// When false (default), standard single-agent streaming is used.
    pub enabled: bool,

    // --- Planning loop ---
    /// Maximum number of plan-execute-continue cycles.
    pub max_planning_cycles: usize,

    /// Maximum number of plan parse retries before falling back to single-task.
    pub max_plan_parse_retries: usize,

    // --- Worker defaults ---
    /// Custom system prompt to inject into worker agents.
    pub worker_system_prompt: Option<String>,

    /// Specialized worker configurations.
    pub workers: HashMap<String, WorkerConfig>,

    // --- Coordinator ---
    /// Vector stores available to the coordinator agent.
    pub coordinator_vector_stores: Vec<String>,

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

    // --- Safety ---
    /// Consecutive identical tool calls before appending guidance annotation.
    pub duplicate_call_nudge_threshold: usize,

    /// Consecutive identical tool calls before appending abort annotation
    /// and setting the escalation flag.
    pub duplicate_call_block_threshold: usize,

    // --- Sub-configs ---
    /// Timeout settings for LLM calls.
    pub timeouts: TimeoutsConfig,

    /// Artifact and persistence settings.
    pub artifacts: ArtifactsConfig,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_planning_cycles: default_max_planning_cycles(),
            max_plan_parse_retries: default_max_plan_parse_retries(),
            worker_system_prompt: None,
            workers: HashMap::new(),
            coordinator_vector_stores: Vec::new(),
            allow_direct_answers: true,
            allow_clarification: true,
            tools_in_planning: ToolVisibility::default(),
            max_tools_per_worker: default_max_tools_per_worker(),
            duplicate_call_nudge_threshold: default_duplicate_call_nudge_threshold(),
            duplicate_call_block_threshold: default_duplicate_call_block_threshold(),
            timeouts: TimeoutsConfig::default(),
            artifacts: ArtifactsConfig::default(),
        }
    }
}

impl OrchestrationConfig {
    /// Validate that worker names are well-formed and unique.
    ///
    /// The TOML parser already rejects exact duplicate `[orchestration.worker.X]`
    /// headers, but several latent gaps remain that downstream code (artifact
    /// filename namespacing, per-worker LLM resolution, scratchpad budgeting)
    /// quietly depends on:
    ///
    /// - Empty names: TOML allows `[orchestration.worker.""]`.
    /// - Case-insensitive collisions: `Alpha` and `alpha` are distinct in TOML
    ///   and in [`HashMap`], but collide as filenames on case-insensitive
    ///   filesystems (macOS APFS default, Windows NTFS default).
    pub fn validate_worker_names(&self) -> Result<(), crate::ConfigError> {
        use std::collections::HashMap;

        let mut seen_lower: HashMap<String, &str> = HashMap::new();
        for name in self.workers.keys() {
            if name.trim().is_empty() {
                return Err(crate::ConfigError::Validation(
                    "Empty worker name in [orchestration.worker.*]; \
                     worker section headers must have a non-empty name"
                        .to_string(),
                ));
            }
            let lower = name.to_lowercase();
            if let Some(existing) = seen_lower.insert(lower, name.as_str()) {
                return Err(crate::ConfigError::Validation(format!(
                    "Duplicate worker name in [orchestration.worker.*]: \
                     '{}' and '{}' collide (names must be unique \
                     case-insensitively to avoid filesystem collisions in \
                     artifact persistence)",
                    existing, name
                )));
            }
        }
        Ok(())
    }

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

    /// Total byte budget for dependency context in worker prompts.
    pub fn dependency_context_budget(&self) -> usize {
        self.artifacts.dependency_context_budget
    }

    /// Maximum prior run manifests to inject as session context.
    pub fn session_history_turns(&self) -> usize {
        self.artifacts.session_history_turns
    }

    /// Timeout (ms) for draining in-flight persistence writes.
    pub fn persistence_drain_timeout_ms(&self) -> u64 {
        self.artifacts.persistence_drain_timeout_ms
    }

    /// Character threshold for promoting tool outputs to artifacts.
    pub fn tool_output_artifact_threshold(&self) -> usize {
        self.artifacts.tool_output_artifact_threshold
    }

    /// Duration threshold (ms) for promoting tool outputs to artifacts.
    pub fn tool_output_duration_threshold_ms(&self) -> u64 {
        self.artifacts.tool_output_duration_threshold_ms
    }

    /// Maximum run directories retained per session (0 = no pruning).
    pub fn max_session_runs(&self) -> usize {
        self.artifacts.max_session_runs
    }

    /// Whether to include condensed tool reasoning in continuation prompts.
    pub fn show_tool_reasoning_in_continuation(&self) -> bool {
        self.artifacts.show_tool_reasoning_in_continuation
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
/// Flat fields take precedence over sub-table values when both are present.
#[derive(Deserialize)]
struct RawOrchestrationConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_max_planning_cycles")]
    max_planning_cycles: usize,
    #[serde(default = "default_max_plan_parse_retries")]
    max_plan_parse_retries: usize,
    #[serde(default)]
    worker_system_prompt: Option<String>,
    #[serde(default, rename = "worker")]
    workers: HashMap<String, WorkerConfig>,
    #[serde(default)]
    coordinator_vector_stores: Vec<String>,
    #[serde(default = "default_true")]
    allow_direct_answers: bool,
    #[serde(default = "default_true")]
    allow_clarification: bool,
    #[serde(default)]
    tools_in_planning: ToolVisibility,
    #[serde(default = "default_max_tools_per_worker")]
    max_tools_per_worker: usize,
    #[serde(default = "default_duplicate_call_nudge_threshold")]
    duplicate_call_nudge_threshold: usize,
    #[serde(default = "default_duplicate_call_block_threshold")]
    duplicate_call_block_threshold: usize,
    // Sub-tables
    #[serde(default)]
    timeouts: Option<TimeoutsConfig>,
    #[serde(default)]
    artifacts: Option<ArtifactsConfig>,
    // Flat artifact fields (backward compat)
    #[serde(default, alias = "memory_path")]
    memory_dir: Option<String>,
    #[serde(default)]
    result_artifact_threshold: Option<usize>,
    #[serde(default)]
    result_summary_length: Option<usize>,
    #[serde(default)]
    session_history_turns: Option<usize>,
    #[serde(default)]
    persistence_drain_timeout_ms: Option<u64>,
    #[serde(default)]
    tool_output_artifact_threshold: Option<usize>,
    #[serde(default)]
    tool_output_duration_threshold_ms: Option<u64>,
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
        if let Some(v) = raw.persistence_drain_timeout_ms {
            artifacts.persistence_drain_timeout_ms = v;
        }
        if let Some(v) = raw.tool_output_artifact_threshold {
            artifacts.tool_output_artifact_threshold = v;
        }
        if let Some(v) = raw.tool_output_duration_threshold_ms {
            artifacts.tool_output_duration_threshold_ms = v;
        }

        Ok(OrchestrationConfig {
            enabled: raw.enabled,
            max_planning_cycles: raw.max_planning_cycles,
            max_plan_parse_retries: raw.max_plan_parse_retries,
            worker_system_prompt: raw.worker_system_prompt,
            workers: raw.workers,
            coordinator_vector_stores: raw.coordinator_vector_stores,
            allow_direct_answers: raw.allow_direct_answers,
            allow_clarification: raw.allow_clarification,
            tools_in_planning: raw.tools_in_planning,
            max_tools_per_worker: raw.max_tools_per_worker,
            duplicate_call_nudge_threshold: raw.duplicate_call_nudge_threshold,
            duplicate_call_block_threshold: raw.duplicate_call_block_threshold,
            timeouts,
            artifacts,
        })
    }
}

fn default_true() -> bool {
    true
}

fn default_max_planning_cycles() -> usize {
    3
}

fn default_max_tools_per_worker() -> usize {
    10
}

fn default_per_call_timeout_secs() -> u64 {
    0
}

fn default_max_plan_parse_retries() -> usize {
    3
}

fn default_duplicate_call_nudge_threshold() -> usize {
    3
}

fn default_duplicate_call_block_threshold() -> usize {
    5
}

fn default_result_artifact_threshold() -> usize {
    4000
}

fn default_result_summary_length() -> usize {
    2000
}

fn default_dependency_context_budget() -> usize {
    32_000
}

fn default_max_session_runs() -> usize {
    20
}

fn default_session_history_turns() -> usize {
    3
}

fn default_persistence_drain_timeout_ms() -> u64 {
    2000
}

fn default_tool_output_artifact_threshold() -> usize {
    500
}

fn default_tool_output_duration_threshold_ms() -> u64 {
    5000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let config = OrchestrationConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_planning_cycles, 3);
        assert_eq!(config.max_plan_parse_retries, 3);
        assert!(config.worker_system_prompt.is_none());
        assert!(config.workers.is_empty());
        assert!(!config.has_workers());
        assert_eq!(config.tools_in_planning, ToolVisibility::Summary);
        assert_eq!(config.max_tools_per_worker, 10);
        assert!(config.coordinator_vector_stores.is_empty());
        assert_eq!(config.per_call_timeout_secs(), 0);
        assert_eq!(config.result_artifact_threshold(), 4000);
        assert_eq!(config.result_summary_length(), 2000);
        assert_eq!(config.dependency_context_budget(), 32_000);
        assert!(config.memory_dir().is_none());
        assert_eq!(config.session_history_turns(), 3);
        assert_eq!(config.persistence_drain_timeout_ms(), 2000);
        assert_eq!(config.tool_output_artifact_threshold(), 500);
        assert_eq!(config.tool_output_duration_threshold_ms(), 5000);
    }

    #[test]
    fn test_deserialize_minimal() {
        let toml = r#"
            enabled = true
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.max_planning_cycles, 3);
    }

    #[test]
    fn test_deserialize_full() {
        let toml = r#"
            enabled = true
            max_planning_cycles = 5
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.max_planning_cycles, 5);
    }

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

        let ops = config.get_worker("operations").unwrap();
        assert_eq!(ops.description, "For logs and pipelines");
        assert_eq!(ops.preamble, "Operations specialist.");
        assert_eq!(ops.mcp_filter, vec!["mezmo_*"]);

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

    #[test]
    fn test_tool_visibility_default() {
        assert_eq!(ToolVisibility::default(), ToolVisibility::Summary);
    }

    #[test]
    fn test_tool_visibility_deserialize_variants() {
        #[derive(Deserialize)]
        struct TestConfig {
            tools_in_planning: ToolVisibility,
        }
        let none: TestConfig = toml::from_str(r#"tools_in_planning = "none""#).unwrap();
        assert_eq!(none.tools_in_planning, ToolVisibility::None);
        let summary: TestConfig = toml::from_str(r#"tools_in_planning = "summary""#).unwrap();
        assert_eq!(summary.tools_in_planning, ToolVisibility::Summary);
        let full: TestConfig = toml::from_str(r#"tools_in_planning = "full""#).unwrap();
        assert_eq!(full.tools_in_planning, ToolVisibility::Full);
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
    fn test_backward_compat_existing_config() {
        let toml = r#"
            enabled = true
            max_planning_cycles = 5

            [worker.operations]
            description = "For logs"
            preamble = "Operations."
            mcp_filter = ["mezmo_*"]
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();

        assert!(config.enabled);
        assert_eq!(config.max_planning_cycles, 5);
        assert!(config.has_workers());
        assert_eq!(config.tools_in_planning, ToolVisibility::Summary);
        assert_eq!(config.max_tools_per_worker, 10);
    }

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
        assert!(worker.vector_stores.is_empty());
    }

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
    fn test_persistence_drain_timeout_flat_overrides_sub_table() {
        let toml = r#"
            enabled = true
            persistence_drain_timeout_ms = 3000

            [artifacts]
            persistence_drain_timeout_ms = 1000
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.persistence_drain_timeout_ms(), 3000);
    }

    #[test]
    fn test_tool_output_thresholds_in_artifacts_sub_table() {
        let toml = r#"
            enabled = true

            [artifacts]
            tool_output_artifact_threshold = 100
            tool_output_duration_threshold_ms = 2000
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.tool_output_artifact_threshold(), 100);
        assert_eq!(config.tool_output_duration_threshold_ms(), 2000);
    }
}
