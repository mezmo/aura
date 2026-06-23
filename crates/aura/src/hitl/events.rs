//! The single conversion boundary between the HITL domain and the `aura-events`
//! SSE DTO layer. The only file in this module that imports both worlds: domain
//! types from `super::*` and wire DTOs from `aura_events`.
//!
//! The domain never imports its core types from `aura-events`; conversions only
//! flow one way, domain -> wire, and only here.

use std::time::Duration;

use aura_events::{
    AgentScopeWire, ApprovalCompleted, ApprovalOriginWire, ApprovalOutcomeWire, ApprovalPending,
    ApprovalRequested, CancelReasonWire,
};

use super::decision::{
    AgentScope, ApprovalDecision, ApprovalOrigin, ApprovalOutcome, CancelReason, DecisionId,
};
use super::protocol::{ApprovalRequest, ApprovalRequestWire};
use super::registry::ParkedApproval;

/// `approval_requested`: emitted for both routes when an approval is raised.
impl From<&ApprovalRequest> for ApprovalRequested {
    fn from(request: &ApprovalRequest) -> Self {
        Self {
            decision_id: request.decision_id.to_string(),
            tool_name: request
                .items
                .first()
                .map(|item| item.tool_name.clone())
                .unwrap_or_default(),
            origin: origin_to_wire(&request.origin),
            scope: scope_to_wire(&request.scope),
        }
    }
}

/// The Route A webhook request payload, projected to its wire form. Only
/// `scope`/`origin` are converted; `version`, `decision_id`, `request_id`, and
/// `items` borrow or pass through unchanged.
impl<'a> From<&'a ApprovalRequest> for ApprovalRequestWire<'a> {
    fn from(request: &'a ApprovalRequest) -> Self {
        Self {
            version: request.version,
            decision_id: request.decision_id,
            request_id: request.request_id.as_str(),
            scope: scope_to_wire(&request.scope),
            origin: origin_to_wire(&request.origin),
            items: request.items.as_slice(),
        }
    }
}

/// `approval_pending`: emitted only on the conversational route — the attended
/// prompt the client renders.
impl From<&ParkedApproval> for ApprovalPending {
    fn from(parked: &ParkedApproval) -> Self {
        let first_item = parked.request.items.first();
        Self {
            decision_id: parked.request.decision_id.to_string(),
            tool_name: first_item
                .map(|item| item.tool_name.clone())
                .unwrap_or_default(),
            arguments: first_item
                .map(|item| item.arguments.clone())
                .unwrap_or(serde_json::Value::Null),
            origin: origin_to_wire(&parked.request.origin),
            scope: scope_to_wire(&parked.request.scope),
            expires_at: parked.expires_at.to_rfc3339(),
        }
    }
}

/// Build an `ApprovalPending` event from the request before it is consumed by
/// `register`. Only emitted on the conversational route.
#[must_use]
pub fn pending(
    request: &ApprovalRequest,
    expires_at: &super::decision::Timestamp,
) -> ApprovalPending {
    let first_item = request.items.first();
    ApprovalPending {
        decision_id: request.decision_id.to_string(),
        tool_name: first_item
            .map(|item| item.tool_name.clone())
            .unwrap_or_default(),
        arguments: first_item
            .map(|item| item.arguments.clone())
            .unwrap_or(serde_json::Value::Null),
        origin: origin_to_wire(&request.origin),
        scope: scope_to_wire(&request.scope),
        expires_at: expires_at.to_rfc3339(),
    }
}

/// `approval_completed`: emitted on both routes. Aggregate source (the design
/// note's `From` does not fit a multi-value origin), so this is a named
/// constructor rather than a `From` impl.
#[must_use]
pub fn completed(
    decision_id: DecisionId,
    outcome: &ApprovalOutcome,
    scope: &AgentScope,
    duration: Duration,
) -> ApprovalCompleted {
    ApprovalCompleted {
        decision_id: decision_id.to_string(),
        outcome: outcome_to_wire(outcome),
        duration_ms: duration.as_millis() as u64,
        scope: scope_to_wire(scope),
    }
}

