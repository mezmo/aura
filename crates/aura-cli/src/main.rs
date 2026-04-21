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

    // Validate --standalone + --config pairing when feature is enabled.
    #[cfg(feature = "standalone-cli")]
    aura_cli::cli::validate_standalone_args(&args);

    let mut config = AppConfig::load(&args)?;
    let permissions = PermissionChecker::load(&std::env::current_dir()?)?;
    let mut backend = Backend::from_config(&config, &args)?;

    #[cfg(feature = "standalone-cli")]
    let is_standalone = args.standalone;
    #[cfg(not(feature = "standalone-cli"))]
    let is_standalone = false;

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
    let post_launch_warning = model_warning.or(resume_warnings.model_warning);

    if is_query {
        run_oneshot(config, permissions, &backend)
    } else {
        run_repl(config, permissions, &backend, post_launch_warning)
    }
}
