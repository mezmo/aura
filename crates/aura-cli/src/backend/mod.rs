pub mod http;

#[cfg(feature = "standalone-cli")]
pub mod direct;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;

use crate::api::stream::{StreamHandler, StreamResult};
use crate::api::types::{Message, ToolDefinition};
use crate::cli::Args;
use crate::config::AppConfig;

/// Backend for communicating with an LLM.
///
/// `Http` connects to an aura-web-server via HTTP/SSE (existing behavior).
/// `Direct` (behind `standalone-cli` feature) builds an agent in-process
/// from TOML config and consumes the native stream directly.
pub enum Backend {
    Http(http::HttpBackend),
    #[cfg(feature = "standalone-cli")]
    Direct(direct::DirectBackend),
}

impl Backend {
    /// Create the appropriate backend based on config.
    ///
    /// If `--standalone` is set, uses `DirectBackend` with the config from `--config`.
    /// Otherwise, uses `HttpBackend` (HTTP/SSE to aura-web-server).
    pub fn from_config(
        _rt: &tokio::runtime::Runtime,
        config: &AppConfig,
        _args: &Args,
        telemetry: &aura_telemetry::TelemetryHandle,
    ) -> Result<Self> {
        #[cfg(feature = "standalone-cli")]
        if _args.standalone {
            // validate_standalone_args guarantees --config is present when --standalone is set
            let config_path = _args.agent_config.as_ref().unwrap();
            let direct = _rt.block_on(direct::DirectBackend::from_toml(
                config_path,
                config.extra_headers.clone(),
                telemetry.clone(),
            ))?;
            return Ok(Self::Direct(direct));
        }

        // One-shot `--query` never participates in telemetry (privacy
        // contract in docs/telemetry.md): withhold the handle so the HTTP
        // backend cannot propagate `X-Aura-Telemetry-Consent` to the
        // server even when the user previously opted in. The REPL path
        // keeps the live handle so an Enabled session still propagates.
        let backend_telemetry = if config.query.is_some() {
            None
        } else {
            Some(telemetry.clone())
        };

        Ok(Self::Http(http::HttpBackend::new(
            config.clone(),
            backend_telemetry,
        )))
    }

    /// Send a streaming chat completion and process the response, invoking
    /// `handler`'s methods for each event.
    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        session_id: &str,
        cancel: Arc<AtomicBool>,
        handler: &mut impl StreamHandler,
    ) -> Result<StreamResult> {
        match self {
            Self::Http(http) => {
                http.stream_chat(messages, tools, session_id, cancel, handler)
                    .await
            }
            #[cfg(feature = "standalone-cli")]
            Self::Direct(direct) => {
                direct
                    .stream_chat(messages, tools, session_id, cancel, handler)
                    .await
            }
        }
    }

    /// Ask the LLM for a short summary/title of the given text.
    /// Returns (summary_text, optional (prompt_tokens, completion_tokens)).
    ///
    /// `session_id` should be the resolved summary session ID (typically the
    /// chat session ID with a `-summary` suffix) so server-side tracing can
    /// correlate title-gen calls with their parent chat session without
    /// polluting the chat session itself.
    pub async fn summarize(
        &self,
        text: &str,
        session_id: &str,
    ) -> Result<(String, Option<(u64, u64)>)> {
        match self {
            Self::Http(http) => http.summarize(text, session_id).await,
            #[cfg(feature = "standalone-cli")]
            Self::Direct(direct) => direct.summarize(text, session_id).await,
        }
    }

    /// Fetch available model IDs.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        match self {
            Self::Http(http) => http.list_models().await,
            #[cfg(feature = "standalone-cli")]
            Self::Direct(direct) => direct.list_models().await,
        }
    }

    /// Access the direct backend (standalone mode only). Panics if not Direct.
    #[cfg(feature = "standalone-cli")]
    pub fn as_direct(&self) -> &direct::DirectBackend {
        match self {
            Self::Direct(d) => d,
            _ => panic!("as_direct() called on non-Direct backend"),
        }
    }

    /// Access the direct backend mutably (standalone mode only). Panics if not Direct.
    #[cfg(feature = "standalone-cli")]
    pub fn as_direct_mut(&mut self) -> &mut direct::DirectBackend {
        match self {
            Self::Direct(d) => d,
            _ => panic!("as_direct_mut() called on non-Direct backend"),
        }
    }

    /// Initialize the model cache for the `/model` command.
    ///
    /// - HttpBackend: configures background HTTP fetching from `/v1/models`
    /// - DirectBackend: seeds the cache immediately from loaded configs
    pub fn setup_model_cache(&self, config: &AppConfig) {
        match self {
            Self::Http(_) => {
                crate::ui::prompt::set_model_fetch_config(
                    config.models_url(),
                    config.api_key.clone(),
                    config.extra_headers.clone(),
                );
            }
            #[cfg(feature = "standalone-cli")]
            Self::Direct(direct) => {
                crate::ui::prompt::seed_model_cache(direct.model_ids());
            }
        }
    }
}