/// `approval_completed` for channel faults where no approval outcome exists.
#[must_use]
pub fn completed_error(
    decision_id: DecisionId,
    message: String,
    scope: &AgentScope,
    duration: Duration,
) -> ApprovalCompleted {
    ApprovalCompleted {
        decision_id: decision_id.to_string(),
        outcome: ApprovalOutcomeWire::Errored { message },
        duration_ms: duration.as_millis() as u64,
        scope: scope_to_wire(scope),
    }
}

fn origin_to_wire(origin: &ApprovalOrigin) -> ApprovalOriginWire {
    match origin {
        ApprovalOrigin::ConfigGate { matched_pattern } => ApprovalOriginWire::ConfigGate {
            matched_pattern: matched_pattern.clone(),
        },
        ApprovalOrigin::AgentRequested { reason } => ApprovalOriginWire::AgentRequested {
            reason: reason.clone(),
        },
    }
}

fn scope_to_wire(scope: &AgentScope) -> AgentScopeWire {
    match scope {
        AgentScope::Single { session_id } => AgentScopeWire::Single {
            session_id: session_id.as_ref().map(|id| id.as_str().to_string()),
        },
        AgentScope::Worker {
            run_id,
            task,
            session_id,
        } => AgentScopeWire::Worker {
            run_id: run_id.to_string(),
            task_id: task.task_id,
            worker: task.worker.clone(),
            session_id: session_id.as_ref().map(|id| id.as_str().to_string()),
        },
        AgentScope::Coordinator { run_id } => AgentScopeWire::Coordinator {
            run_id: run_id.to_string(),
        },
    }
}

fn outcome_to_wire(outcome: &ApprovalOutcome) -> ApprovalOutcomeWire {
    match outcome {
        ApprovalOutcome::Decided(ApprovalDecision::Approved) => ApprovalOutcomeWire::Approved,
        ApprovalOutcome::Decided(ApprovalDecision::Denied { reason }) => {
            ApprovalOutcomeWire::Denied {
                reason: reason.clone(),
            }
        }
        ApprovalOutcome::TimedOut { waited } => ApprovalOutcomeWire::TimedOut {
            waited_ms: waited.as_millis() as u64,
        },
        ApprovalOutcome::Cancelled(CancelReason::ClientDisconnected) => {
            ApprovalOutcomeWire::Cancelled {
                reason: CancelReasonWire::ClientDisconnected,
            }
        }
        ApprovalOutcome::Cancelled(CancelReason::Shutdown) => ApprovalOutcomeWire::Cancelled {
            reason: CancelReasonWire::Shutdown,
        },
        ApprovalOutcome::Cancelled(CancelReason::SenderDropped) => ApprovalOutcomeWire::Cancelled {
            reason: CancelReasonWire::SenderDropped,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hitl::decision::{AgentScope, CancelReason, DecisionId};
    use crate::orchestration::{RunId, TaskIdentity};

    #[test]
    fn sender_dropped_outcome_serializes_as_sender_dropped() {
        let id = DecisionId::generate();
        let run_id: RunId = "0191e8c0-1111-7000-8000-000000000000".parse().unwrap();
        let scope = AgentScope::Worker {
            run_id,
            task: TaskIdentity::new(0, Some("ops".to_string())),
            session_id: None,
        };
        let completed = completed(
            id,
            &ApprovalOutcome::Cancelled(CancelReason::SenderDropped),
            &scope,
            std::time::Duration::from_millis(42),
        );

        let json = serde_json::to_value(&completed).unwrap();
        assert_eq!(json["decision_id"], id.to_string());
        assert_eq!(json["outcome"]["kind"], "cancelled");
        assert_eq!(json["outcome"]["reason"], "sender_dropped");
        assert_eq!(json["duration_ms"], 42);
    }

    #[test]
    fn client_disconnected_outcome_serializes_as_client_disconnected() {
        let id = DecisionId::generate();
        let scope = AgentScope::Single { session_id: None };
        let completed = completed(
            id,
            &ApprovalOutcome::Cancelled(CancelReason::ClientDisconnected),
            &scope,
            std::time::Duration::from_millis(10),
        );

        let json = serde_json::to_value(&completed).unwrap();
        assert_eq!(json["outcome"]["reason"], "client_disconnected");
    }
}
