//! The Slack Web API calls the ingress makes.

use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::fmt;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://slack.com/api";
/// Deadline for one Web API call, connect and response included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard stop on `conversations.replies` pagination for one thread.
const MAX_REPLY_PAGES: usize = 50;

/// The most hits `assistant.search.context` returns per page.
pub const MAX_SEARCH_HITS: usize = 20;

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

/// A Slack per-message action token.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct ActionToken(String);

impl ActionToken {
    pub fn new(raw: String) -> Self {
        Self(raw)
    }
}

impl fmt::Debug for ActionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ActionToken(***)")
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

/// One message hit as `assistant.search.context` returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchHit {
    pub channel_id: String,
    #[serde(default)]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub author_name: Option<String>,
    #[serde(default)]
    pub author_user_id: Option<String>,
    #[serde(default)]
    pub is_author_bot: bool,
    pub message_ts: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub content: String,
    pub permalink: String,
}

/// One page of search hits plus the cursor for the next page.
#[derive(Debug, Clone)]
pub struct SearchPage {
    pub messages: Vec<SearchHit>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: SearchResults,
    #[serde(default)]
    response_metadata: Option<ResponseMetadata>,
}

#[derive(Default, Deserialize)]
struct SearchResults {
    #[serde(default)]
    messages: Vec<SearchHit>,
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
        Self::with_timeout(bot_token, app_token, REQUEST_TIMEOUT)
    }

    /// `new` with a different per-call deadline.
    pub fn with_timeout(bot_token: BotToken, app_token: AppToken, timeout: Duration) -> Self {
        Self {
            http: Client::builder()
                .timeout(timeout)
                .build()
                .expect("reqwest client with a timeout builds"),
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

    /// Up to `limit` top-level messages in `channel` older than `before_ts`,
    /// oldest first.
    pub async fn conversations_history(
        &self,
        channel: &str,
        before_ts: &str,
        limit: usize,
    ) -> Result<Vec<SlackMessage>, SlackApiError> {
        let params = [
            ("channel", channel.to_owned()),
            ("latest", before_ts.to_owned()),
            ("inclusive", "false".to_owned()),
            ("limit", limit.to_string()),
        ];
        let page: RepliesPage = self
            .call("conversations.history", &self.bot_token.0, &params)
            .await?;
        let mut messages = page.messages;
        messages.reverse();
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

    /// One page of messages matching `query`, searched as the person whose
    /// message carried `action_token`: Slack applies that person's
    /// visibility and narrows further to where the message was sent. Only
    /// public channels are requested, since a bot token cannot hold the
    /// private, DM, or group-DM search scopes. `limit` is clamped to
    /// `1..=MAX_SEARCH_HITS`; `cursor` continues an earlier page. Every
    /// page counts against Slack's per-person search rate limit.
    pub async fn assistant_search_context(
        &self,
        action_token: &ActionToken,
        query: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<SearchPage, SlackApiError> {
        let mut params = vec![
            ("query", query.to_owned()),
            ("action_token", action_token.0.clone()),
            ("channel_types", "public_channel".to_owned()),
            ("content_types", "messages".to_owned()),
            ("limit", limit.clamp(1, MAX_SEARCH_HITS).to_string()),
        ];
        if let Some(cursor) = cursor.filter(|c| !c.is_empty()) {
            params.push(("cursor", cursor.to_owned()));
        }
        let page: SearchResponse = self
            .call("assistant.search.context", &self.bot_token.0, &params)
            .await?;
        Ok(SearchPage {
            messages: page.results.messages,
            next_cursor: page
                .response_metadata
                .map(|m| m.next_cursor)
                .filter(|c| !c.is_empty()),
        })
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
    async fn a_stalled_response_is_a_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_json(serde_json::json!({"ok": true})),
            )
            .mount(&server)
            .await;

        let api = SlackApi::with_timeout(
            BotToken::new("xoxb-bot".to_owned()).unwrap(),
            AppToken::new("xapp-app".to_owned()).unwrap(),
            Duration::from_millis(100),
        )
        .with_base_url(server.uri());
        let err = api.auth_test().await.unwrap_err();
        assert!(matches!(err, SlackApiError::Transport { .. }), "{err}");
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
    async fn history_is_bounded_before_the_message_and_oldest_first() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/conversations.history"))
            .and(body_string_contains("latest=9.0"))
            .and(body_string_contains("inclusive=false"))
            .and(body_string_contains("limit=50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "messages": [
                    {"ts": "8.0", "user": "U1", "text": "newest"},
                    {"ts": "7.0", "bot_id": "B1", "text": "older"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let messages = api(&server)
            .conversations_history("D1", "9.0", 50)
            .await
            .unwrap();
        let ts: Vec<&str> = messages.iter().map(|m| m.ts.as_str()).collect();
        assert_eq!(ts, ["7.0", "8.0"]);
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

    #[test]
    fn action_token_debug_is_redacted() {
        let token = ActionToken::new("12345.98765.secret".to_owned());
        assert!(!format!("{token:?}").contains("secret"));
    }

    #[tokio::test]
    async fn search_sends_the_asker_token_and_decodes_hits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.search.context"))
            .and(header("authorization", "Bearer xoxb-bot"))
            .and(body_string_contains("query=project+gizmo"))
            .and(body_string_contains("action_token=12345.98765.abcd"))
            .and(body_string_contains("channel_types=public_channel"))
            .and(body_string_contains("content_types=messages"))
            .and(body_string_contains("limit=5"))
            .and(body_string_contains("cursor=c2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "results": {
                    "messages": [{
                        "author_name": "Jennifer Hynes",
                        "author_user_id": "U0123456",
                        "team_id": "T0123456",
                        "channel_id": "C0123456",
                        "channel_name": "proj-gizmo",
                        "message_ts": "123456.7890",
                        "content": "Hey team, kicking off the revamp",
                        "is_author_bot": false,
                        "permalink": "https://x.slack.com/archives/C0123456/p1234567890",
                        "blocks": [{"type": "rich_text"}]
                    }, {
                        "channel_id": "C0123456",
                        "message_ts": "123457.0001",
                        "thread_ts": "123456.7890",
                        "permalink": "https://x.slack.com/archives/C0123456/p1234570001",
                        "is_author_bot": true
                    }]
                },
                "response_metadata": {"next_cursor": "c3"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let token = ActionToken::new("12345.98765.abcd".to_owned());
        let page = api(&server)
            .assistant_search_context(&token, "project gizmo", 5, Some("c2"))
            .await
            .unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("c3"));
        assert_eq!(page.messages.len(), 2);
        let first = &page.messages[0];
        assert_eq!(first.channel_name.as_deref(), Some("proj-gizmo"));
        assert_eq!(first.author_name.as_deref(), Some("Jennifer Hynes"));
        assert!(!first.is_author_bot);
        let second = &page.messages[1];
        assert!(second.is_author_bot);
        assert_eq!(second.thread_ts.as_deref(), Some("123456.7890"));
        assert_eq!(second.content, "");
    }

    #[tokio::test]
    async fn search_clamps_limit_and_omits_empty_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.search.context"))
            .and(body_string_contains("limit=20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "results": {"messages": []},
                "response_metadata": {"next_cursor": ""}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let token = ActionToken::new("t".to_owned());
        let page = api(&server)
            .assistant_search_context(&token, "q", 500, Some(""))
            .await
            .unwrap();
        assert!(page.messages.is_empty());
        assert_eq!(page.next_cursor, None);
        let sent = &server.received_requests().await.unwrap()[0];
        assert!(!String::from_utf8_lossy(&sent.body).contains("cursor="));
    }

    #[tokio::test]
    async fn search_surfaces_slack_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.search.context"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"ok": false, "error": "invalid_action_token"}),
                ),
            )
            .mount(&server)
            .await;

        let token = ActionToken::new("stale".to_owned());
        let err = api(&server)
            .assistant_search_context(&token, "q", 10, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SlackApiError::Api { method: "assistant.search.context", ref error } if error == "invalid_action_token"),
            "{err}"
        );
        assert!(!err.to_string().contains("stale"));
    }
}
