//! Pre-launch validation and interactive prompts.
//!
//! All validation that must happen **before** the REPL starts lives here.
//! Interactive prompts wait indefinitely for user input (no timeouts).
//! When `is_query` is true, prompts become errors with actionable guidance.

use std::io::{self, Write};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use crate::backend::Backend;
use crate::config::AppConfig;
use crate::repl::conversations::ConversationStore;

// ---------------------------------------------------------------------------
// Interactive prompt helpers
// ---------------------------------------------------------------------------

/// Read a single keypress (blocking, no timeout). Returns the char pressed.
/// Enables raw mode, reads one key, disables raw mode.
fn read_keypress() -> Option<char> {
    crossterm::terminal::enable_raw_mode().ok()?;
    let ch = loop {
        if let Ok(Event::Key(key)) = event::read() {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char(c) => break Some(c),
                KeyCode::Enter => break Some('\n'),
                KeyCode::Esc => break None,
                _ => continue,
            }
        }
    };
    crossterm::terminal::disable_raw_mode().ok();
    println!(); // newline after keypress
    ch
}

/// Prompt the user with numbered options. Returns the 1-based index of their choice.
/// Loops until a valid option is selected. Returns None if Esc is pressed.
fn prompt_choice(message: &str, options: &[&str]) -> Option<usize> {
    println!("\n{}", message);
    for (i, opt) in options.iter().enumerate() {
        println!("  ({}) {}", i + 1, opt);
    }
    print!("\nYour choice: ");
    let _ = io::stdout().flush();

    loop {
        if let Some(ch) = read_keypress() {
            if let Some(digit) = ch.to_digit(10) {
                let idx = digit as usize;
                if idx >= 1 && idx <= options.len() {
                    return Some(idx);
                }
            }
            print!("Please enter a number between 1 and {}: ", options.len());
            let _ = io::stdout().flush();
        } else {
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// New conversation + --model (standalone)
// ---------------------------------------------------------------------------

/// Validate `--model` against loaded standalone configs.
///
/// - Directory config, no match: exits with error listing available models.
/// - Single file config, no match: returns `Ok(Some(warning))` to display post-REPL-launch.
/// - Match found: sets `config.model` to canonical effective ID, returns `Ok(None)`.
#[cfg(feature = "standalone-cli")]
pub fn validate_standalone_model(
    config: &mut AppConfig,
    backend: &Backend,
) -> anyhow::Result<Option<String>> {
    let direct = backend.as_direct();
    let model_name = match config.model.as_ref() {
        Some(m) => m.clone(),
        None => return Ok(None),
    };

    if let Some(canonical) = direct.find_matching_model(&model_name) {
        config.model = Some(canonical);
        return Ok(None);
    }

    if direct.has_multiple_configs() {
        let available = direct.model_ids();
        eprintln!(
            "error: --model \"{}\" does not match any loaded agent configuration\n",
            model_name
        );
        eprintln!("Available models:");
        for id in &available {
            eprintln!("  - {}", id);
        }
        eprintln!(
            "\nPlease ensure that the --model value matches either the agent.name \
             or agent.alias specified in one of your config files in the directory provided."
        );
        std::process::exit(2);
    } else {
        config.model = None;
        Ok(Some(
            "In standalone mode '--model' is used to select a configuration from the \
             available configurations — you only have a single configuration file, \
             ignoring --model."
                .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// New conversation + --system-prompt
// ---------------------------------------------------------------------------

/// Resolve `--system-prompt` for a new standalone conversation.
///
/// In query/one-shot mode, silently appends to the agent's TOML prompt.
/// In REPL mode, interactively asks the user whether to append or replace.
#[cfg(feature = "standalone-cli")]
pub fn resolve_standalone_system_prompt(
    config: &mut AppConfig,
    backend: &mut Backend,
    is_query: bool,
) -> anyhow::Result<()> {
    let cli_prompt = match config.system_prompt.as_ref() {
        Some(p) => p.clone(),
        None => return Ok(()),
    };

    let direct = backend.as_direct();
    let toml_prompt = direct.get_config_system_prompt(config.model.as_deref());

    let toml_prompt = match toml_prompt {
        Some(p) if !p.is_empty() => p,
        _ => {
            // No existing TOML prompt — just use CLI prompt as replacement
            let direct_mut = backend.as_direct_mut();
            direct_mut.override_system_prompt(config.model.as_deref(), cli_prompt);
            return Ok(());
        }
    };

    if is_query {
        let combined = format!("{}\n\n{}", toml_prompt, cli_prompt);
        let direct_mut = backend.as_direct_mut();
        direct_mut.override_system_prompt(config.model.as_deref(), combined.clone());
        config.system_prompt = Some(combined);
        return Ok(());
    }

    let choice = prompt_choice(
        "The agent config already has a system prompt. \
         How would you like to handle --system-prompt?",
        &[
            "Append to the agent's existing system prompt",
            "Replace the agent's system prompt",
        ],
    );

    match choice {
        Some(1) => {
            let combined = format!("{}\n\n{}", toml_prompt, cli_prompt);
            let direct_mut = backend.as_direct_mut();
            direct_mut.override_system_prompt(config.model.as_deref(), combined.clone());
            config.system_prompt = Some(combined);
        }
        Some(2) => {
            let direct_mut = backend.as_direct_mut();
            direct_mut.override_system_prompt(config.model.as_deref(), cli_prompt);
        }
        _ => {
            eprintln!("Cancelled.");
            std::process::exit(0);
        }
    }

    Ok(())
}

/// Resolve `--system-prompt` for a new HTTP conversation.
///
/// In query/one-shot mode, errors unless `--force` is provided (Aura doesn't support
/// system messages). In REPL mode, asks if the user is connecting to Aura or an
/// OpenAI-compatible service.
pub fn resolve_http_system_prompt(config: &AppConfig, is_query: bool) -> anyhow::Result<()> {
    if config.system_prompt.is_none() {
        return Ok(());
    }

    if is_query {
        if config.force {
            return Ok(());
        }
        eprintln!(
            "error: --system-prompt is not supported when connecting to Aura HTTP service\n\n\
             The Aura HTTP service does not support system messages. Switch to \
             '--standalone' mode if you want to use system messages with Aura.\n\n\
             If you are connecting to an OpenAI compatible service other than Aura \
             you can use '--force' to ignore this warning."
        );
        std::process::exit(2);
    }

    let choice = prompt_choice(
        "You provided --system-prompt. What type of service are you connecting to?",
        &[
            "Aura (system messages are not supported)",
            "An OpenAI-compatible chat completion service",
        ],
    );

    match choice {
        Some(1) => {
            eprintln!(
                "\nThe Aura HTTP service does not support system messages. \
                 Use '--standalone' mode if you want to use system messages with Aura."
            );
            std::process::exit(2);
        }
        Some(2) => {}
        _ => {
            eprintln!("Cancelled.");
            std::process::exit(0);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Resume conflicts
// ---------------------------------------------------------------------------

/// Warnings to display after the REPL has launched.
#[derive(Default)]
pub struct ResumeWarnings {
    pub model_warning: Option<String>,
}

/// Resolve resume conflicts for model and system prompt.
///
/// Peeks at the resume store to compare saved values against CLI args.
/// When `is_query` is true, conflicts become errors. Otherwise, interactive prompts.
///
/// Returns any warnings to show post-REPL-launch (e.g. model mismatch in standalone).
pub fn resolve_resume_conflicts(
    config: &mut AppConfig,
    #[allow(unused_variables)] backend: &mut Backend,
    is_query: bool,
    #[allow(unused_variables)] is_standalone: bool,
) -> anyhow::Result<ResumeWarnings> {
    let resume_id = match config.resume.as_ref() {
        Some(id) => id.clone(),
        None => return Ok(ResumeWarnings::default()),
    };

    let full_uuid = match ConversationStore::find_by_prefix(&resume_id) {
        Ok(uuid) => uuid,
        Err(_) => {
            // Can't find conversation — let the REPL handle the error later
            return Ok(ResumeWarnings::default());
        }
    };
    let store = match ConversationStore::open(&full_uuid) {
        Ok(s) => s,
        Err(_) => return Ok(ResumeWarnings::default()),
    };

    #[allow(unused_mut)]
    let mut warnings = ResumeWarnings::default();

    // --- Model conflicts ---
    let saved_model = store.load_model();
    let cli_model = config.model.clone();

    if let Some(ref cli_m) = cli_model {
        if let Some(ref saved_m) = saved_model
            && !cli_m.eq_ignore_ascii_case(saved_m)
        {
            #[cfg(feature = "standalone-cli")]
            if is_standalone {
                let direct = backend.as_direct();

                if direct.has_multiple_configs() {
                    if direct.find_matching_model(cli_m).is_none() {
                        if is_query {
                            let available = direct.model_ids();
                            eprintln!(
                                "error: --model \"{}\" does not match any loaded agent \
                                 configuration\n",
                                cli_m
                            );
                            eprintln!("Available models:");
                            for id in &available {
                                eprintln!("  - {}", id);
                            }
                            eprintln!(
                                "\nPlease ensure that the --model value matches either \
                                 the agent.name or agent.alias specified in one of your \
                                 config files."
                            );
                            std::process::exit(2);
                        }
                        warnings.model_warning = Some(
                            "The model you selected with --model does not match the name \
                             or alias of any agent from available configuration file(s). \
                             Use /model to choose a model."
                                .to_string(),
                        );
                        config.model = saved_model.clone();
                    } else {
                        resolve_model_conflict(
                            config,
                            saved_m,
                            cli_m,
                            is_query,
                            "The model specified in your command line does not match the \
                             model saved in your conversation history. However, it does \
                             match a model found in one of your configuration files.",
                        )?;
                    }
                } else {
                    warnings.model_warning = Some(
                        "In standalone mode '--model' is used to select a configuration \
                         from the available configurations — you only have a single \
                         configuration file, ignoring --model."
                            .to_string(),
                    );
                    config.model = saved_model.clone();
                }
            }

            #[cfg(not(feature = "standalone-cli"))]
            {
                resolve_model_conflict(
                    config,
                    saved_m,
                    cli_m,
                    is_query,
                    "The model specified in your command line does not match the \
                     model saved in your conversation history.",
                )?;
            }

            #[cfg(feature = "standalone-cli")]
            if !is_standalone {
                resolve_model_conflict(
                    config,
                    saved_m,
                    cli_m,
                    is_query,
                    "The model specified in your command line does not match the \
                     model saved in your conversation history.",
                )?;
            }
        }
    } else {
        #[cfg(feature = "standalone-cli")]
        if is_standalone && let Some(ref saved_m) = saved_model {
            let direct = backend.as_direct();
            if direct.find_matching_model(saved_m).is_none() {
                warnings.model_warning = Some(
                    "The model saved in your conversation history does not match the name \
                     or alias of any agent from available configuration file(s). \
                     Use /model to choose a model."
                        .to_string(),
                );
            }
        }
    }

    // --- System prompt conflicts ---
    let saved_prompt = store.load_system_prompt();
    let cli_prompt = config.system_prompt.clone();

    if let (Some(cli_p), Some(saved_p)) = (&cli_prompt, &saved_prompt)
        && cli_p != saved_p
    {
        #[cfg(feature = "standalone-cli")]
        if is_standalone {
            resolve_system_prompt_conflict_standalone(config, saved_p, cli_p, is_query)?;
        }

        #[cfg(feature = "standalone-cli")]
        if !is_standalone {
            resolve_system_prompt_conflict_http(config, saved_p, cli_p, is_query)?;
        }

        #[cfg(not(feature = "standalone-cli"))]
        {
            resolve_system_prompt_conflict_http(config, saved_p, cli_p, is_query)?;
        }
    }

    Ok(warnings)
}

// ---------------------------------------------------------------------------
// Internal conflict resolution helpers
// ---------------------------------------------------------------------------

fn resolve_model_conflict(
    config: &mut AppConfig,
    saved_model: &str,
    cli_model: &str,
    is_query: bool,
    message: &str,
) -> anyhow::Result<()> {
    if is_query {
        eprintln!(
            "error: model conflict when resuming conversation\n\n\
             {message}\n\n\
             Saved model:   \"{saved_model}\"\n\
             CLI --model:   \"{cli_model}\"\n\n\
             To resolve, either omit --model to use the saved model, \
             or start a new conversation instead of resuming."
        );
        std::process::exit(2);
    }

    let choice = prompt_choice(
        &format!(
            "{message}\n\n  Saved model:   \"{saved_model}\"\n  CLI --model:   \"{cli_model}\""
        ),
        &[
            &format!("Use the previous model (\"{}\")", saved_model),
            &format!("Use the CLI model (\"{}\")", cli_model),
        ],
    );

    match choice {
        Some(1) => {
            config.model = Some(saved_model.to_string());
        }
        Some(2) => {
            config.model = Some(cli_model.to_string());
        }
        _ => {
            eprintln!("Cancelled.");
            std::process::exit(0);
        }
    }

    Ok(())
}

/// Standalone resume with system prompt conflict — offers append as a third option.
#[cfg(feature = "standalone-cli")]
fn resolve_system_prompt_conflict_standalone(
    config: &mut AppConfig,
    saved_prompt: &str,
    cli_prompt: &str,
    is_query: bool,
) -> anyhow::Result<()> {
    if is_query {
        eprintln!(
            "error: system prompt conflict when resuming conversation\n\n\
             The system prompt specified in your command line does not match the \
             system prompt saved in your conversation history.\n\n\
             To resolve, either omit --system-prompt to use the saved prompt, \
             or start a new conversation instead of resuming."
        );
        std::process::exit(2);
    }

    let choice = prompt_choice(
        "The system prompt specified in your command line does not match the \
         system prompt saved in your conversation history.",
        &[
            "Use the previous system prompt from the conversation",
            "Use the system prompt specified by --system-prompt",
            "Append the CLI system prompt to the saved one",
        ],
    );

    match choice {
        Some(1) => {
            config.system_prompt = Some(saved_prompt.to_string());
        }
        Some(2) => {
            // Keep config.system_prompt as-is (CLI value)
        }
        Some(3) => {
            if saved_prompt.contains(cli_prompt) {
                eprintln!(
                    "The CLI system prompt is already contained in the saved prompt. \
                     Using the saved prompt as-is."
                );
                config.system_prompt = Some(saved_prompt.to_string());
            } else {
                config.system_prompt = Some(format!("{}\n\n{}", saved_prompt, cli_prompt));
            }
        }
        _ => {
            eprintln!("Cancelled.");
            std::process::exit(0);
        }
    }

    Ok(())
}

/// HTTP resume with system prompt conflict.
fn resolve_system_prompt_conflict_http(
    config: &mut AppConfig,
    saved_prompt: &str,
    _cli_prompt: &str,
    is_query: bool,
) -> anyhow::Result<()> {
    if is_query {
        if config.force {
            return Ok(());
        }
        eprintln!(
            "error: system prompt conflict when resuming conversation\n\n\
             The system prompt specified in your command line does not match the \
             system prompt saved in your conversation history.\n\n\
             To resolve, either omit --system-prompt to use the saved prompt, \
             or start a new conversation instead of resuming.\n\n\
             If you are connecting to an OpenAI compatible service other than Aura \
             you can use '--force' to ignore this warning."
        );
        std::process::exit(2);
    }

    let choice = prompt_choice(
        "The system prompt specified in your command line does not match the \
         system prompt saved in your conversation history.",
        &[
            "Use the previous system prompt from the conversation",
            "Use the system prompt specified by --system-prompt",
        ],
    );

    match choice {
        Some(1) => {
            config.system_prompt = Some(saved_prompt.to_string());
        }
        Some(2) => {
            // Keep config.system_prompt as-is (CLI value)
        }
        _ => {
            eprintln!("Cancelled.");
            std::process::exit(0);
        }
    }

    Ok(())
}
