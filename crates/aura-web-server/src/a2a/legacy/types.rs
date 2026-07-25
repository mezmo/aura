//! A2A v0.3 JSON-RPC wire types, and their translation to and from the v1.0
//! types [`crate::a2a::AuraRequestHandler`] speaks.
//!
//! The two protocol versions differ in enum spellings (`working` vs
//! `TASK_STATE_WORKING`), in how unions are discriminated (a `kind` tag vs
//! field presence), and in a handful of field names. Everything that differs is
//! confined to this module: the request handler only ever sees v1.0 types.

use std::collections::HashMap;

use a2a::{
    Artifact, CancelTaskRequest, GetTaskRequest, Message, Part, PartContent, Role,
    SendMessageConfiguration, SendMessageRequest, SendMessageResponse, StreamResponse,
    SubscribeToTaskRequest, Task, TaskArtifactUpdateEvent, TaskId, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

type Metadata = HashMap<String, Value>;

/// Part-metadata key marking a `DataPart` whose payload sits under a synthetic
/// `value` key rather than being the payload itself.
const DATA_PART_COMPAT: &str = "data_part_compat";

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LegacyRole {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "agent")]
    Agent,
    #[serde(rename = "")]
    Unspecified,
}

impl From<Role> for LegacyRole {
    fn from(role: Role) -> Self {
        match role {
            Role::User => LegacyRole::User,
            Role::Agent => LegacyRole::Agent,
            Role::Unspecified => LegacyRole::Unspecified,
        }
    }
}

impl From<LegacyRole> for Role {
    fn from(role: LegacyRole) -> Self {
        match role {
            LegacyRole::User => Role::User,
            LegacyRole::Agent => Role::Agent,
            LegacyRole::Unspecified => Role::Unspecified,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegacyTaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
    InputRequired,
    Rejected,
    AuthRequired,
    Unknown,
}

impl From<TaskState> for LegacyTaskState {
    /// `Unspecified` has no v0.3 spelling; v0.3 clients read `unknown` as
    /// "still running, state not reported", which is the closest fit.
    fn from(state: TaskState) -> Self {
        match state {
            TaskState::Submitted => LegacyTaskState::Submitted,
            TaskState::Working => LegacyTaskState::Working,
            TaskState::Completed => LegacyTaskState::Completed,
            TaskState::Failed => LegacyTaskState::Failed,
            TaskState::Canceled => LegacyTaskState::Canceled,
            TaskState::InputRequired => LegacyTaskState::InputRequired,
            TaskState::Rejected => LegacyTaskState::Rejected,
            TaskState::AuthRequired => LegacyTaskState::AuthRequired,
            TaskState::Unspecified => LegacyTaskState::Unknown,
        }
    }
}

impl From<LegacyTaskState> for TaskState {
    fn from(state: LegacyTaskState) -> Self {
        match state {
            LegacyTaskState::Submitted => TaskState::Submitted,
            LegacyTaskState::Working => TaskState::Working,
            LegacyTaskState::Completed => TaskState::Completed,
            LegacyTaskState::Failed => TaskState::Failed,
            LegacyTaskState::Canceled => TaskState::Canceled,
            LegacyTaskState::InputRequired => TaskState::InputRequired,
            LegacyTaskState::Rejected => TaskState::Rejected,
            LegacyTaskState::AuthRequired => TaskState::AuthRequired,
            LegacyTaskState::Unknown => TaskState::Unspecified,
        }
    }
}

// ---------------------------------------------------------------------------
// Parts
// ---------------------------------------------------------------------------

/// The v0.3 `FilePart` payload — exactly one of `bytes` or `uri`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LegacyPart {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
    },
    #[serde(rename = "file")]
    File {
        file: LegacyFile,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
    },
    #[serde(rename = "data")]
    Data {
        data: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<Metadata>,
    },
}

