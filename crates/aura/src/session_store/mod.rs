//! Pluggable cross-instance session-state capabilities: a durable store for parked
//! HITL approvals and a pub/sub event bus.
//!
//! The in-memory implementations are the default; a file backend adds
//! restart durability with no infrastructure; a networked backend (e.g.
//! Redis/Valkey) implements the same traits to make a load-balanced multi-instance
//! deployment behave like one process. The `conformance` module (behind the
//! `test-support` feature) holds the contract all of them are held to.
//!
//! See `docs/design/session-storage.md`,
//! `docs/adr/2026-07-08-session-storage.md`, and
//! `docs/adr/2026-07-21-hitl-park-reify.md`.

#[cfg(any(test, feature = "test-support"))]
pub mod conformance;

mod file;
mod memory;
mod record;

#[cfg(test)]
mod memory_run_store_failpoint_tests;

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;

use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};
use crate::orchestration::park::{
    AgentInstanceId, CasError, FencingGeneration, Lease, LeaseTtl, ParkCommit, RunEvent, SessionId,
    SessionRecord, WakeReason,
};

pub use file::{FileApprovalStore, FileRunStore};
pub use memory::{InMemoryApprovalStore, InMemoryEventBus, InMemoryRunStore};
pub use record::{
    DECIDED_RECORD_VERSION, DecidedRecord, DecisionRecord, InvalidRecord, OriginRecord,
    ParkedApprovalRecord, RUN_RECORD_VERSION, RunRecordError, ScopeRecord, decode_decided_record,
    decode_run_record, encode_decided_record, encode_run_record,
};

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
    /// at-most-once resolution is enforced here, in the store. The
    /// destructive path: superseded by [`Self::resolve_durable`] wherever a
    /// parked run must survive the decision.
    async fn resolve(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError>;

    /// Look up the decision [`Self::resolve_durable`] recorded for an
    /// approval.
    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ApprovalDecision>, SessionStoreError>;

    /// Record a terminal decision as a durable wake reason WITHOUT
    /// destroying the parked entry (ADR 2026-07-21, decision 8): the entry
    /// and the returned wake reason survive until a claim consumes them.
    /// At-most-once consumption moves out of resolution and into the
    /// dispatch FSM's digest-bound claim.
    ///
    /// The first decision recorded for an id wins. A later call — repeating
    /// that decision or contradicting it — records nothing and returns `Ok`
    /// carrying the stored wake reason, so every caller reads the same
    /// resolution. An unknown or removed id returns
    /// [`ResolveError::NotFound`].
    async fn resolve_durable(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<WakeReason, ResolveError>;

    /// Remove a parked entry and the decision recorded for it: an approval
    /// that is gone reads as undecided, never as a decision no run will
    /// claim.
    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError>;

    /// Remove every approval parked under a request id, and their recorded
    /// decisions, on the same rule as [`Self::remove`].
    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError>;
}

/// Why a [`RunStore`] operation could not complete.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum RunStoreError {
    /// No record exists for the session.
    #[error("no session record exists for session {session}")]
    UnknownSession { session: SessionId },
    /// [`RunStore::create`] found an existing record; session identities are
    /// minted once and never overwritten.
    #[error("a session record already exists for session {session}")]
    SessionExists { session: SessionId },
    /// The lease is held by a live owner; two agent instances never own one
    /// session (ADR 2026-07-21, decision 5).
    #[error("session lease held by agent instance {holder} until {expires_at}")]
    LeaseHeld {
        holder: AgentInstanceId,
        expires_at: crate::hitl::Timestamp,
    },
    /// A fenced mutation was rejected: stale or unissued generation, an
    /// illegal FSM transition, or a state that does not admit the operation.
    #[error("fenced session mutation rejected: {0:?}")]
    Cas(CasError),
    /// A stored run record failed to decode.
    #[error(transparent)]
    Record(#[from] RunRecordError),
    /// The backing store failed; nothing can be said about the record.
    #[error(transparent)]
    Store(#[from] SessionStoreError),
}

/// Durable storage for the run-level park/reify protocol: the fenced
/// [`SessionRecord`] (run FSM plus checkpoint while parked), the session
/// lease, and the CAS operations every backend must execute inside its
/// atomic primitive (ADR 2026-07-21, decisions 3, 5, 7).
///
/// The transition semantics live on [`SessionRecord::apply`] and
/// [`SessionRecord::park`]; a backend's job is to run them atomically
/// against its stored record and persist the result, so every backend
/// enforces identical rules. Exposed as an optional capability on the
/// session-store factory: a backend without it cannot durably park, and the
/// park path refuses fail-closed rather than falling back.
#[async_trait]
pub trait RunStore: Send + Sync {
    /// Persist the record for a newly minted session. The record must be in
    /// `Created` state; an existing record is [`RunStoreError::SessionExists`].
    async fn create(&self, record: SessionRecord) -> Result<(), RunStoreError>;

    /// Load the current record snapshot.
    async fn load(&self, session: SessionId) -> Result<Option<SessionRecord>, RunStoreError>;

    /// Acquire the session lease by CAS: free or expired leases transfer to
    /// `holder` at the next fencing generation; a live lease held by another
    /// instance is [`RunStoreError::LeaseHeld`]. The returned [`Lease`]
    /// carries the fencing token every later mutation must present.
    async fn acquire_lease(
        &self,
        session: SessionId,
        holder: AgentInstanceId,
        ttl: LeaseTtl,
    ) -> Result<Lease, RunStoreError>;

    /// Extend the held lease by `ttl` from now. The presented generation
    /// must be the record's current one; a stale token means the lease was
    /// lost and the caller must stop mutating.
    async fn heartbeat_lease(
        &self,
        session: SessionId,
        generation: FencingGeneration,
        ttl: LeaseTtl,
    ) -> Result<Lease, RunStoreError>;

    /// Release the held lease, leaving the record unleased at the same
    /// generation so the next claim fences correctly.
    async fn release_lease(
        &self,
        session: SessionId,
        generation: FencingGeneration,
    ) -> Result<(), RunStoreError>;

    /// Apply one run-FSM event through [`SessionRecord::apply`], atomically.
    async fn apply(
        &self,
        session: SessionId,
        presented: FencingGeneration,
        event: RunEvent,
    ) -> Result<SessionRecord, RunStoreError>;

    /// Commit a park through [`SessionRecord::park`], atomically: the
    /// `Running -> Parked` transition and its checkpoint are one write, and
    /// no reader observes a parked record without its checkpoint.
    async fn park(
        &self,
        session: SessionId,
        presented: FencingGeneration,
        commit: ParkCommit,
    ) -> Result<SessionRecord, RunStoreError>;
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
