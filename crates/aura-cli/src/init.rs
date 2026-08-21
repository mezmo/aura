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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use std::io::IsTerminal;

use model_list::HttpModelLister;
use prompt::Prompter;
use provider::Provider;
use render::{env_value, next_steps, render_config};
pub(crate) use render::{merge_env, render_env};
use spec::{ApiKeySource, resolve_spec};

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Output path for the generated config. When omitted, `init` asks
    /// whether to install locally or globally (and defaults to
    /// `config.toml` in the current directory when non-interactive).
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,

    /// Install the generated config into `~/.aura/agents/` so `aura` picks
    /// it up from any directory, instead of asking. Mutually exclusive with
    /// `--output`.
    #[arg(long, conflicts_with = "output")]
    pub global: bool,

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

    let Destination {
        path: output,
        scope,
        name,
    } = resolve_output(args, &mut prompter)?;

    // Reducing a name to a filename is lossy, so two distinct agents can point
    // at one file. `--force` authorises replacing the agent that was asked
    // for; it must not silently destroy a different one that happens to
    // reduce the same way. Checked ahead of the overwrite prompt because
    // `--force` does not excuse it.
    if scope == Scope::Global
        && let Some(existing) = existing_agent_name(&output)
        && existing != name
    {
        bail!(
            "{} already holds a different agent, `{existing}`.\n\
             `{name}` and `{existing}` reduce to the same filename, so installing \
             here would replace it.\n\
             Choose a name that differs by more than punctuation, or pass --output \
             to place this config yourself.",
            output.display()
        );
    }

    // Resolve an existing config before asking anything else: prompt to
    // overwrite (interactive) or fail fast with --force guidance
    // (non-interactive).
    if output.exists() && !args.force {
        if prompter.interactive {
            let overwrite = prompter.ask_yes_no(
                &format!("\n{} already exists. Overwrite?", output.display()),
                false,
            )?;
            if !overwrite {
                println!("Exiting — {} left unchanged.", output.display());
                return Ok(());
            }
        } else {
            bail!(
                "{} already exists — pass --force to overwrite",
                output.display()
            );
        }
    }

    let key_is_set = |var: &str| std::env::var(var).is_ok_and(|v| !v.trim().is_empty());
    let key_value = |var: &str| std::env::var(var).ok().filter(|v| !v.trim().is_empty());
    let mut spec = resolve_spec(
        args,
        &mut prompter,
        &HttpModelLister,
        &key_is_set,
        &key_value,
    )?;
    // A global install derives its filename from the agent name, so the two
    // must come from the same answer.
    spec.name = name;
    let rendered = render_config(&spec);

    toml::from_str::<toml::Value>(&rendered).context("generated config is not valid TOML (bug)")?;
    #[cfg(feature = "standalone-cli")]
    render::validate_rendered(&spec, &rendered)?;

    let dir = output.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }

    // Only write .env when the user provided a new key
    let mut wrote_env = false;
    if let Some(ApiKeySource::Provided { env_var, value }) = &spec.api_key {
        let env_path = dir.map_or_else(|| PathBuf::from(".env"), |dir| dir.join(".env"));
        write_env(&env_path, env_var, value)?;
        wrote_env = true;
        println!("Wrote {}", env_path.display());
    }

    std::fs::write(&output, &rendered)
        .with_context(|| format!("failed to write {}", output.display()))?;
    println!("Wrote {}", output.display());

    println!("{}", next_steps(&output, wrote_env, scope));
    Ok(())
}

/// Bind `env_var` to `value` in the `.env` at `env_path`, creating the file
/// or merging into an existing one.
///
/// Agents installed side by side share the one `.env` in their directory, and
/// each config names only the variable — so replacing a value re-points every
/// agent that references it. Warn before doing that.
fn write_env(env_path: &Path, env_var: &str, value: &str) -> Result<()> {
    let contents = if env_path.exists() {
        let existing = std::fs::read_to_string(env_path)
            .with_context(|| format!("failed to read {}", env_path.display()))?;
        if let Some(current) = env_value(&existing, env_var)
            && current != value
        {
            eprintln!(
                "warning: {env_var} is already set to a different value in {} — \
                 replacing it changes the key used by any agent already installed \
                 there.\n         Pass --api-key-env to give this agent its own \
                 variable instead.",
                env_path.display()
            );
        }
        merge_env(&existing, env_var, value)
    } else {
        render_env(env_var, value)
    };
    std::fs::write(env_path, &contents)
        .with_context(|| format!("failed to write {}", env_path.display()))
}

