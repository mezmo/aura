pub mod config;
pub mod env;
pub mod error;
pub mod lenient_bool;
pub mod lenient_int;
pub mod loader;
pub mod orchestration;
pub mod scratchpad;
pub mod session_store;
pub mod skills;
pub mod writer;

#[cfg(test)]
mod config_test;

#[cfg(test)]
mod test_env_lock;

pub use config::*;
pub use env::resolve_env_vars;
pub use error::ConfigError;
pub use loader::ConfigLoader;
pub use orchestration::{
    ArtifactsConfig, OrchestrationConfig, TimeoutsConfig, ToolVisibility, WorkerConfig,
};
pub use scratchpad::{ScratchpadConfig, ScratchpadToolEntry};
pub use session_store::{RedisSessionStoreConfig, SessionStoreBackend, SessionStoreConfig};
pub use skills::SkillName;
pub use writer::upsert_mcp_server;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Load a single TOML file into a Config.
fn load_single_config<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
    let contents = fs::read_to_string(path)?;
    let resolved = resolve_env_vars(&contents)?;
    check_legacy_top_level_llm(&resolved)?;

    let config: Config = toml::from_str(&resolved)?;
    config.validate()?;
    config.validate_memory_dir_writable()?;

    Ok(config)
}

/// Detect the legacy top-level `[llm]` table shape and emit a migration error.
///
/// As of 2026-04-21, `[llm]` lives under `[agent.llm]`. Without this check, a
/// stale top-level `[llm]` table is silently ignored (Config does not use
/// `deny_unknown_fields`) and the user gets a confusing downstream error.
fn check_legacy_top_level_llm(toml_str: &str) -> Result<(), ConfigError> {
    // Best-effort parse — if this fails, let the main deserialization surface
    // the real parse error rather than masking it.
    let Ok(value) = toml::from_str::<toml::Value>(toml_str) else {
        return Ok(());
    };
    if value.get("llm").is_some() {
        return Err(ConfigError::Validation(
            "Configuration uses the legacy top-level [llm] table. \
             Move it under [agent.llm] (and any [llm.additional_params] \
             under [agent.llm.additional_params]). Workers may optionally \
             override the LLM via [orchestration.worker.<name>.llm]."
                .to_string(),
        ));
    }
    Ok(())
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
    check_legacy_top_level_llm(&resolved)?;
    let config: Config = toml::from_str(&resolved)?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod load_config_tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_config(dir: &TempDir, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn minimal_toml(extra: &str) -> String {
        format!(
            r#"
{extra}
[agent]
name = "Test"
system_prompt = "p"
[agent.llm]
provider = "openai"
api_key = "test-key"
model = "gpt-4o"
"#
        )
    }

    #[test]
    fn load_config_no_memory_dir_passes() {
        let dir = TempDir::new().unwrap();
        let path = write_config(&dir, "agent.toml", &minimal_toml(""));
        load_config(path).expect("config without memory_dir should load");
    }

    #[test]
    fn load_config_writable_memory_dir_passes() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        let toml = minimal_toml(&format!(
            r#"memory_dir = "{}"
"#,
            memory_dir.display()
        ));
        let path = write_config(&dir, "agent.toml", &toml);
        load_config(&path).expect("config with writable memory_dir should load");
        assert!(
            memory_dir.exists(),
            "memory_dir should have been created by load_config"
        );
    }

    #[test]
    #[cfg(unix)]
    fn load_config_unwritable_memory_dir_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("locked");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::set_permissions(&memory_dir, std::fs::Permissions::from_mode(0o444)).unwrap();

        // Root ignores permission bits — skip rather than fail the assertion.
        let probe = memory_dir.join(".aura-startup-probe");
        if std::fs::write(&probe, b"probe").is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(&memory_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let toml = minimal_toml(&format!(
            r#"memory_dir = "{}"
"#,
            memory_dir.display()
        ));
        let path = write_config(&dir, "agent.toml", &toml);
        let err = load_config(&path).expect_err("unwritable memory_dir should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("not writable") || msg.contains("Permission denied"),
            "error should describe the write failure: {msg}"
        );

        // Restore permissions so TempDir cleanup succeeds.
        std::fs::set_permissions(&memory_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
