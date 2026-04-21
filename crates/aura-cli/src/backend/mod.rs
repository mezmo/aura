pub mod http;

#[cfg(feature = "standalone-cli")]
pub mod direct;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Result;

use crate::api::stream::StreamResult;
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
    pub fn from_config(config: &AppConfig, _args: &Args) -> Result<Self> {
        #[cfg(feature = "standalone-cli")]
        if _args.standalone {
            // validate_standalone_args guarantees --config is present when --standalone is set
            let config_path = _args.agent_config.as_ref().unwrap();
            let rt = tokio::runtime::Runtime::new()?;
            let direct = rt.block_on(direct::DirectBackend::from_toml(
                config_path,
                config.extra_headers.clone(),
            ))?;
            return Ok(Self::Direct(direct));
        }

        Ok(Self::Http(http::HttpBackend::new(config.clone())))
    }

    /// Send a streaming chat completion and process the response,
    /// invoking callbacks for each event.
    ///
    /// The callback signatures match `process_stream` exactly so that
    /// callers (REPL, oneshot) need minimal changes.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        session_id: &str,
        cancel: Arc<AtomicBool>,
        on_token: impl FnMut(&str),
        on_tool_requested: impl FnMut(&str, &str, &BTreeMap<String, serde_json::Value>),
        on_tool_start: impl FnMut(&str, &str),
        on_tool_complete: impl FnMut(&str, &str, Duration, Option<&str>),
        on_usage: impl FnMut(u64, u64),
        on_raw_event: impl FnMut(&str, &str),
        on_orchestrator_event: impl FnMut(&str, &serde_json::Value),
    ) -> Result<StreamResult> {
        match self {
            Self::Http(http) => {
                http.stream_chat(
                    messages,
                    tools,
                    session_id,
                    cancel,
                    on_token,
                    on_tool_requested,
                    on_tool_start,
                    on_tool_complete,
                    on_usage,
                    on_raw_event,
                    on_orchestrator_event,
                )
                .await
            }
            #[cfg(feature = "standalone-cli")]
            Self::Direct(direct) => {
                direct
                    .stream_chat(
                        messages,
                        tools,
                        session_id,
                        cancel,
                        on_token,
                        on_tool_requested,
                        on_tool_start,
                        on_tool_complete,
                        on_usage,
                        on_raw_event,
                        on_orchestrator_event,
                    )
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
