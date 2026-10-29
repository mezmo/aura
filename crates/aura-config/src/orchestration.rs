//! Configuration types for orchestration mode.
//!
//! These are the pure, serializable orchestration knobs. The runtime helpers
//! that depend on aura's prompt templates (coordinator/worker preamble
//! building, vector-store context strings) live in the `aura` crate's
//! `orchestration::config` module.

use crate::GlobPattern;
use crate::config::{LlmConfig, SkillsConfig};
use crate::scratchpad::ScratchpadConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

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
pub struct LlmWorkerConfig {
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
    /// - omitted - all tools (default)
    /// - `["mezmo_*"]` - all tools starting with "mezmo_"
    /// - `["ListKnowledgeBases", "QueryKnowledgeBases"]` - specific tools
    /// - `[]` - no MCP tools
    ///
    /// Patterns are matched using glob syntax (supports `*`, `**`, `?`, `[abc]`).
    #[serde(default)]
    pub mcp_filter: Option<Vec<GlobPattern>>,

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

    /// Per-worker skill sources. Parsed from `[orchestration.worker.<name>.skills]`.
    /// `None` inherits `[agent.skills]`; an explicit empty list disables skills
    /// for this worker; a non-empty list replaces the agent's skills entirely
    /// (no merging).
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
}

/// Fewest levels a score question's rubric may have.
pub const JEV_SCORE_MIN_LEVELS: usize = 2;
/// Most levels a score question's rubric may have.
pub const JEV_SCORE_MAX_LEVELS: usize = 10;
/// Most options a choice question may have.
pub const JEV_CHOICE_MAX_OPTIONS: usize = 255;

/// One typed question put to a Jev worker.
///
/// ```toml
/// [[orchestration.worker.triage.questions]]
/// id = "owner"
/// instruction = "Which team owns this alert?"
/// criteria = { platform = "Infra and k8s", pipeline = "Pipeline processors" }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum JevQuestion {
    /// Pick one option. `criteria` maps option key → what that option means.
    Choice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        instruction: String,
        criteria: BTreeMap<String, String>,
    },
    /// Rate against an ordered rubric, lowest level first.
    Score {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        instruction: String,
        criteria: Vec<String>,
    },
    /// Probability that the instruction's statement is true.
    Noul {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        instruction: String,
        /// What a yes means.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yes: Option<String>,
        /// What a no means.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no: Option<String>,
    },
}

