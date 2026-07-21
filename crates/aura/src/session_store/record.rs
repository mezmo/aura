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
    AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId, ParkedApproval,
    Timestamp,
};
use crate::orchestration::{RunId, TaskIdentity};

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
    ConfigGate { matched_pattern: String },
    AgentRequested { reason: String },
}

/// A stored approval record whose contents cannot be restored to the domain.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid stored approval record: {reason}")]
pub struct InvalidRecord {
    pub reason: String,
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
            ApprovalOrigin::ConfigGate { matched_pattern } => OriginRecord::ConfigGate {
                matched_pattern: matched_pattern.clone(),
            },
            ApprovalOrigin::AgentRequested { reason } => OriginRecord::AgentRequested {
                reason: reason.clone(),
            },
        }
    }
}

impl From<OriginRecord> for ApprovalOrigin {
    fn from(record: OriginRecord) -> Self {
        match record {
            OriginRecord::ConfigGate { matched_pattern } => {
                ApprovalOrigin::ConfigGate { matched_pattern }
            }
            OriginRecord::AgentRequested { reason } => ApprovalOrigin::AgentRequested { reason },
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
            },
        ));
    }

    #[test]
    fn scope_tags_are_stable_snake_case() {
        let record = ParkedApprovalRecord::from(&parked(
            AgentScope::Single { session_id: None },
            ApprovalOrigin::AgentRequested {
                reason: "r".to_string(),
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
}
