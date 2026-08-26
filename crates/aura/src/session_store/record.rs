//! The storage projection of a parked approval: the single conversion boundary
//! between the HITL domain and the record a networked [`ApprovalStore`]
//! persists.
//!
//! The domain types ([`ApprovalRequest`], [`AgentScope`], [`ApprovalOrigin`])
//! are deliberately unserializable so no wire can leak Rust variant names (see
//! `hitl::protocol`); each wire owns its own stable projection. `hitl::events`
//! is that boundary for the SSE/webhook DTOs; this module is the storage
//! counterpart, and the only one that also converts *back* — a stored record
//! must round-trip so any instance can restore the approval it did not park.
//!
//! [`ApprovalStore`]: super::ApprovalStore

use serde::{Deserialize, Serialize};

use crate::config::SessionId;
use crate::hitl::{
    AgentScope, ApprovalDecision, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId,
    ParkedApproval, Timestamp,
};
use crate::orchestration::park::{self, SessionRecord, WakeReason};
use crate::orchestration::{RunId, TaskIdentity};

use super::SessionStoreError;

/// Round-trippable storage form of a [`ParkedApproval`]. Field and tag names
/// are a persisted contract shared by every instance reading the store — rename
/// only with a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedApprovalRecord {
    pub version: u32,
    pub decision_id: DecisionId,
    pub request_id: String,
    pub scope: ScopeRecord,
    pub origin: OriginRecord,
    pub items: Vec<ApprovalItem>,
    pub registered_at: Timestamp,
    pub expires_at: Timestamp,
}

/// Storage form of [`AgentScope`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScopeRecord {
    Single {
        session_id: Option<String>,
    },
    Worker {
        run_id: String,
        task_id: usize,
        worker: Option<String>,
        session_id: Option<String>,
    },
    Coordinator {
        run_id: String,
    },
}

/// Storage form of [`ApprovalOrigin`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OriginRecord {
    ConfigGate {
        matched_pattern: String,
        #[serde(default)]
        agent_name: String,
    },
    AgentRequested {
        reason: String,
        #[serde(default)]
        agent_name: String,
    },
}

/// Storage form of a recorded [`ApprovalDecision`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub approved: bool,
    pub reason: Option<String>,
    pub decided_at: Timestamp,
}

/// Stamps `decided_at` with the resolve time.
impl From<&ApprovalDecision> for DecisionRecord {
    fn from(decision: &ApprovalDecision) -> Self {
        let (approved, reason) = match decision {
            ApprovalDecision::Approved => (true, None),
            ApprovalDecision::Denied { reason } => (false, reason.clone()),
        };
        Self {
            approved,
            reason,
            decided_at: chrono::Utc::now(),
        }
    }
}

impl From<DecisionRecord> for ApprovalDecision {
    fn from(record: DecisionRecord) -> Self {
        if record.approved {
            ApprovalDecision::Approved
        } else {
            ApprovalDecision::Denied {
                reason: record.reason,
            }
        }
    }
}

/// Version every decided record is written at today. A breaking shape change
/// bumps this and adds a decoder for the new version; decoders for old
/// versions are kept for as long as records of that version can exist.
pub const DECIDED_RECORD_VERSION: u32 = 1;

/// Round-trippable storage form of a resolved approval: the decision and the
/// wake reason that carries it back to a parked run, in one record. Field and
/// tag names are a persisted contract shared by every instance reading the
/// store — rename only with a migration. The version tag is inlined in the
/// record object — no wrapper envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecidedRecord {
    pub version: u32,
    pub decision: DecisionRecord,
    pub wake: WakeReason,
}

impl DecidedRecord {
    /// Stamp both halves from one instant, so the decision and its wake
    /// reason cannot disagree about when the approval was resolved.
    #[must_use]
    pub fn new(decision_id: &DecisionId, decision: &ApprovalDecision) -> Self {
        let decision = DecisionRecord::from(decision);
        let wake = WakeReason::DecisionResolved {
            decision_id: *decision_id,
            resolved_at: decision.decided_at,
        };
        Self {
            version: DECIDED_RECORD_VERSION,
            decision,
            wake,
        }
    }
}

