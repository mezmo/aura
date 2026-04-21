use clap::Parser;

/// Aura CLI — interactive chat completions REPL
#[derive(Parser, Debug)]
#[command(name = "aura-cli", version, about)]
pub struct Args {
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

    /// Run in standalone mode (requires --config). Builds agents in-process from TOML config.
    #[cfg(feature = "standalone-cli")]
    #[arg(long)]
    pub standalone: bool,

    /// Path to TOML agent config file or directory (requires --standalone)
    #[cfg(feature = "standalone-cli")]
    #[arg(long = "config")]
    pub agent_config: Option<String>,
}

/// Pre-parse check for `--config` and `--standalone` when the standalone-cli feature is not enabled.
/// Gives a helpful error instead of clap's generic "unexpected argument" message.
#[cfg(not(feature = "standalone-cli"))]
pub fn check_standalone_flag() {
    let has_standalone = std::env::args().any(|a| a == "--standalone");
    let has_config = std::env::args().any(|a| a == "--config");

    if has_standalone || has_config {
        let flag = if has_standalone {
            "--standalone"
        } else {
            "--config"
        };
        eprintln!(
            "error: {flag} requires the standalone-cli feature\n\n\
             This build of aura-cli is an HTTP client only and cannot load \
             agent configs directly.\n\n\
             To run standalone (without an aura-web-server), you'll need a \
             \"standalone-cli\" build of aura-cli.\n\n\
             Without --standalone, aura-cli connects to a server via HTTP.\n\
             Use --api-url to specify the server address."
        );
        std::process::exit(2);
    }
}

/// Post-parse validation for standalone mode flag pairing.
/// Ensures --standalone and --config are used together.
#[cfg(feature = "standalone-cli")]
pub fn validate_standalone_args(args: &Args) {
    if args.agent_config.is_some() && !args.standalone {
        eprintln!(
            "error: --config requires --standalone\n\n\
             The --config flag is only valid in standalone mode. Add --standalone \
             to run agents in-process from TOML config:\n\n\
             aura-cli --standalone --config <path>"
        );
        std::process::exit(2);
    }

    if args.standalone && args.agent_config.is_none() {
        eprintln!(
            "error: --standalone requires --config\n\n\
             Standalone mode needs a TOML agent config to load. Provide the path \
             to a config file or directory:\n\n\
             aura-cli --standalone --config <path>"
        );
        std::process::exit(2);
    }
}
