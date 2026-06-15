//! The built-in `aura-bootstrap` agent: a token-gated configuration
//! assistant served alongside the configured agents.
//!
//! Enabled via the `[bootstrap]` TOML table (disabled by default), the
//! bootstrap agent carries three built-in tools — `read_config`,
//! `inspect_mcp_servers`, and `write_config` — that let it build a fresh
//! configuration conversationally and make targeted day-2 modifications.
//! Configs are **agent-authored**: `write_config` validates mechanically
//! (strict parse, env-resolution check, secret-literal rejection,
//! read-only-worker enforcement, atomic write with timestamped backup) but
//! expands nothing. What the agent writes is exactly what runs.
//!
//! After a successful write the host's [`ReloadHook`] is invoked so the
//! running server can swap in the new roster in-process (hot reload); the
//! hook's outcome is appended to the tool result so the model can react —
//! and, on failure, restore from the backup it just created.
//!
//! Safety boundaries enforced here rather than by prompt alone:
//! - workers marked `read_only = true` may only be assigned MCP tools that
//!   `inspect_mcp_servers` discovered in this conversation and that no
//!   server annotated as mutating (fail-closed: undiscovered names are
//!   rejected, and same-named tools on different servers merge
//!   most-restrictive). Glob and empty filters are rejected earlier by
//!   config validation, and worker construction re-checks annotations at
//!   runtime for configs that arrive by other paths;
//! - literal credentials are rejected wherever they appear (api keys, MCP
//!   header values, stdio env maps — any credential-looking key); secrets
//!   travel only as `{{ env.* }}` references and the on-disk file is
//!   written from the raw input so they stay references;
//! - `stdio` MCP transports (arbitrary command execution at attach time)
//!   are rejected unless the operator set `AURA_BOOTSTRAP_ALLOW_STDIO=true`;
//! - no agent may take the reserved `aura-bootstrap` name.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aura_config::{Config, McpConfig, McpServerConfig};
use tracing::{info, warn};

/// Reserved model/agent name under which the bootstrap agent is served.
pub const BOOTSTRAP_AGENT_NAME: &str = "aura-bootstrap";

/// Env var that permits `stdio` MCP transports in agent-written configs.
pub const ALLOW_STDIO_ENV: &str = "AURA_BOOTSTRAP_ALLOW_STDIO";

/// Framework prompt for the bootstrap agent (compile-time include).
const BOOTSTRAP_PROMPT: &str = include_str!("prompts/bootstrap_prompt.md");

/// Env-var names surfaced (presence only, never values) to the bootstrap
/// agent so it can steer the operator toward `{{ env.* }}` references that
/// will actually resolve.
const KNOWN_KEY_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
    "MEZMO_API_KEY",
    "AWS_PROFILE",
    "AWS_REGION",
];

/// Whether the operator allowed stdio MCP transports for this process.
pub fn stdio_allowed_from_env() -> bool {
    std::env::var(ALLOW_STDIO_ENV).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Invoked after every successful `write_config`; returns a human-readable
/// summary of the applied roster, or an error message. Either way the text
/// is appended to the tool result so the model sees the outcome.
pub type ReloadHook = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// Where the bootstrap agent reads and writes configuration.
#[derive(Debug, Clone)]
pub struct ConfigTarget {
    /// `CONFIG_PATH` as the server was started with (file or directory).
    pub config_path: PathBuf,
    /// Default write target: the file that declared `[bootstrap]`.
    pub target: PathBuf,
}

impl ConfigTarget {
    /// Single-file deployment helper.
    pub fn single_file(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            config_path: path.clone(),
            target: path,
        }
    }

    fn is_dir(&self) -> bool {
        self.config_path.is_dir()
    }

    /// Resolve an optional `file` tool argument to a concrete path.
    ///
    /// In single-file mode only the default target is addressable. In
    /// directory mode the name must be a plain `*.toml` filename inside the
    /// config directory — no separators, no traversal.
    fn resolve_file(&self, file: Option<&str>) -> Result<PathBuf, String> {
        let Some(name) = file else {
            return Ok(self.target.clone());
        };
        let name = name.trim();
        if !self.is_dir() {
            let default_name = self.target.file_name().and_then(|n| n.to_str());
            if Some(name) == default_name {
                return Ok(self.target.clone());
            }
            return Err(format!(
                "this instance loads a single config file ({}); the `file` \
                 argument is only available for directory deployments",
                self.target.display()
            ));
        }
        if name.contains(['/', '\\']) || name.contains("..") || !name.ends_with(".toml") {
            return Err(format!(
                "invalid file name '{name}': must be a plain *.toml filename \
                 inside the config directory"
            ));
        }
        Ok(self.config_path.join(name))
    }

    /// All `*.toml` files in the deployment (just the target in file mode).
    fn config_files(&self) -> Vec<PathBuf> {
        if !self.is_dir() {
            return vec![self.target.clone()];
        }
        let mut files: Vec<PathBuf> = fs::read_dir(&self.config_path)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        files.sort();
        files
    }
}

// ============================================================================
// Tool discovery / classification signals
// ============================================================================

/// Annotation signals for one discovered MCP tool.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolSignal {
    /// `readOnlyHint` from the server, if provided.
    pub read_only: Option<bool>,
    /// `destructiveHint` from the server, if provided.
    pub destructive: Option<bool>,
}

impl ToolSignal {
    /// Extract the annotation signals from a discovered MCP tool.
    pub fn of_tool(tool: &rmcp::model::Tool) -> Self {
        Self {
            read_only: tool.annotations.as_ref().and_then(|a| a.read_only_hint),
            destructive: tool.annotations.as_ref().and_then(|a| a.destructive_hint),
        }
    }

    /// Whether the server itself declared this tool unsafe for read-only
    /// workers. Annotations are restrictive-only signals: they can bar a
    /// tool from read-only workers, never admit one.
    pub fn declared_mutating(&self) -> bool {
        self.destructive == Some(true) || self.read_only == Some(false)
    }

    /// Most-restrictive merge for same-named tools discovered on different
    /// servers: a mutating signal from either side survives, and agreement
    /// is required for a positive read-only/non-destructive claim.
    fn merge_most_restrictive(self, other: Self) -> Self {
        Self {
            read_only: match (self.read_only, other.read_only) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            },
            destructive: match (self.destructive, other.destructive) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            },
        }
    }
}