/// Encode a decided record for storage at [`DECIDED_RECORD_VERSION`].
#[must_use]
pub fn encode_decided_record(record: &DecidedRecord) -> Vec<u8> {
    serde_json::to_vec(record).expect("decided record serializes to JSON")
}

/// Decode a stored decided record, dispatching on its inline version tag
/// before any body field is interpreted; an unknown version is refused, never
/// guessed at or defaulted.
pub fn decode_decided_record(raw: &[u8]) -> Result<DecidedRecord, SessionStoreError> {
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(decided_decode_err)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SessionStoreError::Decode {
            reason: "decided record: missing or non-numeric version".to_string(),
        })?;
    if version != u64::from(DECIDED_RECORD_VERSION) {
        return Err(SessionStoreError::Decode {
            reason: format!(
                "decided record version {version} has no decoder in this binary \
                 (latest known: {DECIDED_RECORD_VERSION})"
            ),
        });
    }
    serde_json::from_value(value).map_err(decided_decode_err)
}

fn decided_decode_err(err: serde_json::Error) -> SessionStoreError {
    SessionStoreError::Decode {
        reason: format!("decided record: {err}"),
    }
}

/// A stored approval record whose contents cannot be restored to the domain.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid stored approval record: {reason}")]
pub struct InvalidRecord {
    pub reason: String,
}

/// Version every run record is written at today. A breaking shape change
/// bumps this and adds a decoder for the new version; decoders for old
/// versions are kept for as long as records of that version can exist.
pub const RUN_RECORD_VERSION: u32 = 1;

/// A stored run record this binary refuses to read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunRecordError {
    /// Fail-closed refusal of a version with no decoder here: a reader
    /// never guesses at a shape it does not know.
    #[error("run record version {found} has no decoder in this binary (latest known: {supported})")]
    UnknownVersion { found: u32, supported: u32 },
    /// The version was known but the body did not decode.
    #[error("run record failed to decode: {reason}")]
    Malformed { reason: String },
}

