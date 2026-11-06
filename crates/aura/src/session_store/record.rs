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
use crate::orchestration::{PersistenceRunId, TaskIdentity};

/// Round-trippable storage form of a [`ParkedApproval`]. Field and tag names
/// are a persisted contract shared by every instance reading the store — rename
/// only with a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedApprovalRecord {
    pub version: u32,
    #[serde(default)]
    pub instance_id: String,
    pub decision_id: DecisionId,
    pub request_id: String,
    pub scope: ScopeRecord,
    pub origin: OriginRecord,
    pub items: Vec<ApprovalItem>,
    pub registered_at: Timestamp,
    pub expires_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polling_target: Option<PollingTargetRecord>,
}

/// Storage form of a polling target and its original invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollingTargetRecord {
    pub server_id: String,
    pub namespace: String,
    pub tool_name: String,
    pub invocation_arguments: serde_json::Value,
}

impl From<&crate::hitl::PollingTarget> for PollingTargetRecord {
    fn from(target: &crate::hitl::PollingTarget) -> Self {
        Self {
            server_id: target.server_id.clone(),
            namespace: target.namespace.clone(),
            tool_name: target.tool_name.clone(),
            invocation_arguments: target.invocation_arguments.clone(),
        }
    }
}

impl From<PollingTargetRecord> for crate::hitl::PollingTarget {
    fn from(record: PollingTargetRecord) -> Self {
        Self {
            server_id: record.server_id,
            namespace: record.namespace,
            tool_name: record.tool_name,
            invocation_arguments: record.invocation_arguments,
        }
    }
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
            instance_id: request.instance_id.clone(),
            decision_id: request.decision_id,
            request_id: request.request_id.clone(),
            scope: ScopeRecord::from(&request.scope),
            origin: OriginRecord::from(&request.origin),
            items: request.items.clone(),
            registered_at: parked.registered_at,
            expires_at: parked.expires_at,
            polling_target: parked.polling_target.as_ref().map(Into::into),
        }
    }
}

impl TryFrom<ParkedApprovalRecord> for ParkedApproval {
    type Error = InvalidRecord;

    fn try_from(record: ParkedApprovalRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            request: ApprovalRequest {
                version: record.version,
                instance_id: record.instance_id,
                decision_id: record.decision_id,
                request_id: record.request_id,
                scope: record.scope.try_into()?,
                origin: record.origin.into(),
                items: record.items,
            },
            registered_at: record.registered_at,
            expires_at: record.expires_at,
            polling_target: record.polling_target.map(Into::into),
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
                session_id: session_id.and_then(SessionId::non_empty),
            },
            ScopeRecord::Worker {
                run_id,
                task_id,
                worker,
                session_id,
            } => AgentScope::Worker {
                run_id: parse_run_id(&run_id)?,
                task: TaskIdentity::new(task_id, worker),
                session_id: session_id.and_then(SessionId::non_empty),
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

fn parse_run_id(raw: &str) -> Result<PersistenceRunId, InvalidRecord> {
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
            polling_target: None,
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "test-instance".to_string(),
                decision_id: DecisionId::generate(),
                request_id: "req-1".to_string(),
                scope,
                origin,
                items: vec![ApprovalItem {
                    tool_name: "test_tool".to_string(),
                    tool_namespace: None,
                    arguments: serde_json::json!({"arg": 1}),
                    tool_call_intent: None,
                    polling: None,
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
                session_id: Some(SessionId::new("sess-9").unwrap()),
            },
            ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
        ));
    }

    #[test]
    fn polling_target_and_raw_invocation_survive_storage() {
        let mut approval = parked(
            AgentScope::Single { session_id: None },
            ApprovalOrigin::ConfigGate {
                matched_pattern: "*".to_owned(),
                agent_name: "worker".to_owned(),
            },
        );
        approval.polling_target = Some(crate::hitl::PollingTarget {
            server_id: "server-id".to_owned(),
            namespace: "test".to_owned(),
            tool_name: "query".to_owned(),
            invocation_arguments: serde_json::json!({"probe": {"tool": "query", "args": {}}}),
        });
        assert_round_trip(approval);
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

    /// A stored scope with an empty session id loads with no session instead
    /// of failing to decode.
    #[test]
    fn a_blank_session_id_in_a_stored_scope_restores_as_none() {
        let single = ScopeRecord::Single {
            session_id: Some(String::new()),
        };
        assert_eq!(
            AgentScope::try_from(single).unwrap(),
            AgentScope::Single { session_id: None }
        );

        let worker = ScopeRecord::Worker {
            run_id: "0191e8c0-1111-7000-8000-000000000000".to_string(),
            task_id: 1,
            worker: None,
            session_id: Some(String::new()),
        };
        let AgentScope::Worker { session_id, .. } = AgentScope::try_from(worker).unwrap() else {
            panic!("a worker scope restores as one");
        };
        assert_eq!(session_id, None);
    }

    #[test]
    fn malformed_run_id_is_an_invalid_record() {
        let scope = ScopeRecord::Coordinator {
            run_id: "not-a-uuid".to_string(),
        };
        let err = AgentScope::try_from(scope).unwrap_err();
        assert!(err.reason.contains("not-a-uuid"));
    }

    #[test]
    fn polling_target_projection_preserves_json_and_round_trips_scope() {
        let mut approval = parked(
            AgentScope::Single { session_id: None },
            ApprovalOrigin::ConfigGate {
                matched_pattern: "*".to_owned(),
                agent_name: "test".to_owned(),
            },
        );
        let target_json = serde_json::json!({
            "server_id": "server", "namespace": "test", "tool_name": "probe_tool",
            "invocation_arguments": {"probe": {"tool": "probe_tool", "args": {}}, "until": {"matches": "ok"}}
        });
        let record: PollingTargetRecord = serde_json::from_value(target_json.clone()).unwrap();
        approval.polling_target = Some(record.into());
        approval.request.items[0].polling = Some(crate::hitl::PollingScope {
            every_secs: 2,
            for_up_to_secs: 120,
        });
        let json = serde_json::to_value(ParkedApprovalRecord::from(&approval)).unwrap();
        assert_eq!(json["polling_target"], target_json);
        let decoded: ParkedApprovalRecord = serde_json::from_value(json).unwrap();
        let restored = ParkedApproval::try_from(decoded).unwrap();
        assert_eq!(restored.polling_target, approval.polling_target);
        assert_eq!(restored.request.items, approval.request.items);

        let mut legacy = serde_json::to_value(ParkedApprovalRecord::from(&approval)).unwrap();
        legacy.as_object_mut().unwrap().remove("polling_target");
        legacy["items"][0]
            .as_object_mut()
            .unwrap()
            .remove("polling");
        let decoded: ParkedApprovalRecord = serde_json::from_value(legacy).unwrap();
        let restored = ParkedApproval::try_from(decoded).unwrap();
        assert!(restored.polling_target.is_none());
        assert!(restored.request.items[0].polling.is_none());
    }
}
