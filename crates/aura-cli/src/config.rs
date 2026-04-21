use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use serde::Deserialize;

use crate::aura_dir::{find_project_aura_dir_with_home, global_aura_dir};
use crate::cli::Args;

const DEFAULT_API_URL: &str = "http://localhost:8080";

/// Filename for the human-edited CLI preferences file. Lives at
/// `~/.aura/cli.toml` (global) and `<project>/.aura/cli.toml` (per-project
/// override). Named `cli.toml` rather than `config.toml` so it can never be
/// confused with an Aura **agent** config TOML — those also use `.toml`
/// and the overlap was a real footgun.
const CLI_TOML_FILENAME: &str = "cli.toml";

/// Pre-rename filename. Read with a deprecation warning if `cli.toml` is
/// absent from the same directory; new writes always go to `cli.toml`.
const LEGACY_CLI_TOML_FILENAME: &str = "config.toml";

#[derive(Debug, Deserialize, Default, Clone)]
struct FileConfig {
    api_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    enable_client_tools: Option<bool>,
    enable_final_response_summary: Option<bool>,
}

impl FileConfig {
    /// Merge `other` on top of `self` — fields set in `other` win, fields
    /// only in `self` are preserved. Used to layer a project-local
    /// `cli.toml` on top of the global one.
    fn merge_over(self, other: FileConfig) -> FileConfig {
        FileConfig {
            api_url: other.api_url.or(self.api_url),
            api_key: other.api_key.or(self.api_key),
            model: other.model.or(self.model),
            system_prompt: other.system_prompt.or(self.system_prompt),
            enable_client_tools: other.enable_client_tools.or(self.enable_client_tools),
            enable_final_response_summary: other
                .enable_final_response_summary
                .or(self.enable_final_response_summary),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub api_url: String,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub query: Option<String>,
    pub resume: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    pub force: bool,
    /// Advertise CLI local tools (Shell, Read, Update, ...) to the model and
    /// execute them locally with permission checks. Defaults to `false` —
    /// pure chat client. When `true`, the CLI sends a `tools` field so the
    /// model can request local execution and the REPL's tool-execution path
    /// runs the requests with permission checks.
    ///
    /// **Local tools only fire when both halves opt in.** This flag controls
    /// the CLI side (advertisement). The agent side must also opt in via
    /// `[agent].enable_client_tools = true` in TOML. This is true in both
    /// HTTP and standalone mode (standalone uses the same handler path as
    /// the web server, so the TOML opt-in is required there too).
    pub enable_client_tools: bool,
    /// Generate a one-line LLM-based title for each final response. Adds an
    /// extra round-trip per turn; disabled by default. When false, callers
    /// fall back to the first line of the response as the bullet header.
    pub enable_final_response_summary: bool,
}

impl AppConfig {
    /// Resolve config with precedence:
    ///     CLI args / env vars (tied — both handled by clap)
    ///       > project `<ancestor>/.aura/cli.toml`
    ///         > global `~/.aura/cli.toml`
    ///           > defaults
    pub fn load(args: &Args) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        Self::load_with_dirs(args, &cwd, global_aura_dir().as_deref())
    }

    /// Same as [`AppConfig::load`] but with injectable cwd and global aura
    /// directory. Used by tests to avoid depending on the developer's
    /// environment.
    pub fn load_with_dirs(args: &Args, cwd: &Path, global_dir: Option<&Path>) -> Result<Self> {
        let file_config = load_layered_cli_toml_with_dirs(cwd, global_dir);

        let api_url = args
            .api_url
            .clone()
            .or(file_config.api_url)
            .unwrap_or_else(|| DEFAULT_API_URL.to_string());

        let api_key = args.api_key.clone().or(file_config.api_key);

        let model = args.model.clone().or(file_config.model);
        let system_prompt = args.system_prompt.clone().or(file_config.system_prompt);

        let query = args.query.clone();
        let resume = args.resume.clone();

        // Format: `Key1: Value1, Key2: Value2`. Splits on `,` then on the first
        // `:`, with no escaping — values containing `,` (multi-value Cookie /
        // Accept headers) will be truncated or split into bogus entries. The
        // common cases (Authorization, X-Api-Key, X-Tenant-Id) don't contain
        // commas, so this is acceptable for an env-var interface; reach for a
        // config-file table if you need richer values.
        let extra_headers = std::env::var("AURA_EXTRA_HEADERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|entry| {
                let mut parts = entry.splitn(2, ':');
                let key = parts.next()?.trim();
                let value = parts.next()?.trim();
                if key.is_empty() {
                    return None;
                }
                Some((key.to_string(), value.to_string()))
            })
            .collect();

        // Standard precedence: explicit CLI flag > config file > default
        // (false). `args.enable_client_tools` is `Option<bool>` precisely so
        // we can tell "user passed --enable-client-tools[=...]" from "user
        // accepted the default" without conflating them.
        let enable_client_tools = args
            .enable_client_tools
            .or(file_config.enable_client_tools)
            .unwrap_or(false);

        // Same precedence pattern: CLI flag or env var > project cli.toml >
        // global cli.toml > default. Clap merges `AURA_ENABLE_FINAL_RESPONSE_SUMMARY`
        // into `args.enable_final_response_summary`, so the env var sits at the
        // same tier as the CLI flag and overrides values from cli.toml. The
        // `unwrap_or_else` env leaf is a redundant safety net for the env var
        // (clap already handled it) and supplies the `false` default.
        let enable_final_response_summary = args
            .enable_final_response_summary
            .or(file_config.enable_final_response_summary)
            .unwrap_or_else(crate::api::session::is_final_response_summary_enabled);

        Ok(Self {
            api_url,
            api_key,
            model,
            system_prompt,
            query,
            resume,
            extra_headers,
            force: args.force,
            enable_client_tools,
            enable_final_response_summary,
        })
    }

