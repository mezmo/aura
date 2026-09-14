//! Pluggable cross-instance session-state capabilities: a durable store for parked
//! HITL approvals and a pub/sub event bus.
//!
//! The in-memory implementations are the default; a file-backed approval
//! store survives a process restart on a single host, and a networked backend
//! (e.g. Redis/Valkey) implements the same traits to make a load-balanced
//! multi-instance deployment behave like one process.
//!
//! See `docs/design/session-storage.md` and
//! `docs/adr/2026-07-08-session-storage.md`.

#[cfg(test)]
pub(crate) mod fault_store;
mod file;
mod memory;
mod record;

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;

use crate::hitl::{
    ApprovalAuthority, ApprovalRead, DecisionId, ParkedApproval, ResolveError, ResolvedDecision,
};

#[cfg(test)]
pub(crate) use fault_store::FaultInjectingStore;
pub use file::FileApprovalStore;
pub(crate) use file::{private_dir, write_private};
pub use memory::{InMemoryApprovalStore, InMemoryEventBus};
pub use record::{DecisionRecord, InvalidRecord, OriginRecord, ParkedApprovalRecord, ScopeRecord};

/// A fault in the backing session-store/bus backend.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SessionStoreError {
    /// The configured backend is not compiled into this binary.
    #[error("session store backend '{backend}' requires the '{feature}' cargo feature")]
    BackendUnavailable { backend: String, feature: String },
    /// The backend connection URL failed to parse.
    #[error("invalid session store url: {reason}")]
    InvalidUrl { reason: String },
    /// Establishing the backend connection failed.
    #[error("session store connection failed: {reason}")]
    Connect { reason: String },
    /// The backend connection was not established in time.
    #[error("timed out connecting to the session store after {}s", .timeout.as_secs())]
    ConnectTimeout { timeout: Duration },
    /// A request to an established backend failed.
    #[error("session store request failed: {reason}")]
    Request { reason: String },
    /// A stored record failed to decode.
    #[error("session store record failed to decode: {reason}")]
    Decode { reason: String },
}

/// The outcome of a conditional acknowledgment transition on a parked row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcknowledgeOutcome {
    /// The row was still pending; its acknowledgment state is now
    /// `Acknowledged`.
    Acknowledged,
    /// No still-pending row matched the id: unknown, already resolved, or
    /// cancelled/removed while the notify was in flight. Nothing was created.
    Missing,
}

/// Durable storage for parked conversational HITL approvals, over the
/// serializable [`ParkedApproval`] record.
#[async_trait]
pub trait ApprovalStore: Send + Sync {
    /// Persist a parked approval, keyed by its `DecisionId`. Backends with
    /// native expiry set the entry's TTL from `expires_at`; the file store
    /// unlinks an expired entry on its next poll scan.
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError>;

    /// Conditionally mark a still-pending row's acknowledgment state as
    /// acknowledged. Updates only a row that is still pending (undecided and
    /// not removed); never recreates a row. Returns an explicit outcome for a
    /// row that is missing (unknown, resolved, or cancelled).
    async fn mark_acknowledged(
        &self,
        id: &DecisionId,
    ) -> Result<AcknowledgeOutcome, SessionStoreError>;

    /// Look up a parked approval.
    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError>;

    /// Record a terminal decision — and the approver identity captured
    /// alongside it, as one carrier — at most once per id; later attempts
    /// read as `NotFound`. The file backend moves the ticket into its decision
    /// record, other backends drop it.
    async fn resolve(
        &self,
        id: &DecisionId,
        decision: ResolvedDecision,
    ) -> Result<(), ResolveError>;

    /// Look up the decision recorded for an already-resolved approval,
    /// carrying any captured identity with it.
    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ResolvedDecision>, SessionStoreError>;

    /// Remove a parked entry.
    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError>;

    /// Remove every approval parked under a request id and return the
    /// approvals cleared. A ticket with a recorded decision is never in
    /// the return: the cleared set is the authoritative record of what
    /// the cancellation applies to.
    async fn cancel_request(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, SessionStoreError>;

    /// List every parked approval that is undecided and non-expired
    /// (`expires_at > now`). No ordering guarantee.
    async fn list_pending(&self) -> Result<Vec<ParkedApproval>, SessionStoreError>;

    /// Read one approval row and, under the same serialization boundary
    /// [`ApprovalStore::resolve`] and [`ApprovalStore::remove`] hold, expire
    /// it when its own deadline has passed strictly.
    ///
    /// A row parked under a different authority than `expected_authority`
    /// reads as [`ApprovalRead::Missing`] with no mutation — a validly signed
    /// local request cannot override governance, and one agent's poller
    /// cannot consume another's rows. A decision recorded exactly at the
    /// deadline remains valid; an existing terminal winner is returned
    /// unchanged. Missing, decode, and I/O failures are errors, never
    /// outcomes.
    async fn read_or_expire(
        &self,
        id: &DecisionId,
        expected_authority: ApprovalAuthority,
    ) -> Result<ApprovalRead, SessionStoreError>;
}

/// The payload stream returned by [`EventBus::subscribe`].
pub type Subscription = Pin<Box<dyn Stream<Item = Bytes> + Send>>;

/// Cross-instance pub/sub.
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