impl JevQuestion {
    /// The caller-chosen id, if one was given.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Choice { id, .. } | Self::Score { id, .. } | Self::Noul { id, .. } => {
                id.as_deref()
            }
        }
    }

    /// The question text.
    pub fn instruction(&self) -> &str {
        match self {
            Self::Choice { instruction, .. }
            | Self::Score { instruction, .. }
            | Self::Noul { instruction, .. } => instruction,
        }
    }

    /// The question type's wire name.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
            Self::Noul { .. } => "noul",
        }
    }

    /// Check the limits the Jev API enforces, so a bad question is reported
    /// at config load or tool call rather than as an HTTP 422.
    pub fn validate(&self) -> Result<(), String> {
        if self.id().is_some_and(|id| id.trim().is_empty()) {
            return Err("question `id` must not be blank".to_string());
        }
        if self.instruction().trim().is_empty() {
            return Err("question `instruction` must not be blank".to_string());
        }
        match self {
            Self::Choice { criteria, .. } => {
                if criteria.is_empty() || criteria.len() > JEV_CHOICE_MAX_OPTIONS {
                    return Err(format!(
                        "choice question `{}` needs 1-{JEV_CHOICE_MAX_OPTIONS} options in \
                         `criteria`, got {}",
                        self.instruction(),
                        criteria.len()
                    ));
                }
                if let Some(key) = criteria.keys().find(|k| k.trim().is_empty()) {
                    return Err(format!(
                        "choice question `{}` has a blank option key {key:?}",
                        self.instruction()
                    ));
                }
            }
            Self::Score { criteria, .. } => {
                if !(JEV_SCORE_MIN_LEVELS..=JEV_SCORE_MAX_LEVELS).contains(&criteria.len()) {
                    return Err(format!(
                        "score question `{}` needs {JEV_SCORE_MIN_LEVELS}-{JEV_SCORE_MAX_LEVELS} \
                         levels in `criteria`, got {}",
                        self.instruction(),
                        criteria.len()
                    ));
                }
                if criteria.iter().any(|level| level.trim().is_empty()) {
                    return Err(format!(
                        "score question `{}` has a blank level in `criteria`",
                        self.instruction()
                    ));
                }
            }
            Self::Noul { yes, no, .. } => {
                if yes.is_some() != no.is_some() {
                    return Err(format!(
                        "noul question `{}` must give both `yes` and `no`, or neither",
                        self.instruction()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate every question and reject duplicate explicit ids.
    pub fn validate_all(questions: &[JevQuestion]) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for question in questions {
            question.validate()?;
            if let Some(id) = question.id()
                && !seen.insert(id)
            {
                return Err(format!("duplicate question id `{id}`"));
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for JevQuestion {
    /// Dispatch on the shape of `criteria` — a table is a choice, an array a
    /// score, and none a noul — then parse that variant strictly, so a
    /// misspelled field is named in the error instead of the question quietly
    /// matching a looser variant.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Choice {
            #[serde(default)]
            id: Option<String>,
            instruction: String,
            criteria: BTreeMap<String, String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Score {
            #[serde(default)]
            id: Option<String>,
            instruction: String,
            criteria: Vec<String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Noul {
            #[serde(default)]
            id: Option<String>,
            instruction: String,
            #[serde(default)]
            yes: Option<String>,
            #[serde(default)]
            no: Option<String>,
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        let Some(map) = value.as_object() else {
            return Err(D::Error::custom(
                "a question must be a table with `instruction` and optional `criteria`",
            ));
        };
        let parsed = match map.get("criteria") {
            Some(serde_json::Value::Object(_)) => {
                serde_json::from_value::<Choice>(value).map(|q| Self::Choice {
                    id: q.id,
                    instruction: q.instruction,
                    criteria: q.criteria,
                })
            }
            Some(serde_json::Value::Array(_)) => {
                serde_json::from_value::<Score>(value).map(|q| Self::Score {
                    id: q.id,
                    instruction: q.instruction,
                    criteria: q.criteria,
                })
            }
            None => serde_json::from_value::<Noul>(value).map(|q| Self::Noul {
                id: q.id,
                instruction: q.instruction,
                yes: q.yes,
                no: q.no,
            }),
            Some(_) => {
                return Err(D::Error::custom(
                    "question `criteria` must be a table (choice) or an array (score); \
                     omit it for a yes/no (noul) question",
                ));
            }
        };
        parsed.map_err(D::Error::custom)
    }
}

/// A worker backed by TypeSafe's Jev (System One) decision model instead of
/// an LLM.
///
/// ```toml
/// [orchestration.worker.triage]
/// type = "jev"
/// description = "Scores alert severity and picks the owning team"
/// api_key = "{{ env.TYPESAFE_API_KEY }}"
/// ```
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JevWorkerConfig {
    /// Short description of this worker's purpose (for planning prompt).
    pub description: String,
    /// Base URL or full endpoint of a System One-compatible API.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Bearer token.
    #[serde(default)]
    pub api_key: String,
    /// Jev model id.
    #[serde(default)]
    pub model: Option<String>,
    /// Background on the states this worker judges.
    #[serde(default)]
    pub preamble: Option<String>,
    /// HTTP request timeout, in seconds.
    #[serde(
        default,
        deserialize_with = "crate::lenient_int::deserialize_option_u64"
    )]
    pub timeout_secs: Option<u64>,
    /// Questions this worker always asks.
    #[serde(default)]
    pub questions: Vec<JevQuestion>,
}

impl std::fmt::Debug for JevWorkerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevWorkerConfig")
            .field("description", &self.description)
            .field("base_url", &self.base_url)
            .field(
                "api_key",
                &(!self.api_key.is_empty()).then_some("<redacted>"),
            )
            .field("model", &self.model)
            .field("preamble", &self.preamble)
            .field("timeout_secs", &self.timeout_secs)
            .field("questions", &self.questions)
            .finish()
    }
}

/// One `[orchestration.worker.<name>]` entry.
// An LLM worker serializes untagged, exactly as its `LlmWorkerConfig`, which
// keeps fingerprints taken over serialized workers stable.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WorkerConfig {
    Jev(JevWorkerConfig),
    #[serde(untagged)]
    Llm(LlmWorkerConfig),
}

impl WorkerConfig {
    /// Short description of this worker's purpose (for planning prompt).
    pub fn description(&self) -> &str {
        match self {
            Self::Jev(w) => &w.description,
            Self::Llm(w) => &w.description,
        }
    }

    pub fn as_llm(&self) -> Option<&LlmWorkerConfig> {
        match self {
            Self::Llm(w) => Some(w),
            Self::Jev(_) => None,
        }
    }

    pub fn as_jev(&self) -> Option<&JevWorkerConfig> {
        match self {
            Self::Jev(w) => Some(w),
            Self::Llm(_) => None,
        }
    }
}

impl From<LlmWorkerConfig> for WorkerConfig {
    fn from(worker: LlmWorkerConfig) -> Self {
        Self::Llm(worker)
    }
}

impl From<JevWorkerConfig> for WorkerConfig {
    fn from(worker: JevWorkerConfig) -> Self {
        Self::Jev(worker)
    }
}

impl<'de> Deserialize<'de> for WorkerConfig {
    /// Read `type` and parse the rest as that worker kind. A missing `type`
    /// is an LLM worker, parsed leniently so existing configs keep loading;
    /// an unrecognised `type` is an error rather than a silent LLM worker.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let mut value = serde_json::Value::deserialize(deserializer)?;
        let Some(map) = value.as_object_mut() else {
            return Err(D::Error::custom("a worker must be a table"));
        };
        let kind = match map.remove("type") {
            None => None,
            Some(serde_json::Value::String(kind)) => Some(kind),
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "worker `type` must be a string, got {other}"
                )));
            }
        };
        match kind.as_deref() {
            None | Some("llm") => serde_json::from_value(value)
                .map(Self::Llm)
                .map_err(D::Error::custom),
            Some("jev") => serde_json::from_value(value)
                .map(Self::Jev)
                .map_err(D::Error::custom),
            Some(other) => Err(D::Error::custom(format!(
                "unknown worker type \"{other}\"; expected \"jev\" or \"llm\""
            ))),
        }
    }
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
/// stream_inactivity_timeout_secs = 45
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeoutsConfig {
    /// Wall-clock budget (seconds) for one coordinator phase or one worker
    /// task. 0 disables.
    #[serde(default = "default_per_call_timeout_secs")]
    pub per_call_timeout_secs: u64,

    /// Inactivity window (seconds) for coordinator and worker streams; governs
    /// silence between stream items. 0 disables.
    #[serde(default = "default_stream_inactivity_timeout_secs")]
    pub stream_inactivity_timeout_secs: u64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            per_call_timeout_secs: default_per_call_timeout_secs(),
            stream_inactivity_timeout_secs: default_stream_inactivity_timeout_secs(),
        }
    }
}

