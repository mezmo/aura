use anyhow::{Context, Result};
use reqwest::Client;

use crate::api::session::CHAT_SESSION_HEADER;
use crate::api::types::{ChatCompletion, ChatRequest, Message, ModelList, ToolDefinition};
use crate::config::AppConfig;
use crate::ui::prompt::get_selected_model;

const SUMMARIZE_PROMPT: &str = "\
You are a title generator. Given an assistant response, produce a single short \
plain-text title (max 72 chars) that summarizes it. No markdown, no quotes, no \
punctuation at the end. Just the title.";

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

    /// Build a request with common headers (Content-Type, auth, extra headers).
    ///
    /// If `session_id` is `Some`, attaches `x-chat-session-id` with that value
    /// and suppresses any same-named header from `extra_headers` (so the caller's
    /// resolved value wins). If `None`, falls through and lets `extra_headers`
    /// pass through unchanged (used for endpoints like `/v1/models` that don't
    /// participate in chat sessions).
    fn build_request(
        &self,
        method: reqwest::Method,
        url: &str,
        session_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, url)
            .header("Content-Type", "application/json");

        if let Some(ref key) = self.config.api_key {
            req = req.bearer_auth(key);
        }

        for (name, value) in &self.config.extra_headers {
            // When a session_id is being injected, drop any user-supplied
            // x-chat-session-id from extra_headers — the resolved value
            // (which may itself originate from extra_headers) is authoritative.
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