/// Where the generated config is installed: at a specific path, or in the
/// global `~/.aura/agents/` search location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Local,
    Global,
}

/// Resolved destination for the generated config.
pub(crate) struct Destination {
    path: PathBuf,
    scope: Scope,
    /// Agent name written as `[agent].name`.
    name: String,
}

/// Decide where to write the generated config, and under what agent name.
///
/// `--output` and `--global` each answer the location outright. Otherwise an
/// interactive run asks, and a non-interactive one takes `config.toml` in the
/// current directory — a scripted `aura init` must not depend on the user's
/// home directory.
fn resolve_output<R: std::io::BufRead>(
    args: &InitArgs,
    prompter: &mut Prompter<R>,
) -> Result<Destination> {
    resolve_output_with(args, prompter, crate::agent_config::global_agents_dir())
}

/// Same as [`resolve_output`] but with `~/.aura/agents/` injected, so tests
/// need no real home directory.
fn resolve_output_with<R: std::io::BufRead>(
    args: &InitArgs,
    prompter: &mut Prompter<R>,
    agents_dir: Option<PathBuf>,
) -> Result<Destination> {
    let local = |path: PathBuf| Destination {
        path,
        scope: Scope::Local,
        name: args.name.clone(),
    };
    let global = |dir: &Path, name: String| Destination {
        path: global_config_path(dir, &name),
        scope: Scope::Global,
        name,
    };

    if let Some(output) = &args.output {
        return Ok(local(output.clone()));
    }
    if args.global {
        let Some(dir) = agents_dir else {
            bail!("--global needs a home directory, and none could be determined");
        };
        return Ok(global(&dir, args.name.clone()));
    }

    let local = local(PathBuf::from(crate::agent_config::CWD_CONFIG_FILENAME));
    if !prompter.interactive {
        return Ok(local);
    }
    let Some(dir) = agents_dir else {
        return Ok(local);
    };

    // `ask_choice` indexes the menu below from zero, and constrains its answer
    // to that range or `None`.
    const LOCAL: usize = 0;
    const GLOBAL: usize = 1;

    println!("\nWhere should this config live?\n");
    println!(
        "  {}. {} — this directory only",
        LOCAL + 1,
        local.path.display()
    );
    println!(
        "  {}. {} — found by `aura` from any directory",
        GLOBAL + 1,
        dir.join("<name>.toml").display()
    );
    println!();
    match prompter.ask_choice("Location", 2, Some(LOCAL))? {
        Some(GLOBAL) => {
            // Asked here rather than left at the `--name` default so a second
            // global install can land beside the first instead of colliding
            // with it.
            let name = prompter
                .ask("Agent name", Some(&args.name))?
                .unwrap_or_else(|| args.name.clone());
            Ok(global(&dir, name))
        }
        Some(_) | None => Ok(local),
    }
}

fn global_config_path(agents_dir: &Path, name: &str) -> PathBuf {
    agents_dir.join(format!("{}.toml", sanitize_filename(name)))
}

