use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("Environment variable error: {0}")]
    EnvVar(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Rig error: {0}")]
    Rig(String),
}
