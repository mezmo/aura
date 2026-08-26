//! CLI governance subcommand handlers.

use anyhow::{Context, Result, bail};
use std::time::Duration;

use crate::cli::GovernanceAction;

/// Run a governance action.
///
/// Governance commands require standalone mode (they need to load TOML configs
/// directly). HTTP mode is not supported.
pub fn run(action: &GovernanceAction) -> Result<()> {
    match action {
        GovernanceAction::Sync { config_path } => run_sync(config_path.as_deref()),
    }
}

/// Run the `governance sync` command.
///
/// Discovers MCP tools from all configs and POSTs a catalog snapshot to the
/// governance webhook. Outputs a JSON DeliveryReport on success.
fn run_sync(config_path: Option<&str>) -> Result<()> {
    #[cfg(not(feature = "standalone-cli"))]
    {
        _ = config_path;
        bail!(
            "governance sync requires standalone mode (the standalone-cli feature)\n\n\
             This build of aura cannot load agent configs directly. Rebuild with \
             the standalone-cli feature (enabled by default) to use governance commands."
        );
    }

    #[cfg(feature = "standalone-cli")]
    {
        // Load .env so {{ env.* }} references resolve
        dotenvy::dotenv().ok();
        if let Some(cfg) = config_path
            && let Some(dir) = std::path::Path::new(cfg).parent()
        {
            dotenvy::from_path(dir.join(".env")).ok();
        }

        let config_path = config_path.unwrap_or("config.toml");
        let configs = aura_config::load_config(config_path)
            .with_context(|| format!("failed to load config from {config_path}"))?;

        if configs.is_empty() {
            bail!("no agent configs found in {config_path}");
        }

        // Find the first config with governance.catalog configured
        let (gov_config, _target_config) = configs
            .iter()
            .find_map(|cfg| {
                cfg.governance
                    .as_ref()
                    .and_then(|g| g.catalog.as_ref())
                    .map(|gc| (gc, cfg))
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "governance catalog webhook not configured\n\n\
                     Add a [governance.catalog] section to your config:\n\n\
                     [governance.catalog]\n\
                     url = \"https://example.com/catalog\"\n\
                     timeout_secs = 30  # optional, defaults to 30"
                )
            })?;

        // Build the catalog and deliver it
        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(async {
            // Build the catalog envelope from all configs
            let timeout = Duration::from_secs(gov_config.timeout_secs);
            let envelope = aura::governance::build_catalog_multi(&configs, None, timeout).await;

            // Deliver to webhook
            aura::governance::deliver(gov_config, &envelope, None).await
        });

        match result {
            Ok(report) => {
                // Output JSON report to stdout
                let json = serde_json::to_string_pretty(&report)?;
                println!("{json}");
                Ok(())
            }
            Err(e) => {
                bail!("governance sync failed: {e}");
            }
        }
    }
}