    /// Build the chat completions endpoint URL from the base URL.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/v1/chat/completions", self.api_url.trim_end_matches('/'))
    }

    /// Build the models endpoint URL from the base URL.
    pub fn models_url(&self) -> String {
        format!("{}/v1/models", self.api_url.trim_end_matches('/'))
    }
}

/// Load and merge the global and project-local `cli.toml` files.
///
/// Lookup:
/// - **Global** (`global_dir`): `~/.aura/cli.toml` in production (with
///   deprecation fallback to `~/.aura/config.toml`).
/// - **Project**: closest `.aura/cli.toml` walking up from `cwd`, skipping
///   `$HOME` so the global file is never double-counted as a project file.
///
/// Project values win on a per-field basis. Missing files are silently
/// treated as empty — only an *invalid* file produces an error, and even
/// then we degrade to defaults rather than failing startup. Parse failures
/// are surfaced via stderr so a typo doesn't silently change behavior.
///
/// `global_dir` is injectable so tests can avoid depending on the
/// developer's real `~/.aura/`.
fn load_layered_cli_toml_with_dirs(cwd: &Path, global_dir: Option<&Path>) -> FileConfig {
    let global = global_dir
        .and_then(|dir| read_cli_toml_in(dir, /* is_project */ false))
        .unwrap_or_default();

    // Pass the global dir's parent as the "home" sentinel so the walk-up
    // skips it — that way a global `~/.aura/cli.toml` is never picked up
    // a second time as a project override.
    let home = global_dir.and_then(|d| d.parent());
    let project = find_project_aura_dir_with_home(cwd, home)
        .and_then(|dir| read_cli_toml_in(&dir, /* is_project */ true))
        .unwrap_or_default();

    global.merge_over(project)
}

