//! Pluggable cross-instance session-state capabilities: a durable store for parked
//! HITL approvals, a per-session, per-agent skill-invocation store, a per-session
//! event journal, and a pub/sub event bus.
//!
//! The in-memory implementations are the default; file-backed approval and
//! skill-invocation stores survive a process restart on a single host, and a
//! networked backend (e.g. Redis/Valkey) implements the same traits to make a
//! load-balanced multi-instance deployment behave like one process.
//!
//! See `docs/design/session-storage.md` and
//! `docs/adr/2026-07-08-session-storage.md`.

#[cfg(test)]
pub(crate) mod fault_store;
mod file;
mod memory;
mod record;
mod skill_record;

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_events::run::{SequenceNumber, SessionEvent};
use bytes::Bytes;
use futures::Stream;

use crate::config::SessionId;
use crate::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};

#[cfg(test)]
pub(crate) use fault_store::FaultInjectingStore;
pub use file::{FileApprovalStore, FileSkillInvocationStore};
pub use memory::{
    DEFAULT_MAX_JOURNAL_ENTRIES_PER_SESSION, DEFAULT_MAX_JOURNAL_SESSIONS, InMemoryApprovalStore,
    InMemoryEventBus, InMemoryJournalStore, InMemorySkillInvocationStore,
};
pub use record::{DecisionRecord, InvalidRecord, OriginRecord, ParkedApprovalRecord, ScopeRecord};
pub use skill_record::{
    SKILL_INVOCATION_RECORD_VERSION, SkillInvocation, SkillInvocationRecord, SkillRecordDecodeError,
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
    /// native expiry set the entry's TTL from `expires_at`; the file store
    /// keeps it until `remove`.
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError>;

    /// Look up a parked approval.
    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError>;

    /// Record a terminal decision at most once per id; later attempts read
    /// as `NotFound`. The file backend moves the ticket into its decision
    /// record, other backends drop it.
    async fn resolve(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError>;

    /// Look up the decision recorded for an already-resolved approval.
    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ApprovalDecision>, SessionStoreError>;

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
}

/// Distinct skill-invocation records one skill log may hold.
pub const MAX_SKILL_RECORDS_PER_LOG: usize = 64;

/// One agent's skill-invocation log within a chat session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkillLogKey {
    pub session_id: SessionId,
    /// The serving agent's public identifier (`aura_config::Config::agent_id`).
    pub agent_id: String,
}

impl SkillLogKey {
    #[must_use]
    pub fn new(session_id: SessionId, agent_id: impl Into<String>) -> Self {
        Self {
            session_id,
            agent_id: agent_id.into(),
        }
    }
}

/// Storage of skill-tool invocations partitioned by [`SkillLogKey`], over the
/// serializable [`SkillInvocationRecord`]. Logs are disjoint: no operation on
/// one log reads or counts another's records.
#[async_trait]
pub trait SkillInvocationStore: Send + Sync {
    /// Persist an invocation under a log. Idempotent per
    /// (log, [`SkillInvocation::dedup_key`]): the first record for a key
    /// wins and later duplicates are no-ops, so a re-invoked skill keeps its
    /// original position. A log holds at most [`MAX_SKILL_RECORDS_PER_LOG`]
    /// distinct invocations; a write past the cap is dropped with a warning,
    /// not failed. Backends with native expiry should TTL the log's entries
    /// so abandoned sessions self-clean.
    async fn record(
        &self,
        log: &SkillLogKey,
        record: SkillInvocationRecord,
    ) -> Result<(), SessionStoreError>;

    /// Every invocation recorded under a log, ordered by (anchor, seq).
    async fn list(
        &self,
        log: &SkillLogKey,
    ) -> Result<Vec<SkillInvocationRecord>, SessionStoreError>;
}

/// One stored entry of a session's journal, as the store hands it back.
#[derive(Debug, Clone)]
pub enum JournalEntry {
    Event(Arc<SessionEvent>),
    /// An entry at `seq` this binary cannot decode into a [`SessionEvent`].
    Undecodable {
        seq: SequenceNumber,
        raw: Bytes,
    },
}

impl JournalEntry {
    pub fn seq(&self) -> SequenceNumber {
        match self {
            Self::Event(event) => event.seq,
            Self::Undecodable { seq, .. } => *seq,
        }
    }
}

/// Append-only storage of each session's [`SessionEvent`]s, keyed by session.
///
/// A session's entries arrive from one writer at a time, each carrying the
/// sequence number that writer minted, so a store sees every session as a
/// strictly increasing sequence and never has to order or merge. A store bounds
/// what it keeps on its own — by count here, by TTL where the backend has one —
/// and drops a session's oldest entries first, so what it still holds is a
/// suffix of the session's stream and a reader tells what is missing from the
/// sequence numbers alone.
#[async_trait]
pub trait JournalStore: Send + Sync {
    /// Persist an event under its session. Returns once the event is kept to
    /// this store's standard: at once in memory, on commit elsewhere.
    async fn append(&self, event: Arc<SessionEvent>) -> Result<(), SessionStoreError>;

    /// The session's entries with `seq` in `from..=to` that the store still
    /// holds, in sequence order. An entry the store holds but cannot decode is
    /// returned as [`JournalEntry::Undecodable`] in its place rather than
    /// dropped, so a reader sees the position and not a hole.
    async fn read(
        &self,
        session: &SessionId,
        from: SequenceNumber,
        to: SequenceNumber,
    ) -> Result<Vec<JournalEntry>, SessionStoreError>;

    /// The highest sequence number stored for the session, or `None` for a
    /// session the store holds nothing of.
    async fn latest(
        &self,
        session: &SessionId,
    ) -> Result<Option<SequenceNumber>, SessionStoreError>;
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