#[cfg(test)]
mod telemetry_propagation_tests {
    use super::*;
    use crate::cli::Args;
    use aura_telemetry::properties::{DeploymentMethod, OsFamily, Source};
    use aura_telemetry::{TelemetryConfig, TelemetryState};
    use std::time::Duration;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn args_with_query(query: Option<&str>) -> Args {
        Args {
            api_url: None,
            api_key: None,
            model: None,
            system_prompt: None,
            query: query.map(str::to_string),
            resume: None,
            force: false,
            pretty: false,
            enable_client_tools: None,
            enable_final_response_summary: None,
            #[cfg(feature = "standalone-cli")]
            standalone: false,
            #[cfg(feature = "standalone-cli")]
            agent_config: None,
            log_file: None,
        }
    }

    /// An `Enabled` handle with no sink (no runtime is current here). The
    /// consent gate only reads `state()`, so this is enough to prove the
    /// header would be propagated in REPL mode and withheld in one-shot.
    fn enabled_handle() -> aura_telemetry::TelemetryHandle {
        let cfg = TelemetryConfig {
            endpoint: "http://127.0.0.1:1/no-such-host".into(),
            api_key: "phc_test".into(),
            install_id: Uuid::new_v4(),
            install_id_path: None,
            session_id: Uuid::new_v4(),
            source: Source::Cli,
            os_family: OsFamily::current(),
            deployment_method: DeploymentMethod::Local,
            aura_version: "9.9.9-test",
            inspection_log_path: None,
            state: TelemetryState::Enabled,
            channel_capacity: 16,
            batch_size: 8,
            flush_interval: Duration::from_secs(60),
            post_timeout: Duration::from_millis(500),
            http_client: None,
        };
        aura_telemetry::init(cfg)
    }

    fn load_config(args: &Args) -> AppConfig {
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        std::fs::create_dir(home.path().join(".aura")).unwrap();
        let global = home.path().join(".aura");
        AppConfig::load_with_dirs(args, cwd.path(), Some(&global)).unwrap()
    }

    #[test]
    fn repl_backend_carries_telemetry_handle() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let args = args_with_query(None);
        let config = load_config(&args);
        let handle = enabled_handle();
        let backend = Backend::from_config(&rt, &config, &args, &handle).unwrap();
        match backend {
            Backend::Http(b) => assert!(
                b.has_telemetry(),
                "REPL backend must keep the telemetry handle so an Enabled session \
                 propagates consent"
            ),
            #[cfg(feature = "standalone-cli")]
            _ => panic!("expected Http backend"),
        }
    }

    #[test]
    fn one_shot_query_backend_drops_telemetry_handle() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let args = args_with_query(Some("hello"));
        let config = load_config(&args);
        let handle = enabled_handle();
        let backend = Backend::from_config(&rt, &config, &args, &handle).unwrap();
        match backend {
            Backend::Http(b) => assert!(
                !b.has_telemetry(),
                "one-shot --query must not carry a telemetry handle \
                 (no X-Aura-Telemetry-Consent header)"
            ),
            #[cfg(feature = "standalone-cli")]
            _ => panic!("expected Http backend"),
        }
    }
}