/// Discovery results shared between `inspect_mcp_servers` and
/// `write_config` across the bootstrap conversation (the cache lives in the
/// tools factory, so it survives across requests).
pub type SignalCache = Arc<Mutex<HashMap<String, ToolSignal>>>;

// ============================================================================
// Shared error type (the tools report outcomes as Ok(message) so the model
// reliably sees them; the Error type exists to satisfy the trait)
// ============================================================================

#[derive(Debug)]
pub struct BootstrapToolError(String);

impl std::fmt::Display for BootstrapToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BootstrapToolError {}

// ============================================================================
// read_config tool
// ============================================================================

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ReadConfigArgs {
    /// File to read in a directory deployment (plain *.toml name).
    /// Omit for the default configuration file.
    #[serde(default)]
    pub file: Option<String>,
}

/// Returns the raw on-disk configuration (env references intact — the disk
/// file never holds secrets) so the agent can ground modifications in what
/// actually exists.
pub struct ReadConfigTool {
    target: ConfigTarget,
}

impl ReadConfigTool {
    pub fn new(target: ConfigTarget) -> Self {
        Self { target }
    }
}

impl crate::RigTool for ReadConfigTool {
    const NAME: &'static str = "read_config";

    type Error = BootstrapToolError;
    type Args = ReadConfigArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> crate::RigToolDefinition {
        crate::RigToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read this instance's configuration file as it exists on disk \
                          (secrets appear as {{ env.VAR }} references). Call this before \
                          proposing or modifying configuration."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Specific *.toml file in a directory deployment; \
                                        omit for the default configuration file"
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = match self.target.resolve_file(args.file.as_deref()) {
            Ok(p) => p,
            Err(e) => return Ok(format!("READ_CONFIG FAILED: {e}")),
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                return Ok(format!(
                    "READ_CONFIG FAILED: cannot read {}: {e}",
                    path.display()
                ));
            }
        };
        let mut out = String::new();
        if self.target.is_dir() {
            let names: Vec<String> = self
                .target
                .config_files()
                .iter()
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
                .collect();
            out.push_str(&format!(
                "Directory deployment at {} — files: {}\n\n",
                self.target.config_path.display(),
                names.join(", ")
            ));
        }
        out.push_str(&format!(
            "# {}\n\n```toml\n{}\n```",
            path.display(),
            content.trim_end()
        ));
        Ok(out)
    }
}

// ============================================================================
// inspect_mcp_servers tool
// ============================================================================

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct InspectArgs {
    /// TOML containing the `[mcp.servers.<name>]` tables to inspect (for
    /// servers not yet written to the config). Omit to inspect the servers
    /// in the current configuration file(s).
    #[serde(default)]
    pub servers_toml: Option<String>,
}

/// Deserialization helper: extracts `[mcp]` from a TOML fragment or a full
/// candidate config without requiring the rest of the document.
#[derive(serde::Deserialize)]
struct McpFragment {
    mcp: Option<McpConfig>,
}

/// Connects to MCP servers and reports every discovered tool with its
/// annotation signals, for classification and connectivity verification.
/// Results are cached for `write_config`'s read-only-worker enforcement.
pub struct InspectMcpTool {
    target: ConfigTarget,
    signals: SignalCache,
    allow_stdio: bool,
}

impl InspectMcpTool {
    pub fn new(target: ConfigTarget, signals: SignalCache, allow_stdio: bool) -> Self {
        Self {
            target,
            signals,
            allow_stdio,
        }
    }

    /// Gather the MCP config to inspect from the argument or from disk.
    fn gather_mcp(&self, servers_toml: Option<&str>) -> Result<McpConfig, String> {
        if let Some(fragment) = servers_toml {
            let resolved = aura_config::resolve_env_vars(fragment)
                .map_err(|e| format!("env resolution failed: {e}"))?;
            let parsed: McpFragment =
                toml::from_str(&resolved).map_err(|e| format!("TOML parse failed: {e}"))?;
            return parsed
                .mcp
                .filter(|m| !m.servers.is_empty())
                .ok_or_else(|| "no [mcp.servers.<name>] tables found in the input".to_string());
        }
        // From disk: merge servers across the deployment's files.
        let mut merged = McpConfig::default();
        for path in self.target.config_files() {
            let raw = fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let resolved = aura_config::resolve_env_vars(&raw)
                .map_err(|e| format!("env resolution failed for {}: {e}", path.display()))?;
            let parsed: McpFragment = toml::from_str(&resolved)
                .map_err(|e| format!("TOML parse failed for {}: {e}", path.display()))?;
            if let Some(mcp) = parsed.mcp {
                merged.sanitize_schemas = mcp.sanitize_schemas;
                merged.servers.extend(mcp.servers);
            }
        }
        if merged.servers.is_empty() {
            return Err("the current configuration defines no MCP servers; pass \
                        `servers_toml` with the [mcp.servers.<name>] tables to inspect"
                .to_string());
        }
        Ok(merged)
    }
}

impl crate::RigTool for InspectMcpTool {
    const NAME: &'static str = "inspect_mcp_servers";

