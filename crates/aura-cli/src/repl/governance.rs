//! REPL handler for `/governance` commands.

use super::registry::{CommandContext, CommandOutcome};
use crate::backend::Backend;
use crate::theme::{AuraStyle, Themed};

/// Handle `/governance [subcommand]`.
///
/// Governance commands are standalone-mode only. HTTP mode returns an error.
pub(crate) fn handle_governance(ctx: &mut CommandContext, args: &str) -> CommandOutcome {
    let mut parts = args.split_whitespace();
    let subcommand = parts.next();
    let subargs = parts.collect::<Vec<&str>>();

    match subcommand {
        Some("sync") if subargs.is_empty() => handle_sync(ctx),
        _ => {
            println!(
                "{}: /governance <subcommand>\n\n \
                Subcommands:\n\n \
                   sync    {}\n\n",
                "Usage".themed(AuraStyle::Primary),
                "Discover MCP tools and send catalog to governance webhook"
                    .themed(AuraStyle::Muted)
            );
            CommandOutcome::Handled
        }
    }
}

/// Handle `/governance sync`.
fn handle_sync(ctx: &mut CommandContext) -> CommandOutcome {
    match ctx.backend {
        Backend::Http(_) => {
            println!(
                "{}: governance commands are only supported in standalone mode\n\n \
                The /governance sync command loads agent configs directly and cannot run\n \
                when connected to an AURA web server over HTTP.\n\n \
                {}\n\n",
                "Error".themed(AuraStyle::Error),
                "To use governance commands, run the CLI in standalone mode:\n  \
                 • omit --api-url to use the default config.toml\n  \
                 • pass --config <path> to specify a config file or directory"
                    .themed(AuraStyle::Muted)
            );
            CommandOutcome::Handled
        }
        #[cfg(feature = "standalone-cli")]
        Backend::Direct(direct) => ctx.rt.block_on(run_sync_standalone(direct)),
    }
}

/// Run catalog sync in standalone mode.
#[cfg(feature = "standalone-cli")]
async fn run_sync_standalone(direct: &crate::backend::direct::DirectBackend) -> CommandOutcome {
    use aura::governance::{CatalogClient, build_catalog};

    println!(
        "{} {}",
        "●".themed(AuraStyle::Emphasis),
        "Syncing governance information...".themed(AuraStyle::Primary),
    );
    let configs = direct.configs();
    for config in configs {
        println!(
            "  {} {}",
            "├─".themed(AuraStyle::Connector),
            format!("inspecting config for agent \"{}\"", config.agent.name)
                .themed(AuraStyle::Muted)
        );
        if let Some(governance_config) = &config.governance
            && let Some(catalog_config) = &governance_config.catalog
            && let Some(catalog) = build_catalog(config).await
        {
            println!(
                "  {} {} {}",
                "│ ".themed(AuraStyle::Connector),
                "├─".themed(AuraStyle::Connector),
                "governance is enabled, syncing information".themed(AuraStyle::Muted)
            );
            let client = match CatalogClient::from_config(catalog_config, None) {
                Ok(c) => c,
                Err(e) => {
                    println!(
                        "  {} {} {}\n       {}",
                        "│ ".themed(AuraStyle::Connector),
                        "└─".themed(AuraStyle::Connector),
                        "Failed to create webhook client".themed(AuraStyle::Emphasis),
                        format!("{e}").themed(AuraStyle::Error)
                    );
                    return CommandOutcome::Handled;
                }
            };

            match client.send(&catalog).await {
                Ok(()) => {
                    println!(
                        "  {} {} {}",
                        "│ ".themed(AuraStyle::Connector),
                        "└─".themed(AuraStyle::Connector),
                        format!("Governance sync complete (event_id: {})", catalog.event_id)
                            .themed(AuraStyle::Success)
                    );
                }
                Err(e) => {
                    println!(
                        "  {} {} {}\n       {}",
                        "│ ".themed(AuraStyle::Connector),
                        "└─".themed(AuraStyle::Connector),
                        format!("Governance sync failed (event_id: {})", catalog.event_id)
                            .themed(AuraStyle::Emphasis),
                        format!("{e}").themed(AuraStyle::Error)
                    );
                }
            }
        }
    }

    println!(
        "{} {}\n",
        "●".themed(AuraStyle::Emphasis),
        "Finished governence sync".themed(AuraStyle::Primary)
    );

    CommandOutcome::Handled
}
