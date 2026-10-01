//! The Slack Web API calls the ingress makes.

use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::fmt;

pub const DEFAULT_BASE_URL: &str = "https://slack.com/api";

/// Hard stop on `conversations.replies` pagination for one thread.
const MAX_REPLY_PAGES: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("slack {kind} token must start with `{prefix}`")]
    WrongPrefix {
        kind: &'static str,
        prefix: &'static str,
    },
}

/// Bot user OAuth token (`xoxb-...`).
#[derive(Clone)]
pub struct BotToken(String);

impl BotToken {
    pub fn new(raw: String) -> Result<Self, TokenError> {
        if raw.starts_with("xoxb-") {
            Ok(Self(raw))
        } else {
            Err(TokenError::WrongPrefix {
                kind: "bot",
                prefix: "xoxb-",
            })
        }
    }
}

impl fmt::Debug for BotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BotToken(xoxb-***)")
    }
}

/// App-level token (`xapp-...`).
#[derive(Clone)]
pub struct AppToken(String);

impl AppToken {
    pub fn new(raw: String) -> Result<Self, TokenError> {
        if raw.starts_with("xapp-") {
            Ok(Self(raw))
        } else {
            Err(TokenError::WrongPrefix {
                kind: "app",
                prefix: "xapp-",
            })
        }
    }
}

