pub mod builder;
pub mod config;
pub mod env;
pub mod error;
pub mod loader;

#[cfg(test)]
mod config_test;

pub use builder::RigBuilder;
pub use config::*;
pub use env::resolve_env_vars;
pub use error::ConfigError;
pub use loader::ConfigLoader;

use std::fs;
use std::path::Path;

/// Load and parse a TOML configuration file
pub fn load_config<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
    let contents = fs::read_to_string(path)?;
    let resolved = resolve_env_vars(&contents)?;
    let config: Config = toml::from_str(&resolved)?;
    config.validate()?;
    Ok(config)
}

/// Load config from a string (useful for testing)
pub fn load_config_from_str(contents: &str) -> Result<Config, ConfigError> {
    let resolved = resolve_env_vars(contents)?;
    let config: Config = toml::from_str(&resolved)?;
    config.validate()?;
    Ok(config)
}
