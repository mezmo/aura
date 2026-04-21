use crate::api::types::Message;

pub struct ConversationHistory {
    messages: Vec<Message>,
}

impl ConversationHistory {
    pub fn new(system_prompt: Option<&str>) -> Self {
        let mut messages = Vec::new();
        if let Some(prompt) = system_prompt {
            messages.push(Message::system(prompt));
        }
        Self { messages }
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }

    pub fn add_user(&mut self, content: &str) {
        self.messages.push(Message::user(content));
    }

    pub fn add_assistant(&mut self, content: &str) {
        self.messages.push(Message::assistant(content));
    }

    /// Add an assistant message that contains tool calls.
    pub fn add_assistant_with_tool_calls(
        &mut self,
        content: Option<String>,
        tool_calls: Vec<crate::api::types::ToolCallInfo>,
    ) {
        self.messages
            .push(Message::assistant_with_tool_calls(content, tool_calls));
    }

    /// Add a tool result message.
    pub fn add_tool_result(&mut self, call_id: &str, name: &str, result: &str) {
        self.messages
            .push(Message::tool_result(call_id, name, result));
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Remove the last message if it is a user message (used on cancel).
    pub fn pop_last_user(&mut self) {
        if self.messages.last().map(|m| m.role.as_str()) == Some("user") {
            self.messages.pop();
        }
    }

    /// Compact the conversation by keeping only the system prompt (if present)
    /// and the second half of the remaining messages. Avoids splitting a
    /// tool_calls/tool_result pair by adjusting the cut point past any trailing
    /// role="tool" messages.
    ///
    /// Returns the number of messages that were removed.
    pub fn compact(&mut self) -> usize {
        let has_system = self
            .messages
            .first()
            .map(|m| m.role == "system")
            .unwrap_or(false);
        let start = if has_system { 1 } else { 0 };
        let remaining = self.messages.len() - start;

        if remaining <= 2 {
            return 0;
        }

        // Target midpoint: keep second half
        let mut cut = start + remaining / 2;

        // Advance cut past any role="tool" messages to avoid orphaned tool results
        while cut < self.messages.len() && self.messages[cut].role == "tool" {
            cut += 1;
        }

        // Fallback: if we advanced past everything, keep last 2
        if cut >= self.messages.len() {
            cut = self.messages.len().saturating_sub(2);
        }

        let removed = cut - start;
        if removed == 0 {
            return 0;
        }

        self.messages.drain(start..cut);
        removed
    }

    pub fn clear(&mut self) {
        let system = self
            .messages
            .first()
            .filter(|m| m.role == "system")
            .cloned();
        self.messages.clear();
        if let Some(msg) = system {
            self.messages.push(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{FunctionCallInfo, Message, ToolCallInfo};

    #[test]
    fn new_without_system_prompt() {
        let history = ConversationHistory::new(None);
        assert!(history.messages().is_empty());
    }

    #[test]
    fn new_with_system_prompt() {
        let history = ConversationHistory::new(Some("Be helpful"));
        assert_eq!(history.messages().len(), 1);
        assert_eq!(history.messages()[0].role, "system");
        assert_eq!(history.messages()[0].content.as_deref(), Some("Be helpful"));
    }

    #[test]
    fn add_user_and_assistant() {
        let mut history = ConversationHistory::new(None);
        history.add_user("Hello");
        history.add_assistant("Hi there");
        assert_eq!(history.messages().len(), 2);
        assert_eq!(history.messages()[0].role, "user");
        assert_eq!(history.messages()[1].role, "assistant");
    }

    #[test]
    fn add_tool_calls_and_result() {
        let mut history = ConversationHistory::new(None);
        let tool_calls = vec![ToolCallInfo {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCallInfo {
                name: "Shell".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            },
        }];
        history.add_assistant_with_tool_calls(None, tool_calls);
        history.add_tool_result("call_1", "Shell", "file.txt");
        assert_eq!(history.messages().len(), 2);
        assert_eq!(history.messages()[0].role, "assistant");
        assert_eq!(history.messages()[1].role, "tool");
    }

    #[test]
    fn pop_last_user_removes_user() {
        let mut history = ConversationHistory::new(None);
        history.add_user("Hello");
        history.pop_last_user();
        assert!(history.messages().is_empty());
    }

    #[test]
    fn pop_last_user_no_op_on_assistant() {
        let mut history = ConversationHistory::new(None);
        history.add_assistant("Hi");
        history.pop_last_user();
        assert_eq!(history.messages().len(), 1);
    }

    #[test]
    fn pop_last_user_no_op_on_empty() {
        let mut history = ConversationHistory::new(None);
        history.pop_last_user();
        assert!(history.messages().is_empty());
    }

    #[test]
    fn clear_preserves_system_prompt() {
        let mut history = ConversationHistory::new(Some("system"));
        history.add_user("Hello");
        history.add_assistant("Hi");
        history.clear();
        assert_eq!(history.messages().len(), 1);
        assert_eq!(history.messages()[0].role, "system");
    }

    #[test]
    fn clear_without_system() {
        let mut history = ConversationHistory::new(None);
        history.add_user("Hello");
        history.clear();
        assert!(history.messages().is_empty());
    }

    #[test]
    fn compact_with_system_prompt() {
        let mut history = ConversationHistory::new(Some("system"));
        // Add 10 user/assistant pairs
        for i in 0..10 {
            history.add_user(&format!("q{i}"));
            history.add_assistant(&format!("a{i}"));
        }
        // 1 system + 20 messages = 21 total
        assert_eq!(history.messages().len(), 21);

        let removed = history.compact();
        assert!(removed > 0);
        // System prompt should still be first
        assert_eq!(history.messages()[0].role, "system");
        // Should have fewer messages than before
        assert!(history.messages().len() < 21);
    }

    #[test]
    fn compact_without_system_prompt() {
        let mut history = ConversationHistory::new(None);
        for i in 0..10 {
            history.add_user(&format!("q{i}"));
            history.add_assistant(&format!("a{i}"));
        }
        assert_eq!(history.messages().len(), 20);

        let removed = history.compact();
        assert!(removed > 0);
        assert!(history.messages().len() < 20);
    }

    #[test]
    fn compact_too_few_messages() {
        let mut history = ConversationHistory::new(Some("system"));
        history.add_user("Hello");
        history.add_assistant("Hi");
        // 1 system + 2 messages = 3 total, remaining = 2
        let removed = history.compact();
        assert_eq!(removed, 0);
        assert_eq!(history.messages().len(), 3);
    }

    #[test]
    fn compact_skips_tool_boundary() {
        let mut history = ConversationHistory::new(Some("system"));
        // Add enough messages to trigger compaction
        for _ in 0..5 {
            history.add_user("q");
            let tool_calls = vec![ToolCallInfo {
                id: "c1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCallInfo {
                    name: "Shell".to_string(),
                    arguments: "{}".to_string(),
                },
            }];
            history.add_assistant_with_tool_calls(None, tool_calls);
            history.add_tool_result("c1", "Shell", "ok");
            history.add_assistant("done");
        }

        let before = history.messages().len();
        let removed = history.compact();
        assert!(removed > 0);

        // After compaction, the first non-system message should NOT be a tool result
        if history.messages().len() > 1 {
            assert_ne!(
                history.messages()[1].role,
                "tool",
                "compaction should not leave orphaned tool results"
            );
        }
        assert!(history.messages().len() < before);
    }

    #[test]
    fn from_messages() {
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let history = ConversationHistory::from_messages(messages);
        assert_eq!(history.messages().len(), 3);
    }
}