    type Error = BootstrapToolError;
    type Args = InspectArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> crate::RigToolDefinition {
        crate::RigToolDefinition {
            name: Self::NAME.to_string(),
            description: "Connect to MCP servers and list every tool they expose, with \
                          name, description, and server-declared annotations (read-only / \
                          destructive hints). Use this to verify connectivity and to \
                          classify tools onto workers before writing the config. Pass \
                          `servers_toml` for servers not yet in the configuration; omit \
                          it to inspect the currently configured servers."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "servers_toml": {
                        "type": "string",
                        "description": "TOML with [mcp.servers.<name>] tables to inspect; \
                                        omit to inspect the current configuration's servers"
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mcp = match self.gather_mcp(args.servers_toml.as_deref()) {
            Ok(m) => m,
            Err(e) => return Ok(format!("INSPECT FAILED: {e}")),
        };
        if let Err(e) = check_stdio_policy(&mcp, self.allow_stdio) {
            return Ok(format!("INSPECT REFUSED: {e}"));
        }

        let manager = match crate::McpManager::initialize_from_config(&mcp).await {
            Ok(m) => m,
            Err(e) => return Ok(format!("MCP discovery failed entirely: {e}")),
        };

        let mut report = String::from("MCP tool discovery report:\n");
        let mut signals = self.signals.lock().expect("signal cache poisoned");
        for (server, info) in &manager.server_info {
            match &info.status {
                crate::mcp::ConnectionStatus::Connected => {
                    report.push_str(&format!(
                        "\n## server '{server}' — connected, {} tool(s)\n",
                        info.tools_count
                    ));
                    let tools = manager
                        .streamable_tools
                        .get(server)
                        .or_else(|| manager.sse_tools.get(server))
                        .or_else(|| manager.stdio_tools.get(server));
                    for tool in tools.into_iter().flatten() {
                        let signal = ToolSignal::of_tool(tool);
                        let label = if signal.declared_mutating() {
                            "MUTATING (annotated)"
                        } else if signal.read_only == Some(true) {
                            "read-only (annotated)"
                        } else {
                            "unannotated"
                        };
                        let description: String = tool
                            .description
                            .as_deref()
                            .unwrap_or("(no description)")
                            .chars()
                            .take(140)
                            .collect();
                        report.push_str(&format!("- {} — {label} — {description}\n", tool.name));
                        // Same-named tools on different servers merge
                        // most-restrictive: one server's read-only annotation
                        // must not mask another server's mutating one.
                        signals
                            .entry(tool.name.to_string())
                            .and_modify(|existing| {
                                *existing = existing.merge_most_restrictive(signal);
                            })
                            .or_insert(signal);
                    }
                }
                crate::mcp::ConnectionStatus::Failed(err) => {
                    report.push_str(&format!(
                        "\n## server '{server}' — CONNECTION FAILED: {err}\n\
                         Tell the operator and confirm the URL/auth before writing the config.\n"
                    ));
                }
                other => {
                    report.push_str(&format!("\n## server '{server}' — status: {other:?}\n"));
                }
            }
        }
        report.push_str(
            "\nClassification rules: read-only workers (read_only = true) may only be \
             assigned tools that are annotated read-only or unannotated-but-clearly-\
             read-only, by exact name (no globs). Anything MUTATING, argument-dependent, \
             or uncertain stays out of read-only workers. write_config enforces the \
             annotated signals mechanically.",
        );
        Ok(report)
    }
}

/// Reject stdio MCP servers unless the operator opted in via env.
fn check_stdio_policy(mcp: &McpConfig, allow_stdio: bool) -> Result<(), String> {
    if allow_stdio {
        return Ok(());
    }
    let offenders: Vec<&str> = mcp
        .servers
        .iter()
        .filter(|(_, s)| matches!(s, McpServerConfig::Stdio { .. }))
        .map(|(name, _)| name.as_str())
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    Err(format!(
        "stdio MCP transports spawn arbitrary processes on this server and are \
         disabled for the bootstrap agent (server(s): {}). The operator can allow \
         them by starting the server with {ALLOW_STDIO_ENV}=true, or add the \
         server to the config file by hand.",
        offenders.join(", ")
    ))
}

// ============================================================================
// write_config tool
// ============================================================================

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct WriteConfigArgs {
    /// The complete configuration file content (TOML). This replaces the
    /// whole file — never a fragment.
    pub content: String,
    /// File to write in a directory deployment (plain *.toml name).
    /// Omit for the default configuration file.
    #[serde(default)]
    pub file: Option<String>,
    /// Validate only: run the full validation pipeline and report the
    /// verdict without touching disk.
    #[serde(default)]
    pub validate_only: bool,
}

/// Validates and atomically writes the configuration, then triggers the
/// host's hot reload.
pub struct WriteConfigTool {
    target: ConfigTarget,
    reload: ReloadHook,
    signals: SignalCache,
    allow_stdio: bool,
}

impl WriteConfigTool {
    pub fn new(
        target: ConfigTarget,
        reload: ReloadHook,
        signals: SignalCache,
        allow_stdio: bool,
    ) -> Self {
        Self {
            target,
            reload,
            signals,
            allow_stdio,
        }
    }

    /// Mechanical backstop for the LLM's classification: workers declared
    /// `read_only = true` may only list discovered tools that the server did
    /// not declare mutating. Fails closed — every tool name in a read-only
    /// worker's filter must have been returned by `inspect_mcp_servers` in
    /// this conversation, so an empty discovery cache (model skipped
    /// inspection, or the server restarted since) rejects the write instead
    /// of waving the filter through. Glob/empty-filter rejection lives in
    /// config validation (`OrchestrationConfig::validate_read_only_workers`).
    fn enforce_read_only_workers(&self, config: &Config) -> Result<(), String> {
        let signals = self.signals.lock().expect("signal cache poisoned");
        let Some(orch) = config.orchestration.as_ref() else {
            return Ok(());
        };
        for (worker_name, worker) in orch.workers.iter().filter(|(_, w)| w.read_only) {
            for entry in &worker.mcp_filter {
                match signals.get(entry) {
                    Some(signal) if signal.declared_mutating() => {
                        return Err(format!(
                            "tool '{entry}' is annotated as mutating/destructive by its \
                             MCP server and cannot be assigned to read_only worker \
                             '{worker_name}'"
                        ));
                    }
                    Some(_) => {}
                    None => {
                        return Err(format!(
                            "tool '{entry}' (assigned to read_only worker \
                             '{worker_name}') has not been verified by \
                             inspect_mcp_servers in this conversation — run \
                             inspect_mcp_servers first and use exact discovered \
                             tool names"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Full validation pipeline. Returns the parsed (env-resolved) config
    /// and any non-fatal warnings.
    fn validate_candidate(
        &self,
        content: &str,
        target_file: &Path,
    ) -> Result<(Config, Vec<String>), String> {
        // Syntax check on the raw body first, so malformed TOML is reported
        // as a parse error rather than a misleading env-resolution failure.
        let raw_value: toml::Value =
            toml::from_str(content).map_err(|e| format!("TOML parse failed: {e}"))?;

        // Secret-literal check runs on the RAW tree: api_key values must be
        // env references (validation below sees them resolved).
        check_secret_literals(&raw_value)?;

        let resolved = aura_config::resolve_env_vars(content).map_err(|e| {
            format!("env resolution failed: {e} (the variable must be set on this server)")
        })?;
        let config: Config =
            toml::from_str(&resolved).map_err(|e| format!("TOML parse failed: {e}"))?;
        config
            .validate()
            .map_err(|e| format!("validation failed: {e}"))?;

        for candidate in [
            Some(config.agent.name.as_str()),
            config.agent.alias.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if candidate.eq_ignore_ascii_case(BOOTSTRAP_AGENT_NAME) {
                return Err(format!(
                    "'{BOOTSTRAP_AGENT_NAME}' is reserved for the bootstrap agent and \
                     cannot be used as an agent name or alias"
                ));
            }
        }

        if let Some(mcp) = &config.mcp {
            check_stdio_policy(mcp, self.allow_stdio)?;
        }
        self.enforce_read_only_workers(&config)?;

        // Directory deployments: the candidate must stay uniquely
        // identifiable next to its sibling files, and the full roster must
        // pass bootstrap-specific invariants (reserved name already checked
        // above for the candidate; this catches siblings and multi-enabler).
        let mut roster_pairs: Vec<(PathBuf, Config)> =
            vec![(target_file.to_path_buf(), config.clone())];
        for path in self.target.config_files() {
            if path == target_file {
                continue;
            }
            let siblings = aura_config::load_config_with_paths(&path)
                .map_err(|e| format!("sibling config {} failed to load: {e}", path.display()))?;
            roster_pairs.extend(siblings);
        }
        let roster: Vec<Config> = roster_pairs.iter().map(|(_, c)| c.clone()).collect();
        aura_config::validate_unique_identifiers(&roster).map_err(|e| e.to_string())?;
        validate_roster(&roster_pairs)?;

        let mut warnings = Vec::new();
        let previously_enabled = fs::read_to_string(target_file)
            .ok()
            .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
            .and_then(|v| v.get("bootstrap")?.get("enabled")?.as_bool())
            .unwrap_or(false);
        let now_enabled = config.bootstrap.as_ref().is_some_and(|b| b.enabled);
        if previously_enabled && !now_enabled {
            warnings.push(
                "this configuration no longer enables [bootstrap]: after the next \
                 server restart the bootstrap agent will be unavailable"
                    .to_string(),
            );
        }

        Ok((config, warnings))
    }

    /// Validate, back up, and atomically write the file from the RAW input
    /// (so `{{ env.* }}` references are written as references).
    fn write(&self, args: &WriteConfigArgs) -> Result<String, String> {
        let target_file = self.target.resolve_file(args.file.as_deref())?;
        let (config, warnings) = self.validate_candidate(&args.content, &target_file)?;

        let mode = if config.orchestration_enabled() {
            let workers = config
                .orchestration
                .as_ref()
                .map(|o| o.workers.len())
                .unwrap_or(0);
            format!("orchestration with {workers} worker(s)")
        } else {
            "single-agent".to_string()
        };
        let (provider, model) = config.agent.llm.model_info();
        let mut summary = format!("agent '{}' ({provider}/{model}, {mode})", config.agent.name);
        for w in &warnings {
            summary.push_str(&format!("\nWARNING: {w}"));
        }

        if args.validate_only {
            return Ok(format!(
                "VALIDATION PASSED (nothing written): {summary}\n\
                 Call write_config without validate_only to apply."
            ));
        }

        if target_file.exists() {
            let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
            let backup = target_file.with_file_name(format!(
                "{}.bak.{stamp}",
                target_file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("config.toml")
            ));
            fs::copy(&target_file, &backup)
                .map_err(|e| format!("failed to back up to {}: {e}", backup.display()))?;
            summary.push_str(&format!(
                "\nPrevious config backed up to {}.",
                backup.display()
            ));
        } else if let Some(parent) = target_file.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        let tmp = target_file.with_extension("toml.tmp");
        fs::write(&tmp, &args.content)
            .map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &target_file)
            .map_err(|e| format!("failed to move config into place: {e}"))?;
        warn!(
            "Bootstrap: configuration written to {}",
            target_file.display()
        );

        match (self.reload)() {
            Ok(applied) => Ok(format!(
                "Configuration written to {} and applied: {summary}\n{applied}",
                target_file.display()
            )),
            Err(e) => Ok(format!(
                "Configuration written to {} ({summary}) but RELOAD FAILED: {e}\n\
                 The running server is still serving the previous configuration. \
                 Fix the problem and write again, or restore the backup.",
                target_file.display()
            )),
        }
    }
}

/// Reject literal secrets anywhere in the raw TOML tree: every string value
/// under a credential-looking key (`api_key`, `*token*`, `*secret*`,
/// `*password*`, `*authorization*`, …) must carry its secret as an
/// `{{ env.VAR }}` reference. This covers `[mcp.servers.*.headers]` values
/// and stdio `env` maps, not just `api_key` fields. Values may mix text and
/// references (`Authorization = "Bearer {{ env.TOKEN }}"`) — what matters is
/// that the secret itself travels as a reference.
///
/// `headers_from_request` tables are exempt: their values are *names* of
/// incoming request headers, never secrets.
fn check_secret_literals(value: &toml::Value) -> Result<(), String> {
    fn contains_env_reference(s: &str) -> bool {
        s.find("{{")
            .is_some_and(|start| s[start + 2..].trim_start().starts_with("env."))
            && s.contains("}}")
    }
    fn credential_key(key: &str) -> bool {
        let k = key.to_ascii_lowercase().replace('-', "_");
        ["api_key", "apikey", "token", "secret", "password", "authorization", "credential"]
            .iter()
            .any(|marker| k.contains(marker))
    }
    fn walk(value: &toml::Value, path: &str, violations: &mut Vec<String>) {
        match value {
            toml::Value::Table(table) => {
                for (key, v) in table {
                    if key == "headers_from_request" {
                        continue;
                    }
                    let child = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    if credential_key(key)
                        && let toml::Value::String(s) = v
                        && !s.is_empty()
                        && !contains_env_reference(s)
                    {
                        violations.push(child.clone());
                    }
                    walk(v, &child, violations);
                }
            }
            toml::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    walk(v, &format!("{path}[{i}]"), violations);
                }
            }
            _ => {}
        }
    }
    let mut violations = Vec::new();
    walk(value, "", &mut violations);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "literal API key(s)/secret(s) at {} — secrets must be written as \
             {{{{ env.VAR_NAME }}}} references to variables set on this server, \
             never inline",
            violations.join(", ")
        ))
    }
}

impl crate::RigTool for WriteConfigTool {
    const NAME: &'static str = "write_config";

    type Error = BootstrapToolError;
    type Args = WriteConfigArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> crate::RigToolDefinition {
        crate::RigToolDefinition {
            name: Self::NAME.to_string(),
            description: "Validate the complete configuration file content and write it to \
                          disk (replacing the whole file; the previous version is backed up \
                          with a timestamp). On success the running server applies it \
                          immediately. On validation failure nothing is written and the \
                          errors are returned so you can fix the content and call this tool \
                          again. Set validate_only to check a draft without writing."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The complete configuration file (TOML), with secrets \
                                        as {{ env.VAR }} references"
                    },
                    "file": {
                        "type": "string",
                        "description": "Specific *.toml file in a directory deployment; \
                                        omit for the default configuration file"
                    },
                    "validate_only": {
                        "type": "boolean",
                        "description": "Validate and report without writing (default false)"
                    }
                },
                "required": ["content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        match self.write(&args) {
            Ok(message) => Ok(message),
            Err(e) => Ok(format!(
                "WRITE_CONFIG FAILED — nothing was changed.\n\n{e}\n\n\
                 Fix the configuration and call write_config again. Remember that \
                 `{{{{ env.VAR }}}}` references must name variables that are set on \
                 this server."
            )),
        }
    }
}

// ============================================================================
// Bootstrap agent assembly
// ============================================================================

/// Build the served bootstrap agent's config from the `[bootstrap]`-declaring
/// config.
///
/// The agent runs on `[bootstrap].llm` when set, otherwise the declaring
/// file's `[agent.llm]`. Everything else from the declaring config —
/// orchestration, MCP servers, vector stores, scratchpad, client tools — is
/// stripped: the bootstrap agent talks to the operator and manages
/// configuration, nothing else.
pub fn bootstrap_agent_config(
    declaring: &Config,
    target: &ConfigTarget,
    roster: &[String],
) -> Config {
    let mut config = declaring.clone();
    config.agent.name = BOOTSTRAP_AGENT_NAME.to_string();
    config.agent.alias = None;
    config.agent.turn_depth = Some(8);
    if let Some(llm) = declaring.bootstrap.as_ref().and_then(|b| b.llm.clone()) {
        config.agent.llm = llm;
    }
    config.agent.system_prompt = format!(
        "{}\n{}",
        BOOTSTRAP_PROMPT.trim_end(),
        instance_context(target, roster)
    );
    config.agent.mcp_filter = None;
    config.agent.enable_client_tools = false;
    config.agent.client_tool_filter = None;
    config.agent.scratchpad = None;
    config.orchestration = None;
    config.mcp = None;
    config.tools = None;
    config.vector_stores = Vec::new();
    config.bootstrap = None;

    let (provider, model) = config.agent.llm.model_info();
    info!(
        "Bootstrap agent ready on {provider}/{model}; configuration target: {}",
        target.target.display()
    );
    config
}

/// Instance-specific context appended to the bootstrap prompt.
fn instance_context(target: &ConfigTarget, roster: &[String]) -> String {
    let mut out = String::from("\n## This instance\n\n");
    if target.is_dir() {
        out.push_str(&format!(
            "- Configuration directory: `{}`; your default write target is \
             `{}` (use the `file` argument for sibling files).\n",
            target.config_path.display(),
            target.target.display()
        ));
    } else {
        out.push_str(&format!(
            "- Configuration file: `{}`\n",
            target.target.display()
        ));
    }
    out.push_str(
        "- Successful writes are applied immediately by the running server \
         (hot reload); this conversation continues across changes.\n",
    );
    if roster.is_empty() {
        out.push_str("- No agents are currently configured.\n");
    } else {
        out.push_str(&format!(
            "- Agents currently served: {}\n",
            roster.join(", ")
        ));
    }
    out.push_str(&format!(
        "- stdio MCP transports: {}\n",
        if stdio_allowed_from_env() {
            "ALLOWED (the operator set AURA_BOOTSTRAP_ALLOW_STDIO=true)"
        } else {
            "disabled (AURA_BOOTSTRAP_ALLOW_STDIO is not set)"
        }
    ));
    out.push_str(
        "- Environment variables currently set on this server (presence only; \
         values are never shown):\n",
    );
    for var in KNOWN_KEY_VARS {
        let mark = if std::env::var(var).is_ok() {
            "set"
        } else {
            "not set"
        };
        out.push_str(&format!("  - {var}: {mark}\n"));
    }
    out
}

/// Validate a loaded roster against bootstrap-specific invariants:
///
/// 1. No roster agent may use the reserved `BOOTSTRAP_AGENT_NAME`.
/// 2. At most one config may enable `[bootstrap]`.
///
/// Called by both startup paths and both reload hooks so the invariants
/// are enforced uniformly regardless of how the roster was loaded.
pub fn validate_roster(configs: &[(impl AsRef<std::path::Path>, Config)]) -> Result<(), String> {
    // Reserved name
    if let Some((_, config)) = configs.iter().find(|(_, c)| {
        [Some(c.agent.name.as_str()), c.agent.alias.as_deref()]
            .into_iter()
            .flatten()
            .any(|id| id.eq_ignore_ascii_case(BOOTSTRAP_AGENT_NAME))
    }) {
        return Err(format!(
            "agent '{}' uses the reserved name '{BOOTSTRAP_AGENT_NAME}'",
            config.agent.name,
        ));
    }
    // Single enabler
    let enablers: Vec<String> = configs
        .iter()
        .filter(|(_, c)| c.bootstrap.as_ref().is_some_and(|b| b.enabled))
        .map(|(p, _)| p.as_ref().display().to_string())
        .collect();
    if enablers.len() > 1 {
        return Err(format!(
            "[bootstrap] is enabled in more than one config file ({}) — enable it \
             in exactly one so the bootstrap agent's LLM and write target are \
             unambiguous",
            enablers.join(", ")
        ));
    }
    Ok(())
}

/// Factory for the bootstrap agent's tools, in the shape
/// `AppState.additional_tools` expects.
///
/// The discovery signal cache is created here, outside the per-request
/// closure, so classification survives across conversation rounds. The
/// stdio policy is read from the environment once at startup.
pub fn bootstrap_tools_factory(
    target: ConfigTarget,
    reload: ReloadHook,
) -> Arc<dyn Fn() -> Vec<Box<dyn crate::ToolDyn>> + Send + Sync> {
    let signals: SignalCache = SignalCache::default();
    let allow_stdio = stdio_allowed_from_env();
    Arc::new(move || {
        vec![
            Box::new(ReadConfigTool::new(target.clone())) as Box<dyn crate::ToolDyn>,
            Box::new(InspectMcpTool::new(
                target.clone(),
                signals.clone(),
                allow_stdio,
            )) as Box<dyn crate::ToolDyn>,
            Box::new(WriteConfigTool::new(
                target.clone(),
                reload.clone(),
                signals.clone(),
                allow_stdio,
            )) as Box<dyn crate::ToolDyn>,
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RigTool as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Complete config with no env-var or API-key dependencies (ollama).
    const COMPLETE: &str = r#"
[agent]
name = "assistant"
system_prompt = "You are helpful."

[agent.llm]
provider = "ollama"
model = "qwen3:8b"

[bootstrap]
enabled = true
"#;

    fn target_with(dir: &Path, content: &str) -> ConfigTarget {
        let path = dir.join("config.toml");
        fs::write(&path, content).unwrap();
        ConfigTarget::single_file(path)
    }

    fn noop_reload() -> ReloadHook {
        Arc::new(|| Ok("reloaded".to_string()))
    }

    fn write_tool(target: ConfigTarget) -> WriteConfigTool {
        WriteConfigTool::new(target, noop_reload(), SignalCache::default(), false)
    }

    fn write_args(content: &str) -> WriteConfigArgs {
        WriteConfigArgs {
            content: content.to_string(),
            file: None,
            validate_only: false,
        }
    }

    // ------------------------------------------------------------------
    // write_config
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn write_rejects_incomplete_config_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);

        // No system_prompt: strict serde must reject it.
        let result = write_tool(target)
            .call(write_args(
                "[agent]\nname = \"x\"\n\n[agent.llm]\nprovider = \"ollama\"\nmodel = \"m\"",
            ))
            .await
            .unwrap();

        assert!(result.contains("WRITE_CONFIG FAILED"), "got: {result}");
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            COMPLETE
        );
    }

    #[tokio::test]
    async fn write_applies_and_backs_up_with_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let updated = COMPLETE.replace("You are helpful.", "You are terse.");

        let result = write_tool(target).call(write_args(&updated)).await.unwrap();

        assert!(result.contains("written to"), "got: {result}");
        assert!(result.contains("reloaded"), "got: {result}");
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            updated
        );
        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("config.toml.bak."))
            .collect();
        assert_eq!(backups.len(), 1, "backups: {backups:?}");
        let backup_content = fs::read_to_string(dir.path().join(&backups[0])).unwrap();
        assert_eq!(backup_content, COMPLETE);
    }

    #[tokio::test]
    async fn validate_only_writes_nothing_and_skips_reload() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let reloads = Arc::new(AtomicUsize::new(0));
        let counter = reloads.clone();
        let hook: ReloadHook = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok("reloaded".to_string())
        });
        let tool = WriteConfigTool::new(target, hook, SignalCache::default(), false);

        let result = tool
            .call(WriteConfigArgs {
                content: COMPLETE.replace("helpful", "terse"),
                file: None,
                validate_only: true,
            })
            .await
            .unwrap();

        assert!(result.contains("VALIDATION PASSED"), "got: {result}");
        assert_eq!(reloads.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            COMPLETE
        );
    }

    #[tokio::test]
    async fn reload_failure_is_reported_but_file_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let hook: ReloadHook = Arc::new(|| Err("boom".to_string()));
        let tool = WriteConfigTool::new(target, hook, SignalCache::default(), false);
        let updated = COMPLETE.replace("helpful", "terse");

        let result = tool.call(write_args(&updated)).await.unwrap();

        assert!(result.contains("RELOAD FAILED: boom"), "got: {result}");
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            updated
        );
    }

    #[tokio::test]
    async fn write_rejects_literal_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let content = r#"