impl From<Part> for LegacyPart {
    /// v0.3 splits the file metadata (`mimeType`, `name`) into the part's `file`
    /// object, and requires `data` to be a JSON object — a scalar or array
    /// payload is wrapped under a `value` key and flagged in the metadata so
    /// [`From<LegacyPart> for Part`] can unwrap it again.
    fn from(part: Part) -> Self {
        let Part {
            content,
            filename,
            media_type,
            metadata,
        } = part;

        match content {
            PartContent::Text(text) => LegacyPart::Text { text, metadata },
            PartContent::Raw(bytes) => LegacyPart::File {
                file: LegacyFile {
                    bytes: Some(BASE64.encode(bytes)),
                    uri: None,
                    mime_type: media_type,
                    name: filename,
                },
                metadata,
            },
            PartContent::Url(uri) => LegacyPart::File {
                file: LegacyFile {
                    bytes: None,
                    uri: Some(uri),
                    mime_type: media_type,
                    name: filename,
                },
                metadata,
            },
            PartContent::Data(Value::Object(data)) => LegacyPart::Data { data, metadata },
            PartContent::Data(value) => {
                let mut metadata = metadata.unwrap_or_default();
                metadata.insert(DATA_PART_COMPAT.to_string(), Value::Bool(true));
                LegacyPart::Data {
                    data: Map::from_iter([("value".to_string(), value)]),
                    metadata: Some(metadata),
                }
            }
        }
    }
}

impl From<LegacyPart> for Part {
    fn from(part: LegacyPart) -> Self {
        match part {
            LegacyPart::Text { text, metadata } => Part {
                content: PartContent::Text(text),
                filename: None,
                media_type: None,
                metadata,
            },
            LegacyPart::File { file, metadata } => {
                // `bytes` wins when a sender sets both; a v0.3 `FilePart` is a
                // union, so only one is ever meaningful.
                let content = match (file.bytes, file.uri) {
                    (Some(bytes), _) => match BASE64.decode(&bytes) {
                        Ok(decoded) => PartContent::Raw(decoded),
                        Err(_) => PartContent::Raw(bytes.into_bytes()),
                    },
                    (None, Some(uri)) => PartContent::Url(uri),
                    (None, None) => PartContent::Data(Value::Null),
                };
                Part {
                    content,
                    filename: file.name,
                    media_type: file.mime_type,
                    metadata,
                }
            }
            LegacyPart::Data { data, mut metadata } => {
                let wrapped = metadata
                    .as_ref()
                    .and_then(|m| m.get(DATA_PART_COMPAT))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                let value = if wrapped {
                    if let Some(m) = metadata.as_mut() {
                        m.remove(DATA_PART_COMPAT);
                    }
                    data.get("value").cloned().unwrap_or(Value::Null)
                } else {
                    Value::Object(data)
                };

                Part {
                    content: PartContent::Data(value),
                    filename: None,
                    media_type: None,
                    metadata: metadata.filter(|m| !m.is_empty()),
                }
            }
        }
    }
}

fn legacy_parts(parts: Vec<Part>) -> Vec<LegacyPart> {
    parts.into_iter().map(LegacyPart::from).collect()
}

fn v1_parts(parts: Vec<LegacyPart>) -> Vec<Part> {
    parts.into_iter().map(Part::from).collect()
}