impl fmt::Debug for AppToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppToken(xapp-***)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SlackApiError {
    #[error("{method}: {source}")]
    Transport {
        method: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{method}: slack returned `{error}`")]
    Api { method: &'static str, error: String },
    #[error("{method}: unexpected response shape: {source}")]
    Shape {
        method: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

/// The bot's own identity in the workspace.
#[derive(Debug, Clone, Deserialize)]
pub struct BotIdentity {
    pub user_id: String,
    #[serde(default)]
    pub bot_id: Option<String>,
    pub team: String,
    pub user: String,
}

/// One message as `conversations.replies` returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackMessage {
    pub ts: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub subtype: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SlackApi {
    http: Client,
    base_url: String,
    bot_token: BotToken,
    app_token: AppToken,
}

#[derive(Deserialize)]
struct RepliesPage {
    #[serde(default)]
    messages: Vec<SlackMessage>,
    #[serde(default)]
    response_metadata: Option<ResponseMetadata>,
}

#[derive(Deserialize)]
struct ResponseMetadata {
    #[serde(default)]
    next_cursor: String,
}

impl SlackApi {
    pub fn new(bot_token: BotToken, app_token: AppToken) -> Self {
        Self {
            http: Client::new(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            bot_token,
            app_token,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Verify the bot token and learn the bot's user id.
    pub async fn auth_test(&self) -> Result<BotIdentity, SlackApiError> {
        self.call("auth.test", &self.bot_token.0, &[]).await
    }

    /// Mint a fresh Socket Mode WebSocket URL.
    pub async fn connections_open(&self) -> Result<String, SlackApiError> {
        #[derive(Deserialize)]
        struct Opened {
            url: String,
        }
        let opened: Opened = self
            .call("apps.connections.open", &self.app_token.0, &[])
            .await?;
        Ok(opened.url)
    }

    /// Every message in a thread, oldest first, the parent included. A
    /// thread longer than `MAX_REPLY_PAGES` pages of 200 is cut off at the
    /// newest end with a warning.
    pub async fn conversations_replies(
        &self,
        channel: &str,
        thread_ts: &str,
    ) -> Result<Vec<SlackMessage>, SlackApiError> {
        let mut messages = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_REPLY_PAGES {
            let mut params = vec![
                ("channel", channel.to_owned()),
                ("ts", thread_ts.to_owned()),
                ("limit", "200".to_owned()),
            ];
            if !cursor.is_empty() {
                params.push(("cursor", cursor.clone()));
            }
            let page: RepliesPage = self
                .call("conversations.replies", &self.bot_token.0, &params)
                .await?;
            messages.extend(page.messages);
            cursor = page
                .response_metadata
                .map(|m| m.next_cursor)
                .unwrap_or_default();
            if cursor.is_empty() {
                return Ok(messages);
            }
        }
        tracing::warn!(
            channel,
            thread_ts,
            fetched = messages.len(),
            "slack thread longer than the reply page limit, history truncated"
        );
        Ok(messages)
    }

    /// Post `text` into `channel`, inside `thread_ts` when given. Returns
    /// the new message's `ts`.
    pub async fn post_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<String, SlackApiError> {
        #[derive(Deserialize)]
        struct Posted {
            ts: String,
        }
        let mut params = vec![("channel", channel.to_owned()), ("text", text.to_owned())];
        if let Some(thread_ts) = thread_ts {
            params.push(("thread_ts", thread_ts.to_owned()));
        }
        let posted: Posted = self
            .call("chat.postMessage", &self.bot_token.0, &params)
            .await?;
        Ok(posted.ts)
    }

    /// React to a message. A reaction that is already there is not an error.
    pub async fn add_reaction(
        &self,
        channel: &str,
        ts: &str,
        name: &str,
    ) -> Result<(), SlackApiError> {
        let params = [
            ("channel", channel.to_owned()),
            ("timestamp", ts.to_owned()),
            ("name", name.to_owned()),
        ];
        match self
            .call::<serde_json::Value>("reactions.add", &self.bot_token.0, &params)
            .await
        {
            Ok(_) => Ok(()),
            Err(SlackApiError::Api { error, .. }) if error == "already_reacted" => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// One form-encoded POST. Slack signals failure inside a 200 body as
    /// `ok: false` plus an `error` code, so that is checked before the
    /// payload is decoded.
    async fn call<T: DeserializeOwned>(
        &self,
        method: &'static str,
        token: &str,
        params: &[(&str, String)],
    ) -> Result<T, SlackApiError> {
        let body: serde_json::Value = self
            .http
            .post(format!("{}/{method}", self.base_url))
            .bearer_auth(token)
            .form(params)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| SlackApiError::Transport { method, source })?
            .json()
            .await
            .map_err(|source| SlackApiError::Transport { method, source })?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            let error = body
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown_error")
                .to_owned();
            return Err(SlackApiError::Api { method, error });
        }
        serde_json::from_value(body).map_err(|source| SlackApiError::Shape { method, source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api(server: &MockServer) -> SlackApi {
        SlackApi::new(
            BotToken::new("xoxb-bot".to_owned()).unwrap(),
            AppToken::new("xapp-app".to_owned()).unwrap(),
        )
        .with_base_url(server.uri())
    }

    #[test]
    fn tokens_reject_wrong_prefix() {
        assert!(BotToken::new("xoxp-user".to_owned()).is_err());
        assert!(AppToken::new("xoxb-bot".to_owned()).is_err());
        assert!(BotToken::new("xoxb-ok".to_owned()).is_ok());
        assert!(AppToken::new("xapp-ok".to_owned()).is_ok());
    }

    #[test]
    fn token_debug_is_redacted() {
        let token = BotToken::new("xoxb-secret-value".to_owned()).unwrap();
        assert!(!format!("{token:?}").contains("secret"));
    }

    #[tokio::test]
    async fn auth_test_uses_bot_token_and_decodes_identity() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .and(header("authorization", "Bearer xoxb-bot"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "user_id": "U1", "bot_id": "B1", "team": "T", "user": "aura"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let identity = api(&server).auth_test().await.unwrap();
        assert_eq!(identity.user_id, "U1");
        assert_eq!(identity.bot_id.as_deref(), Some("B1"));
    }

    #[tokio::test]
    async fn connections_open_uses_app_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps.connections.open"))
            .and(header("authorization", "Bearer xapp-app"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "url": "wss://wss.slack.com/link/?ticket=1"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let url = api(&server).connections_open().await.unwrap();
        assert_eq!(url, "wss://wss.slack.com/link/?ticket=1");
    }

    #[tokio::test]
    async fn ok_false_becomes_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": false, "error": "invalid_auth"})),
            )
            .mount(&server)
            .await;

        let err = api(&server).auth_test().await.unwrap_err();
        assert!(
            matches!(err, SlackApiError::Api { method: "auth.test", ref error } if error == "invalid_auth"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn replies_follow_the_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/conversations.replies"))
            .and(body_string_contains("cursor=c2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "messages": [{"ts": "3", "user": "U2", "text": "third"}],
                "response_metadata": {"next_cursor": ""}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/conversations.replies"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "messages": [
                    {"ts": "1", "user": "U1", "text": "first"},
                    {"ts": "2", "bot_id": "B1", "text": "second"}
                ],
                "response_metadata": {"next_cursor": "c2"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let messages = api(&server).conversations_replies("C1", "1").await.unwrap();
        let ts: Vec<&str> = messages.iter().map(|m| m.ts.as_str()).collect();
        assert_eq!(ts, ["1", "2", "3"]);
        assert_eq!(messages[1].bot_id.as_deref(), Some("B1"));
    }

    #[tokio::test]
    async fn post_message_threads_and_returns_ts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(body_string_contains("thread_ts=1.0"))
            .and(body_string_contains("text=hi+there"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "ts": "2.0"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let ts = api(&server)
            .post_message("C1", Some("1.0"), "hi there")
            .await
            .unwrap();
        assert_eq!(ts, "2.0");
    }

    #[tokio::test]
    async fn already_reacted_is_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions.add"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": false, "error": "already_reacted"})),
            )
            .mount(&server)
            .await;

        api(&server)
            .add_reaction("C1", "1.0", "eyes")
            .await
            .unwrap();
    }
}
