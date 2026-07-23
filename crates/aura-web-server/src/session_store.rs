//! The session-store factory: one backend handing out the capability handles
//! for cross-instance session state ([`ApprovalStore`] and [`EventBus`] from
//! `aura::session_store`, plus the upstream `a2a_server::TaskStore`).
//!
//! See `docs/design/session-storage.md` and
//! `docs/adr/2026-07-08-session-storage.md`.

#[cfg(feature = "session-store-redis")]
mod redis;

use std::sync::Arc;

use a2a_server::{InMemoryTaskStore, TaskStore};
use async_trait::async_trait;
use aura::session_store::{
    ApprovalStore, EventBus, InMemoryApprovalStore, InMemoryEventBus, SessionStoreError,
};
use aura_config::{SessionStoreBackend, SessionStoreConfig};

#[cfg(feature = "session-store-redis")]
pub use redis::RedisSessionStore;

/// A pluggable backend for cross-instance session state, handing out one handle
/// per capability.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Which configured backend this is.
    fn backend(&self) -> SessionStoreBackend;

    /// Durable parked HITL approvals.
    fn approvals(&self) -> Arc<dyn ApprovalStore>;

    /// Durable A2A tasks (the upstream `a2a_server::TaskStore` trait).
    fn tasks(&self) -> Arc<dyn TaskStore>;

    /// Allows for cross-instance pub/sub (in-memory SessionStore would be single-instance only).
    fn bus(&self) -> Arc<dyn EventBus>;

    /// Cheap liveness check.
    async fn ping(&self) -> Result<(), SessionStoreError>;
}

/// Construct the configured backend. Fails fast on an unreachable networked
/// backend or a `redis` config in a build without `session-store-redis`.
pub async fn build_session_store(
    config: &SessionStoreConfig,
) -> Result<Arc<dyn SessionStore>, SessionStoreError> {
    match config {
        SessionStoreConfig::Memory => Ok(Arc::new(InMemorySessionStore::new())),
        #[cfg(feature = "session-store-redis")]
        SessionStoreConfig::Redis(redis_config) => {
            Ok(Arc::new(RedisSessionStore::connect(redis_config).await?))
        }
        #[cfg(not(feature = "session-store-redis"))]
        SessionStoreConfig::Redis(_) => Err(SessionStoreError::BackendUnavailable {
            backend: SessionStoreBackend::Redis.to_string(),
            feature: "session-store-redis".to_string(),
        }),
    }
}

/// The default backend: every capability is process-local, so state is scoped
/// to one process.
pub struct InMemorySessionStore {
    approvals: Arc<InMemoryApprovalStore>,
    tasks: Arc<InMemoryTaskStore>,
    bus: Arc<InMemoryEventBus>,
}

impl InMemorySessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            approvals: Arc::new(InMemoryApprovalStore::new()),
            tasks: Arc::new(InMemoryTaskStore::new()),
            bus: Arc::new(InMemoryEventBus::new()),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    fn backend(&self) -> SessionStoreBackend {
        SessionStoreBackend::Memory
    }

    fn approvals(&self) -> Arc<dyn ApprovalStore> {
        self.approvals.clone()
    }

    fn tasks(&self) -> Arc<dyn TaskStore> {
        self.tasks.clone()
    }

    fn bus(&self) -> Arc<dyn EventBus> {
        self.bus.clone()
    }

    async fn ping(&self) -> Result<(), SessionStoreError> {
        Ok(())
    }
}