/// Read `cli.toml` from `aura_dir`, falling back to the legacy `config.toml`
/// name with a one-time deprecation warning. Returns `None` if neither file
/// exists; logs and returns `None` if the file is present but unparseable.
fn read_cli_toml_in(aura_dir: &Path, is_project: bool) -> Option<FileConfig> {
    let primary = aura_dir.join(CLI_TOML_FILENAME);
    if primary.is_file() {
        return parse_cli_toml(&primary);
    }

    let legacy = aura_dir.join(LEGACY_CLI_TOML_FILENAME);
    if legacy.is_file() {
        warn_legacy_cli_toml_once(&legacy, is_project);
        return parse_cli_toml(&legacy);
    }

    None
}

fn parse_cli_toml(path: &Path) -> Option<FileConfig> {
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: could not read {}: {e}", path.display());
            return None;
        }
    };
    match toml::from_str::<FileConfig>(&contents) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            eprintln!("warning: could not parse {}: {e}", path.display());
            None
        }
    }
}

/// Warn once per process per location that the legacy `config.toml` name is
/// being used. Two distinct one-shots so a user with both a global legacy
/// file and a project-local one sees both warnings, not just the first.
fn warn_legacy_cli_toml_once(path: &Path, is_project: bool) {
    static GLOBAL_WARNED: OnceLock<()> = OnceLock::new();
    static PROJECT_WARNED: OnceLock<()> = OnceLock::new();

    let cell = if is_project {
        &PROJECT_WARNED
    } else {
        &GLOBAL_WARNED
    };
    if cell.set(()).is_err() {
        return;
    }

    eprintln!(
        "warning: {} is deprecated; rename to {} (the old name collided \
         with Aura agent configs and will stop being read in a future release).",
        path.display(),
        path.with_file_name(CLI_TOML_FILENAME).display(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Args;
    use std::fs;
    use tempfile::TempDir;

    fn default_args() -> Args {
        Args {
            api_url: None,
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            force: false,
            enable_client_tools: None,
            enable_final_response_summary: None,
            #[cfg(feature = "standalone-cli")]
            standalone: false,
            #[cfg(feature = "standalone-cli")]
            agent_config: None,
        }
    }

    /// Set up an empty cwd + an empty fake `~/.aura/` so tests don't pick
    /// up the developer's real `cli.toml`. Returns `(cwd, global_dir)`.
    fn empty_env() -> (TempDir, TempDir) {
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        fs::create_dir(home.path().join(".aura")).unwrap();
        (cwd, home)
    }

    fn empty_global(home: &TempDir) -> std::path::PathBuf {
        home.path().join(".aura")
    }

    #[test]
    fn load_defaults_when_no_args() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);
        let args = default_args();
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        assert_eq!(config.api_url, "http://localhost:8080");
        assert!(config.api_key.is_none());
        assert!(config.model.is_none());
        assert!(config.system_prompt.is_none());
        assert!(config.query.is_none());
        assert!(config.resume.is_none());
    }

    #[test]
    fn cli_args_override_defaults() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);
        let args = Args {
            api_url: Some("https://custom.api".to_string()),
            api_key: Some("secret".to_string()),
            model: Some("gpt-4".to_string()),
            system_prompt: Some("Be helpful".to_string()),
            query: Some("hello".to_string()),
            resume: None,
            force: false,
            enable_client_tools: None,
            enable_final_response_summary: None,
            #[cfg(feature = "standalone-cli")]
            standalone: false,
            #[cfg(feature = "standalone-cli")]
            agent_config: None,
        };
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        assert_eq!(config.api_url, "https://custom.api");
        assert_eq!(config.api_key.as_deref(), Some("secret"));
        assert_eq!(config.model.as_deref(), Some("gpt-4"));
        assert_eq!(config.system_prompt.as_deref(), Some("Be helpful"));
        assert_eq!(config.query.as_deref(), Some("hello"));
    }

    #[test]
    fn enable_client_tools_defaults_false() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);
        let args = default_args();
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        assert!(!config.enable_client_tools);
    }

    #[test]
    fn enable_client_tools_can_be_enabled_via_args() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);
        let mut args = default_args();
        args.enable_client_tools = Some(true);
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        assert!(config.enable_client_tools);
    }

    #[test]
    fn enable_client_tools_explicit_false_via_args() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);
        let mut args = default_args();
        args.enable_client_tools = Some(false);
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        assert!(!config.enable_client_tools);
    }

    #[test]
    fn global_cli_toml_is_loaded() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);
        fs::write(
            global.join("cli.toml"),
            r#"api_url = "https://global.example"
model = "global-model"
"#,
        )
        .unwrap();

        let args = default_args();
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        assert_eq!(config.api_url, "https://global.example");
        assert_eq!(config.model.as_deref(), Some("global-model"));
    }

    #[test]
    fn project_cli_toml_overrides_global_per_field() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);

        // Global sets api_url + model
        fs::write(
            global.join("cli.toml"),
            r#"api_url = "https://global.example"
model = "global-model"
"#,
        )
        .unwrap();

        // Project overrides only model
        let project_aura = cwd.path().join(".aura");
        fs::create_dir(&project_aura).unwrap();
        fs::write(project_aura.join("cli.toml"), r#"model = "project-model""#).unwrap();

        let args = default_args();
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        // Global wins for api_url (not overridden), project wins for model.
        assert_eq!(config.api_url, "https://global.example");
        assert_eq!(config.model.as_deref(), Some("project-model"));
    }

    #[test]
    fn project_cli_toml_found_via_walk_up_from_deep_subdir() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);

        // Project root holds .aura/cli.toml
        let project_aura = cwd.path().join(".aura");
        fs::create_dir(&project_aura).unwrap();
        fs::write(
            project_aura.join("cli.toml"),
            r#"api_url = "https://project.example""#,
        )
        .unwrap();

        // CLI invoked from a deeply nested subdir
        let deep = cwd.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();

        let args = default_args();
        let config = AppConfig::load_with_dirs(&args, &deep, Some(&global)).unwrap();
        assert_eq!(config.api_url, "https://project.example");
    }

    #[test]
    fn legacy_config_toml_is_read_when_cli_toml_absent() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);
        // Old filename only — should still be honored, with a deprecation
        // warning written to stderr (not asserted here).
        fs::write(
            global.join("config.toml"),
            r#"api_url = "https://legacy.example""#,
        )
        .unwrap();

        let args = default_args();
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        assert_eq!(config.api_url, "https://legacy.example");
    }

    #[test]
    fn cli_toml_wins_over_legacy_config_toml_in_same_dir() {
        let (cwd, home) = empty_env();
        let global = empty_global(&home);
        fs::write(
            global.join("cli.toml"),
            r#"api_url = "https://new.example""#,
        )
        .unwrap();
        fs::write(
            global.join("config.toml"),
            r#"api_url = "https://legacy.example""#,
        )
        .unwrap();

        let args = default_args();
        let config = AppConfig::load_with_dirs(&args, cwd.path(), Some(&global)).unwrap();
        assert_eq!(config.api_url, "https://new.example");
    }

    #[test]
    fn chat_completions_url_no_trailing_slash() {
        let config = AppConfig {
            api_url: "http://localhost:8080".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            enable_client_tools: true,
            enable_final_response_summary: false,
        };
        assert_eq!(
            config.chat_completions_url(),
            "http://localhost:8080/v1/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_with_trailing_slash() {
        let config = AppConfig {
            api_url: "http://localhost:8080/".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            enable_client_tools: true,
            enable_final_response_summary: false,
        };
        assert_eq!(
            config.chat_completions_url(),
            "http://localhost:8080/v1/chat/completions"
        );
    }

    #[test]
    fn models_url_no_trailing_slash() {
        let config = AppConfig {
            api_url: "https://api.example.com".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            enable_client_tools: true,
            enable_final_response_summary: false,
        };
        assert_eq!(config.models_url(), "https://api.example.com/v1/models");
    }

    #[test]
    fn models_url_with_trailing_slash() {
        let config = AppConfig {
            api_url: "https://api.example.com/".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            enable_client_tools: true,
            enable_final_response_summary: false,
        };
        assert_eq!(config.models_url(), "https://api.example.com/v1/models");
    }
}
