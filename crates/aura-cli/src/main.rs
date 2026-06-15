use anyhow::Result;
use clap::Parser;

use aura_cli::backend::Backend;
use aura_cli::cli::Args;
use aura_cli::config::AppConfig;
use aura_cli::oneshot::run_oneshot;
use aura_cli::permissions::PermissionChecker;
use aura_cli::repl::r#loop::run_repl;
use aura_cli::ui::pre_launch;

fn main() -> Result<()> {
    // Catch --config/--standalone before clap parses when standalone-cli is not enabled.
    #[cfg(not(feature = "standalone-cli"))]
    aura_cli::cli::check_standalone_flag();

    let args = Args::parse();

    // Subcommands run before any backend/REPL setup (and before the tokio
    // runtime exists — init uses blocking HTTP for model discovery).
    if let Some(aura_cli::cli::Command::Init(init_args)) = &args.command {
        return aura_cli::init::run_init(init_args);
    }

    // Load .env from the working directory so a standalone config's
    // {{ env.* }} references resolve without manual exporting. Only fills vars
    // not already set (shell exports win); an absent .env is not an error.
    dotenv::dotenv().ok();

    // Validate --standalone + --config pairing when feature is enabled.
    #[cfg(feature = "standalone-cli")]
    aura_cli::cli::validate_standalone_args(&args);

    let mut config = AppConfig::load(&args)?;

    #[cfg(feature = "standalone-cli")]
    let is_standalone = args.standalone;
    #[cfg(not(feature = "standalone-cli"))]
    let is_standalone = false;

    // One process-wide tokio runtime, owned by `main` and threaded into
    // `Backend::from_config`, `run_oneshot`, and `run_repl`.
    let rt = tokio::runtime::Runtime::new()?;

    // Brief `enter` window: `init_otel_provider` reads `Handle::current()`
    // during the tonic exporter build. Drops as soon as init returns — we
    // can't `block_on` from inside `_enter`, and `Backend::from_config`
    // does exactly that.
    {
        let _enter = rt.enter();
        aura_cli::logging::init(config.log_file.as_deref(), is_standalone)?;
    }

    // Make sure `~/.aura/cli.toml` exists and has a `style` line. First-run
    // users get a discoverable file with `style = "normal"` they can edit.
    // Failure is silent — read-only filesystems and weird home setups
    // shouldn't block startup, and the in-memory default is `"normal"`
    // anyway.
    if config.style.is_none() {
        let _ = aura_cli::config::save_style_to_global_cli_toml("normal");
        config.style = Some("normal".to_string());
    }

    // Apply the persisted visual style before any output is rendered. An
    // unknown name falls back to the default theme; we don't fail startup
    // over a bad `style` value in `cli.toml`.
    if let Some(name) = config.style.as_deref()
        && let Some(t) = aura_cli::theme::theme_by_name(name)
    {
        aura_cli::theme::set_theme(t);
    }

    // Visual-flourish gate. Non-default OFF — `--pretty` / `AURA_PRETTY`
    // opts in. Read by the welcome printer in `repl::loop` and by
    // `render_queued_wave` in `ui::animation`.
    aura_cli::ui::prompt::set_pretty(config.pretty);
    let permissions = PermissionChecker::load(&std::env::current_dir()?)?;
    let mut backend = Backend::from_config(&rt, &config, &args)?;

    let is_query = config.query.is_some();

    // Validate --model against loaded configs in standalone mode (new conversation only)
    let model_warning = if is_standalone && config.model.is_some() && config.resume.is_none() {
        #[cfg(feature = "standalone-cli")]
        {
            pre_launch::validate_standalone_model(&mut config, &backend)?
        }
        #[cfg(not(feature = "standalone-cli"))]
        {
            None
        }
    } else {
        None
    };

    // Warn at startup if --enable-client-tools is set but no loaded config
    // opts in via [agent].enable_client_tools = true. Without this, the
    // request fires but the in-process server silently drops the tools.
    let client_tools_warning = if is_standalone {
        #[cfg(feature = "standalone-cli")]
        {
            pre_launch::validate_standalone_client_tools(&config, &backend)
        }
        #[cfg(not(feature = "standalone-cli"))]
        {
            None
        }
    } else {
        None
    };

    // Handle --resume conflicts (model and system prompt)
    let resume_warnings = if config.resume.is_some() {
        pre_launch::resolve_resume_conflicts(&mut config, &mut backend, is_query, is_standalone)?
    } else {
        pre_launch::ResumeWarnings::default()
    };

    // Resolve --system-prompt for new conversations
    if config.resume.is_none() && config.system_prompt.is_some() {
        if is_standalone {
            #[cfg(feature = "standalone-cli")]
            pre_launch::resolve_standalone_system_prompt(&mut config, &mut backend, is_query)?;
        } else {
            pre_launch::resolve_http_system_prompt(&config, is_query)?;
        }
    }

    // Merge warnings for the REPL to display post-launch
    let post_launch_warning = model_warning
        .or(resume_warnings.model_warning)
        .or(client_tools_warning.clone());

    let result = if is_query {
        // One-shot mode skips the REPL panel — surface the warning on stderr
        // so it remains visible in scripted contexts.
        if let Some(msg) = client_tools_warning {
            eprintln!("warning: {msg}");
        }
        run_oneshot(&rt, config, permissions, &backend)
    } else {
        run_repl(&rt, config, permissions, &backend, post_launch_warning)
    };

    // Flush any buffered OTel spans before `rt` drops — the
    // `BatchSpanProcessor` exports on a timer (~5s) and we'd lose the
    // tail of the trace otherwise.
    #[cfg(feature = "standalone-cli")]
    rt.block_on(aura::logging::shutdown_tracer());

    result
}
