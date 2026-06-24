use anyhow::{Context, Result};
use reqwest::{Client, RequestBuilder};

use crate::api::session::CHAT_SESSION_HEADER;
use crate::api::types::{ChatCompletion, ChatRequest, Message, ModelList, ToolDefinition};
use crate::config::AppConfig;
use crate::ui::prompt::get_selected_model;

const SUMMARIZE_PROMPT: &str = "\
You are a title generator. Given an assistant response, produce a single short \
plain-text title (max 72 chars) that summarizes it. No markdown, no quotes, no \
punctuation at the end. Just the title.";

/// Apply common headers to a request: Content-Type, `Authorization: Bearer`
/// (when [`AppConfig::api_key`] is set), and the user-supplied `extra_headers`.
///
/// If `session_id` is `Some`, attaches `x-chat-session-id` with that value and
/// suppresses any same-named header from `extra_headers` (so the resolved
/// value wins). Pass `None` for endpoints that don't participate in chat
/// sessions (e.g. `/v1/models`, `/v1/approvals/{id}`).
///
/// Shared between [`ChatClient::build_request`] and
/// [`crate::api::approval::ApprovalPoster::post_decision`] so auth policy
/// changes land in one place.
pub(crate) fn apply_common_headers(
    config: &AppConfig,
    builder: RequestBuilder,
    session_id: Option<&str>,
) -> RequestBuilder {
    let mut req = builder.header("Content-Type", "application/json");

    if let Some(ref key) = config.api_key {
        req = req.bearer_auth(key);
    }

    for (name, value) in &config.extra_headers {
        if session_id.is_some() && name.eq_ignore_ascii_case(CHAT_SESSION_HEADER) {
            continue;
        }
        req = req.header(name, value);
    }

    if let Some(sid) = session_id {
        req = req.header(CHAT_SESSION_HEADER, sid);
    }

    req
}

/// Check if an error is a model-related error from the API (not found, missing, invalid).
pub fn is_model_error(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err);
    msg.contains("model_not_found")
        || (msg.contains("404") && msg.contains("does not exist"))
        || (msg.contains("\"param\":\"model\""))
        || (msg.contains("model") && msg.contains("required"))
        || (msg.contains("must provide a model"))
}

pub struct ChatClient {
    http: Client,
    config: AppConfig,
}

impl ChatClient {
    pub fn new(config: AppConfig) -> Self {
        Self {
            http: Client::new(),
            config,
        }
    }

    /// Build a request with common headers.
    ///
    /// Delegates to [`apply_common_headers`] so chat and approval requests
    /// share the same auth/header policy.
    fn build_request(
        &self,
        method: reqwest::Method,
        url: &str,
        session_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        apply_common_headers(&self.config, self.http.request(method, url), session_id)
    }

    pub async fn send_streaming(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        session_id: &str,
    ) -> Result<reqwest::Response> {
        let request = ChatRequest {
            model: get_selected_model(),
            messages: messages.to_vec(),
            stream: true,
            tools: tools.map(|t| t.to_vec()),
        };

        let response = self
            .build_request(
                reqwest::Method::POST,
                &self.config.chat_completions_url(),
                Some(session_id),
            )
            .json(&request)
            .send()
            .await
            .context("Failed to connect to API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            anyhow::bail!("API error ({}): {}", status, body);
        }

        Ok(response)
    }

    /// Ask the LLM for a short one-line summary/title of the given text.
    /// Returns the summary string and optional (prompt_tokens, completion_tokens).
    pub async fn summarize(
        &self,
        text: &str,
        session_id: &str,
    ) -> Result<(String, Option<(u64, u64)>)> {
        let prompt = format!("{}\n\n{}", SUMMARIZE_PROMPT, text);
        let messages = vec![Message::user(prompt)];

        let request = ChatRequest {
            model: get_selected_model(),
            messages,
            stream: false,
            tools: None,
        };

        let response = self
            .build_request(
                reqwest::Method::POST,
                &self.config.chat_completions_url(),
                Some(session_id),
            )
            .json(&request)
            .send()
            .await
            .context("Failed to connect to API for summary")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            anyhow::bail!("Summary API error ({}): {}", status, body);
        }

        let completion: ChatCompletion = response
            .json()
            .await
            .context("Failed to parse summary response")?;

        let summary = completion
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default()
            .trim()
            .to_string();

        let usage = completion
            .usage
            .map(|u| (u.prompt_tokens, u.completion_tokens));

        Ok((summary, usage))
    }

    /// Fetch the list of available model IDs from the server.
    #[allow(dead_code)]
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .build_request(reqwest::Method::GET, &self.config.models_url(), None)
            .send()
            .await
            .context("Failed to connect to models endpoint")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            anyhow::bail!("Models API error ({}): {}", status, body);
        }

        let model_list: ModelList = response
            .json()
            .await
            .context("Failed to parse models response")?;

        Ok(model_list.data.into_iter().map(|m| m.id).collect())
    }

    /// Describe the connected server for the startup banner: the base URL and,
    /// when the server advertises it via `/health`, its version. Best-effort —
    /// an unreachable or older server (no `aura_version` field) yields just the
    /// base URL. Uses a short timeout so startup never stalls on a dead server.
    pub async fn connection_summary(&self) -> String {
        let base = self.config.api_url.trim_end_matches('/').to_string();
        match self.server_version().await {
            Some(version) => format!("{base} (server v{version})"),
            None => base,
        }
    }

    /// Fetch the server's reported aura version from `/health`, or `None` if the
    /// server is unreachable or doesn't report one.
    async fn server_version(&self) -> Option<String> {
        let response = self
            .build_request(reqwest::Method::GET, &self.config.health_url(), None)
            .timeout(std::time::Duration::from_millis(1500))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        body.get("aura_version")?.as_str().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_not_found_keyword() {
        let err = anyhow::anyhow!("model_not_found");
        assert!(is_model_error(&err));
    }

    #[test]
    fn error_404_does_not_exist() {
        let err = anyhow::anyhow!("404 The model 'gpt-99' does not exist");
        assert!(is_model_error(&err));
    }

    #[test]
    fn param_model() {
        let err = anyhow::anyhow!(r#"invalid request: "param":"model""#);
        assert!(is_model_error(&err));
    }

    #[test]
    fn model_required() {
        let err = anyhow::anyhow!("model field is required");
        assert!(is_model_error(&err));
    }

    #[test]
    fn unrelated_error_returns_false() {
        let err = anyhow::anyhow!("connection refused");
        assert!(!is_model_error(&err));
    }

    #[test]
    fn partial_match_404_not_triggered() {
        // Has "404" but NOT "does not exist"
        let err = anyhow::anyhow!("404 page not found");
        assert!(!is_model_error(&err));
    }

    #[test]
    fn empty_error_returns_false() {
        let err = anyhow::anyhow!("");
        assert!(!is_model_error(&err));
    }
}
