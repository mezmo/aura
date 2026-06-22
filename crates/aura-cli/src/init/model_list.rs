//! Model listing behind a trait so tests inject fakes — no live HTTP in
//! `cargo test`. The live implementation queries each provider's model-list
//! endpoint with a short timeout.

use std::time::Duration;

use anyhow::Result;

use super::provider::{DEFAULT_OLLAMA_URL, Provider};

/// Pinned Anthropic API version header for the models endpoint.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Outcome of a model-list attempt.
pub(crate) enum ModelList {
    /// Models fetched — the key works.
    Verified(Vec<String>),
    /// This provider has no cheap HTTP listing (bedrock).
    Unsupported,
}

pub(crate) trait ModelLister {
    /// List model ids for the provider. `Err` carries a human-readable
    /// reason (bad key, no network, …) — callers warn and continue.
    fn list(
        &self,
        provider: Provider,
        api_key: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<ModelList, String>;
}

/// Live `reqwest::blocking` implementation (5s timeout).
pub(crate) struct HttpModelLister;

impl HttpModelLister {
    fn client() -> Result<reqwest::blocking::Client, String> {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("http client: {e}"))
    }

    fn get_json(request: reqwest::blocking::RequestBuilder) -> Result<serde_json::Value, String> {
        let response = request.send().map_err(|e| format!("request failed: {e}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!(
                "the provider rejected the API key ({status}) — is the right \
                 variable exported in this shell?"
            ));
        }
        if !status.is_success() {
            return Err(format!("unexpected response: {status}"));
        }
        response
            .json::<serde_json::Value>()
            .map_err(|e| format!("invalid JSON response: {e}"))
    }

    /// Pull a list of ids out of `json[field][*][id_key]`.
    fn extract(json: &serde_json::Value, field: &str, id_key: &str) -> Vec<String> {
        json[field]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|m| m[id_key].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl ModelLister for HttpModelLister {
    fn list(
        &self,
        provider: Provider,
        api_key: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<ModelList, String> {
        let key = api_key.unwrap_or_default();
        let models = match provider {
            Provider::OpenAI => Self::extract(
                &Self::get_json(
                    Self::client()?
                        .get("https://api.openai.com/v1/models")
                        .bearer_auth(key),
                )?,
                "data",
                "id",
            ),
            Provider::Anthropic => Self::extract(
                &Self::get_json(
                    Self::client()?
                        .get("https://api.anthropic.com/v1/models")
                        .header("x-api-key", key)
                        .header("anthropic-version", ANTHROPIC_VERSION),
                )?,
                "data",
                "id",
            ),
            Provider::OpenRouter => Self::extract(
                &Self::get_json(Self::client()?.get("https://openrouter.ai/api/v1/models"))?,
                "data",
                "id",
            ),
            Provider::Gemini => Self::extract(
                &Self::get_json(
                    Self::client()?
                        .get("https://generativelanguage.googleapis.com/v1beta/models")
                        .header("x-goog-api-key", key),
                )?,
                "models",
                "name",
            )
            .into_iter()
            .map(|name| name.trim_start_matches("models/").to_string())
            .collect(),
            Provider::Ollama => Self::extract(
                &Self::get_json(Self::client()?.get(format!(
                    "{}/api/tags",
                    base_url.unwrap_or(DEFAULT_OLLAMA_URL).trim_end_matches('/')
                )))?,
                "models",
                "name",
            ),
            // Bedrock needs the AWS SDK (ListFoundationModels); skipped in v1.
            Provider::Bedrock => return Ok(ModelList::Unsupported),
        };
        if models.is_empty() {
            return Err("the provider returned an empty model list".to_string());
        }
        Ok(ModelList::Verified(models))
    }
}
