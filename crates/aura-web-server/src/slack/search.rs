//! The `slack_search` tool: search the workspace as the person who sent the
//! message being answered.

use aura::{RigTool, RigToolDefinition};
use serde::{Deserialize, Serialize};

use super::api::{ActionToken, MAX_SEARCH_HITS, SearchHit, SlackApi, SlackApiError};

pub const TOOL_NAME: &str = "slack_search";
/// Page size when the agent names none.
const DEFAULT_LIMIT: usize = 10;

/// The system-prompt addition that goes with the tool.
pub const CITATION_PROMPT: &str = "\n\nYou have a `slack_search` tool that searches this Slack \
workspace with the permissions of the person you are talking to. When an answer draws on a \
result, cite it inline as <permalink|short label> so Slack renders a link, and never invent a \
permalink. Each call returns one page; ask for the next page only when the first did not \
answer the question, because searches are rate limited per person.";

/// One run's search tool, built on the token from the message that
/// started the run.
pub struct SlackSearchTool {
    api: SlackApi,
    action_token: ActionToken,
}

impl SlackSearchTool {
    pub fn new(api: SlackApi, action_token: ActionToken) -> Self {
        Self { api, action_token }
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SearchResult {
    pub permalink: String,
    pub channel: String,
    pub author: String,
    pub is_author_bot: bool,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    pub text: String,
}

impl From<SearchHit> for SearchResult {
    fn from(hit: SearchHit) -> Self {
        Self {
            permalink: hit.permalink,
            channel: hit.channel_name.unwrap_or(hit.channel_id),
            author: hit
                .author_name
                .or(hit.author_user_id)
                .unwrap_or_else(|| "unknown".to_owned()),
            is_author_bot: hit.is_author_bot,
            ts: hit.message_ts,
            thread_ts: hit.thread_ts,
            text: hit.content,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("{0}")]
    Slack(String),
}

impl From<SlackApiError> for SearchError {
    /// Name the cause in the agent's terms; Slack's own codes say nothing
    /// to a model deciding what to tell the person.
    fn from(err: SlackApiError) -> Self {
        let text = match &err {
            SlackApiError::Api { error, .. } if error == "invalid_action_token" => {
                "the search token for this message is no longer valid; answer without searching"
                    .to_owned()
            }
            SlackApiError::Api { error, .. } if error == "missing_scope" => {
                "the Slack app lacks the search:read.public scope, so search is unavailable"
                    .to_owned()
            }
            SlackApiError::Api { error, .. } if error == "feature_not_enabled" => {
                "Slack search is not enabled for this workspace or app".to_owned()
            }
            SlackApiError::Api { error, .. } if error.starts_with("rate") => {
                "Slack search is rate limited right now; do not retry".to_owned()
            }
            other => format!("slack search failed: {other}"),
        };
        Self::Slack(text)
    }
}

impl RigTool for SlackSearchTool {
    const NAME: &'static str = TOOL_NAME;

    type Error = SearchError;
    type Args = SearchArgs;
    type Output = SearchOutput;

    async fn definition(&self, _prompt: String) -> RigToolDefinition {
        RigToolDefinition {
            name: TOOL_NAME.to_owned(),
            description: "Search messages in this Slack workspace's public channels as the \
                          person you are talking to, with their visibility. Use a natural \
                          language question for semantic search or keywords for exact \
                          matches; Slack filters such as `in:<#C123>`, `from:<@U123>`, \
                          `before:2025-01-31` go inside the query. Each result carries a \
                          permalink to cite. Returns one page; pass `cursor` to continue."
                .to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to search for"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_SEARCH_HITS,
                        "description": format!("Results per page, default {DEFAULT_LIMIT}")
                    },
                    "cursor": {
                        "type": "string",
                        "description": "next_cursor from an earlier page of the same query"
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let page = self
            .api
            .assistant_search_context(
                &self.action_token,
                &args.query,
                args.limit.unwrap_or(DEFAULT_LIMIT),
                args.cursor.as_deref(),
            )
            .await?;
        Ok(SearchOutput {
            results: page.messages.into_iter().map(SearchResult::from).collect(),
            next_cursor: page.next_cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::api::{AppToken, BotToken};
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tool(server: &MockServer, token: &str) -> SlackSearchTool {
        let api = SlackApi::new(
            BotToken::new("xoxb-bot".to_owned()).unwrap(),
            AppToken::new("xapp-app".to_owned()).unwrap(),
        )
        .with_base_url(server.uri());
        SlackSearchTool::new(api, ActionToken::new(token.to_owned()))
    }

    fn hit(channel_name: Option<&str>, author_name: Option<&str>) -> SearchHit {
        SearchHit {
            channel_id: "C1".to_owned(),
            channel_name: channel_name.map(str::to_owned),
            author_name: author_name.map(str::to_owned),
            author_user_id: Some("U7".to_owned()),
            is_author_bot: false,
            message_ts: "1.0".to_owned(),
            thread_ts: None,
            content: "hello".to_owned(),
            permalink: "https://x.slack.com/archives/C1/p10".to_owned(),
        }
    }

    #[tokio::test]
    async fn definition_names_the_tool_and_requires_a_query() {
        let server = MockServer::start().await;
        let def = tool(&server, "t").definition(String::new()).await;
        assert_eq!(def.name, TOOL_NAME);
        assert_eq!(def.parameters["required"], serde_json::json!(["query"]));
        assert_eq!(def.parameters["properties"]["limit"]["maximum"], 20);
        assert!(def.description.contains("permalink"));
    }

    #[test]
    fn results_prefer_names_and_fall_back_to_ids() {
        let named = SearchResult::from(hit(Some("proj-gizmo"), Some("Jen")));
        assert_eq!(named.channel, "proj-gizmo");
        assert_eq!(named.author, "Jen");
        let bare = SearchResult::from(hit(None, None));
        assert_eq!(bare.channel, "C1");
        assert_eq!(bare.author, "U7");
        let mut nobody = hit(None, None);
        nobody.author_user_id = None;
        assert_eq!(SearchResult::from(nobody).author, "unknown");
    }

    #[tokio::test]
    async fn call_searches_with_the_run_token_and_renders_permalinks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.search.context"))
            .and(body_string_contains("action_token=run-token"))
            .and(body_string_contains("query=gizmo+launch"))
            .and(body_string_contains("limit=10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "results": {"messages": [{
                    "channel_id": "C1", "channel_name": "proj-gizmo",
                    "author_name": "Jen", "message_ts": "1.0",
                    "content": "launch is friday", "is_author_bot": false,
                    "permalink": "https://x.slack.com/archives/C1/p10"
                }]},
                "response_metadata": {"next_cursor": "c2"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let out = tool(&server, "run-token")
            .call(SearchArgs {
                query: "gizmo launch".to_owned(),
                limit: None,
                cursor: None,
            })
            .await
            .unwrap();
        assert_eq!(out.next_cursor.as_deref(), Some("c2"));
        assert_eq!(out.results.len(), 1);
        assert_eq!(
            out.results[0].permalink,
            "https://x.slack.com/archives/C1/p10"
        );
        assert_eq!(out.results[0].text, "launch is friday");
        let json = serde_json::to_value(&out).unwrap();
        assert!(json["results"][0].get("thread_ts").is_none());
    }

    #[tokio::test]
    async fn slack_errors_become_tool_errors_in_plain_words() {
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

        let err = tool(&server, "stale-secret")
            .call(SearchArgs {
                query: "q".to_owned(),
                limit: Some(3),
                cursor: None,
            })
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("no longer valid"), "{text}");
        assert!(!text.contains("stale-secret"));
    }
}
