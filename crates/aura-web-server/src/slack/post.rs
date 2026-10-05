//! The `slack_post_message` tool: post into the workspace as the bot.

use std::sync::Arc;

use aura::{RigTool, RigToolDefinition, RunToolFactory, ToolDyn, no_run_tools};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::api::{SlackApi, SlackApiError};
use super::budget::Budget;

pub const POST_TOOL_NAME: &str = "slack_post_message";
/// Posts one run may make, across every agent working on it.
pub const POSTS_PER_RUN: usize = 5;

/// The per-run tool factory for a request that `config` answers: a post
/// tool when the server has a Slack client (the ingress is enabled) and the
/// agent opted in with `[agent].enable_slack_tools`, otherwise nothing, so
/// an agent that did not opt in builds exactly as it would on a server
/// without Slack. Every tool the factory builds for one run draws on one
/// post budget.
pub fn run_tools(api: Option<&SlackApi>, config: &aura_config::Config) -> RunToolFactory {
    let Some(api) = api.filter(|_| config.agent.enable_slack_tools) else {
        return no_run_tools();
    };
    let api = api.clone();
    let budget = Budget::new(POSTS_PER_RUN);
    Arc::new(move || {
        let tool = SlackPostTool::new(api.clone(), Arc::clone(&budget));
        vec![Box::new(tool) as Box<dyn ToolDyn>]
    })
}

pub struct SlackPostTool {
    api: SlackApi,
    budget: Arc<Budget>,
}

impl SlackPostTool {
    pub fn new(api: SlackApi, budget: Arc<Budget>) -> Self {
        Self { api, budget }
    }
}