/// Frozen v1 wire shape of a stored session-run record, owned by the
/// decoder and never public: [`decode_run_record`] is the only read path,
/// so nothing can deserialize around the version gate. The version tag is
/// inlined in the record object — no wrapper envelope.
///
/// Every nested shape is a version-owned copy of today's serialized form,
/// not the domain type, so a later serde change to a domain type cannot
/// silently rewrite v1: `v1_wire_matches_domain_serialization` pins the
/// copies to the domain derives field-for-field. Frozen means a breaking
/// change lands as a `RunRecordV2` with its own decoder (upcasting into the
/// current domain form), never as an edit here. Two boundaries pass
/// through deliberately:
///
/// - timestamps are `chrono::DateTime<Utc>`, whose RFC 3339 serde form is
///   chrono's own stable contract;
/// - the checkpoint blob is an opaque `serde_json::Value`, because it is
///   already an independently frozen boundary — its own inline
///   `schema_version`, its own fail-closed version-gated codec, and its own
///   golden fixture (`park/testdata/checkpoint-v1.json`).
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct RunRecordV1 {
    version: u32,
    session: SessionV1Wire,
    run_id: Option<String>,
    state: RunStateV1Wire,
    lease: Option<LeaseV1Wire>,
    generation: u64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct SessionV1Wire {
    id: String,
    chat_session_id: Option<String>,
    created_at: Timestamp,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum RunStateV1Wire {
    Created,
    Running,
    Parked {
        reason: ParkReasonV1Wire,
        parked_at: Timestamp,
        expires_at: Timestamp,
        checkpoint: serde_json::Value,
    },
    Completed,
    Failed {
        cause: RunFailureCauseV1Wire,
    },
    Cancelled,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ParkReasonV1Wire {
    ApprovalsBlocked { decisions: Vec<String> },
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
enum RunFailureCauseV1Wire {
    ParkExpired { summary: String },
    ExecutionFailed { summary: String },
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct LeaseV1Wire {
    holder: String,
    acquired_at: Timestamp,
    heartbeat_at: Timestamp,
    expires_at: Timestamp,
    generation: u64,
}

impl From<&SessionRecord> for RunRecordV1 {
    fn from(record: &SessionRecord) -> Self {
        Self {
            version: RUN_RECORD_VERSION,
            session: SessionV1Wire {
                id: record.session.id.to_string(),
                chat_session_id: record
                    .session
                    .chat_session_id
                    .as_ref()
                    .map(|id| id.as_str().to_string()),
                created_at: record.session.created_at,
            },
            run_id: record.run_id.map(|id| id.to_string()),
            state: RunStateV1Wire::from(&record.state),
            lease: record.lease.as_ref().map(LeaseV1Wire::from),
            generation: record.generation.into(),
        }
    }
}

impl From<&park::RunState> for RunStateV1Wire {
    fn from(state: &park::RunState) -> Self {
        match state {
            park::RunState::Created => RunStateV1Wire::Created,
            park::RunState::Running => RunStateV1Wire::Running,
            park::RunState::Parked {
                reason,
                parked_at,
                expires_at,
                checkpoint,
            } => RunStateV1Wire::Parked {
                reason: reason.into(),
                parked_at: *parked_at,
                expires_at: *expires_at,
                checkpoint: serde_json::to_value(checkpoint.as_ref())
                    .expect("a checkpoint envelope serializes to JSON"),
            },
            park::RunState::Completed => RunStateV1Wire::Completed,
            park::RunState::Failed { cause } => RunStateV1Wire::Failed {
                cause: cause.into(),
            },
            park::RunState::Cancelled => RunStateV1Wire::Cancelled,
        }
    }
}

impl From<&park::ParkReason> for ParkReasonV1Wire {
    fn from(reason: &park::ParkReason) -> Self {
        match reason {
            park::ParkReason::ApprovalsBlocked { decisions } => {
                ParkReasonV1Wire::ApprovalsBlocked {
                    decisions: decisions.iter().map(|id| id.to_string()).collect(),
                }
            }
        }
    }
}

impl From<&park::RunFailureCause> for RunFailureCauseV1Wire {
    fn from(cause: &park::RunFailureCause) -> Self {
        match cause {
            park::RunFailureCause::ParkExpired { summary } => RunFailureCauseV1Wire::ParkExpired {
                summary: summary.clone(),
            },
            park::RunFailureCause::ExecutionFailed { summary } => {
                RunFailureCauseV1Wire::ExecutionFailed {
                    summary: summary.clone(),
                }
            }
        }
    }
}

impl From<&park::Lease> for LeaseV1Wire {
    fn from(lease: &park::Lease) -> Self {
        Self {
            holder: lease.holder.to_string(),
            acquired_at: lease.acquired_at,
            heartbeat_at: lease.heartbeat_at,
            expires_at: lease.expires_at,
            generation: lease.generation.into(),
        }
    }
}

fn v1_malformed(field: &str, err: impl std::fmt::Display) -> RunRecordError {
    RunRecordError::Malformed {
        reason: format!("{field}: {err}"),
    }
}

impl TryFrom<RunRecordV1> for SessionRecord {
    type Error = RunRecordError;

    fn try_from(wire: RunRecordV1) -> Result<Self, Self::Error> {
        Ok(Self {
            session: park::Session {
                id: park::SessionId::parse(&wire.session.id)
                    .map_err(|e| v1_malformed("session.id", e))?,
                chat_session_id: wire.session.chat_session_id.map(park::ChatSessionId::new),
                created_at: wire.session.created_at,
            },
            run_id: wire
                .run_id
                .map(|raw| raw.parse::<RunId>().map_err(|e| v1_malformed("run_id", e)))
                .transpose()?,
            state: wire.state.try_into()?,
            lease: wire.lease.map(park::Lease::try_from).transpose()?,
            generation: wire.generation.into(),
        })
    }
}

impl TryFrom<RunStateV1Wire> for park::RunState {
    type Error = RunRecordError;

    fn try_from(wire: RunStateV1Wire) -> Result<Self, Self::Error> {
        Ok(match wire {
            RunStateV1Wire::Created => park::RunState::Created,
            RunStateV1Wire::Running => park::RunState::Running,
            RunStateV1Wire::Parked {
                reason,
                parked_at,
                expires_at,
                checkpoint,
            } => park::RunState::Parked {
                reason: reason.try_into()?,
                parked_at,
                expires_at,
                // Routed through the checkpoint's own version-gated codec so
                // its refusal discipline applies here too.
                checkpoint: Box::new(
                    park::CheckpointEnvelope::from_json(&checkpoint.to_string())
                        .map_err(|e| v1_malformed("state.checkpoint", format!("{e:?}")))?,
                ),
            },
            RunStateV1Wire::Completed => park::RunState::Completed,
            RunStateV1Wire::Failed { cause } => park::RunState::Failed {
                cause: cause.into(),
            },
            RunStateV1Wire::Cancelled => park::RunState::Cancelled,
        })
    }
}

impl TryFrom<ParkReasonV1Wire> for park::ParkReason {
    type Error = RunRecordError;

    fn try_from(wire: ParkReasonV1Wire) -> Result<Self, Self::Error> {
        match wire {
            ParkReasonV1Wire::ApprovalsBlocked { decisions } => {
                let decisions = decisions
                    .iter()
                    .map(|raw| DecisionId::parse(raw))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| v1_malformed("state.reason.decisions", e))?;
                Ok(park::ParkReason::ApprovalsBlocked {
                    decisions: park::NonEmpty::new(decisions)
                        .map_err(|e| v1_malformed("state.reason.decisions", e))?,
                })
            }
        }
    }
}

impl From<RunFailureCauseV1Wire> for park::RunFailureCause {
    fn from(wire: RunFailureCauseV1Wire) -> Self {
        match wire {
            RunFailureCauseV1Wire::ParkExpired { summary } => {
                park::RunFailureCause::ParkExpired { summary }
            }
            RunFailureCauseV1Wire::ExecutionFailed { summary } => {
                park::RunFailureCause::ExecutionFailed { summary }
            }
        }
    }
}

impl TryFrom<LeaseV1Wire> for park::Lease {
    type Error = RunRecordError;

    fn try_from(wire: LeaseV1Wire) -> Result<Self, Self::Error> {
        Ok(Self {
            holder: park::AgentInstanceId::parse(&wire.holder)
                .map_err(|e| v1_malformed("lease.holder", e))?,
            acquired_at: wire.acquired_at,
            heartbeat_at: wire.heartbeat_at,
            expires_at: wire.expires_at,
            generation: wire.generation.into(),
        })
    }
}

/// Encode a record for storage at [`RUN_RECORD_VERSION`].
pub fn encode_run_record(record: &SessionRecord) -> Result<String, RunRecordError> {
    serde_json::to_string(&RunRecordV1::from(record)).map_err(|e| RunRecordError::Malformed {
        reason: e.to_string(),
    })
}

/// Decode a stored record, dispatching on its inline version tag before any
/// body field is interpreted; an unknown version is refused, never guessed
/// at. One decoder per version, each upcasting into the current
/// [`SessionRecord`].
pub fn decode_run_record(raw: &str) -> Result<SessionRecord, RunRecordError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| RunRecordError::Malformed {
            reason: e.to_string(),
        })?;
    let version = value
        .get("version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| RunRecordError::Malformed {
            reason: "missing or non-numeric version".to_string(),
        })?;
    let version = u32::try_from(version).map_err(|_| RunRecordError::Malformed {
        reason: "version out of u32 range".to_string(),
    })?;

    match version {
        RUN_RECORD_VERSION => {
            let wire: RunRecordV1 =
                serde_json::from_value(value).map_err(|e| RunRecordError::Malformed {
                    reason: e.to_string(),
                })?;
            SessionRecord::try_from(wire)
        }
        other => Err(RunRecordError::UnknownVersion {
            found: other,
            supported: RUN_RECORD_VERSION,
        }),
    }
}

impl From<&ParkedApproval> for ParkedApprovalRecord {
    fn from(parked: &ParkedApproval) -> Self {
        let request = &parked.request;
        Self {
            version: request.version,
            decision_id: request.decision_id,
            request_id: request.request_id.clone(),
            scope: ScopeRecord::from(&request.scope),
            origin: OriginRecord::from(&request.origin),
            items: request.items.clone(),
            registered_at: parked.registered_at,
            expires_at: parked.expires_at,
        }
    }
}

impl TryFrom<ParkedApprovalRecord> for ParkedApproval {
    type Error = InvalidRecord;

    fn try_from(record: ParkedApprovalRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            request: ApprovalRequest {
                version: record.version,
                decision_id: record.decision_id,
                request_id: record.request_id,
                scope: record.scope.try_into()?,
                origin: record.origin.into(),
                items: record.items,
            },
            registered_at: record.registered_at,
            expires_at: record.expires_at,
        })
    }
}

