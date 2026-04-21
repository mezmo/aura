use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct ModelList {
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool calls made by the assistant (present when role is "assistant")
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallInfo>>,
    /// Tool call ID this message is a result for (present when role is "tool")
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool name (present when role is "tool")
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Tool call info in an assistant message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInfo {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCallInfo,
}

/// Function call details within a tool call
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallInfo {
    pub name: String,
    pub arguments: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// Create an assistant message with tool calls (and optional text content).
    pub fn assistant_with_tool_calls(
        content: Option<String>,
        tool_calls: Vec<ToolCallInfo>,
    ) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
        }
    }

    /// Create a tool result message.
    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        result: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(result.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            name: Some(name.into()),
        }
    }

    /// Get content as a string reference, defaulting to empty string.
    #[allow(dead_code)]
    pub fn content_str(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}

/// Tool definition sent to the API (OpenAI-compatible format)
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

/// Function definition within a tool definition
#[derive(Debug, Clone, Serialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct ChatRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub messages: Vec<Message>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
}

#[derive(Debug, Deserialize)]
pub struct ChatCompletionChunk {
    pub choices: Vec<ChunkChoice>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkChoice {
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Delta {
    #[allow(dead_code)]
    pub role: Option<String>,
    pub content: Option<String>,
    /// Tool calls from the assistant (may arrive incrementally across chunks)
    pub tool_calls: Option<Vec<DeltaToolCall>>,
}

/// A tool call delta from a streaming chunk
#[derive(Debug, Clone, Deserialize)]
pub struct DeltaToolCall {
    pub index: usize,
    pub id: Option<String>,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub call_type: Option<String>,
    pub function: Option<DeltaFunctionCall>,
}

/// Function call delta (name and arguments may arrive incrementally)
#[derive(Debug, Clone, Deserialize)]
pub struct DeltaFunctionCall {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Accumulated tool call built from streaming deltas
#[derive(Debug, Clone)]
pub struct AccumulatedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct CompletionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub struct ChatCompletion {
    pub choices: Vec<CompletionChoice>,
    pub usage: Option<CompletionUsage>,
}

#[derive(Debug, Deserialize)]
pub struct CompletionChoice {
    pub message: CompletionMessage,
}

#[derive(Debug, Deserialize)]
pub struct CompletionMessage {
    pub content: Option<String>,
}

// AuraToolEvent and AuraUsageEvent removed — replaced by shared types from
// the aura-events crate (AuraStreamEvent enum with Serialize + Deserialize).

/// Details of a single Shell call executed within an Update group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellCallDetail {
    pub command_name: String,
    pub full_command: String,
    pub result: String,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
}

/// An event recorded during a REPL session for replay via `/expand`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DisplayEvent {
    UserInput(String),
    ToolCall {
        tool_name: String,
        arguments: BTreeMap<String, serde_json::Value>,
        #[serde(with = "duration_millis")]
        duration: Duration,
        result: Option<String>,
    },
    AssistantResponse {
        summary: String,
        text: String,
    },
    Cancelled,
    Error(String),
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    Compacted {
        messages_removed: usize,
    },
    FileUpdate {
        file_path: String,
        commands_used: Vec<String>,
        shell_calls: Vec<ShellCallDetail>,
        diff_text: String,
        lines_added: usize,
        lines_removed: usize,
        #[serde(with = "duration_millis")]
        duration: Duration,
    },
    OrchestratorPlanCreated {
        goal: String,
        bullet_color: (u8, u8, u8),
        fields: BTreeMap<String, serde_json::Value>,
    },
    OrchestratorTaskStarted {
        worker_id: String,
        task_id: String,
        description: String,
        bullet_color: (u8, u8, u8),
        fields: BTreeMap<String, serde_json::Value>,
    },
    OrchestratorToolCallStarted {
        tool_name: String,
        tool_initiator_id: String,
        bullet_color: (u8, u8, u8),
        fields: BTreeMap<String, serde_json::Value>,
    },
    OrchestratorToolCallCompleted {
        tool_name: String,
        tool_initiator_id: String,
        bullet_color: (u8, u8, u8),
        duration_ms: Option<u64>,
        fields: BTreeMap<String, serde_json::Value>,
    },
    OrchestratorTaskCompleted {
        worker_id: String,
        task_id: String,
        result: String,
        bullet_color: (u8, u8, u8),
        fields: BTreeMap<String, serde_json::Value>,
    },
    OrchestratorSynthesizing {
        bullet_color: (u8, u8, u8),
    },
    OrchestratorIterationComplete {
        iteration: u64,
        quality_score: String,
        bullet_color: (u8, u8, u8),
        fields: BTreeMap<String, serde_json::Value>,
    },
    OrchestratorScratchpadSavings {
        tokens_intercepted: u64,
        tokens_extracted: u64,
    },
}

/// Convert a snake_case string to PascalCase. E.g. `get_me` → `GetMe`.
pub fn snake_to_pascal_case(s: &str) -> String {
    s.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => {
                    let mut word = c.to_uppercase().to_string();
                    word.extend(chars);
                    word
                }
                None => String::new(),
            }
        })
        .collect()
}

