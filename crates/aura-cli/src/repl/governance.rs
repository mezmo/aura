//! `/governance` — MCP tool catalog sync command for governance webhooks.
//!
//! Standalone-mode only. HTTP mode returns a clear error message.

use super::registry::CommandContext;
use crate::theme::{AuraStyle, Themed};
use crate::ui::prompt::redraw_input_frame;

/// Handle the `/governance` slash command.
pub(crate) fn handle_governance(ctx: &mut CommandContext, args: &str) {
    match args.trim() {
        "sync" => handle_sync(ctx),
        "" => {
            println!("Usage: /governance sync\n");
            println!(
                "The sync command discovers all MCP tools and sends a catalog \
                 snapshot to the configured governance webhook."
            );
        }
        other => {
            println!(
                "Unknown /governance subcommand: {other}\n\
                 Run /governance sync to send the MCP tool catalog."
            );
        }
    }
    redraw_input_frame();
}

/// Handle the `/governance sync` command.
fn handle_sync(ctx: &mut CommandContext) {
    #[cfg(not(feature = "standalone-cli"))]
    {
        _ = ctx;
        println!(
            "{}\n\n\
             Governance commands require standalone mode. This build of aura \
             is HTTP-only and cannot load agent configs directly.\n\n\
             Rebuild with the standalone-cli feature (enabled by default) or \
             use the `aura governance sync` CLI command from a standalone build.",
            "Error: standalone mode required".themed(AuraStyle::Error)
        );
        return;
    }

    #[cfg(feature = "standalone-cli")]
    {
        use crate::backend::Backend;
        use std::time::Duration;

        // Check if we're in standalone mode
        let Backend::Direct(direct) = ctx.backend else {
            println!(
                "{}\n\n\
                 Governance commands are only available in standalone mode.\n\
                 Use the `aura governance sync` CLI command instead.",
                "Error: HTTP mode not supported".themed(AuraStyle::Error)
            );
            return;
        };

        // Get the configs from the backend
        let configs = direct.configs();

        // Find the first config with governance.catalog configured
        let gov_config = configs
            .iter()
            .find_map(|cfg| cfg.governance.as_ref().and_then(|g| g.catalog.as_ref()));

        let Some(gov_config) = gov_config else {
            println!(
                "{}\n\n\
                 No governance catalog webhook is configured. Add a \
                 [governance.catalog] section to your config:\n\n\
                 [governance.catalog]\n\
                 url = \"https://example.com/catalog\"\n\
                 timeout_secs = 30  # optional, defaults to 30",
                "Error: webhook not configured".themed(AuraStyle::Error)
            );
            return;
        };

        println!("{}", "Discovering MCP tools...".themed(AuraStyle::Muted));

        let result = ctx.rt.block_on(async {
            let timeout = Duration::from_secs(gov_config.timeout_secs);
            let envelope = aura::governance::build_catalog_multi(configs, None, timeout).await;

            println!(
                "{} {} agent(s), {} server(s)",
                "Catalog built:".themed(AuraStyle::Muted),
                envelope.agents.len(),
                envelope
                    .agents
                    .iter()
                    .map(|a| a.mcp_servers.len())
                    .sum::<usize>()
            );

            println!("{}", "Sending to webhook...".themed(AuraStyle::Muted));

            aura::governance::deliver(gov_config, &envelope, None).await
        });

        match result {
            Ok(report) => {
                println!(
                    "\n{} (HTTP {})",
                    "Catalog sync successful".themed(AuraStyle::Success),
                    report.status_code
                );
                println!(
                    "  Event ID: {}",
                    report.event_id.as_str().themed(AuraStyle::Identifier)
                );
                println!("  Agents:   {}", report.agents_count);
                println!("  Servers:  {}", report.servers_count);
                println!("  Tools:    {}", report.tools_count);
            }
            Err(e) => {
                println!(
                    "\n{}\n{}",
                    "Catalog sync failed".themed(AuraStyle::Error),
                    e.to_string().themed(AuraStyle::Muted)
                );
            }
        }
    }
}