impl From<&AgentScope> for ScopeRecord {
    fn from(scope: &AgentScope) -> Self {
        match scope {
            AgentScope::Single { session_id } => ScopeRecord::Single {
                session_id: session_id.as_ref().map(|id| id.as_str().to_string()),
            },
            AgentScope::Worker {
                run_id,
                task,
                session_id,
            } => ScopeRecord::Worker {
                run_id: run_id.to_string(),
                task_id: task.task_id,
                worker: task.worker.clone(),
                session_id: session_id.as_ref().map(|id| id.as_str().to_string()),
            },
            AgentScope::Coordinator { run_id } => ScopeRecord::Coordinator {
                run_id: run_id.to_string(),
            },
        }
    }
}

impl TryFrom<ScopeRecord> for AgentScope {
    type Error = InvalidRecord;

    fn try_from(record: ScopeRecord) -> Result<Self, Self::Error> {
        Ok(match record {
            ScopeRecord::Single { session_id } => AgentScope::Single {
                session_id: session_id.map(SessionId::new),
            },
            ScopeRecord::Worker {
                run_id,
                task_id,
                worker,
                session_id,
            } => AgentScope::Worker {
                run_id: parse_run_id(&run_id)?,
                task: TaskIdentity::new(task_id, worker),
                session_id: session_id.map(SessionId::new),
            },
            ScopeRecord::Coordinator { run_id } => AgentScope::Coordinator {
                run_id: parse_run_id(&run_id)?,
            },
        })
    }
}

