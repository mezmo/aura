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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::SessionId;
use crate::hitl::{
    AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId, ParkedApproval,
    ResolvedDecision, Timestamp,
};
use crate::orchestration::{RunId, TaskIdentity};

/// Round-trippable storage form of a [`ParkedApproval`]. Field and tag names
/// are a persisted contract shared by every instance reading the store — rename
/// only with a migration.
///
/// Manual `Debug`: `egress_headers` values are credentials, so the rendered
/// form carries names only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Resolved egress headers the parked row's notify POST authenticates
    /// with (lowercased name → value). Additive: absent on rows stored before
    /// poll-delivery egress capture existed, decoding to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_headers: Option<BTreeMap<String, String>>,
}

/// The names of an optional pair map, for names-only Debug rendering.
fn pair_names(pairs: &Option<BTreeMap<String, String>>) -> Option<Vec<&String>> {
    pairs.as_ref().map(BTreeMap::keys).map(Iterator::collect)
}

impl std::fmt::Debug for ParkedApprovalRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkedApprovalRecord")
            .field("version", &self.version)
            .field("instance_id", &self.instance_id)
            .field("decision_id", &self.decision_id)
            .field("request_id", &self.request_id)
            .field("scope", &self.scope)
            .field("origin", &self.origin)
            .field("items", &self.items)
            .field("registered_at", &self.registered_at)
            .field("expires_at", &self.expires_at)
            .field("egress_header_names", &pair_names(&self.egress_headers))
            .finish()
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

/// Storage form of a recorded [`ResolvedDecision`]: the decision fields plus
/// the approver identity captured at resolve time.
///
/// Manual `Debug`: identity values are credentials, so the rendered form
/// carries names only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub approved: bool,
    pub reason: Option<String>,
    pub decided_at: Timestamp,
    /// Approver identity captured off the poll-200 (lowercased outbound
    /// header name → value), persisted in the same record as the decision.
    /// Additive: absent on records stored before identity docking existed,
    /// decoding to `None` (uncaptured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<BTreeMap<String, String>>,
}

impl std::fmt::Debug for DecisionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionRecord")
            .field("approved", &self.approved)
            .field("reason", &self.reason)
            .field("decided_at", &self.decided_at)
            .field("identity_names", &pair_names(&self.identity))
            .finish()
    }
}

/// Stamps `decided_at` with the resolve time.
impl From<&ResolvedDecision> for DecisionRecord {
    fn from(resolved: &ResolvedDecision) -> Self {
        let (approved, reason, identity) = match resolved {
            ResolvedDecision::Approved { identity } => (
                true,
                None,
                identity
                    .as_ref()
                    .map(crate::approver_headers::ApproverHeaders::to_pair_map),
            ),
            ResolvedDecision::Denied { reason } => (false, reason.clone(), None),
        };
        Self {
            approved,
            reason,
            identity,
            decided_at: chrono::Utc::now(),
        }
    }
}

impl TryFrom<DecisionRecord> for ResolvedDecision {
    type Error = InvalidRecord;

    fn try_from(record: DecisionRecord) -> Result<Self, Self::Error> {
        if record.approved {
            let identity = match record.identity {
                Some(pairs) => Some(
                    crate::approver_headers::ApproverHeaders::from_pairs(pairs).map_err(
                        |reason| InvalidRecord {
                            reason: format!("stored approver identity: {reason}"),
                        },
                    )?,
                ),
                None => None,
            };
            Ok(ResolvedDecision::approved(identity))
        } else {
            // A denial never captures identity; a stored one would be a
            // record no production path wrote.
            Ok(ResolvedDecision::Denied {
                reason: record.reason,
            })
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
            egress_headers: parked
                .egress_headers
                .as_ref()
                .map(crate::webhook_utils::header_map_to_pairs),
        }
    }
}

impl TryFrom<ParkedApprovalRecord> for ParkedApproval {
    type Error = InvalidRecord;

