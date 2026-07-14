//! Pluggable cross-pod session-state capabilities: a durable store for parked
//! HITL approvals and a pub/sub event bus.
//!
//! The in-memory implementations are the default; a networked backend (e.g.
//! Redis/Valkey) implements the same traits to make a load-balanced multi-pod
//! deployment behave like one process.
//!
//! See `docs/design/session-storage.md` and
//! `docs/adr/2026-07-08-session-storage.md`.

mod memory;

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;

use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};

pub use memory::{InMemoryApprovalStore, InMemoryEventBus};

/// A fault in the backing session-store/bus backend (connection, protocol,
/// serialization at the backend boundary).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SessionStoreError {
    #[error("session store backend error: {0}")]
    Backend(String),
}

/// Durable storage for parked conversational HITL approvals, over the
/// serializable [`ParkedApproval`] record.
#[async_trait]
pub trait ApprovalStore: Send + Sync {
    /// Persist a parked approval, keyed by its `DecisionId`. Backends with
    /// native expiry should TTL the entry from `expires_at` so abandoned
    /// approvals self-clean.
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError>;

    /// Look up a parked approval.
    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError>;

    /// Record a terminal decision and remove the parked entry, atomically —
    /// at-most-once resolution is enforced here, in the store.
    async fn resolve(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError>;

    /// Remove a parked entry.
    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError>;

    /// Remove every approval parked under a request id.
    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError>;
}

/// The payload stream returned by [`EventBus::subscribe`].
pub type Subscription = Pin<Box<dyn Stream<Item = Bytes> + Send>>;

/// Cross-pod pub/sub.
///
/// Payloads are opaque bytes; topic naming and payload encoding belong to the
/// publishing subsystem.
#[async_trait]
pub trait EventBus: Send + Sync {
    /// Publish a payload to a topic. Fire-and-forget; delivery is
    /// best-effort and publishing to a topic with no subscribers is not an
    /// error.
    async fn publish(&self, topic: &str, payload: Bytes) -> Result<(), SessionStoreError>;

    /// Subscribe to a topic, receiving every payload published after this
    /// call returns. The stream ends when the subscription is dropped or the
    /// backend closes the topic.
    async fn subscribe(&self, topic: &str) -> Result<Subscription, SessionStoreError>;
}