impl From<&ApprovalOrigin> for OriginRecord {
    fn from(origin: &ApprovalOrigin) -> Self {
        match origin {
            ApprovalOrigin::ConfigGate {
                matched_pattern,
                agent_name,
            } => OriginRecord::ConfigGate {
                matched_pattern: matched_pattern.clone(),
                agent_name: agent_name.clone(),
            },
            ApprovalOrigin::AgentRequested { reason, agent_name } => OriginRecord::AgentRequested {
                reason: reason.clone(),
                agent_name: agent_name.clone(),
            },
        }
    }
}

impl From<OriginRecord> for ApprovalOrigin {
    fn from(record: OriginRecord) -> Self {
        match record {
            OriginRecord::ConfigGate {
                matched_pattern,
                agent_name,
            } => ApprovalOrigin::ConfigGate {
                matched_pattern,
                agent_name,
            },
            OriginRecord::AgentRequested { reason, agent_name } => {
                ApprovalOrigin::AgentRequested { reason, agent_name }
            }
        }
    }
}

fn parse_run_id(raw: &str) -> Result<RunId, InvalidRecord> {
    raw.parse().map_err(|e| InvalidRecord {
        reason: format!("run_id '{raw}': {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hitl::PROTOCOL_VERSION;

    fn parked(scope: AgentScope, origin: ApprovalOrigin) -> ParkedApproval {
        let now = chrono::Utc::now();
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                decision_id: DecisionId::generate(),
                request_id: "req-1".to_string(),
                scope,
                origin,
                items: vec![ApprovalItem {
                    tool_name: "test_tool".to_string(),
                    arguments: serde_json::json!({"arg": 1}),
                    tool_call_intent: None,
                }],
            },
            registered_at: now,
            expires_at: now + chrono::Duration::seconds(60),
        }
    }

    /// Domain → record → JSON → record → domain → record: the final record
    /// equals the first, so every field survives storage.
    fn assert_round_trip(parked: ParkedApproval) {
        let record = ParkedApprovalRecord::from(&parked);
        let json = serde_json::to_string(&record).expect("record serializes");
        let stored: ParkedApprovalRecord = serde_json::from_str(&json).expect("record parses");
        assert_eq!(stored, record);
        let restored = ParkedApproval::try_from(stored).expect("record restores");
        assert_eq!(ParkedApprovalRecord::from(&restored), record);
    }

    #[test]
    fn single_scope_round_trips() {
        assert_round_trip(parked(
            AgentScope::Single {
                session_id: Some(SessionId::new("sess-9")),
            },
            ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
        ));
    }

    #[test]
    fn worker_scope_round_trips() {
        assert_round_trip(parked(
            AgentScope::Worker {
                run_id: "0191e8c0-1111-7000-8000-000000000000".parse().unwrap(),
                task: TaskIdentity::new(3, Some("ops".to_string())),
                session_id: None,
            },
            ApprovalOrigin::AgentRequested {
                reason: "risky".to_string(),
                agent_name: "ops-agent".to_string(),
            },
        ));
    }

    #[test]
    fn coordinator_scope_round_trips() {
        assert_round_trip(parked(
            AgentScope::Coordinator {
                run_id: "0191e8c0-1111-7000-8000-000000000000".parse().unwrap(),
            },
            ApprovalOrigin::ConfigGate {
                matched_pattern: "*".to_string(),
                agent_name: "test-agent".to_string(),
            },
        ));
    }

    #[test]
    fn scope_tags_are_stable_snake_case() {
        let record = ParkedApprovalRecord::from(&parked(
            AgentScope::Single { session_id: None },
            ApprovalOrigin::AgentRequested {
                reason: "r".to_string(),
                agent_name: "test-agent".to_string(),
            },
        ));
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["scope"]["kind"], "single");
        assert_eq!(json["origin"]["kind"], "agent_requested");
    }

    #[test]
    fn malformed_run_id_is_an_invalid_record() {
        let scope = ScopeRecord::Coordinator {
            run_id: "not-a-uuid".to_string(),
        };
        let err = AgentScope::try_from(scope).unwrap_err();
        assert!(err.reason.contains("not-a-uuid"));
    }

    /// A version tag that fits in JSON's `u64` but not in `u32` must not be
    /// silently truncated into a known version.
    #[test]
    fn u64_version_outside_u32_range_is_rejected() {
        let raw = r#"{"version":4294967297,"session":{"id":"sess-1","created_at":"2026-01-01T00:00:00Z"},"run_id":null,"state":"created","lease":null,"generation":1}"#;
        let err = decode_run_record(raw).unwrap_err();
        assert!(
            matches!(err, RunRecordError::Malformed { .. }),
            "expected malformed version, got {err:?}"
        );
    }

    /// Pins each v1 wire copy to the domain serde derives at the byte
    /// level: for every run state, serializing through the wire tree must
    /// produce a byte-identical JSON string to serializing the domain
    /// record directly (minus the wire's inline version tag), so a field
    /// reorder trips the pin too. One audited, location-anchored
    /// normalization: the checkpoint payload is compared structurally and
    /// spliced out of both strings before the byte comparison, because it
    /// crosses the wire as the declared pinned-opaque boundary (its byte
    /// shape is owned by its own codec and golden fixture, and JSON key
    /// order inside an opaque `Value` is not part of the v1 contract).
    #[test]
    fn v1_wire_matches_domain_serialization() {
        use crate::orchestration::park::{
            AgentInstanceId, ChatSessionId, CheckpointEnvelope, FencingGeneration, Lease, NonEmpty,
            ParkReason, RunCheckpoint, RunFailureCause, RunState, Session, SessionId,
            SessionRecord,
        };
        let now = chrono::Utc::now();
        let states = [
            RunState::Created,
            RunState::Running,
            RunState::Parked {
                reason: ParkReason::ApprovalsBlocked {
                    decisions: NonEmpty::new(vec![DecisionId::generate()]).unwrap(),
                },
                parked_at: now,
                expires_at: now + chrono::Duration::seconds(300),
                checkpoint: Box::new(CheckpointEnvelope::new(RunCheckpoint::test_minimal())),
            },
            RunState::Completed,
            RunState::Failed {
                cause: RunFailureCause::ParkExpired {
                    summary: "expired".to_string(),
                },
            },
            RunState::Failed {
                cause: RunFailureCause::ExecutionFailed {
                    summary: "boom".to_string(),
                },
            },
            RunState::Cancelled,
        ];
        for state in states {
            let record = SessionRecord {
                session: Session {
                    id: SessionId::generate(),
                    chat_session_id: Some(ChatSessionId::new("cs_wire")),
                    created_at: now,
                },
                run_id: Some("018f9d2e-7c3a-7000-8000-000000000271".parse().unwrap()),
                state,
                lease: Some(Lease {
                    holder: AgentInstanceId::generate(),
                    acquired_at: now,
                    heartbeat_at: now,
                    expires_at: now + chrono::Duration::seconds(60),
                    generation: FencingGeneration::INITIAL.next(),
                }),
                generation: FencingGeneration::INITIAL.next(),
            };
            let mut domain = serde_json::to_string(&record).expect("domain serializes");
            let wire_record = RunRecordV1::from(&record);
            let mut wire = serde_json::to_string(&wire_record).expect("wire serializes");

            // The wire record leads with its inline version tag; the domain
            // record has no version field. The literal pins V1's version
            // byte: bumping RUN_RECORD_VERSION alone must fail this test.
            let tag = "{\"version\":1,";
            assert!(
                wire.starts_with(tag),
                "wire must lead with the version tag: {wire}"
            );
            wire.replace_range(..tag.len(), "{");

            if let RunState::Parked { checkpoint, .. } = &record.state {
                let domain_ck =
                    serde_json::to_string(checkpoint.as_ref()).expect("checkpoint serializes");
                let RunStateV1Wire::Parked {
                    checkpoint: wire_ck_value,
                    ..
                } = &wire_record.state
                else {
                    panic!("wire state must mirror the parked domain state");
                };
                let wire_ck =
                    serde_json::to_string(wire_ck_value).expect("checkpoint value serializes");
                let domain_ck_value: serde_json::Value =
                    serde_json::from_str(&domain_ck).expect("checkpoint parses");
                assert_eq!(
                    &domain_ck_value, wire_ck_value,
                    "the opaque checkpoint payload drifted structurally",
                );
                assert_eq!(
                    domain.matches(&domain_ck).count(),
                    1,
                    "checkpoint text must appear exactly once in the domain record",
                );
                assert_eq!(
                    wire.matches(&wire_ck).count(),
                    1,
                    "checkpoint text must appear exactly once in the wire record",
                );
                domain = domain.replacen(&domain_ck, "\"<checkpoint>\"", 1);
                wire = wire.replacen(&wire_ck, "\"<checkpoint>\"", 1);
            }

            assert_eq!(wire, domain, "wire copy drifted from the domain shape");
        }
    }

    #[test]
    fn decided_record_carries_one_resolution_instant() {
        let id = DecisionId::generate();
        let record = DecidedRecord::new(&id, &ApprovalDecision::Approved);
        let WakeReason::DecisionResolved {
            decision_id,
            resolved_at,
        } = record.wake;
        assert_eq!(decision_id, id);
        assert_eq!(resolved_at, record.decision.decided_at);
    }

    #[test]
    fn decided_record_round_trips() {
        let record = DecidedRecord::new(
            &DecisionId::generate(),
            &ApprovalDecision::Denied {
                reason: Some("not safe".to_string()),
            },
        );
        let decoded = decode_decided_record(&encode_decided_record(&record)).expect("decodes");
        assert_eq!(decoded, record);
    }

    /// A version with no decoder here is refused rather than read as v1: an
    /// old node must never interpret a shape it does not know.
    #[test]
    fn unknown_decided_record_version_is_refused() {
        let mut value = serde_json::to_value(DecidedRecord::new(
            &DecisionId::generate(),
            &ApprovalDecision::Approved,
        ))
        .expect("record serializes");
        value["version"] = serde_json::json!(DECIDED_RECORD_VERSION + 1);
        let raw = serde_json::to_vec(&value).expect("value serializes");

        let err = decode_decided_record(&raw).unwrap_err();
        assert!(
            matches!(err, SessionStoreError::Decode { ref reason } if reason.contains("no decoder")),
            "expected a decode refusal, got {err:?}",
        );
    }

    #[test]
    fn decided_record_without_a_version_is_refused() {
        let err = decode_decided_record(br#"{"decision":{"approved":true}}"#).unwrap_err();
        assert!(
            matches!(err, SessionStoreError::Decode { ref reason } if reason.contains("version")),
            "expected a decode refusal, got {err:?}",
        );
    }
}