[agent]
name = "assistant"
system_prompt = "You are helpful."

[agent.llm]
provider = "openai"
api_key = "sk-proj-abc123"
model = "gpt-5.1"
"#;

        let result = write_tool(target).call(write_args(content)).await.unwrap();

        assert!(result.contains("literal API key"), "got: {result}");
        assert!(result.contains("agent.llm.api_key"), "got: {result}");
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            COMPLETE
        );
    }

    #[tokio::test]
    async fn write_rejects_literal_secret_in_mcp_headers() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let content = format!(
            "{COMPLETE}\n[mcp.servers.k8s]\ntransport = \"http_streamable\"\n\
             url = \"http://x/mcp\"\n\n[mcp.servers.k8s.headers]\n\
             Authorization = \"Bearer sk-live-abc123\"\n"
        );

        let result = write_tool(target).call(write_args(&content)).await.unwrap();

        assert!(result.contains("literal API key(s)/secret(s)"), "got: {result}");
        assert!(
            result.contains("mcp.servers.k8s.headers.Authorization"),
            "got: {result}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            COMPLETE
        );
    }

    #[test]
    fn secret_scan_covers_credential_keys_and_allows_references() {
        let check = |toml_str: &str| {
            check_secret_literals(&toml::from_str::<toml::Value>(toml_str).unwrap())
        };

        // Credential-looking keys with literals are rejected wherever they sit.
        for bad in [
            "[mcp.servers.x.headers]\n\"x-api-key\" = \"abc\"",
            "[mcp.servers.x.env]\nOPENAI_API_KEY = \"sk-123\"",
            "[mcp.servers.x.headers]\nProxy-Authorization = \"Basic xyz\"",
            "[a]\nmy_token = \"t\"",
            "[a]\nclient_secret = \"s\"",
            "[a]\ndb_password = \"p\"",
        ] {
            assert!(check(bad).is_err(), "expected rejection for: {bad}");
        }

        // References (whole-value or embedded) pass; so do non-credential
        // values and headers_from_request name mappings.
        for ok in [
            "[a]\napi_key = \"{{ env.OPENAI_API_KEY }}\"",
            "[mcp.servers.x.headers]\nAuthorization = \"Bearer {{ env.TOKEN }}\"",
            "[mcp.servers.x.headers]\nContent-Type = \"application/json\"",
            "[mcp.servers.x.env]\nAWS_REGION = \"us-east-1\"",
            "[mcp.servers.x.headers_from_request]\nAuthorization = \"x-user-token\"",
        ] {
            assert!(check(ok).is_ok(), "expected pass for: {ok}");
        }
    }

    #[tokio::test]
    async fn write_rejects_unresolvable_env_reference() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let content = r#"
