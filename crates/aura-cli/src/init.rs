//! `aura init` — generate a starter configuration.
//!
//! The flow:
//!
//! 1. **Sense** conventional API-key env vars (OPENAI_API_KEY, …).
//! 2. **Provider**: exactly one key found → suggested as the default;
//!    several → list prioritized by the ones found; none → default order.
//! 3. **API key**: if the provider's conventional env var is set, tell the
//!    user and ask whether to use it. If not set, prompt for the key value
//!    (masked input). The generated config references the provider's native
//!    env var directly (`{{ env.OPENAI_API_KEY }}`), not an intermediate
//!    `LLM_*` name. A `.env` is only written when the user provides a new
//!    key that isn't already in the environment.
//! 4. **Verify** the key by querying the provider's live model-list
//!    endpoint (blocking HTTP, short timeout; bedrock has no cheap HTTP
//!    listing and is skipped with a note).
//! 5. **Model**: rank the fetched list into a short, best-first shortlist of
//!    per-provider recommended ids (clean id preferred over dated snapshots).
//!    OpenRouter and Ollama are uncurated — the user types an id / picks from
//!    what's installed. Pick by number, accept the default, or type any id.
//! 6. Write a minimal **complete** config referencing the provider's native
//!    env vars.
//!
//! Verification is best-effort: network or key failures warn and continue
//! (`--offline` skips the attempt entirely); init never hard-blocks on the
//! network. Output is deterministic given the same choices.
//!
//! Module layout:
//! - [`provider`] — provider identity and per-provider metadata
//! - [`model_list`] — live/fake model-list fetching behind a trait
//! - [`ranking`] — pure sensing, filtering, and shortlist curation
//! - [`prompt`] — the interactive [`Prompter`](prompt::Prompter)
//! - [`spec`] — fold everything into a resolved `ConfigSpec`
//! - [`render`] — serialize the spec to `config.toml` / `.env`

mod model_list;
mod prompt;
mod provider;
mod ranking;
mod render;
mod spec;
#[cfg(test)]
mod test_support;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use std::io::IsTerminal;

use model_list::HttpModelLister;
use prompt::Prompter;
use provider::Provider;
use render::{merge_env, next_steps, render_config, render_env};
use spec::{ApiKeySource, resolve_spec};

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Output path for the generated config
    #[arg(long, short = 'o', default_value = "config.toml")]
    pub output: PathBuf,

    /// LLM provider (openai, anthropic, bedrock, gemini, ollama, openrouter)
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,

    /// Model name (verified against the provider's model list when possible)
    #[arg(long)]
    pub model: Option<String>,

    /// Environment variable whose value is used as the API key. Defaults to
    /// the provider's conventional var (e.g. OPENAI_API_KEY); not used for
    /// bedrock/ollama.
    #[arg(long)]
    pub api_key_env: Option<String>,

    /// AWS region (bedrock only)
    #[arg(long)]
    pub region: Option<String>,

    /// Base URL (ollama only; default http://localhost:11434)
    #[arg(long)]
    pub base_url: Option<String>,

    /// Agent name written to the config
    #[arg(long, default_value = "assistant")]
    pub name: String,

    /// Skip live model-list verification entirely (air-gapped / CI)
    #[arg(long)]
    pub offline: bool,

    /// Fail on missing required values instead of prompting (automatic
    /// when stdin is not a terminal)
    #[arg(long)]
    pub non_interactive: bool,

    /// Overwrite the output file if it exists
    #[arg(long)]
    pub force: bool,
}

pub fn run_init(args: &InitArgs) -> Result<()> {
    dotenvy::dotenv().ok();
    let is_tty = std::io::stdin().is_terminal();
    let interactive = !args.non_interactive && is_tty;
    let mut prompter = Prompter {
        interactive,
        is_tty,
        stdin: std::io::stdin().lock(),
    };
    if prompter.interactive {
        println!(
            "Welcome to AURA. This init process will generate a starter config \
             you can run right away. I'll ask a couple of questions, then write \
             your config."
        );
    }

    // Resolve an existing config before asking anything: prompt to overwrite
    // (interactive) or fail fast with --force guidance (non-interactive).
    if args.output.exists() && !args.force {
        if prompter.interactive {
            let overwrite = prompter.ask_yes_no(
                &format!("\n{} already exists. Overwrite?", args.output.display()),
                false,
            )?;
            if !overwrite {
                println!("Exiting — {} left unchanged.", args.output.display());
                return Ok(());
            }
        } else {
            bail!(
                "{} already exists — pass --force to overwrite",
                args.output.display()
            );
        }
    }

    let key_is_set = |var: &str| std::env::var(var).is_ok_and(|v| !v.trim().is_empty());
    let key_value = |var: &str| std::env::var(var).ok().filter(|v| !v.trim().is_empty());
    let spec = resolve_spec(
        args,
        &mut prompter,
        &HttpModelLister,
        &key_is_set,
        &key_value,
    )?;
    let rendered = render_config(&spec);

    toml::from_str::<toml::Value>(&rendered).context("generated config is not valid TOML (bug)")?;
    #[cfg(feature = "standalone-cli")]
    render::validate_rendered(&spec, &rendered)?;

    // Only write .env when the user provided a new key
    let mut wrote_env = false;
    if let Some(ApiKeySource::Provided { env_var, value }) = &spec.api_key {
        let env_path = args
            .output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from(".env"), |dir| dir.join(".env"));
        let env_contents = if env_path.exists() {
            let existing = std::fs::read_to_string(&env_path)
                .with_context(|| format!("failed to read {}", env_path.display()))?;
            merge_env(&existing, env_var, value)
        } else {
            render_env(env_var, value)
        };
        std::fs::write(&env_path, &env_contents)
            .with_context(|| format!("failed to write {}", env_path.display()))?;
        wrote_env = true;
        println!("Wrote {}", env_path.display());
    }

    std::fs::write(&args.output, &rendered)
        .with_context(|| format!("failed to write {}", args.output.display()))?;
    println!("Wrote {}", args.output.display());

    println!("{}", next_steps(&args.output, wrote_env));
    Ok(())
}
