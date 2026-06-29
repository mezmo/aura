use clap::Parser;

/// Subcommands that run before any backend/REPL setup.
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Generate a starter configuration: senses API-key env vars, verifies
    /// provider and model against the provider's live model list, and writes
    /// a minimal config.toml.
    Init(crate::init::InitArgs),
}

/// Aura CLI — interactive chat completions REPL
#[derive(Parser, Debug)]
#[command(name = "aura", version, about)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Base API URL (e.g. https://api.example.com)
    #[arg(long, env = "AURA_API_URL")]
    pub api_url: Option<String>,

    /// Bearer token for authentication
    #[arg(long, env = "AURA_API_KEY")]
    pub api_key: Option<String>,

    /// Model name to use for the conversation
    #[arg(long, env = "AURA_MODEL")]
    pub model: Option<String>,

    /// System prompt prepended to the conversation
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Run a single query without the REPL (one-shot mode)
    #[arg(long)]
    pub query: Option<String>,

    /// Resume a previous conversation by ID (full or short prefix)
    #[arg(long)]
    pub resume: Option<String>,

    /// Bypass warnings and non-critical errors (useful in one-shot/query mode)
    #[arg(long)]
    pub force: bool,

    /// Enable visual flourishes — the `.welcome` fade-in animation and the
    /// brightness wave on the queued-input bar. Both default to OFF so the
    /// REPL stays predictable in CI logs, screen-readers, and `tee`'d
    /// captures. Pass `--pretty` (or set `AURA_PRETTY=true`) to opt in.
    #[arg(long, env = "AURA_PRETTY")]
    pub pretty: bool,

    /// Advertise CLI local tools (Shell, Read, Update, ...) to the model and
    /// execute them locally with permission checks.
    ///
    /// **Both halves of the system must opt in for local tools to fire.** This
    /// flag turns advertisement on at the CLI side; the connected agent's
    /// TOML config must also set `[agent].enable_client_tools = true` (single-
    /// agent configs only — orchestrated configs drop client tools). This is
    /// true in both HTTP and standalone mode: standalone uses the same
    /// handler path as the web server, so the TOML opt-in is required there
    /// too.
    ///
    /// Defaults to disabled. Pass `--enable-client-tools` (or set
    /// `AURA_ENABLE_CLIENT_TOOLS=true`) to opt in to local tool execution.
    // `Option<bool>` (rather than `bool` with a `default_value_t`) so
    // `AppConfig::load` can distinguish "user explicitly passed the flag"
    // from "user accepted the default" and resolve precedence with the
    // config file correctly.
    #[arg(long, env = "AURA_ENABLE_CLIENT_TOOLS",
          num_args = 0..=1, default_missing_value = "true")]
    pub enable_client_tools: Option<bool>,

    /// Generate a one-line LLM-based title for each final response (adds an
    /// extra round-trip per turn). Useful for the REPL's response summary
    /// header; disable when running fast inference or when the extra
    /// round-trip is undesirable.
    ///
    /// Defaults to disabled. Pass `--enable-final-response-summary` (or set
    /// `AURA_ENABLE_FINAL_RESPONSE_SUMMARY=true`) to opt in.
    // `Option<bool>` to distinguish "user explicitly passed" from "user
    // accepted the default" — same pattern as `enable_client_tools`.
    #[arg(long, env = "AURA_ENABLE_FINAL_RESPONSE_SUMMARY",
          num_args = 0..=1, default_missing_value = "true")]
    pub enable_final_response_summary: Option<bool>,

    /// Run in standalone mode — builds agents in-process from TOML config
    /// instead of connecting to an aura-web-server over HTTP. This is the
    /// default when --api-url is not provided. Mutually exclusive with the
    /// --api-url flag, but overrides the AURA_API_URL env var.
    #[cfg(feature = "standalone-cli")]
    #[arg(long)]
    pub standalone: bool,

    /// Path to TOML agent config file or directory for standalone mode.
    /// When --api-url is not set, standalone mode is the default and this
    /// flag selects which config to load. When omitted, defaults to
    /// `config.toml` in the current directory.
    #[cfg(feature = "standalone-cli")]
    #[arg(long = "config")]
    pub agent_config: Option<String>,

    /// Path to a file for diagnostic logs. When unset, the CLI emits no
    /// log output. When set, `tracing` events are written to this path
    /// in both REPL and one-shot mode — the file is opened in append
    /// mode and created if missing.
    ///
    /// **Log rotation, truncation, and pruning are the user's
    /// responsibility.** The CLI will append indefinitely; use `logrotate`,
    /// `truncate -s 0`, or a shell wrapper if the file grows unbounded.
    ///
    /// Precedence: `--log-file` / `AURA_LOG_FILE` > project `cli.toml`
    /// `log_file` > global `cli.toml` `log_file` > no logging.
    #[arg(long, env = "AURA_LOG_FILE")]
    pub log_file: Option<String>,
}