[agent]
name = "assistant"
system_prompt = "You are helpful."

[agent.llm]
provider = "openai"
api_key = "{{ env.DEFINITELY_NOT_SET_AURA_TEST_VAR }}"
model = "gpt-5.1"
"#;

        let result = write_tool(target).call(write_args(content)).await.unwrap();

        assert!(result.contains("env resolution failed"), "got: {result}");
    }

    #[tokio::test]
    async fn write_rejects_reserved_bootstrap_name() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let content = COMPLETE.replace("name = \"assistant\"", "name = \"AURA-Bootstrap\"");

        let result = write_tool(target).call(write_args(&content)).await.unwrap();

        assert!(result.contains("reserved"), "got: {result}");
    }

    #[tokio::test]
    async fn write_rejects_stdio_server_unless_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            format!("{COMPLETE}\n[mcp.servers.local]\ntransport = \"stdio\"\ncmd = [\"npx\"]\n");

        let target = target_with(dir.path(), COMPLETE);
        let result = write_tool(target.clone())
            .call(write_args(&content))
            .await
            .unwrap();
        assert!(result.contains("stdio MCP transports"), "got: {result}");

        // Same content passes when the operator allowed stdio.
        let tool = WriteConfigTool::new(target, noop_reload(), SignalCache::default(), true);
        let result = tool.call(write_args(&content)).await.unwrap();
        assert!(result.contains("written to"), "got: {result}");
    }

    #[tokio::test]
    async fn write_warns_when_bootstrap_gets_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let content = COMPLETE.replace("enabled = true", "enabled = false");

        let result = write_tool(target).call(write_args(&content)).await.unwrap();

        assert!(
            result.contains("no longer enables [bootstrap]"),
            "got: {result}"
        );
    }

    // ------------------------------------------------------------------
    // read_only worker enforcement
    // ------------------------------------------------------------------

    fn cache_with(entries: &[(&str, ToolSignal)]) -> SignalCache {
        let cache = SignalCache::default();
        {
            let mut guard = cache.lock().unwrap();
            for (name, signal) in entries {
                guard.insert(name.to_string(), *signal);
            }
        }
        cache
    }

    fn worker_config(filter_entry: &str, read_only: bool) -> String {
        format!(
            "{COMPLETE}\n[orchestration]\nenabled = true\n\n\
             [orchestration.worker.watcher]\n\
             description = \"d\"\npreamble = \"p\"\nread_only = {read_only}\n\
             mcp_filter = [\"{filter_entry}\"]\n"
        )
    }

    const READ_ONLY_LIST: ToolSignal = ToolSignal {
        read_only: Some(true),
        destructive: None,
    };
    const MUTATING: ToolSignal = ToolSignal {
        read_only: Some(false),
        destructive: Some(true),
    };

    #[tokio::test]
    async fn backstop_rejects_annotated_mutating_tool_in_read_only_worker() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let cache = cache_with(&[("list_pods", READ_ONLY_LIST), ("restart_pod", MUTATING)]);
        let tool = WriteConfigTool::new(target, noop_reload(), cache, false);

        let result = tool
            .call(write_args(&worker_config("restart_pod", true)))
            .await
            .unwrap();

        assert!(result.contains("annotated as mutating"), "got: {result}");
    }

    #[tokio::test]
    async fn backstop_rejects_undiscovered_tool() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let cache = cache_with(&[("list_pods", READ_ONLY_LIST)]);
        let tool = WriteConfigTool::new(target, noop_reload(), cache, false);

        let result = tool
            .call(write_args(&worker_config("hallucinated_tool", true)))
            .await
            .unwrap();

        assert!(
            result.contains("has not been verified by inspect_mcp_servers"),
            "got: {result}"
        );
    }

    #[tokio::test]
    async fn backstop_fails_closed_when_discovery_never_ran() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        // Empty signal cache: inspect_mcp_servers was never called.
        let tool = write_tool(target);

        let result = tool
            .call(write_args(&worker_config("list_pods", true)))
            .await
            .unwrap();

        assert!(
            result.contains("has not been verified by inspect_mcp_servers"),
            "got: {result}"
        );
    }

    #[test]
    fn signal_merge_keeps_mutating_across_servers() {
        // Server A says mutating, server B says read-only: mutating wins
        // regardless of discovery order.
        assert!(MUTATING.merge_most_restrictive(READ_ONLY_LIST).declared_mutating());
        assert!(READ_ONLY_LIST.merge_most_restrictive(MUTATING).declared_mutating());
        // Agreement on read-only survives the merge.
        assert!(
            !READ_ONLY_LIST
                .merge_most_restrictive(READ_ONLY_LIST)
                .declared_mutating()
        );
        // Read-only + unannotated degrades to unannotated (no positive claim).
        let merged = READ_ONLY_LIST.merge_most_restrictive(ToolSignal::default());
        assert_eq!(merged.read_only, None);
    }

    #[tokio::test]
    async fn backstop_rejects_globs_in_read_only_worker() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let cache = cache_with(&[("list_pods", READ_ONLY_LIST)]);
        let tool = WriteConfigTool::new(target, noop_reload(), cache, false);

        let result = tool
            .call(write_args(&worker_config("list_*", true)))
            .await
            .unwrap();

        assert!(result.contains("not glob patterns"), "got: {result}");
    }

    #[tokio::test]
    async fn backstop_ignores_workers_not_marked_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let cache = cache_with(&[("list_pods", READ_ONLY_LIST), ("restart_pod", MUTATING)]);
        let tool = WriteConfigTool::new(target, noop_reload(), cache, false);

        // Mutating tool + glob on a NON-read-only worker: allowed.
        let result = tool
            .call(write_args(&worker_config("restart_pod", false)))
            .await
            .unwrap();

        assert!(result.contains("written to"), "got: {result}");
    }

    #[tokio::test]
    async fn backstop_allows_discovered_read_only_tool() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let cache = cache_with(&[("list_pods", READ_ONLY_LIST)]);
        let tool = WriteConfigTool::new(target, noop_reload(), cache, false);

        let result = tool
            .call(write_args(&worker_config("list_pods", true)))
            .await
            .unwrap();

        assert!(result.contains("written to"), "got: {result}");
    }

    // ------------------------------------------------------------------
    // directory deployments
    // ------------------------------------------------------------------

    fn dir_target(dir: &Path) -> ConfigTarget {
        fs::write(dir.join("main.toml"), COMPLETE).unwrap();
        fs::write(
            dir.join("other.toml"),
            COMPLETE
                .replace("name = \"assistant\"", "name = \"other\"")
                .replace("enabled = true", "enabled = false"),
        )
        .unwrap();
        ConfigTarget {
            config_path: dir.to_path_buf(),
            target: dir.join("main.toml"),
        }
    }

    #[tokio::test]
    async fn write_rejects_duplicate_identifier_across_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir_target(dir.path());
        let content = COMPLETE.replace("name = \"assistant\"", "name = \"other\"");

        let result = write_tool(target).call(write_args(&content)).await.unwrap();

        assert!(result.contains("WRITE_CONFIG FAILED"), "got: {result}");
    }

    #[tokio::test]
    async fn write_file_arg_targets_sibling_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir_target(dir.path());
        let updated = COMPLETE
            .replace("name = \"assistant\"", "name = \"other\"")
            .replace("You are helpful.", "You are other.")
            .replace("enabled = true", "enabled = false");

        let tool = write_tool(target.clone());
        let result = tool
            .call(WriteConfigArgs {
                content: updated.clone(),
                file: Some("other.toml".to_string()),
                validate_only: false,
            })
            .await
            .unwrap();
        assert!(result.contains("written to"), "got: {result}");
        assert_eq!(
            fs::read_to_string(dir.path().join("other.toml")).unwrap(),
            updated
        );

        for bad in ["../evil.toml", "/etc/evil.toml", "evil.txt"] {
            let result = tool
                .call(WriteConfigArgs {
                    content: COMPLETE.to_string(),
                    file: Some(bad.to_string()),
                    validate_only: false,
                })
                .await
                .unwrap();
            assert!(
                result.contains("WRITE_CONFIG FAILED"),
                "expected rejection for {bad}, got: {result}"
            );
        }
    }

    // ------------------------------------------------------------------
    // read_config
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn read_config_returns_raw_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);

        let result = ReadConfigTool::new(target)
            .call(ReadConfigArgs { file: None })
            .await
            .unwrap();

        assert!(result.contains("You are helpful."), "got: {result}");
        assert!(result.contains("config.toml"), "got: {result}");
    }

    #[tokio::test]
    async fn read_config_lists_directory_and_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir_target(dir.path());
        let tool = ReadConfigTool::new(target);

        let result = tool.call(ReadConfigArgs { file: None }).await.unwrap();
        assert!(result.contains("main.toml"), "got: {result}");
        assert!(result.contains("other.toml"), "got: {result}");

        let result = tool
            .call(ReadConfigArgs {
                file: Some("../secret.toml".to_string()),
            })
            .await
            .unwrap();
        assert!(result.contains("READ_CONFIG FAILED"), "got: {result}");
    }

    // ------------------------------------------------------------------
    // inspect_mcp_servers (offline paths)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn inspect_refuses_stdio_fragment_unless_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let tool = InspectMcpTool::new(target, SignalCache::default(), false);

        let result = tool
            .call(InspectArgs {
                servers_toml: Some(
                    "[mcp.servers.local]\ntransport = \"stdio\"\ncmd = [\"npx\"]\n".to_string(),
                ),
            })
            .await
            .unwrap();

        assert!(result.contains("INSPECT REFUSED"), "got: {result}");
        assert!(result.contains(ALLOW_STDIO_ENV), "got: {result}");
    }

    #[tokio::test]
    async fn inspect_without_servers_reports_helpfully() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_with(dir.path(), COMPLETE);
        let tool = InspectMcpTool::new(target, SignalCache::default(), false);

        let result = tool.call(InspectArgs { servers_toml: None }).await.unwrap();

        assert!(result.contains("INSPECT FAILED"), "got: {result}");
        assert!(result.contains("servers_toml"), "got: {result}");
    }

    // ------------------------------------------------------------------
    // bootstrap agent assembly
    // ------------------------------------------------------------------

    #[test]
    fn bootstrap_agent_inherits_declaring_llm_without_baggage() {
        let declaring: Config = toml::from_str(
            &format!("{COMPLETE}\n[mcp.servers.k8s]\ntransport = \"http_streamable\"\nurl = \"http://x/mcp\"\n"),
        )
        .unwrap();
        let target = ConfigTarget::single_file("/tmp/config.toml");

        let agent = bootstrap_agent_config(&declaring, &target, &["assistant".to_string()]);

        assert_eq!(agent.agent.name, BOOTSTRAP_AGENT_NAME);
        assert_eq!(agent.agent.llm.model_info(), ("ollama", "qwen3:8b"));
        assert!(agent.mcp.is_none());
        assert!(agent.orchestration.is_none());
        assert!(agent.bootstrap.is_none());
        assert!(!agent.agent.enable_client_tools);
        assert!(agent.agent.system_prompt.contains("/tmp/config.toml"));
        assert!(agent.agent.system_prompt.contains("assistant"));
    }

    #[test]
    fn bootstrap_agent_prefers_bootstrap_llm_override() {
        let declaring: Config = toml::from_str(&format!(
            "{COMPLETE}\n[bootstrap.llm]\nprovider = \"ollama\"\nmodel = \"qwen3:32b\"\n"
        ))
        .unwrap();
        let target = ConfigTarget::single_file("/tmp/config.toml");

        let agent = bootstrap_agent_config(&declaring, &target, &[]);

        assert_eq!(agent.agent.llm.model_info(), ("ollama", "qwen3:32b"));
    }
}