/// Agent name declared by the config at `path`.
///
/// Read leniently through a plain TOML parse rather than the real loader,
/// which would resolve `{{ env.* }}` references and reject a config whose
/// keys are absent from this environment. A file that cannot be read or
/// parsed yields `None` — an unreadable neighbour should not block an
/// install the overwrite prompt already covers.
fn existing_agent_name(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: toml::Value = text.parse().ok()?;
    parsed
        .get("agent")?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

/// Reduce an agent name to a safe single filename component, so a name
/// carrying path separators or `..` cannot escape `~/.aura/agents/`.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    match cleaned.trim_matches(['.', '-']) {
        "" => "agent".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::{args, non_interactive, scripted};

    fn agents_dir() -> Option<PathBuf> {
        Some(PathBuf::from("/home/u/.aura/agents"))
    }

    #[test]
    fn explicit_output_is_never_second_guessed() {
        let mut a = args();
        a.output = Some(PathBuf::from("custom/agent.toml"));
        let d = resolve_output_with(&a, &mut non_interactive(), agents_dir()).unwrap();
        assert_eq!(d.path, PathBuf::from("custom/agent.toml"));
        assert_eq!(d.scope, Scope::Local);
    }

    #[test]
    fn non_interactive_defaults_to_cwd_not_home() {
        let mut a = args();
        a.output = None;
        let d = resolve_output_with(&a, &mut non_interactive(), agents_dir()).unwrap();
        assert_eq!(d.path, PathBuf::from("config.toml"));
        assert_eq!(d.scope, Scope::Local);
    }

    #[test]
    fn global_flag_skips_the_prompt() {
        let mut a = args();
        a.output = None;
        a.global = true;
        let d = resolve_output_with(&a, &mut non_interactive(), agents_dir()).unwrap();
        assert_eq!(d.path, PathBuf::from("/home/u/.aura/agents/assistant.toml"));
        assert_eq!(d.scope, Scope::Global);
    }

    #[test]
    fn global_flag_without_a_home_dir_is_an_error() {
        let mut a = args();
        a.output = None;
        a.global = true;
        assert!(resolve_output_with(&a, &mut non_interactive(), None).is_err());
    }

    #[test]
    fn prompt_picks_local_by_default() {
        let mut a = args();
        a.output = None;
        let d = resolve_output_with(&a, &mut scripted("\n"), agents_dir()).unwrap();
        assert_eq!(d.path, PathBuf::from("config.toml"));
        assert_eq!(d.scope, Scope::Local);
    }

    #[test]
    fn prompt_picks_global_on_choice_two() {
        let mut a = args();
        a.output = None;
        // Choice 2, then an empty line accepting the default agent name.
        let d = resolve_output_with(&a, &mut scripted("2\n\n"), agents_dir()).unwrap();
        assert_eq!(d.path, PathBuf::from("/home/u/.aura/agents/assistant.toml"));
        assert_eq!(d.scope, Scope::Global);
        assert_eq!(d.name, "assistant");
    }

    #[test]
    fn global_prompt_names_the_file_after_the_agent() {
        let mut a = args();
        a.output = None;
        let d = resolve_output_with(&a, &mut scripted("2\nreviewer\n"), agents_dir()).unwrap();
        assert_eq!(d.path, PathBuf::from("/home/u/.aura/agents/reviewer.toml"));
        assert_eq!(d.scope, Scope::Global);
        // The filename and `[agent].name` must agree.
        assert_eq!(d.name, "reviewer");
    }

    #[test]
    fn a_prompted_name_with_separators_cannot_escape_the_agents_dir() {
        let mut a = args();
        a.output = None;
        let d =
            resolve_output_with(&a, &mut scripted("2\n../../etc/passwd\n"), agents_dir()).unwrap();
        assert_eq!(
            d.path,
            PathBuf::from("/home/u/.aura/agents/etc-passwd.toml")
        );
    }

    #[test]
    fn agent_name_becomes_the_global_filename() {
        assert_eq!(sanitize_filename("reviewer"), "reviewer");
        assert_eq!(sanitize_filename("sre agent"), "sre-agent");
    }

    #[test]
    fn existing_agent_name_reads_a_generated_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("a.toml");
        let spec = test_support::resolve(&args()).unwrap();
        std::fs::write(&path, render_config(&spec)).unwrap();
        // Read without resolving `{{ env.* }}`, which the real loader would.
        assert_eq!(existing_agent_name(&path).as_deref(), Some("assistant"));
    }

    #[test]
    fn existing_agent_name_tolerates_unreadable_and_unparsable_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(existing_agent_name(&tmp.path().join("missing.toml")), None);

        let junk = tmp.path().join("junk.toml");
        std::fs::write(&junk, "this is not toml {{{").unwrap();
        assert_eq!(existing_agent_name(&junk), None);

        let no_agent = tmp.path().join("no-agent.toml");
        std::fs::write(&no_agent, "[other]\nkey = 1\n").unwrap();
        assert_eq!(existing_agent_name(&no_agent), None);
    }

    #[test]
    fn distinct_names_still_reduce_to_one_filename() {
        // The mapping is lossy by design; `run_init` refuses the collision
        // rather than letting one agent overwrite the other.
        assert_eq!(
            global_config_path(Path::new("/agents"), "sre agent"),
            global_config_path(Path::new("/agents"), "sre-agent")
        );
    }

    #[test]
    fn a_name_with_separators_cannot_escape_the_agents_dir() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_filename("/"), "agent");
        assert_eq!(sanitize_filename(".."), "agent");
    }
}
