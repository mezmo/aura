pub mod builder;
pub mod config;
pub mod env;
pub mod error;
pub mod loader;

#[cfg(test)]
mod config_test;

#[cfg(test)]
mod test_env_lock;

pub use builder::RigBuilder;
pub use config::*;
pub use env::resolve_env_vars;
pub use error::ConfigError;
pub use loader::ConfigLoader;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Load a single TOML file into a Config.
fn load_single_config<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
    let contents = fs::read_to_string(path)?;
    let resolved = resolve_env_vars(&contents)?;
    let migrated = migrate_legacy_top_level_llm(&resolved)?;
    let config: Config = toml::from_str(&migrated)?;
    config.validate()?;
    Ok(config)
}

/// Auto-migrate a deprecated top-level `[llm]` table into `[agent.llm]`.
///
/// As of 2026-04-21, LLM configuration lives under `[agent.llm]`. The
/// top-level `Config` struct does not use `deny_unknown_fields`, so a stale
/// `[llm]` table would be silently dropped and produce a confusing downstream
/// "missing LLM" error.
///
/// This function provides a temporary grace period for pre-existing configs
/// (e.g. auto-updating containers) by moving the entire `[llm]` value into
/// `[agent.llm]` and emitting a deprecation warning. It errors when the
/// intent is ambiguous (both `[llm]` and `[agent.llm]` present) or when
/// there is no `[agent]` section to migrate into. The fallback will be
/// removed in a future release.
fn migrate_legacy_top_level_llm(toml_str: &str) -> Result<String, ConfigError> {
    // Best-effort parse — if this fails, let the main deserialization surface
    // the real parse error rather than masking it.
    let Ok(mut value) = toml::from_str::<toml::Value>(toml_str) else {
        return Ok(toml_str.to_string());
    };

    // No top-level [llm] — nothing to do.
    let Some(top) = value.as_table_mut() else {
        return Ok(toml_str.to_string());
    };
    if !top.contains_key("llm") {
        return Ok(toml_str.to_string());
    }

    // [llm] exists. Make sure [agent] also exists so we have somewhere to move it.
    let agent = top.get("agent").ok_or_else(|| {
        ConfigError::Validation(
            "Configuration contains a top-level [llm] table but no [agent] section. \
             Move [llm] under [agent.llm] or add the required [agent] section."
                .to_string(),
        )
    })?;
    let agent_table = agent.as_table().ok_or_else(|| {
        ConfigError::Validation(
            "Configuration contains a top-level [llm] table but [agent] is not a table. \
             Move [llm] under [agent.llm]."
                .to_string(),
        )
    })?;

    // Refuse to migrate when both [llm] and [agent.llm] are present — ambiguous.
    if agent_table.contains_key("llm") {
        return Err(ConfigError::Validation(
            "Configuration contains both a top-level [llm] table and [agent.llm]. \
             Remove the top-level [llm] table and keep only [agent.llm]."
                .to_string(),
        ));
    }

    // Perform the migration: move top-level llm into agent.llm.
    let llm_value = top.remove("llm").expect("checked contains_key above");
    let agent_table = top
        .get_mut("agent")
        .expect("checked above")
        .as_table_mut()
        .expect("checked above");
    agent_table.insert("llm".to_string(), llm_value);

    tracing::warn!(
        "DEPRECATED CONFIG: top-level [llm] was auto-migrated to [agent.llm]. \
         Please update your TOML to use [agent.llm] directly — this fallback \
         will be removed in a future release."
    );

    let migrated = toml::to_string(&value).map_err(|e| {
        ConfigError::Validation(format!("Failed to serialize migrated config: {e}"))
    })?;
    Ok(migrated)
}

/// Load and parse TOML configuration(s) from a file or directory.
///
/// - If `path` is a file, returns a single-element vec.
/// - If `path` is a directory, loads all `.toml` files in it.
///
/// Light validation occurs to ensure that:
/// - Each config can be serialized and deserialized correctly.
/// - Each config is uniquely identifiable by alias or name.
pub fn load_config<P: AsRef<Path>>(path: P) -> Result<Vec<Config>, ConfigError> {
    let path = path.as_ref();

    let configs = if path.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
            .collect();
        entries.sort_by_key(|e| e.path());

        let mut configs = Vec::new();
        for entry in entries {
            configs.push(load_single_config(entry.path())?);
        }
        if configs.is_empty() {
            return Err(ConfigError::Validation(
                "No .toml configuration files found in directory".to_string(),
            ));
        }
        configs
    } else {
        vec![load_single_config(path)?]
    };

    validate_unique_identifiers(&configs)?;
    Ok(configs)
}

/// Validate that each config is uniquely identifiable by alias or name.
///
/// Each config's effective identifier is its alias (if set) or its name.
/// All effective identifiers must be unique. Additionally, duplicate aliases
/// get a distinct error message to help the user fix the right thing.
pub fn validate_unique_identifiers(configs: &[Config]) -> Result<(), ConfigError> {
    let mut seen_aliases = HashSet::new();
    let mut seen_ids = HashSet::new();

    for config in configs {
        let id = config.agent.alias.as_deref().unwrap_or(&config.agent.name);

        if config.agent.alias.is_some() && !seen_aliases.insert(id) {
            return Err(ConfigError::Validation(format!(
                "Duplicate alias '{id}'! Configurations must have a unique alias."
            )));
        }

        if !seen_ids.insert(id) {
            return Err(ConfigError::Validation(format!(
                "Multiple configurations with the same agent name '{id}'! Use an alias to differentiate between two agents with the same name."
            )));
        }
    }

    Ok(())
}

/// Load config from a string (useful for testing)
pub fn load_config_from_str(contents: &str) -> Result<Config, ConfigError> {
    let resolved = resolve_env_vars(contents)?;
    let migrated = migrate_legacy_top_level_llm(&resolved)?;
    let config: Config = toml::from_str(&migrated)?;
    config.validate()?;
    Ok(config)
}
