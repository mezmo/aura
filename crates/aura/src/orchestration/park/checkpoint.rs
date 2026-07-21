//! The versioned run checkpoint (ADR 2026-07-21, decisions 6 and 7).

use rig::completion::Message;
use serde::{Deserialize, Serialize};

use crate::hitl::{DecisionId, Timestamp};
use crate::orchestration::types::{IterationTimings, Plan, RunId, TaskIdentity};

use super::dispatch::{ArgsDigest, DecisionDispatchState};
use super::headers::IdentityHeader;
use super::ids::{ChatSessionId, ConfigFingerprint, SessionId};
use super::run_fsm::{ResumePoint, WakeReason};

/// Current checkpoint schema version. Bump on any breaking shape change; a
/// reader never guesses at a version it does not know.
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// The single blob a park commits: everything a fresh process needs to
/// reify the run, minus pod-local artifacts (carried as explicit refs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunCheckpoint {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub chat_session_id: Option<ChatSessionId>,
    pub config_fingerprint: ConfigFingerprint,
    pub original_query: String,
    /// Client-supplied conversation history from the parked request.
    pub external_history: Vec<Message>,
    /// The coordinator's own conversation across planning iterations.
    pub coordinator_conversation: Vec<Message>,
    /// Plan snapshot at the drained boundary; blocked tasks appear
    /// `Pending`, with their bindings in [`Self::blocked`].
    pub plan: Plan,
    pub blocked: Vec<BlockedTaskBinding>,
    pub approvals: Vec<ParkedApprovalSnapshot>,
    /// Durable wake reasons already present at park time.
    pub wake_reasons: Vec<WakeReason>,
    pub resume_point: ResumePoint,
    pub identity_headers: Vec<IdentityHeader>,
    /// Pod-local paths referenced by completed-task output.
    pub pod_local_refs: Vec<PodLocalRef>,
    pub iteration: u32,
    pub timings: IterationTimings,
}

/// A blocked task's binding to the approval it waits on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockedTaskBinding {
    pub task: TaskIdentity,
    pub decision_id: DecisionId,
}

/// Serializable form of a parked approval, embedded in the checkpoint;
/// the storage mirror of the in-process `ParkedApproval`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParkedApprovalSnapshot {
    pub decision_id: DecisionId,
    pub task: TaskIdentity,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    /// Digest binding the eventual decision to exactly these arguments.
    pub args_digest: ArgsDigest,
    pub origin: ApprovalOriginSnapshot,
    pub registered_at: Timestamp,
    pub expires_at: Timestamp,
    pub dispatch: DecisionDispatchState,
}

/// Storage mirror of `hitl::ApprovalOrigin`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum ApprovalOriginSnapshot {
    ConfigGate { matched_pattern: String },
    AgentRequested { reason: String },
}

/// A pod-local path a completed task's output points at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodLocalRef {
    pub task: TaskIdentity,
    pub path: String,
}

/// Version envelope around the checkpoint blob: the storage codec
/// surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointEnvelope {
    pub schema_version: u32,
    pub checkpoint: RunCheckpoint,
}

/// A checkpoint that could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckpointCodecError {
    /// The blob's schema version is newer than this binary supports.
    UnknownSchemaVersion {
        found: u32,
        supported: u32,
    },
    Serde(String),
}

impl CheckpointEnvelope {
    pub fn new(checkpoint: RunCheckpoint) -> Self {
        Self {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            checkpoint,
        }
    }

    /// Encode for storage.
    pub fn to_json(&self) -> Result<String, CheckpointCodecError> {
        todo!("staged for #271 P-cards: checkpoint encode")
    }

    /// Decode from storage, rejecting schema versions newer than
    /// [`CHECKPOINT_SCHEMA_VERSION`] - a reader never guesses at a shape it
    /// does not know (rolling upgrades can park on new pods and reap on old
    /// ones).
    pub fn from_json(raw: &str) -> Result<Self, CheckpointCodecError> {
        let _ = raw;
        todo!("staged for #271 P-cards: checkpoint decode + version gate")
    }
}

#[cfg(test)]
pub(crate) const TEST_RUN_ID: &str = "018f9d2e-7c3a-7000-8000-000000000271";
#[cfg(test)]
pub(crate) const TEST_SESSION_ID: &str = "018f9d2e-7c3a-7000-8000-0000000000aa";

#[cfg(test)]
impl RunCheckpoint {
    /// Minimal fixture matching `testdata/checkpoint-v1.json`.
    pub(crate) fn test_minimal() -> Self {
        RunCheckpoint {
            run_id: TEST_RUN_ID.parse().expect("valid uuid"),
            session_id: SessionId::parse(TEST_SESSION_ID).expect("valid uuid"),
            chat_session_id: Some(ChatSessionId::new("cs_golden")),
            config_fingerprint: ConfigFingerprint::new("cfg-digest"),
            original_query: "original query".to_string(),
            external_history: vec![],
            coordinator_conversation: vec![],
            plan: Plan {
                goal: "golden goal".to_string(),
                steps: None,
                tasks: vec![],
            },
            blocked: vec![],
            approvals: vec![],
            wake_reasons: vec![],
            resume_point: ResumePoint::WaveBoundary { iteration: 1 },
            identity_headers: vec![IdentityHeader {
                name: "x-user-id".to_string(),
                value: "user-1".to_string(),
            }],
            pod_local_refs: vec![],
            iteration: 1,
            timings: IterationTimings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_V1: &str = include_str!("testdata/checkpoint-v1.json");

    #[test]
    fn checkpoint_envelope_round_trips() {
        let envelope = CheckpointEnvelope::new(RunCheckpoint::test_minimal());
        let encoded = envelope.to_json().expect("encodes");
        let decoded = CheckpointEnvelope::from_json(&encoded).expect("decodes");
        assert_eq!(decoded.schema_version, CHECKPOINT_SCHEMA_VERSION);
        let recoded = decoded.to_json().expect("re-encodes");
        let a: serde_json::Value = serde_json::from_str(&encoded).expect("json");
        let b: serde_json::Value = serde_json::from_str(&recoded).expect("json");
        assert_eq!(a, b);
    }

    #[test]
    fn golden_v1_fixture_parses() {
        let envelope = CheckpointEnvelope::from_json(GOLDEN_V1).expect("v1 stays readable");
        assert_eq!(envelope.schema_version, 1);
        assert_eq!(envelope.checkpoint.run_id.to_string(), TEST_RUN_ID);
        assert_eq!(envelope.checkpoint.session_id.to_string(), TEST_SESSION_ID);
        assert_eq!(envelope.checkpoint.original_query, "original query");
        assert_eq!(envelope.checkpoint.plan.goal, "golden goal");
        assert_eq!(
            envelope.checkpoint.resume_point,
            ResumePoint::WaveBoundary { iteration: 1 }
        );
    }

    #[test]
    fn unknown_future_schema_version_rejected_before_body_decode() {
        // The body is deliberately not a valid checkpoint of any known
        // shape: the version gate must fire before body decoding, so the
        // error is UnknownSchemaVersion, never a serde failure.
        let future = r#"{"schema_version": 999, "checkpoint": {"shape": "from the future"}}"#;
        assert!(matches!(
            CheckpointEnvelope::from_json(future),
            Err(CheckpointCodecError::UnknownSchemaVersion {
                found: 999,
                supported: CHECKPOINT_SCHEMA_VERSION,
            })
        ));
    }
}