    fn try_from(record: ParkedApprovalRecord) -> Result<Self, Self::Error> {
        let egress_headers = match record.egress_headers {
            Some(pairs) => Some(crate::webhook_utils::pairs_to_header_map(pairs).map_err(
                |reason| InvalidRecord {
                    reason: format!("stored egress headers: {reason}"),
                },
            )?),
            None => None,
        };
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
            egress_headers,
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
                instance_id: "test-instance".to_string(),
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
            egress_headers: None,
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

    fn pairs(values: &[(&str, &str)]) -> BTreeMap<String, String> {
        values
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Egress headers ride the parked record: new JSON decodes to the map,
    /// restores to the domain, and round-trips.
    #[test]
    fn egress_headers_round_trip() {
        let mut parked = parked(
            AgentScope::Single { session_id: None },
            ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
        );
        parked.egress_headers = Some(
            crate::webhook_utils::pairs_to_header_map(pairs(&[(
                "authorization",
                "Bearer from-request",
            )]))
            .unwrap(),
        );

        let record = ParkedApprovalRecord::from(&parked);
        assert_eq!(
            record.egress_headers.as_ref().unwrap(),
            &pairs(&[("authorization", "Bearer from-request")]),
        );

        let json = serde_json::to_string(&record).unwrap();
        let decoded: ParkedApprovalRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, record);
        let restored = ParkedApproval::try_from(decoded).unwrap();
        assert_eq!(
            ParkedApprovalRecord::from(&restored),
            ParkedApprovalRecord::from(&parked),
        );
    }

    /// A row stored before egress capture existed (no `egress_headers` key)
    /// decodes with `None`: absence means no captured values, never a decode
    /// failure.
    #[test]
    fn legacy_row_without_egress_headers_is_readable() {
        let legacy_json = r#"{
            "version": 1,
            "instance_id": "test-instance",
            "decision_id": "0191e8c0-1111-7000-8000-000000000001",
            "request_id": "req-legacy",
            "scope": { "kind": "single", "session_id": null },
            "origin": { "kind": "config_gate", "matched_pattern": "kubectl_*", "agent_name": "t" },
            "items": [],
            "registered_at": "2026-08-01T00:00:00Z",
            "expires_at": "2026-08-01T01:00:00Z"
        }"#;

        let record: ParkedApprovalRecord = serde_json::from_str(legacy_json).unwrap();
        assert!(record.egress_headers.is_none());
        let restored = ParkedApproval::try_from(record).unwrap();
        assert!(restored.egress_headers.is_none());
        // The old shape is also exactly what a legacy domain row serializes
        // to (absent, not null).
        let legacy_domain = ParkedApprovalRecord::from(&parked(
            AgentScope::Single { session_id: None },
            ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "t".to_string(),
            },
        ));
        assert!(
            !serde_json::to_value(&legacy_domain)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("egress_headers"),
            "a row with no egress headers must serialize without the key"
        );
    }

    /// The record's Debug is a names-only surface: egress values are
    /// credentials and never render.
    #[test]
    fn parked_record_debug_prints_names_not_values() {
        let mut parked = parked(
            AgentScope::Single { session_id: None },
            ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
                agent_name: "test-agent".to_string(),
            },
        );
        parked.egress_headers = Some(
            crate::webhook_utils::pairs_to_header_map(pairs(&[(
                "authorization",
                "Bearer sentinel-egress",
            )]))
            .unwrap(),
        );
        let rendered = format!("{:?}", ParkedApprovalRecord::from(&parked));
        assert!(
            rendered.contains("authorization"),
            "names render for audit: {rendered}"
        );
        assert!(
            !rendered.contains("sentinel-egress"),
            "values must never render: {rendered}"
        );
    }

    /// The decision record carries identity in the SAME record; the identity
    /// round-trips through JSON into the carrier.
    #[test]
    fn decision_record_round_trips_identity() {
        let identity = crate::approver_headers::ApproverHeaders::from_pairs(pairs(&[(
            "x-forwarded-user",
            "approver-alice",
        )]))
        .unwrap();
        let resolved = ResolvedDecision::approved(Some(identity));

        let record = DecisionRecord::from(&resolved);
        assert_eq!(
            record.identity.as_ref().unwrap(),
            &pairs(&[("x-forwarded-user", "approver-alice")]),
        );
        let json = serde_json::to_string(&record).unwrap();
        let decoded: DecisionRecord = serde_json::from_str(&json).unwrap();
        let restored = ResolvedDecision::try_from(decoded).unwrap();
        assert_eq!(restored, resolved, "identity and decision survive storage");
    }

    /// A decision record stored before identity docking existed (no
    /// `identity` key) decodes as an uncaptured approval, and a denial never
    /// carries identity in either direction.
    #[test]
    fn legacy_decision_record_and_denials_are_uncaptured() {
        let legacy_json =
            r#"{"approved": true, "reason": null, "decided_at": "2026-08-01T00:00:00Z"}"#;
        let record: DecisionRecord = serde_json::from_str(legacy_json).unwrap();
        let restored = ResolvedDecision::try_from(record).unwrap();
        assert_eq!(
            restored,
            ResolvedDecision::approved(None),
            "absence means uncaptured"
        );

        let denied = ResolvedDecision::Denied {
            reason: Some("no".to_string()),
        };
        let record = DecisionRecord::from(&denied);
        assert!(
            record.identity.is_none(),
            "a denial never captures identity"
        );
        assert_eq!(
            ResolvedDecision::try_from(record).unwrap(),
            denied,
            "a denial round-trips without an identity half",
        );
    }

    /// The decision record's Debug is a names-only surface.
    #[test]
    fn decision_record_debug_prints_names_not_values() {
        let identity = crate::approver_headers::ApproverHeaders::from_pairs(pairs(&[(
            "x-forwarded-user",
            "approver-alice",
        )]))
        .unwrap();
        let record = DecisionRecord::from(&ResolvedDecision::approved(Some(identity)));
        let rendered = format!("{record:?}");
        assert!(
            rendered.contains("x-forwarded-user"),
            "names render: {rendered}"
        );
        assert!(
            !rendered.contains("approver-alice"),
            "identity values must never render: {rendered}"
        );
    }

    /// The carrier's Debug is a names-only surface.
    #[test]
    fn resolved_decision_debug_prints_names_not_values() {
        let identity = crate::approver_headers::ApproverHeaders::from_pairs(pairs(&[(
            "x-forwarded-user",
            "approver-carol",
        )]))
        .unwrap();
        let rendered = format!("{:?}", ResolvedDecision::approved(Some(identity)));
        assert!(
            rendered.contains("x-forwarded-user"),
            "names render: {rendered}"
        );
        assert!(
            !rendered.contains("approver-carol"),
            "identity values must never render: {rendered}"
        );
    }

    /// A stored pair whose value fails `HeaderValue` validation decodes
    /// fail-loud rather than as a partial map.
    #[test]
    fn corrupt_pair_is_rejected_fail_loud() {
        let err =
            crate::webhook_utils::pairs_to_header_map(pairs(&[("authorization", "sentinel\nbad")]))
                .expect_err("a control character in a header value is corrupt");
        assert!(
            err.to_string().contains("is not a valid header value"),
            "error names the corrupt value: {err}"
        );
    }
}
