//! An approval-store double for fault injection in tests.

use async_trait::async_trait;

use super::{ApprovalStore, InMemoryApprovalStore, SessionStoreError};
use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};

/// Delegates to an in-memory store; each `fail_*` flag makes that operation
/// answer `SessionStoreError::Request`.
#[derive(Default)]
pub(crate) struct FaultInjectingStore {
    inner: InMemoryApprovalStore,
    fail_register: bool,
}

impl FaultInjectingStore {
    pub(crate) fn failing_register() -> Self {
        Self {
            fail_register: true,
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

    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
        self.inner.get(id).await
    }

    async fn resolve(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        self.inner.resolve(id, decision).await
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ApprovalDecision>, SessionStoreError> {
        self.inner.decision(id).await
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        self.inner.remove(id).await
    }

    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError> {
        self.inner.cancel_request(request_id).await
    }
}
