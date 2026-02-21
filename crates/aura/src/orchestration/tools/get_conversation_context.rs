//! Tool for workers to retrieve conversation history on demand.

use rig::completion::message::{AssistantContent, UserContent};
use rig::completion::{Message, ToolDefinition};
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;

/// Provides workers with read-only access to the conversation history.
///
/// Follows the same injection pattern as `ReadArtifactTool`:
/// - `AgentConfig.orchestration_chat_history` holds an `Arc<Vec<Message>>`
/// - `builder.rs` injects this tool when the field is `Some`
/// - The orchestrator sets the field before the worker loop
#[derive(Clone)]
pub struct GetConversationContextTool {
    history: Arc<Vec<Message>>,
}

impl GetConversationContextTool {
    pub fn new(history: Arc<Vec<Message>>) -> Self {
        Self { history }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetConversationContextArgs {
    /// Return only the last N messages. Omit or set to 0 to return all messages.
    #[serde(default)]
    pub last_n: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct GetConversationContextOutput {
    pub messages: Vec<ConversationMessage>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

/// Extract the first text content from a message, skipping tool calls/results.
fn extract_text(msg: &Message) -> (String, String) {
    match msg {
        Message::User { content } => {
            let text = content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            ("user".to_string(), text)
        }
        Message::Assistant { content, .. } => {
            let text = content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            ("assistant".to_string(), text)
        }
    }
}

impl Tool for GetConversationContextTool {
    const NAME: &'static str = "get_conversation_context";

    type Error = Infallible;
    type Args = GetConversationContextArgs;
    type Output = GetConversationContextOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Retrieve conversation history between the user and assistant. \
                 Use this when the task references prior conversation context that wasn't \
                 included in the task description."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "last_n": {
                        "type": "integer",
                        "description": "Return only the last N messages. Omit to return all."
                    }
                },
                "required": []
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let messages: Vec<ConversationMessage> = self
            .history
            .iter()
            .map(|msg| {
                let (role, content) = extract_text(msg);
                ConversationMessage { role, content }
            })
            .collect();

        let messages = match args.last_n {
            Some(n) if n > 0 && n < messages.len() => messages[messages.len() - n..].to_vec(),
            _ => messages,
        };

        let count = messages.len();
        tracing::info!(
            "get_conversation_context called (returning {} messages)",
            count
        );

        Ok(GetConversationContextOutput { messages, count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_history() -> Arc<Vec<Message>> {
        Arc::new(vec![
            Message::user("I have the numbers 10, 20, 30"),
            Message::assistant("Got it, I'll remember those numbers."),
            Message::user("Compute the mean of those numbers"),
        ])
    }

    #[tokio::test]
    async fn test_get_conversation_context_all() {
        let tool = GetConversationContextTool::new(make_history());
        let result = tool
            .call(GetConversationContextArgs { last_n: None })
            .await
            .unwrap();

        assert_eq!(result.count, 3);
        assert_eq!(result.messages[0].role, "user");
        assert!(result.messages[0].content.contains("10, 20, 30"));
        assert_eq!(result.messages[1].role, "assistant");
        assert_eq!(result.messages[2].role, "user");
    }

    #[tokio::test]
    async fn test_get_conversation_context_last_n() {
        let tool = GetConversationContextTool::new(make_history());
        let result = tool
            .call(GetConversationContextArgs { last_n: Some(2) })
            .await
            .unwrap();

        assert_eq!(result.count, 2);
        assert_eq!(result.messages[0].role, "assistant");
        assert_eq!(result.messages[1].role, "user");
        assert!(result.messages[1].content.contains("mean"));
    }

    #[tokio::test]
    async fn test_get_conversation_context_empty() {
        let tool = GetConversationContextTool::new(Arc::new(vec![]));
        let result = tool
            .call(GetConversationContextArgs { last_n: None })
            .await
            .unwrap();

        assert_eq!(result.count, 0);
        assert!(result.messages.is_empty());
    }

    #[tokio::test]
    async fn test_get_conversation_context_definition() {
        let tool = GetConversationContextTool::new(Arc::new(vec![]));
        let def = tool.definition("".to_string()).await;
        assert_eq!(def.name, "get_conversation_context");
        assert!(def.description.contains("conversation history"));
    }
}
