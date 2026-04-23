use anyhow::Result;
use serde::Deserialize;

use crate::cli::Args;

const DEFAULT_API_URL: &str = "http://localhost:8080";
#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    api_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub api_url: String,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub query: Option<String>,
    pub resume: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    pub force: bool,
    pub emit_events: bool,
}

impl AppConfig {
    /// Resolve config with precedence: CLI args > env vars (handled by clap) > config file > defaults
    pub fn load(args: &Args) -> Result<Self> {
        let file_config = load_config_file().unwrap_or_default();

        let api_url = args
            .api_url
            .clone()
            .or(file_config.api_url)
            .unwrap_or_else(|| DEFAULT_API_URL.to_string());

        let api_key = args.api_key.clone().or(file_config.api_key);

        let model = args.model.clone().or(file_config.model);
        let system_prompt = args.system_prompt.clone().or(file_config.system_prompt);

        let query = args.query.clone();
        let resume = args.resume.clone();

        let extra_headers = std::env::var("AURA_EXTRA_HEADERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|entry| {
                let mut parts = entry.splitn(2, ':');
                let key = parts.next()?.trim();
                let value = parts.next()?.trim();
                if key.is_empty() {
                    return None;
                }
                Some((key.to_string(), value.to_string()))
            })
            .collect();

        Ok(Self {
            api_url,
            api_key,
            model,
            system_prompt,
            query,
            resume,
            extra_headers,
            force: args.force,
            emit_events: args.emit_events,
        })
    }

    /// Build the chat completions endpoint URL from the base URL.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/v1/chat/completions", self.api_url.trim_end_matches('/'))
    }

    /// Build the models endpoint URL from the base URL.
    pub fn models_url(&self) -> String {
        format!("{}/v1/models", self.api_url.trim_end_matches('/'))
    }
}

fn load_config_file() -> Option<FileConfig> {
    let home_dir = dirs::home_dir()?;
    let config_path = home_dir.join(".aura").join("config.toml");

    if !config_path.exists() {
        return None;
    }

    let contents = std::fs::read_to_string(config_path).ok()?;
    toml::from_str(&contents).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Args;

    fn default_args() -> Args {
        Args {
            api_url: None,
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            force: false,
            emit_events: false,
            #[cfg(feature = "standalone-cli")]
            standalone: false,
            #[cfg(feature = "standalone-cli")]
            agent_config: None,
        }
    }

    #[test]
    fn load_defaults_when_no_args() {
        let args = default_args();
        let config = AppConfig::load(&args).unwrap();
        assert_eq!(config.api_url, "http://localhost:8080");
        assert!(config.api_key.is_none());
        assert!(config.model.is_none());
        assert!(config.system_prompt.is_none());
        assert!(config.query.is_none());
        assert!(config.resume.is_none());
    }

    #[test]
    fn cli_args_override_defaults() {
        let args = Args {
            api_url: Some("https://custom.api".to_string()),
            api_key: Some("secret".to_string()),
            model: Some("gpt-4".to_string()),
            system_prompt: Some("Be helpful".to_string()),
            query: Some("hello".to_string()),
            resume: None,
            force: false,
            emit_events: false,
            #[cfg(feature = "standalone-cli")]
            standalone: false,
            #[cfg(feature = "standalone-cli")]
            agent_config: None,
        };
        let config = AppConfig::load(&args).unwrap();
        assert_eq!(config.api_url, "https://custom.api");
        assert_eq!(config.api_key.as_deref(), Some("secret"));
        assert_eq!(config.model.as_deref(), Some("gpt-4"));
        assert_eq!(config.system_prompt.as_deref(), Some("Be helpful"));
        assert_eq!(config.query.as_deref(), Some("hello"));
    }

    #[test]
    fn chat_completions_url_no_trailing_slash() {
        let config = AppConfig {
            api_url: "http://localhost:8080".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            emit_events: false,
        };
        assert_eq!(
            config.chat_completions_url(),
            "http://localhost:8080/v1/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_with_trailing_slash() {
        let config = AppConfig {
            api_url: "http://localhost:8080/".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            emit_events: false,
        };
        assert_eq!(
            config.chat_completions_url(),
            "http://localhost:8080/v1/chat/completions"
        );
    }

    #[test]
    fn models_url_no_trailing_slash() {
        let config = AppConfig {
            api_url: "https://api.example.com".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            emit_events: false,
        };
        assert_eq!(config.models_url(), "https://api.example.com/v1/models");
    }

    #[test]
    fn models_url_with_trailing_slash() {
        let config = AppConfig {
            api_url: "https://api.example.com/".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            emit_events: false,
        };
        assert_eq!(config.models_url(), "https://api.example.com/v1/models");
    }
}
