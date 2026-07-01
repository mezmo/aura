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
    /// When `is_standalone` is true, uses `DirectBackend` with the config from
    /// `--config` (or `config.toml` in the current directory if omitted).
    /// Otherwise, uses `HttpBackend` (HTTP/SSE to aura-web-server).
    pub fn from_config(
        _rt: &tokio::runtime::Runtime,
        config: &AppConfig,
        _args: &Args,
        _is_standalone: bool,
    ) -> Result<Self> {
        #[cfg(feature = "standalone-cli")]
        if _is_standalone {
            let default_config = String::from("config.toml");
            let config_path = _args.agent_config.as_ref().unwrap_or(&default_config);
            let direct = _rt.block_on(direct::DirectBackend::from_toml(
                config_path,
                config.extra_headers.clone(),
            ))?;
            return Ok(Self::Direct(direct));
        }

        Ok(Self::Http(http::HttpBackend::new(config.clone())))
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

    /// Human-readable description of what this CLI is connected to, for the
    /// startup banner. Standalone mode runs the agent in-process (no remote
    /// server); HTTP mode reports the server's base URL and version.
    pub async fn connection_summary(&self) -> String {
        match self {
            Self::Http(http) => http.connection_summary().await,
            #[cfg(feature = "standalone-cli")]
            Self::Direct(_) => "standalone".to_string(),
        }
    }

    /// Worker overviews for the startup banner. Empty when no workers are
    /// configured or the HTTP fetch fails.
    pub async fn all_worker_overviews(&self) -> crate::worker::WorkerOverviews {
        match self {
            Self::Http(http) => http.worker_overviews().await,
            #[cfg(feature = "standalone-cli")]
            Self::Direct(direct) => direct.worker_overviews(),
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