// ---------------------------------------------------------------------------
// Message / Task / Artifact
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyMessage {
    #[serde(default)]
    pub kind: String,
    pub message_id: String,
    pub role: LegacyRole,
    pub parts: Vec<LegacyPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_task_ids: Option<Vec<TaskId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl From<Message> for LegacyMessage {
    fn from(message: Message) -> Self {
        LegacyMessage {
            kind: "message".to_string(),
            message_id: message.message_id,
            role: message.role.into(),
            parts: legacy_parts(message.parts),
            context_id: message.context_id,
            task_id: message.task_id,
            reference_task_ids: message.reference_task_ids,
            extensions: message.extensions,
            metadata: message.metadata,
        }
    }
}

impl From<LegacyMessage> for Message {
    fn from(message: LegacyMessage) -> Self {
        Message {
            message_id: message.message_id,
            context_id: message.context_id,
            task_id: message.task_id,
            role: message.role.into(),
            parts: v1_parts(message.parts),
            metadata: message.metadata,
            extensions: message.extensions,
            reference_task_ids: message.reference_task_ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyTaskStatus {
    pub state: LegacyTaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<LegacyMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

impl From<TaskStatus> for LegacyTaskStatus {
    fn from(status: TaskStatus) -> Self {
        LegacyTaskStatus {
            state: status.state.into(),
            message: status.message.map(LegacyMessage::from),
            timestamp: status.timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyArtifact {
    pub artifact_id: String,
    pub parts: Vec<LegacyPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl From<Artifact> for LegacyArtifact {
    fn from(artifact: Artifact) -> Self {
        LegacyArtifact {
            artifact_id: artifact.artifact_id,
            parts: legacy_parts(artifact.parts),
            name: artifact.name,
            description: artifact.description,
            extensions: artifact.extensions,
            metadata: artifact.metadata,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyTask {
    #[serde(default)]
    pub kind: String,
    pub id: String,
    pub context_id: String,
    pub status: LegacyTaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<LegacyArtifact>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<LegacyMessage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl From<Task> for LegacyTask {
    fn from(task: Task) -> Self {
        LegacyTask {
            kind: "task".to_string(),
            id: task.id,
            context_id: task.context_id,
            status: task.status.into(),
            artifacts: task
                .artifacts
                .map(|a| a.into_iter().map(LegacyArtifact::from).collect()),
            history: task
                .history
                .map(|h| h.into_iter().map(LegacyMessage::from).collect()),
            metadata: task.metadata,
        }
    }
}

// ---------------------------------------------------------------------------
// Stream events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyTaskStatusUpdateEvent {
    #[serde(default)]
    pub kind: String,
    pub task_id: String,
    pub context_id: String,
    pub status: LegacyTaskStatus,
    #[serde(rename = "final")]
    pub is_final: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl From<TaskStatusUpdateEvent> for LegacyTaskStatusUpdateEvent {
    /// v0.3 has no `TaskState` for "waiting on the client", so `final` is what
    /// tells a v0.3 client to stop reading the stream: it is set for terminal
    /// states and for `input-required`, which also ends the turn.
    fn from(event: TaskStatusUpdateEvent) -> Self {
        let is_final =
            event.status.state.is_terminal() || event.status.state == TaskState::InputRequired;
        LegacyTaskStatusUpdateEvent {
            kind: "status-update".to_string(),
            task_id: event.task_id,
            context_id: event.context_id,
            status: event.status.into(),
            is_final,
            metadata: event.metadata,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyTaskArtifactUpdateEvent {
    #[serde(default)]
    pub kind: String,
    pub task_id: String,
    pub context_id: String,
    pub artifact: LegacyArtifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl From<TaskArtifactUpdateEvent> for LegacyTaskArtifactUpdateEvent {
    fn from(event: TaskArtifactUpdateEvent) -> Self {
        LegacyTaskArtifactUpdateEvent {
            kind: "artifact-update".to_string(),
            task_id: event.task_id,
            context_id: event.context_id,
            artifact: event.artifact.into(),
            append: event.append,
            last_chunk: event.last_chunk,
            metadata: event.metadata,
        }
    }
}

/// A v0.3 agent event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LegacyEvent {
    Task(LegacyTask),
    Message(LegacyMessage),
    StatusUpdate(LegacyTaskStatusUpdateEvent),
    ArtifactUpdate(LegacyTaskArtifactUpdateEvent),
}

impl From<SendMessageResponse> for LegacyEvent {
    fn from(response: SendMessageResponse) -> Self {
        match response {
            SendMessageResponse::Task(task) => LegacyEvent::Task(task.into()),
            SendMessageResponse::Message(message) => LegacyEvent::Message(message.into()),
        }
    }
}

impl From<StreamResponse> for LegacyEvent {
    fn from(response: StreamResponse) -> Self {
        match response {
            StreamResponse::Task(task) => LegacyEvent::Task(task.into()),
            StreamResponse::Message(message) => LegacyEvent::Message(message.into()),
            StreamResponse::StatusUpdate(event) => LegacyEvent::StatusUpdate(event.into()),
            StreamResponse::ArtifactUpdate(event) => LegacyEvent::ArtifactUpdate(event.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Request params
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyMessageSendConfiguration {
    #[serde(default)]
    pub accepted_output_modes: Option<Vec<String>>,
    #[serde(default)]
    pub history_length: Option<i32>,
    #[serde(default)]
    pub blocking: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyMessageSendParams {
    pub message: LegacyMessage,
    #[serde(default)]
    pub configuration: Option<LegacyMessageSendConfiguration>,
    #[serde(default)]
    pub metadata: Option<Metadata>,
}

impl From<LegacyMessageSendParams> for SendMessageRequest {
    /// v0.3 `blocking` is the inverse of v1.0 `returnImmediately`.
    fn from(params: LegacyMessageSendParams) -> Self {
        SendMessageRequest {
            message: params.message.into(),
            configuration: params.configuration.map(|c| SendMessageConfiguration {
                accepted_output_modes: c.accepted_output_modes,
                task_push_notification_config: None,
                history_length: c.history_length,
                return_immediately: c.blocking.map(|blocking| !blocking),
            }),
            metadata: params.metadata,
            tenant: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyTaskQueryParams {
    pub id: String,
    #[serde(default)]
    pub history_length: Option<i32>,
    #[serde(default)]
    pub metadata: Option<Metadata>,
}

impl From<LegacyTaskQueryParams> for GetTaskRequest {
    fn from(params: LegacyTaskQueryParams) -> Self {
        GetTaskRequest {
            id: params.id,
            history_length: params.history_length,
            tenant: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyTaskIdParams {
    pub id: String,
    #[serde(default)]
    pub metadata: Option<Metadata>,
}

impl From<LegacyTaskIdParams> for CancelTaskRequest {
    fn from(params: LegacyTaskIdParams) -> Self {
        CancelTaskRequest {
            id: params.id,
            metadata: params.metadata,
            tenant: None,
        }
    }
}

impl From<LegacyTaskIdParams> for SubscribeToTaskRequest {
    fn from(params: LegacyTaskIdParams) -> Self {
        SubscribeToTaskRequest {
            id: params.id,
            tenant: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v1_message() -> Message {
        Message {
            message_id: "m1".into(),
            context_id: Some("c1".into()),
            task_id: Some("t1".into()),
            role: Role::Agent,
            parts: vec![Part::text("hello")],
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        }
    }

    #[test]
    fn message_serializes_with_the_v0_3_kind_tag_and_role_spelling() {
        let value = serde_json::to_value(LegacyMessage::from(v1_message())).unwrap();

        assert_eq!(
            value,
            json!({
                "kind": "message",
                "messageId": "m1",
                "role": "agent",
                "parts": [{"kind": "text", "text": "hello"}],
                "contextId": "c1",
                "taskId": "t1",
            })
        );
    }

    #[test]
    fn task_serializes_with_kebab_case_state() {
        let task = Task {
            id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::InputRequired,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };

        let value = serde_json::to_value(LegacyTask::from(task)).unwrap();

        assert_eq!(
            value,
            json!({
                "kind": "task",
                "id": "t1",
                "contextId": "c1",
                "status": {"state": "input-required"},
            })
        );
    }

    #[test]
    fn status_update_is_final_for_terminal_and_input_required_states() {
        let event_with = |state: TaskState| TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            metadata: None,
        };

        for state in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
            TaskState::InputRequired,
        ] {
            let converted = LegacyTaskStatusUpdateEvent::from(event_with(state.clone()));
            assert!(converted.is_final, "{state:?} should be final");
        }

        for state in [TaskState::Submitted, TaskState::Working] {
            let converted = LegacyTaskStatusUpdateEvent::from(event_with(state.clone()));
            assert!(!converted.is_final, "{state:?} should not be final");
        }
    }

    #[test]
    fn status_update_serializes_final_under_its_reserved_word_name() {
        let value =
            serde_json::to_value(LegacyTaskStatusUpdateEvent::from(TaskStatusUpdateEvent {
                task_id: "t1".into(),
                context_id: "c1".into(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            }))
            .unwrap();

        assert_eq!(
            value,
            json!({
                "kind": "status-update",
                "taskId": "t1",
                "contextId": "c1",
                "status": {"state": "working"},
                "final": false,
            })
        );
    }

    #[test]
    fn raw_part_round_trips_as_a_base64_file_part() {
        let part = Part {
            content: PartContent::Raw(vec![1, 2, 3]),
            filename: Some("blob.bin".into()),
            media_type: Some("application/octet-stream".into()),
            metadata: None,
        };

        let legacy = LegacyPart::from(part.clone());
        assert_eq!(
            serde_json::to_value(&legacy).unwrap(),
            json!({
                "kind": "file",
                "file": {
                    "bytes": "AQID",
                    "mimeType": "application/octet-stream",
                    "name": "blob.bin",
                },
            })
        );
        assert_eq!(Part::from(legacy), part);
    }

    #[test]
    fn url_part_round_trips_as_a_uri_file_part() {
        let part = Part {
            content: PartContent::Url("https://example.com/a.png".into()),
            filename: None,
            media_type: Some("image/png".into()),
            metadata: None,
        };

        let legacy = LegacyPart::from(part.clone());
        assert_eq!(
            serde_json::to_value(&legacy).unwrap(),
            json!({
                "kind": "file",
                "file": {"uri": "https://example.com/a.png", "mimeType": "image/png"},
            })
        );
        assert_eq!(Part::from(legacy), part);
    }

    #[test]
    fn object_data_part_round_trips_without_a_compat_marker() {
        let part = Part::data(json!({"a": 1}));

        let legacy = LegacyPart::from(part.clone());
        assert_eq!(
            serde_json::to_value(&legacy).unwrap(),
            json!({"kind": "data", "data": {"a": 1}})
        );
        assert_eq!(Part::from(legacy), part);
    }

    #[test]
    fn scalar_data_part_is_wrapped_for_v0_3_and_unwrapped_on_the_way_back() {
        let part = Part::data(json!(42));

        let legacy = LegacyPart::from(part.clone());
        assert_eq!(
            serde_json::to_value(&legacy).unwrap(),
            json!({
                "kind": "data",
                "data": {"value": 42},
                "metadata": {"data_part_compat": true},
            })
        );
        assert_eq!(Part::from(legacy), part);
    }

    #[test]
    fn send_params_deserialize_from_the_v0_3_wire_shape() {
        let params: LegacyMessageSendParams = serde_json::from_value(json!({
            "message": {
                "kind": "message",
                "messageId": "m1",
                "role": "user",
                "parts": [{"kind": "text", "text": "say hello"}],
            },
            "configuration": {"blocking": true, "historyLength": 5},
        }))
        .unwrap();

        let request = SendMessageRequest::from(params);
        assert_eq!(request.message.role, Role::User);
        assert_eq!(request.message.text(), Some("say hello"));

        let configuration = request.configuration.unwrap();
        assert_eq!(configuration.return_immediately, Some(false));
        assert_eq!(configuration.history_length, Some(5));
    }

    #[test]
    fn send_params_accept_a_message_without_the_kind_tag() {
        let params: LegacyMessageSendParams = serde_json::from_value(json!({
            "message": {
                "messageId": "m1",
                "role": "user",
                "parts": [{"kind": "text", "text": "hi"}],
            },
        }))
        .unwrap();

        assert_eq!(SendMessageRequest::from(params).message.text(), Some("hi"));
    }

    #[test]
    fn events_deserialize_back_into_the_right_variant() {
        let cases = [
            (
                json!({"kind": "task", "id": "t1", "contextId": "c1",
                    "status": {"state": "working"}}),
                "task",
            ),
            (
                json!({"kind": "message", "messageId": "m1", "role": "agent",
                    "parts": []}),
                "message",
            ),
            (
                json!({"kind": "status-update", "taskId": "t1", "contextId": "c1",
                    "status": {"state": "working"}, "final": false}),
                "status-update",
            ),
            (
                json!({"kind": "artifact-update", "taskId": "t1", "contextId": "c1",
                    "artifact": {"artifactId": "a1", "parts": []}}),
                "artifact-update",
            ),
        ];

        for (value, kind) in cases {
            let event: LegacyEvent = serde_json::from_value(value).unwrap();
            let variant = match event {
                LegacyEvent::Task(_) => "task",
                LegacyEvent::Message(_) => "message",
                LegacyEvent::StatusUpdate(_) => "status-update",
                LegacyEvent::ArtifactUpdate(_) => "artifact-update",
            };
            assert_eq!(variant, kind);
        }
    }
}
