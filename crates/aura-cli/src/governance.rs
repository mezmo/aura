//! Governance subcommand handlers for the CLI.
//!
//! These commands are standalone-mode only and operate on TOML agent configs
//! directly, without requiring a running web server.

use anyhow::{Context, Result};
use aura::governance::build_catalog;

#[derive(Debug, clap::Subcommand)]
pub enum GovernanceCommands {
    /// Discover all MCP tools and send a catalog snapshot to the governance webhook.
    Sync,
    /// Display the computed instance ID for each loaded agent config.
    Info,
}

pub fn run(confs: &[aura_config::Config], command: &GovernanceCommands) -> Result<()> {
    match command {
        GovernanceCommands::Sync => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(sync_all(confs))
        }
        GovernanceCommands::Info => {
            info_all(confs);
            Ok(())
        }
    }
}

fn info_all(confs: &[aura_config::Config]) {
    for conf in confs {
        let id = aura::instance_id::instance_id(&conf.agent);
        println!("agent \"{}\"  instance_id: {id}", conf.agent.name);
    }
}

async fn sync_all(confs: &[aura_config::Config]) -> Result<()> {
    if confs.is_empty() {
        anyhow::bail!("No agent configs found");
    }

    for conf in confs {
        let res = sync_one(conf).await;
        if let Err(e) = res {
            let name = &conf.agent.name;
            tracing::warn!("Failed to sync goverence data for agent {name}: {e}");
        }
    }
    Ok(())
}

async fn sync_one(config: &aura_config::Config) -> Result<()> {
    if let Some(governance_config) = &config.governance
        && let Some(catalog_config) = &governance_config.catalog
        && let Some(catalog) = build_catalog(config).await
    {
        let client = aura::governance::CatalogClient::from_config(catalog_config, None)
            .context("Failed to create catalog client")?;
        client
            .send(&catalog)
            .await
            .context("Failed to send catalog")?;
    }
    Ok(())
}