pub mod duration_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_millis().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let millis = u64::deserialize(d)?;
        Ok(Duration::from_millis(millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Message constructors
    // -----------------------------------------------------------------------

    #[test]
    fn message_system() {
        let msg = Message::system("Be helpful");
        assert_eq!(msg.role, "system");
        assert_eq!(msg.content.as_deref(), Some("Be helpful"));
        assert!(msg.tool_calls.is_none());
        assert!(msg.tool_call_id.is_none());
        assert!(msg.name.is_none());
    }

    #[test]
    fn message_user() {
        let msg = Message::user("Hello");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn message_assistant() {
        let msg = Message::assistant("Hi there");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content.as_deref(), Some("Hi there"));
    }

    #[test]
    fn message_assistant_with_tool_calls() {
        let tool_calls = vec![ToolCallInfo {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCallInfo {
                name: "Shell".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            },
        }];
        let msg = Message::assistant_with_tool_calls(Some("thinking...".to_string()), tool_calls);
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content.as_deref(), Some("thinking..."));
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(msg.tool_calls.as_ref().unwrap()[0].function.name, "Shell");
    }

    #[test]
    fn message_tool_result() {
        let msg = Message::tool_result("call_1", "Shell", "output here");
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.content.as_deref(), Some("output here"));
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msg.name.as_deref(), Some("Shell"));
    }

    #[test]
    fn message_content_str_some() {
        let msg = Message::user("hello");
        assert_eq!(msg.content_str(), "hello");
    }

    #[test]
    fn message_content_str_none() {
        let msg = Message {
            role: "assistant".to_string(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        assert_eq!(msg.content_str(), "");
    }

    // -----------------------------------------------------------------------
    // Message serialization roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn message_serde_roundtrip() {
        let msg = Message::user("test message");
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, "user");
        assert_eq!(parsed.content.as_deref(), Some("test message"));
        // None fields should be omitted
        assert!(!json.contains("tool_calls"));
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("name"));
    }

    #[test]
    fn message_with_tool_calls_serde_roundtrip() {
        let tool_calls = vec![ToolCallInfo {
            id: "call_abc".to_string(),
            call_type: "function".to_string(),
            function: FunctionCallInfo {
                name: "Read".to_string(),
                arguments: r#"{"file_path":"test.txt"}"#.to_string(),
            },
        }];
        let msg = Message::assistant_with_tool_calls(None, tool_calls);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert!(parsed.tool_calls.is_some());
        assert_eq!(parsed.tool_calls.unwrap()[0].id, "call_abc");
    }

    // -----------------------------------------------------------------------
    // snake_to_pascal_case
    // -----------------------------------------------------------------------

    #[test]
    fn snake_to_pascal_empty() {
        assert_eq!(snake_to_pascal_case(""), "");
    }

    #[test]
    fn snake_to_pascal_single_word() {
        assert_eq!(snake_to_pascal_case("hello"), "Hello");
    }

    #[test]
    fn snake_to_pascal_multiple_words() {
        assert_eq!(snake_to_pascal_case("get_me_data"), "GetMeData");
    }

    #[test]
    fn snake_to_pascal_leading_underscore() {
        assert_eq!(snake_to_pascal_case("_private"), "Private");
    }

    #[test]
    fn snake_to_pascal_trailing_underscore() {
        assert_eq!(snake_to_pascal_case("data_"), "Data");
    }

    #[test]
    fn snake_to_pascal_multiple_underscores() {
        assert_eq!(snake_to_pascal_case("a__b"), "AB");
    }

    #[test]
    fn snake_to_pascal_already_pascal() {
        assert_eq!(snake_to_pascal_case("AlreadyPascal"), "AlreadyPascal");
    }

    // -----------------------------------------------------------------------
    // AuraStreamEvent deserialization (shared types from aura-events crate)
    // -----------------------------------------------------------------------

    #[test]
    fn aura_stream_event_usage_roundtrip() {
        use aura_events::AuraStreamEvent;
        let data =
            r#"{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"session_id":"s1"}"#;
        match serde_json::from_str::<AuraStreamEvent>(data).unwrap() {
            AuraStreamEvent::Usage {
                prompt_tokens,
                completion_tokens,
                ..
            } => {
                assert_eq!(prompt_tokens, 100);
                assert_eq!(completion_tokens, 50);
            }
            other => panic!("expected Usage, got {:?}", other),
        }
    }

    #[test]
    fn aura_stream_event_tool_requested_roundtrip() {
        use aura_events::AuraStreamEvent;
        let data = r#"{"tool_id":"call_1","tool_name":"Shell","arguments":{"command":"ls"},"agent_id":"main","session_id":"s1"}"#;
        match serde_json::from_str::<AuraStreamEvent>(data).unwrap() {
            AuraStreamEvent::ToolRequested {
                tool_id,
                tool_name,
                arguments,
                ..
            } => {
                assert_eq!(tool_id, "call_1");
                assert_eq!(tool_name, "Shell");
                assert_eq!(arguments["command"], "ls");
            }
            other => panic!("expected ToolRequested, got {:?}", other),
        }
    }

    #[test]
    fn aura_stream_event_tool_complete_roundtrip() {
        use aura_events::AuraStreamEvent;
        let data = r#"{"tool_id":"call_1","tool_name":"Shell","duration_ms":1500,"success":true,"result":"output","agent_id":"main","session_id":"s1"}"#;
        match serde_json::from_str::<AuraStreamEvent>(data).unwrap() {
            AuraStreamEvent::ToolComplete {
                tool_name,
                duration_ms,
                result,
                success,
                ..
            } => {
                assert_eq!(tool_name, "Shell");
                assert_eq!(duration_ms, 1500);
                assert!(success);
                assert_eq!(result.as_deref(), Some("output"));
            }
            other => panic!("expected ToolComplete, got {:?}", other),
        }
    }

    #[test]
    fn aura_stream_event_invalid_json() {
        use aura_events::AuraStreamEvent;
        assert!(serde_json::from_str::<AuraStreamEvent>("not json").is_err());
    }

    // -----------------------------------------------------------------------
    // DisplayEvent serialization roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn display_event_user_input_roundtrip() {
        let event = DisplayEvent::UserInput("hello".to_string());
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DisplayEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            DisplayEvent::UserInput(s) => assert_eq!(s, "hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn display_event_tool_call_roundtrip() {
        let event = DisplayEvent::ToolCall {
            tool_name: "Shell".to_string(),
            arguments: BTreeMap::new(),
            duration: Duration::from_millis(100),
            result: Some("ok".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DisplayEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            DisplayEvent::ToolCall {
                tool_name,
                duration,
                ..
            } => {
                assert_eq!(tool_name, "Shell");
                assert_eq!(duration, Duration::from_millis(100));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn display_event_file_update_roundtrip() {
        let event = DisplayEvent::FileUpdate {
            file_path: "src/main.rs".to_string(),
            commands_used: vec!["sed".to_string()],
            shell_calls: vec![],
            diff_text: "+new line".to_string(),
            lines_added: 1,
            lines_removed: 0,
            duration: Duration::from_millis(250),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DisplayEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            DisplayEvent::FileUpdate {
                file_path,
                lines_added,
                ..
            } => {
                assert_eq!(file_path, "src/main.rs");
                assert_eq!(lines_added, 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -----------------------------------------------------------------------
    // duration_millis serde module
    // -----------------------------------------------------------------------

    #[test]
    fn duration_millis_roundtrip() {
        // Test via ShellCallDetail which uses #[serde(with = "duration_millis")]
        let detail = ShellCallDetail {
            command_name: "ls".to_string(),
            full_command: "ls -la".to_string(),
            result: "output".to_string(),
            duration: Duration::from_millis(1234),
        };
        let json = serde_json::to_string(&detail).unwrap();
        assert!(json.contains("1234"));
        let parsed: ShellCallDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.duration, Duration::from_millis(1234));
    }
}
