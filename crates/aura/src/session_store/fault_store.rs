//! An approval-store double for fault injection in tests.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use super::{AcknowledgeOutcome, ApprovalStore, InMemoryApprovalStore, SessionStoreError};
use crate::hitl::{
    ApprovalAuthority, ApprovalRead, DecisionId, ParkedApproval, ResolveError, ResolvedDecision,
};

/// Delegates to an in-memory store; each `fail_*` flag makes that operation
/// answer `SessionStoreError::Request` (the `*_once` flag fires one time).
#[derive(Default)]
pub(crate) struct FaultInjectingStore {
    inner: InMemoryApprovalStore,
    fail_register: bool,
    fail_get_once: AtomicBool,
}

impl FaultInjectingStore {
    pub(crate) fn failing_register() -> Self {
        Self {
            fail_register: true,
            ..Default::default()
        }
    }

    pub(crate) fn failing_first_get() -> Self {
        Self {
            fail_get_once: AtomicBool::new(true),
            ..Default::default()
        }
    }
}

#[async_trait]
impl ApprovalStore for FaultInjectingStore {
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError> {
        if self.fail_register {
            return Err(SessionStoreError::Request {
                reason: "disk on fire".to_string(),
            });
        }
        self.inner.register(parked).await
    }

    async fn mark_acknowledged(
        &self,
        id: &DecisionId,
    ) -> Result<AcknowledgeOutcome, SessionStoreError> {
        self.inner.mark_acknowledged(id).await
    }

    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
        if self.fail_get_once.swap(false, Ordering::SeqCst) {
            return Err(SessionStoreError::Request {
                reason: "transient parked-approval lookup fault".to_string(),
            });
        }
        self.inner.get(id).await
    }

    async fn resolve(
        &self,
        id: &DecisionId,
        expected_authority: ApprovalAuthority,
        decision: ResolvedDecision,
    ) -> Result<(), ResolveError> {
        self.inner.resolve(id, expected_authority, decision).await
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ResolvedDecision>, SessionStoreError> {
        self.inner.decision(id).await
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        self.inner.remove(id).await
    }

    async fn cancel_request(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        self.inner.cancel_request(request_id).await
    }

    async fn list_pending(&self) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        self.inner.list_pending().await
    }

    async fn read_or_expire(
        &self,
        id: &DecisionId,
        expected_authority: ApprovalAuthority,
    ) -> Result<ApprovalRead, SessionStoreError> {
        self.inner.read_or_expire(id, expected_authority).await
    }

    async fn retained_rows(&self) -> Result<Vec<super::RetainedApproval>, SessionStoreError> {
        self.inner.retained_rows().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hitl::{
        AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, PROTOCOL_VERSION,
    };

    /// The fault double's own parked-row fixture: the double is cfg(test)
    /// and its inner store registers plain `ParkedApproval` rows.
    fn parked(request_id: &str) -> ParkedApproval {
        let now = chrono::Utc::now();
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                instance_id: "test-instance".to_string(),
                decision_id: DecisionId::generate(),
                request_id: request_id.to_string(),
                scope: AgentScope::Single { session_id: None },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "test_*".to_string(),
                    agent_name: "test-agent".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "test_tool".to_string(),
                    arguments: serde_json::json!({}),
                    tool_call_intent: None,
                }],
            },
            registered_at: now,
            expires_at: now + chrono::Duration::seconds(60),
            authority: ApprovalAuthority::Conversational,
            egress_headers: None,
            acknowledgment: crate::hitl::AcknowledgmentState::RequiresNotification,
        }
    }

    #[tokio::test]
    async fn fault_double_read_or_expire_delegates_to_inner() {
        let store = FaultInjectingStore::default();
        let entry = parked("req-fault-roe");
        let id = entry.request.decision_id;
        store.register(entry).await.unwrap();

        match store
            .read_or_expire(&id, ApprovalAuthority::Conversational)
            .await
            .unwrap()
        {
            ApprovalRead::Pending(got) => {
                assert_eq!(got.request.decision_id, id);
                assert_eq!(got.request.request_id, "req-fault-roe");
            }
            _ => panic!("expected Pending, got another ApprovalRead arm"),
        }
    }

    #[tokio::test]
    async fn fault_double_retained_rows_delegates_to_inner() {
        let store = FaultInjectingStore::default();

        match store.retained_rows().await {
            Err(SessionStoreError::UnsupportedOperation { operation, .. }) => {
                assert_eq!(operation, "retained_rows");
            }
            _ => panic!("expected UnsupportedOperation, got another answer"),
        }
    }
}