/// Pre-parse check when the standalone-cli feature is not enabled.
///
/// Catches `--config`/`--standalone` before clap parses (clap would give a
/// cryptic "unexpected argument" message). Also errors when no `--api-url` is
/// set, since standalone mode (the default) is unavailable without the feature.
#[cfg(not(feature = "standalone-cli"))]
pub fn check_standalone_flag() {
    // Let clap handle --help / --version and subcommands before we error
    let pass_through = std::env::args()
        .any(|a| matches!(a.as_str(), "--help" | "-h" | "--version" | "-V" | "init"));
    if pass_through {
        return;
    }

    let has_standalone = std::env::args().any(|a| a == "--standalone");
    let has_config = std::env::args().any(|a| a == "--config" || a.starts_with("--config="));

    if has_standalone || has_config {
        let flag = if has_standalone {
            "--standalone"
        } else {
            "--config"
        };
        eprintln!(
            "error: {flag} requires the standalone-cli feature\n\n\
             This build of aura is HTTP-only and cannot load agent configs \
             directly. Standalone mode (the default) is not available.\n\n\
             Pass --api-url to connect to an aura-web-server over HTTP, or \
             rebuild with the standalone-cli feature (enabled by default)."
        );
        std::process::exit(2);
    }

    let has_api_url = std::env::args().any(|a| a == "--api-url" || a.starts_with("--api-url="))
        || std::env::var("AURA_API_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some();

    if !has_api_url {
        eprintln!(
            "error: --api-url is required (standalone mode unavailable)\n\n\
             This build of aura was compiled without the standalone-cli \
             feature, so it cannot run agents in-process. Provide --api-url \
             to connect to an aura-web-server, or rebuild with the default \
             features to enable standalone mode."
        );
        std::process::exit(2);
    }
}

/// Resolve whether the CLI should run in standalone mode and which config
/// to use. Standalone is the default when `--api-url` is absent.
///
/// Returns `true` if standalone mode should be used.
#[cfg(feature = "standalone-cli")]
pub fn resolve_standalone(args: &Args) -> bool {
    // clap merges --api-url and AURA_API_URL into args.api_url, so we
    // check raw argv to distinguish the CLI flag from the env var.
    let api_url_from_flag =
        std::env::args().any(|a| a == "--api-url" || a.starts_with("--api-url="));
    let api_url_from_env = std::env::var("AURA_API_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some();

    if args.standalone && api_url_from_flag {
        eprintln!(
            "error: --standalone and --api-url are mutually exclusive\n\n\
             Standalone mode builds agents in-process and never contacts a \
             remote server. Drop one of the two flags.\n\n\
             If AURA_API_URL is set in your environment and you want \
             standalone mode, pass --standalone (without --api-url) to \
             override the env var."
        );
        std::process::exit(2);
    }

    // --standalone overrides AURA_API_URL env var
    if args.standalone {
        return true;
    }

    // No API URL from either source → standalone by default
    if args.api_url.is_none() && !api_url_from_env {
        return true;
    }

    // --api-url is set and --standalone is not → HTTP mode.
    // --config is ignored in HTTP mode; warn if it was passed.
    if args.agent_config.is_some() {
        eprintln!(
            "warning: --config is ignored in HTTP mode (--api-url is set)\n\
             To run standalone with a config, omit --api-url or add --standalone."
        );
    }

    false
}