// ============================================================================
// Retry Sub-Config
// ============================================================================

/// Retry configuration for the orchestration planning loop.
///
/// The schedule is local to aura-config: it does not reuse rig's
/// `ExponentialBackoff` (aura-config stays free of rig-core).
///
/// # Example
///
/// ```toml
/// [orchestration.retry]
/// base_delay_ms = 500
/// max_delay_ms = 5000
/// max_retries = 3
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Base delay (milliseconds). Default: 500.
    #[serde(default = "default_base_delay_ms")]
    pub base_delay_ms: u64,

    /// Maximum delay (milliseconds). Default: 5000.
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,

    /// Maximum transient retries after the initial planning call. Default: 3.
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            base_delay_ms: default_base_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
            max_retries: default_max_retries(),
        }
    }
}

impl RetryConfig {
    /// Delay before transient planning retry `retry_num` (1-based).
    ///
    /// Retry 1 uses the base delay; each subsequent retry doubles the
    /// previous delay, capped at `max_delay_ms`. Saturating arithmetic — a
    /// huge `retry_num` returns the cap without panicking.
    pub fn delay_for_retry(&self, retry_num: usize) -> std::time::Duration {
        // Multiplier is 2^(retry_num - 1): retry 1 -> base, each subsequent doubles.
        let exponent = retry_num.saturating_sub(1).min(128);
        let multiplier = 1u128.checked_shl(exponent as u32).unwrap_or(u128::MAX);
        let delay_ms = (self.base_delay_ms as u128)
            .saturating_mul(multiplier)
            .min(self.max_delay_ms as u128);
        std::time::Duration::from_millis(u64::try_from(delay_ms).expect("capped u128 fits u64"))
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

    /// Retry settings for the planning loop.
    pub retry: RetryConfig,

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
            retry: RetryConfig::default(),
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

    /// Inactivity window (seconds) for coordinator and worker streams.
    pub fn stream_inactivity_timeout_secs(&self) -> u64 {
        self.timeouts.stream_inactivity_timeout_secs
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
            .map(|(name, config)| format!("- {}: {}", name, config.description()))
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
    retry: Option<RetryConfig>,
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
        let retry = raw.retry.unwrap_or_default();

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
            retry,
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

fn default_base_delay_ms() -> u64 {
    500
}

fn default_max_delay_ms() -> u64 {
    5000
}

fn default_max_retries() -> usize {
    3
}

fn default_stream_inactivity_timeout_secs() -> u64 {
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

    fn llm_worker(toml: &str) -> LlmWorkerConfig {
        match toml::from_str::<WorkerConfig>(toml).unwrap() {
            WorkerConfig::Llm(worker) => worker,
            other => panic!("expected an LLM worker, got {other:?}"),
        }
    }

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
        assert_eq!(config.stream_inactivity_timeout_secs(), 0);
        assert_eq!(config.result_artifact_threshold(), 4000);
        assert_eq!(config.result_summary_length(), 2000);
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
        let config = llm_worker(toml);
        assert_eq!(config.description, "For operations");
        assert_eq!(config.preamble, "You are an Operations Specialist.");
        assert_eq!(config.mcp_filter, Some(vec!["mezmo_*".into()]));
    }

    #[test]
    fn test_worker_config_omitted_filter_is_none() {
        let toml = r#"
            description = "Generic tasks"
            preamble = "Generic worker."
        "#;
        let config = llm_worker(toml);
        assert_eq!(config.preamble, "Generic worker.");
        assert_eq!(config.mcp_filter, None);
    }

    #[test]
    fn test_worker_config_empty_filter_is_explicit() {
        let toml = r#"
            description = "No tools"
            preamble = "Tool-free worker."
            mcp_filter = []
        "#;
        let config = llm_worker(toml);
        assert_eq!(config.mcp_filter, Some(vec![]));
    }

    #[test]
    fn test_worker_config_multiple_filters() {
        let toml = r#"
            description = "For knowledge retrieval"
            preamble = "Knowledge worker."
            mcp_filter = ["ListKnowledgeBases", "QueryKnowledgeBases"]
        "#;
        let config = llm_worker(toml);
        let filter = config.mcp_filter.expect("filter present");
        assert_eq!(filter.len(), 2);
        assert!(filter.contains(&"ListKnowledgeBases".into()));
        assert!(filter.contains(&"QueryKnowledgeBases".into()));
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

        let ops = config.get_worker("operations").unwrap().as_llm().unwrap();
        assert_eq!(ops.description, "For logs and pipelines");
        assert_eq!(ops.preamble, "Operations specialist.");
        assert_eq!(ops.mcp_filter, Some(vec!["mezmo_*".into()]));

        let kb = config.get_worker("knowledge").unwrap().as_llm().unwrap();
        assert_eq!(kb.description, "For documentation");
        assert_eq!(kb.preamble, "Knowledge specialist.");
        assert_eq!(kb.mcp_filter.as_ref().map(Vec::len), Some(2));
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

        let worker = config.get_worker("knowledge").unwrap().as_llm().unwrap();
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

        let worker = config.get_worker("operations").unwrap().as_llm().unwrap();
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
            stream_inactivity_timeout_secs = 120

            [artifacts]
            memory_dir = "/tmp/new-style"
            result_artifact_threshold = 8000
            result_summary_length = 1500
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.per_call_timeout_secs(), 45);
        assert_eq!(config.stream_inactivity_timeout_secs(), 120);
        assert_eq!(config.memory_dir(), Some("/tmp/new-style"));
        assert_eq!(config.result_artifact_threshold(), 8000);
        assert_eq!(config.result_summary_length(), 1500);
    }

    #[test]
    fn test_timeouts_default_when_section_omitted() {
        let toml = r#"
            enabled = true
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.per_call_timeout_secs(), 0);
        assert_eq!(config.stream_inactivity_timeout_secs(), 0);

        let toml = r#"
            enabled = true

            [timeouts]
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.per_call_timeout_secs(), 0);
        assert_eq!(config.stream_inactivity_timeout_secs(), 0);
    }

    #[test]
    fn test_timeouts_rejects_unknown_keys() {
        let toml = r#"
            enabled = true

            [timeouts]
            per_call_timeout_secs = 45
            orchestration_timeout_secs = 900
        "#;
        let err = toml::from_str::<OrchestrationConfig>(toml).unwrap_err();
        assert!(
            err.to_string().contains("orchestration_timeout_secs"),
            "error should name the unknown key: {err}"
        );
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

    #[test]
    fn test_retry_defaults_when_absent() {
        let toml = r#"
            enabled = true
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.retry.base_delay_ms, 500);
        assert_eq!(config.retry.max_delay_ms, 5000);
        assert_eq!(config.retry.max_retries, 3);
    }

    #[test]
    fn test_retry_full_custom_round_trips() {
        let toml = r#"
            enabled = true

            [retry]
            base_delay_ms = 250
            max_delay_ms = 8000
            max_retries = 5
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.retry.base_delay_ms, 250);
        assert_eq!(config.retry.max_delay_ms, 8000);
        assert_eq!(config.retry.max_retries, 5);
    }

    #[test]
    fn test_retry_partial_uses_defaults_for_unset() {
        let toml = r#"
            enabled = true

            [retry]
            max_retries = 5
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.retry.base_delay_ms, 500);
        assert_eq!(config.retry.max_delay_ms, 5000);
        assert_eq!(config.retry.max_retries, 5);
    }

    #[test]
    fn test_delay_for_retry_schedule() {
        use std::time::Duration;
        let config = RetryConfig::default();
        assert_eq!(config.delay_for_retry(1), Duration::from_millis(500));
        assert_eq!(config.delay_for_retry(2), Duration::from_millis(1000));
        assert_eq!(config.delay_for_retry(3), Duration::from_millis(2000));
        assert_eq!(config.delay_for_retry(4), Duration::from_millis(4000));
        // Capped at max (5000).
        assert_eq!(config.delay_for_retry(5), Duration::from_millis(5000));
        // Huge retry_num does not panic and returns the cap.
        assert_eq!(config.delay_for_retry(1000), Duration::from_millis(5000));
    }

    #[test]
    fn test_retry_max_retries_zero_round_trips() {
        let toml = r#"
            enabled = true

            [retry]
            max_retries = 0
        "#;
        let config: OrchestrationConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.retry.max_retries, 0);
    }

    #[test]
    fn test_delay_for_retry_base_zero_returns_zero() {
        use std::time::Duration;
        let config = RetryConfig {
            base_delay_ms: 0,
            max_delay_ms: 5000,
            max_retries: 3,
        };
        assert_eq!(config.delay_for_retry(1), Duration::from_millis(0));
        assert_eq!(config.delay_for_retry(10), Duration::from_millis(0));
    }

    #[test]
    fn test_delay_for_retry_max_below_base_caps_at_max() {
        use std::time::Duration;
        let config = RetryConfig {
            base_delay_ms: 500,
            max_delay_ms: 200,
            max_retries: 3,
        };
        // The cap wins even on the first retry.
        assert_eq!(config.delay_for_retry(1), Duration::from_millis(200));
    }

    // ------------------------------------------------------------------
    // Worker types
    // ------------------------------------------------------------------

    #[test]
    fn worker_without_type_is_llm_and_keeps_unknown_keys_lenient() {
        let worker: WorkerConfig = toml::from_str(
            r#"
            description = "Ops"
            preamble = "You are ops."
            retired_setting = "still here from an old config"
        "#,
        )
        .unwrap();
        assert_eq!(worker.as_llm().unwrap().preamble, "You are ops.");
    }

    #[test]
    fn worker_type_llm_is_accepted() {
        let worker: WorkerConfig = toml::from_str(
            r#"
            type = "llm"
            description = "Ops"
            preamble = "You are ops."
        "#,
        )
        .unwrap();
        assert!(worker.as_llm().is_some());
    }

    #[test]
    fn worker_type_jev_parses_every_question_kind() {
        let worker: WorkerConfig = toml::from_str(
            r#"
            type = "jev"
            description = "Triage"
            api_key = "secret"
            model = "jev-1.13.0"
            preamble = "Alerts from pipeline monitors."
            timeout_secs = 45.0

            [[questions]]
            id = "owner"
            instruction = "Which team owns this?"
            criteria = { platform = "Infra", pipeline = "Processors" }

            [[questions]]
            instruction = "How severe is it?"
            criteria = ["minor", "major", "critical"]

            [[questions]]
            instruction = "A human must act on this alert"
            yes = "Needs a person"
            no = "Self-resolving"
        "#,
        )
        .unwrap();
        let jev = worker.as_jev().expect("jev worker");
        assert_eq!(worker.description(), "Triage");
        assert_eq!(jev.model.as_deref(), Some("jev-1.13.0"));
        assert_eq!(jev.timeout_secs, Some(45));
        assert!(matches!(
            &jev.questions[0],
            JevQuestion::Choice { id: Some(id), criteria, .. }
                if id == "owner" && criteria.len() == 2
        ));
        assert!(matches!(
            &jev.questions[1],
            JevQuestion::Score { id: None, criteria, .. } if criteria.len() == 3
        ));
        assert!(matches!(
            &jev.questions[2],
            JevQuestion::Noul {
                yes: Some(_),
                no: Some(_),
                ..
            }
        ));
        assert!(JevQuestion::validate_all(&jev.questions).is_ok());
    }

    #[test]
    fn unknown_worker_type_is_rejected() {
        let err = toml::from_str::<WorkerConfig>(
            r#"
            type = "jevv"
            description = "Triage"
            preamble = "p"
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown worker type \"jevv\""), "{err}");
    }

    #[test]
    fn jev_worker_rejects_llm_only_fields() {
        let err = toml::from_str::<WorkerConfig>(
            r#"
            type = "jev"
            description = "Triage"
            mcp_filter = ["mezmo_*"]
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("mcp_filter"), "{err}");
    }

    #[test]
    fn misspelled_question_field_is_named_not_turned_into_a_noul() {
        let err = toml::from_str::<WorkerConfig>(
            r#"
            type = "jev"
            description = "Triage"

            [[questions]]
            instruction = "How severe is it?"
            critera = ["minor", "major"]
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("critera"), "{err}");
    }

    #[test]
    fn question_criteria_of_the_wrong_shape_is_rejected() {
        let err = serde_json::from_value::<JevQuestion>(serde_json::json!({
            "instruction": "Is it real?",
            "criteria": "yes or no",
        }))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("must be a table (choice) or an array (score)"),
            "{err}"
        );
    }

    #[test]
    fn question_validation_enforces_api_limits() {
        let score = |levels: usize| JevQuestion::Score {
            id: None,
            instruction: "Rate it".to_string(),
            criteria: (0..levels).map(|i| format!("level {i}")).collect(),
        };
        assert!(score(1).validate().is_err());
        assert!(score(2).validate().is_ok());
        assert!(score(10).validate().is_ok());
        assert!(score(11).validate().is_err());

        let empty_choice = JevQuestion::Choice {
            id: None,
            instruction: "Pick".to_string(),
            criteria: BTreeMap::new(),
        };
        assert!(empty_choice.validate().is_err());

        let blank = JevQuestion::Noul {
            id: None,
            instruction: "  ".to_string(),
            yes: None,
            no: None,
        };
        assert!(blank.validate().is_err());

        let one_pole = JevQuestion::Noul {
            id: None,
            instruction: "Is it real?".to_string(),
            yes: Some("It is".to_string()),
            no: None,
        };
        assert!(one_pole.validate().is_err());

        let noul = |id: &str| JevQuestion::Noul {
            id: Some(id.to_string()),
            instruction: "Is it real?".to_string(),
            yes: None,
            no: None,
        };
        let err = JevQuestion::validate_all(&[noul("real"), noul("real")]).unwrap_err();
        assert!(err.contains("duplicate question id `real`"), "{err}");
    }

    #[test]
    fn worker_serialization_round_trips_and_llm_has_no_tag() {
        let llm: WorkerConfig = toml::from_str(
            r#"
            description = "Ops"
            preamble = "You are ops."
            mcp_filter = ["mezmo_*"]
        "#,
        )
        .unwrap();
        // An LLM worker serializes exactly as the bare struct, which keeps
        // fingerprints taken over serialized workers stable.
        assert_eq!(
            serde_json::to_value(&llm).unwrap(),
            serde_json::to_value(llm.as_llm().unwrap()).unwrap()
        );
        let back: WorkerConfig =
            serde_json::from_value(serde_json::to_value(&llm).unwrap()).unwrap();
        assert!(back.as_llm().is_some());

        let jev: WorkerConfig = toml::from_str(
            r#"
            type = "jev"
            description = "Triage"
            api_key = "secret"

            [[questions]]
            instruction = "Is it real?"
        "#,
        )
        .unwrap();
        let value = serde_json::to_value(&jev).unwrap();
        assert_eq!(value["type"], "jev");
        let back: WorkerConfig = serde_json::from_value(value).unwrap();
        assert_eq!(back.as_jev().unwrap().questions.len(), 1);
    }

    #[test]
    fn jev_worker_debug_redacts_the_api_key() {
        let jev: WorkerConfig = toml::from_str(
            r#"
            type = "jev"
            description = "Triage"
            api_key = "super-secret-key"
        "#,
        )
        .unwrap();
        let debug = format!("{jev:?}");
        assert!(!debug.contains("super-secret-key"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }
}