#[derive(Debug, Deserialize)]
pub struct PostArgs {
    pub channel: String,
    pub text: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PostOutput {
    pub channel: String,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PostError {
    #[error("`channel` must name a channel: an id such as C0123456 or a #name")]
    EmptyChannel,
    #[error("`text` is empty; nothing was posted")]
    EmptyText,
    #[error("{0}")]
    Slack(String),
    #[error("the post budget for this run is spent; nothing more can be posted")]
    BudgetSpent,
}

impl PostError {
    /// Name the cause in the agent's terms; Slack's own codes say nothing
    /// to a model deciding what to tell the person or try next. Slack has
    /// no idempotency key on `chat.postMessage`, so the wording says what
    /// is known about delivery: a connection that never opened posted
    /// nothing, a request that went out but got no readable answer may
    /// have landed, and an `ok: true` body that would not decode did land.
    fn from_slack(err: SlackApiError, channel: &str) -> Self {
        let text = match &err {
            SlackApiError::Transport { source, .. } if source.is_connect() => {
                format!("could not reach Slack ({source}); nothing was posted")
            }
            SlackApiError::Transport { source, .. } => format!(
                "Slack did not confirm the post ({source}); the message may have landed in \
                 `{channel}`, so check there before posting it again"
            ),
            SlackApiError::Shape { .. } => format!(
                "Slack accepted the post but its reply could not be read; the message is in \
                 `{channel}`, do not post it again"
            ),
            SlackApiError::Api { error, .. } if error == "channel_not_found" => {
                format!(
                    "no Slack channel `{channel}` is visible to the bot; check the name or use the channel id"
                )
            }
            SlackApiError::Api { error, .. } if error == "not_in_channel" => {
                format!(
                    "the bot is not a member of `{channel}`; invite it there, or grant the app chat:write.public for public channels"
                )
            }
            SlackApiError::Api { error, .. } if error == "is_archived" => {
                format!("`{channel}` is archived, so nothing can be posted there")
            }
            SlackApiError::Api { error, .. } if error == "msg_too_long" => {
                "the message is too long for Slack; shorten it and post again".to_owned()
            }
            SlackApiError::Api { error, .. } if error.contains("thread") => {
                format!("`thread_ts` does not name a message in `{channel}`")
            }
            SlackApiError::Api { error, .. } if error == "missing_scope" => {
                "the Slack app lacks the scope to post here: chat:write, or chat:write.public \
                 for a public channel the bot has not joined"
                    .to_owned()
            }
            SlackApiError::Api { error, .. } if error == "ratelimited" => {
                "Slack is rate limiting posts right now; do not retry".to_owned()
            }
            other => format!("slack post failed: {other}"),
        };
        Self::Slack(text)
    }
}

impl RigTool for SlackPostTool {
    const NAME: &'static str = POST_TOOL_NAME;

    type Error = PostError;
    type Args = PostArgs;
    type Output = PostOutput;

    async fn definition(&self, _prompt: String) -> RigToolDefinition {
        RigToolDefinition {
            name: POST_TOOL_NAME.to_owned(),
            description: "Post a message to Slack as this bot. `channel` is a channel id such \
                          as C0123456 or a #name as Slack shows it; `text` is Slack mrkdwn \
                          (*bold*, _italic_, `code`, <url|label> for links, lines starting \
                          with \"- \" for lists); `thread_ts` replies inside an existing \
                          thread. Everyone in the channel sees the post at once and it cannot \
                          be unsent, so post once with the final text rather than drafting in \
                          the channel. Returns the message ts, the channel id, and a permalink \
                          to cite as <permalink|label> when you report what you posted. The \
                          bot must be a member of a private channel. A run has a budget of a \
                          few posts."
                .to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "Channel id (C0123456) or #name"
                    },
                    "text": {
                        "type": "string",
                        "description": "The message, in Slack mrkdwn"
                    },
                    "thread_ts": {
                        "type": "string",
                        "description": "ts of the thread's parent message, to reply in that thread"
                    }
                },
                "required": ["channel", "text"],
                "additionalProperties": false
            }),
        }
    }

    /// The post lands before the permalink is asked for, so a failed
    /// permalink call still reports a success: the message is in Slack
    /// either way, and telling the agent otherwise would make it post
    /// again.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let channel = args.channel.trim();
        if channel.is_empty() {
            return Err(PostError::EmptyChannel);
        }
        let text = args.text.trim();
        if text.is_empty() {
            return Err(PostError::EmptyText);
        }
        if !self.budget.take() {
            return Err(PostError::BudgetSpent);
        }
        let posted = self
            .api
            .post_message(channel, args.thread_ts.as_deref(), text)
            .await
            .map_err(|e| PostError::from_slack(e, channel))?;
        let permalink = match self.api.get_permalink(&posted.channel, &posted.ts).await {
            Ok(link) => Some(link),
            Err(e) => {
                warn!(
                    channel = posted.channel,
                    ts = posted.ts,
                    error = %e,
                    "posted to slack but could not fetch the permalink"
                );
                None
            }
        };
        Ok(PostOutput {
            channel: posted.channel,
            ts: posted.ts,
            permalink,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::api::{AppToken, BotToken};
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api(server: &MockServer) -> SlackApi {
        SlackApi::new(
            BotToken::new("xoxb-bot".to_owned()).unwrap(),
            AppToken::new("xapp-app".to_owned()).unwrap(),
        )
        .with_base_url(server.uri())
    }

    fn tool(server: &MockServer) -> SlackPostTool {
        SlackPostTool::new(api(server), Budget::new(POSTS_PER_RUN))
    }

    /// `ToolDyn` is in scope for the factory, so the typed `RigTool` methods
    /// are named explicitly.
    async fn call(tool: &SlackPostTool, args: PostArgs) -> Result<PostOutput, PostError> {
        RigTool::call(tool, args).await
    }

    fn args(channel: &str, text: &str, thread_ts: Option<&str>) -> PostArgs {
        PostArgs {
            channel: channel.to_owned(),
            text: text.to_owned(),
            thread_ts: thread_ts.map(str::to_owned),
        }
    }

    fn config(toml: &str) -> aura_config::Config {
        aura_config::Config::parse_toml(toml).unwrap()
    }

    const OPTED_IN: &str = r#"
[agent]
name = "poster"
system_prompt = "p"
enable_slack_tools = true
[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
"#;
    const NOT_OPTED_IN: &str = r#"
[agent]
name = "quiet"
system_prompt = "p"
[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
"#;

    #[tokio::test]
    async fn the_factory_builds_the_tool_only_with_slack_on_and_the_agent_opted_in() {
        let server = MockServer::start().await;
        let api = api(&server);
        let names = |factory: RunToolFactory| -> Vec<String> {
            factory().iter().map(|t| t.name()).collect()
        };
        assert_eq!(
            names(run_tools(Some(&api), &config(OPTED_IN))),
            [POST_TOOL_NAME]
        );
        assert!(names(run_tools(Some(&api), &config(NOT_OPTED_IN))).is_empty());
        assert!(names(run_tools(None, &config(OPTED_IN))).is_empty());
        assert!(names(run_tools(None, &config(NOT_OPTED_IN))).is_empty());
    }

    #[tokio::test]
    async fn every_instance_from_one_factory_shares_the_run_budget() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "channel": "C1", "ts": "1.0"})),
            )
            .expect(POSTS_PER_RUN as u64)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat.getPermalink"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"ok": true, "permalink": "https://x.slack.com/p1"}),
            ))
            .mount(&server)
            .await;

        let factory = run_tools(Some(&api(&server)), &config(OPTED_IN));
        let one = factory().pop().unwrap();
        let two = factory().pop().unwrap();
        let args = serde_json::json!({"channel": "C1", "text": "hi"}).to_string();
        for i in 0..POSTS_PER_RUN {
            let tool = if i % 2 == 0 { &one } else { &two };
            tool.call(args.clone()).await.unwrap();
        }
        let err = two.call(args).await.unwrap_err().to_string();
        assert!(err.contains("budget"), "{err}");
    }

    #[tokio::test]
    async fn definition_requires_channel_and_text() {
        let server = MockServer::start().await;
        let def = RigTool::definition(&tool(&server), String::new()).await;
        assert_eq!(def.name, POST_TOOL_NAME);
        assert_eq!(
            def.parameters["required"],
            serde_json::json!(["channel", "text"])
        );
        assert!(def.description.contains("cannot be unsent"));
    }

    #[tokio::test]
    async fn empty_arguments_are_rejected_before_slack_is_called() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let tool = tool(&server);
        let err = call(&tool, args("  ", "hello", None)).await.unwrap_err();
        assert!(matches!(err, PostError::EmptyChannel), "{err}");
        let err = call(&tool, args("C1", " \n", None)).await.unwrap_err();
        assert!(matches!(err, PostError::EmptyText), "{err}");
    }

    #[tokio::test]
    async fn call_posts_in_the_thread_and_returns_the_permalink() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(body_string_contains("channel=%23ops"))
            .and(body_string_contains("thread_ts=1.0"))
            .and(body_string_contains("text=deploy+done"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "channel": "C1", "ts": "2.0"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat.getPermalink"))
            .and(body_string_contains("channel=C1"))
            .and(body_string_contains("message_ts=2.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "permalink": "https://x.slack.com/archives/C1/p20"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let out = call(
            &tool(&server),
            args(" #ops ", "\n deploy done \n", Some("1.0")),
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            PostOutput {
                channel: "C1".to_owned(),
                ts: "2.0".to_owned(),
                permalink: Some("https://x.slack.com/archives/C1/p20".to_owned()),
            }
        );
    }

    #[tokio::test]
    async fn a_failed_permalink_still_reports_the_post() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "channel": "C1", "ts": "2.0"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat.getPermalink"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": false, "error": "message_not_found"})),
            )
            .mount(&server)
            .await;

        let out = call(&tool(&server), args("C1", "hi", None)).await.unwrap();
        assert_eq!(out.ts, "2.0");
        assert_eq!(out.permalink, None);
        let json = serde_json::to_value(&out).unwrap();
        assert!(json.get("permalink").is_none());
    }

    #[tokio::test]
    async fn a_missing_scope_names_both_posting_scopes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": false, "error": "missing_scope"})),
            )
            .mount(&server)
            .await;

        let err = call(&tool(&server), args("C1", "hi", None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("chat:write,"), "{err}");
        assert!(err.contains("chat:write.public"), "{err}");
    }

    #[tokio::test]
    async fn an_unconfirmed_post_says_it_may_have_landed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(5))
                    .set_body_json(serde_json::json!({"ok": true, "channel": "C1", "ts": "1.0"})),
            )
            .mount(&server)
            .await;
        let api = SlackApi::with_timeout(
            BotToken::new("xoxb-bot".to_owned()).unwrap(),
            AppToken::new("xapp-app".to_owned()).unwrap(),
            std::time::Duration::from_millis(100),
        )
        .with_base_url(server.uri());
        let tool = SlackPostTool::new(api, Budget::new(POSTS_PER_RUN));

        let err = call(&tool, args("#ops", "hi", None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("may have landed in `#ops`"), "{err}");
        assert!(!err.contains("nothing was posted"), "{err}");
    }

    #[tokio::test]
    async fn an_unreachable_slack_posted_nothing() {
        let api = SlackApi::new(
            BotToken::new("xoxb-bot".to_owned()).unwrap(),
            AppToken::new("xapp-app".to_owned()).unwrap(),
        )
        .with_base_url("http://127.0.0.1:9");
        let tool = SlackPostTool::new(api, Budget::new(POSTS_PER_RUN));

        let err = call(&tool, args("C1", "hi", None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing was posted"), "{err}");
    }

    #[tokio::test]
    async fn an_accepted_post_with_an_unreadable_reply_is_reported_as_delivered() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let err = call(&tool(&server), args("C1", "hi", None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("is in `C1`, do not post it again"), "{err}");
    }

    #[tokio::test]
    async fn slack_errors_become_tool_errors_in_plain_words() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": false, "error": "not_in_channel"})),
            )
            .mount(&server)
            .await;

        let err = call(&tool(&server), args("#private-ops", "hi", None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a member of `#private-ops`"), "{err}");
        assert!(err.contains("chat:write.public"), "{err}");
    }
}
