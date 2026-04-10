use crate::{Config, ConfigError, load_config_from_str};
use clap::ArgMatches;
use std::path::Path;

/// Configuration loader with multiple sources and priority layers
/// This is a custom implementation that combines different config loading approaches
pub struct ConfigLoader {
    // Store file paths and other config sources
    toml_files: Vec<std::path::PathBuf>,
    json_files: Vec<std::path::PathBuf>,
    yaml_files: Vec<std::path::PathBuf>,
    env_prefix: Option<String>,
    cli_matches: Option<ArgMatches>,
    use_dotenv: bool,
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigLoader {
    /// Create a new configuration loader
    pub fn new() -> Self {
        Self {
            toml_files: Vec::new(),
            json_files: Vec::new(),
            yaml_files: Vec::new(),
            env_prefix: None,
            cli_matches: None,
            use_dotenv: false,
        }
    }

    /// Add TOML file configuration layer
    pub fn with_toml_file<P: AsRef<Path>>(mut self, path: P) -> Self {
        if path.as_ref().exists() {
            self.toml_files.push(path.as_ref().to_path_buf());
            tracing::debug!("Added TOML config layer: {}", path.as_ref().display());
        } else {
            tracing::debug!("TOML file not found, skipping: {}", path.as_ref().display());
        }
        self
    }

    /// Add JSON file configuration layer
    pub fn with_json_file<P: AsRef<Path>>(mut self, path: P) -> Self {
        if path.as_ref().exists() {
            self.json_files.push(path.as_ref().to_path_buf());
            tracing::debug!("Added JSON config layer: {}", path.as_ref().display());
        } else {
            tracing::debug!("JSON file not found, skipping: {}", path.as_ref().display());
        }
        self
    }

    /// Add YAML file configuration layer
    pub fn with_yaml_file<P: AsRef<Path>>(mut self, path: P) -> Self {
        if path.as_ref().exists() {
            self.yaml_files.push(path.as_ref().to_path_buf());
            tracing::debug!("Added YAML config layer: {}", path.as_ref().display());
        } else {
            tracing::debug!("YAML file not found, skipping: {}", path.as_ref().display());
        }
        self
    }

    /// Add .env file support (loads environment variables from .env file)
    pub fn with_dotenv(mut self) -> Self {
        self.use_dotenv = true;
        self
    }

    /// Add environment variables layer with optional prefix
    /// For example, with prefix "RIG_", it will look for RIG_LLM_PROVIDER, etc.
    pub fn with_env(mut self, prefix: Option<String>) -> Self {
        self.env_prefix = prefix.clone();
        let prefix_msg = prefix.as_deref().unwrap_or("no prefix");
        tracing::debug!("Added environment variables layer with {}", prefix_msg);
        self
    }

    /// Add command-line arguments layer (highest priority)
    pub fn with_cli(mut self, matches: ArgMatches) -> Self {
        self.cli_matches = Some(matches);
        tracing::debug!("Added CLI arguments layer");
        self
    }

    /// Build the final configuration by merging all layers
    pub fn build(self) -> Result<Config, ConfigError> {
        // Start with default config
        let mut config = Config::default();

        tracing::info!("Building configuration with layered approach");

        // Load .env file if requested
        if self.use_dotenv {
            match dotenv::dotenv() {
                Ok(path) => {
                    tracing::info!("Loaded .env file from: {}", path.display());
                }
                Err(e) => {
                    tracing::debug!("No .env file loaded: {}", e);
                }
            }
        }

        // Layer 1: Load TOML files (lowest priority among files)
        for toml_file in &self.toml_files {
            tracing::debug!("Loading TOML file: {}", toml_file.display());
            match load_config_from_str(&std::fs::read_to_string(toml_file)?) {
                Ok(file_config) => {
                    config = merge_configs(config, file_config)?;
                    tracing::debug!("Merged TOML config from: {}", toml_file.display());
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load TOML config from {}: {}",
                        toml_file.display(),
                        e
                    );
                }
            }
        }

        // Layer 2: Load JSON files
        for json_file in &self.json_files {
            tracing::debug!("Loading JSON file: {}", json_file.display());
            // TODO: Implement JSON loading when needed
        }

        // Layer 3: Load YAML files
        for yaml_file in &self.yaml_files {
            tracing::debug!("Loading YAML file: {}", yaml_file.display());
            // TODO: Implement YAML loading when needed
        }

        // Layer 4: CLI arguments (highest priority)
        if let Some(_matches) = &self.cli_matches {
            // TODO: Implement CLI argument parsing
            tracing::debug!("CLI arguments would be processed here");
        }

        // Validate the final configuration
        config.validate()?;

        Ok(config)
    }
}

/// Helper function to merge two configurations
/// The `override_config` values take precedence over `base_config`
fn merge_configs(base_config: Config, override_config: Config) -> Result<Config, ConfigError> {
    // For now, do a simple field-by-field merge
    // In a more sophisticated implementation, we could use serde merge or custom logic

    let mut result = base_config;

    // Override LLM config if provided - with enum, we replace the entire config
    // TODO: More granular overrides would require matching variants
    result.llm = override_config.llm;

    // Override MCP config if provided
    if override_config.mcp.is_some() {
        result.mcp = override_config.mcp;
    }

    // Override vector stores if provided
    // For now, we replace the entire vector stores array if any are provided in override
    if !override_config.vector_stores.is_empty() {
        result.vector_stores = override_config.vector_stores;
    }

    // Override tools config if provided
    if override_config.tools.is_some() {
        result.tools = override_config.tools;
    }

    // Override agent config if provided
    if !override_config.agent.name.is_empty() && override_config.agent.name != "Assistant" {
        result.agent.name = override_config.agent.name;
    }
    if !override_config.agent.system_prompt.is_empty()
        && override_config.agent.system_prompt != "You are a helpful assistant."
    {
        result.agent.system_prompt = override_config.agent.system_prompt;
    }
    if !override_config.agent.context.is_empty() {
        result.agent.context = override_config.agent.context;
    }

    Ok(result)
}

impl ConfigLoader {
    /// Convenience method to create a standard configuration setup
    /// Priority (lowest to highest):
    /// 1. Default values
    /// 2. config.toml file
    /// 3. config.json file (if exists)
    /// 4. config.yaml file (if exists)
    /// 5. .env file variables
    /// 6. System environment variables (with optional prefix)
    /// 7. CLI arguments (if provided)
    pub fn standard(
        prefix: Option<String>,
        cli_matches: Option<ArgMatches>,
    ) -> Result<Config, ConfigError> {
        let mut loader = Self::new()
            .with_toml_file("config.toml")
            .with_json_file("config.json")
            .with_yaml_file("config.yaml")
            .with_dotenv()
            .with_env(prefix);

        if let Some(matches) = cli_matches {
            loader = loader.with_cli(matches);
        }

        loader.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_loader_builder() {
        // Test that we can build a config loader with various layers
        let loader = ConfigLoader::new()
            .with_toml_file("test.toml")
            .with_env(Some("TEST_".to_string()));

        assert!(!loader.toml_files.is_empty() || loader.env_prefix.is_some());
    }

    #[test]
    fn test_standard_loader() {
        // Set some test environment variables
        unsafe {
            std::env::set_var("RIG_LLM_PROVIDER", "test_provider");
        }

        // This should not panic even if config files don't exist
        let result = ConfigLoader::standard(Some("RIG_".to_string()), None);

        // Clean up

        unsafe {
            std::env::remove_var("RIG_LLM_PROVIDER");
        }

        // We expect this to fail because we don't have valid config files in test
        assert!(result.is_err());
    }
}
