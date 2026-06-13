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
    /// When the CLI's telemetry is `Enabled`, requests carry the
    /// consent header so an `Unknown` server can adopt the user's
    /// consent. `None` in contexts that don't have a handle (tests).
    telemetry: Option<aura_telemetry::TelemetryHandle>,
}

impl ChatClient {
    pub fn new(config: AppConfig) -> Self {
        Self::with_telemetry(config, None)
    }

    pub fn with_telemetry(
        config: AppConfig,
        telemetry: Option<aura_telemetry::TelemetryHandle>,
    ) -> Self {
        Self {
            http: Client::new(),
            config,
            telemetry,
        }
    }

    /// Whether this client holds a telemetry handle (and would therefore
    /// propagate consent when Enabled). One-shot `--query` deliberately
    /// holds `None`; exposed for the wiring regression test.
    #[cfg(test)]
    pub(crate) fn has_telemetry(&self) -> bool {
        self.telemetry.is_some()
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

        // Propagate telemetry consent to the server only when this CLI
        // is Enabled — never when Unknown/Disabled. A live `state()`
        // check (not a startup snapshot) so the first request after the
        // first-input consent gate carries the header.
        if self
            .telemetry
            .as_ref()
            .is_some_and(|t| matches!(t.state(), aura_telemetry::TelemetryState::Enabled))
        {
            req = req.header(
                aura_telemetry::CONSENT_HEADER,
                aura_telemetry::CONSENT_HEADER_VALUE,
            );
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

/// On-the-wire propagation of the telemetry consent header. Asserts the
/// actual built request, not just that a handle is present — only an
/// `Enabled` CLI may emit `X-Aura-Telemetry-Consent`.
#[cfg(test)]
mod consent_header_tests {
    use super::*;
    use aura_telemetry::properties::{DeploymentMethod, OsFamily, Source};
    use aura_telemetry::{DisableReason, TelemetryConfig, TelemetryHandle, TelemetryState, init};
    use std::time::Duration;
    use uuid::Uuid;

    fn test_config() -> AppConfig {
        AppConfig {
            api_url: "http://localhost:8080".to_string(),
            api_key: None,
            model: None,
            system_prompt: None,
            query: None,
            resume: None,
            extra_headers: vec![],
            force: false,
            enable_client_tools: false,
            enable_final_response_summary: false,
            style: None,
            pretty: false,
            log_file: None,
            telemetry: None,
        }
    }

    fn handle_in(state: TelemetryState) -> TelemetryHandle {
        init(TelemetryConfig {
            endpoint: "http://127.0.0.1:1".into(),
            api_key: "phc_test".into(),
            install_id: Uuid::nil(),
            install_id_path: None,
            session_id: Uuid::nil(),
            source: Source::Cli,
            os_family: OsFamily::Linux,
            deployment_method: DeploymentMethod::Local,
            aura_version: "9.9.9-test",
            inspection_log_path: None,
            state,
            channel_capacity: 16,
            batch_size: 1,
            flush_interval: Duration::from_millis(50),
            post_timeout: Duration::from_millis(100),
            http_client: None,
        })
    }

    /// The consent header value (if any) on a freshly built request.
    fn consent_on_wire(client: &ChatClient) -> Option<String> {
        let req = client
            .build_request(
                reqwest::Method::POST,
                "http://localhost/v1/chat/completions",
                Some("sess-1"),
            )
            .build()
            .expect("request builds");
        req.headers()
            .get(aura_telemetry::CONSENT_HEADER)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn enabled_cli_sends_consent_header() {
        let telemetry = handle_in(TelemetryState::Unknown);
        assert_eq!(telemetry.enable(), aura_telemetry::EnableOutcome::Enabled);
        let client = ChatClient::with_telemetry(test_config(), Some(telemetry));
        assert_eq!(
            consent_on_wire(&client).as_deref(),
            Some(aura_telemetry::CONSENT_HEADER_VALUE),
            "an Enabled CLI must propagate consent on the wire"
        );
    }

    #[tokio::test]
    async fn unknown_cli_sends_no_consent_header() {
        let telemetry = handle_in(TelemetryState::Unknown);
        let client = ChatClient::with_telemetry(test_config(), Some(telemetry));
        assert_eq!(
            consent_on_wire(&client),
            None,
            "Unknown must not propagate consent"
        );
    }

    #[tokio::test]
    async fn disabled_cli_sends_no_consent_header() {
        let telemetry = handle_in(TelemetryState::Disabled(DisableReason::DoNotTrack));
        let client = ChatClient::with_telemetry(test_config(), Some(telemetry));
        assert_eq!(
            consent_on_wire(&client),
            None,
            "Disabled must not propagate consent"
        );
    }

    #[tokio::test]
    async fn no_handle_sends_no_consent_header() {
        // One-shot `--query` holds no handle; it must never emit the header.
        let client = ChatClient::with_telemetry(test_config(), None);
        assert_eq!(consent_on_wire(&client), None);
    }
}
