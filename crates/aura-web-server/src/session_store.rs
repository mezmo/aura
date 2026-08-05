//! The session-store factory: one backend handing out the capability handles
//! for cross-instance session state ([`ApprovalStore`] and [`EventBus`] from
//! `aura::session_store`, plus the upstream `a2a_server::TaskStore`).
//!
//! `AURA_SESSION_STORE=file` buys durable parked HITL approvals and nothing
//! else. That backend pairs the file approval store with a process-local bus
//! and a process-local A2A task store, so A2A tasks under it are lost on
//! restart. Two reasons, both deliberate: a decision published to a process
//! that no longer exists has no reader, and durable A2A tasks belong to the
//! Redis backend (`docs/adr/2026-07-21-hitl-park-reify.md` decision 14).
//!
//! See `docs/design/session-storage.md`,
//! `docs/adr/2026-07-08-session-storage.md`, and
//! `docs/adr/2026-07-21-hitl-park-reify.md`.

#[cfg(feature = "session-store-redis")]
mod redis;

use std::sync::Arc;

use a2a_server::{InMemoryTaskStore, TaskStore};
use async_trait::async_trait;
use aura::session_store::{
    ApprovalStore, EventBus, FileApprovalStore, FileRunStore, InMemoryApprovalStore,
    InMemoryEventBus, InMemoryRunStore, RunStore, SessionStoreError,
};
use aura_config::{FileSessionStoreConfig, SessionStoreBackend, SessionStoreConfig};

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

    /// The durable-parking capability: run FSM records, checkpoint CAS, and
    /// session leases. Optional because only backends with an atomic
    /// run-store primitive provide it; `None` means durable parking refuses
    /// fail-closed (see [`run_store_for_parking`]) rather than falling back.
    fn runs(&self) -> Option<Arc<dyn RunStore>> {
        None
    }

    /// Cheap liveness check.
    async fn ping(&self) -> Result<(), SessionStoreError>;
}

/// A park was requested on a deployment whose session-store backend hands
/// out no [`RunStore`], so the run cannot durably park and must fail closed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "durable parking refused: session store backend '{backend}' does not provide the run-store capability"
)]
pub struct MissingRunStore {
    pub backend: SessionStoreBackend,
}

/// The single gate between a configured deployment and a park commit: hands
/// out the backend's [`RunStore`] or refuses, naming the missing capability
/// and the identity [`SessionStore::backend`] reports. Never a silent
/// fallback.
#[expect(
    unused_variables,
    reason = "staged for #271: durable-parking preflight"
)]
pub fn run_store_for_parking(
    store: &dyn SessionStore,
) -> Result<Arc<dyn RunStore>, MissingRunStore> {
    todo!("staged for #271: durable-parking preflight")
}

/// Construct the configured backend. Fails fast on an unwritable file root, an
/// unreachable networked backend, or a `redis` config in a build without
/// `session-store-redis`.
pub async fn build_session_store(
    config: &SessionStoreConfig,
) -> Result<Arc<dyn SessionStore>, SessionStoreError> {
    match config {
        SessionStoreConfig::Memory => Ok(Arc::new(InMemorySessionStore::new())),
        SessionStoreConfig::File(file_config) => {
            Ok(Arc::new(FileSessionStore::open(file_config).await?))
        }
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
    runs: Arc<InMemoryRunStore>,
}

impl InMemorySessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            approvals: Arc::new(InMemoryApprovalStore::new()),
            tasks: Arc::new(InMemoryTaskStore::new()),
            bus: Arc::new(InMemoryEventBus::new()),
            runs: Arc::new(InMemoryRunStore::new()),
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

    fn runs(&self) -> Option<Arc<dyn RunStore>> {
        Some(self.runs.clone())
    }

    async fn ping(&self) -> Result<(), SessionStoreError> {
        Ok(())
    }
}

/// Durable approvals and run records under a filesystem root, alongside the
/// process-local bus and A2A task store.
pub struct FileSessionStore {
    approvals: Arc<FileApprovalStore>,
    runs: Arc<FileRunStore>,
    tasks: Arc<InMemoryTaskStore>,
    bus: Arc<InMemoryEventBus>,
}

impl FileSessionStore {
    /// Open the store root, creating it if absent.
    pub async fn open(config: &FileSessionStoreConfig) -> Result<Self, SessionStoreError> {
        Ok(Self {
            approvals: Arc::new(FileApprovalStore::open(config.root.clone()).await?),
            runs: Arc::new(FileRunStore::open(config.root.clone()).await?),
            tasks: Arc::new(InMemoryTaskStore::new()),
            bus: Arc::new(InMemoryEventBus::new()),
        })
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    fn backend(&self) -> SessionStoreBackend {
        SessionStoreBackend::File
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

    fn runs(&self) -> Option<Arc<dyn RunStore>> {
        Some(self.runs.clone())
    }

    async fn ping(&self) -> Result<(), SessionStoreError> {
        self.approvals.ping().await
    }
}

/// Every row here opens a file backend, which refuses to run on windows; the
/// refusal itself is pinned in `aura::session_store`.
#[cfg(all(test, not(windows)))]
mod tests {
    use std::time::Duration;

    use a2a::{Task, TaskState, TaskStatus};
    use aura::hitl::{
        AgentScope, ApprovalItem, ApprovalOrigin, ApprovalRequest, DecisionId, PROTOCOL_VERSION,
        ParkedApproval,
    };
    use bytes::Bytes;
    use futures_util::StreamExt;

    use super::*;

    /// Long enough for a process-local publish to land, short enough that a
    /// backend that never delivers does not stall the suite.
    const NON_DELIVERY_WINDOW: Duration = Duration::from_millis(200);

    fn parked() -> ParkedApproval {
        let now = chrono::Utc::now();
        ParkedApproval {
            request: ApprovalRequest {
                version: PROTOCOL_VERSION,
                decision_id: DecisionId::generate(),
                request_id: "req-file".to_string(),
                scope: AgentScope::Single { session_id: None },
                origin: ApprovalOrigin::ConfigGate {
                    matched_pattern: "test_*".to_string(),
                },
                items: vec![ApprovalItem {
                    tool_name: "test_tool".to_string(),
                    arguments: serde_json::json!({}),
                    tool_call_intent: None,
                }],
            },
            registered_at: now,
            expires_at: now + chrono::Duration::seconds(60),
        }
    }

    fn task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            context_id: "ctx-file".to_string(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    /// The file backend's composition is a deliberate mix, so pin all three
    /// capabilities at once: a second store over the same root sees the first
    /// store's approvals and none of its tasks or published events.
    #[tokio::test]
    async fn file_backend_composes_durable_approvals_with_a_process_local_bus_and_tasks() {
        let root = tempfile::tempdir().unwrap();
        let config = SessionStoreConfig::File(FileSessionStoreConfig {
            root: root.path().to_path_buf(),
        });

        let first = build_session_store(&config).await.unwrap();
        assert_eq!(first.backend(), SessionStoreBackend::File);
        first.ping().await.unwrap();

        let approval = parked();
        let decision_id = approval.request.decision_id;
        first.approvals().register(approval).await.unwrap();
        first.tasks().create(task("task-file")).await.unwrap();
        let mut subscriber = first.bus().subscribe("topic-file").await.unwrap();

        let second = build_session_store(&config).await.unwrap();
        assert!(
            second
                .approvals()
                .get(&decision_id)
                .await
                .unwrap()
                .is_some(),
            "approvals are file-backed, so a fresh store on the same root sees them",
        );
        assert!(
            second.tasks().get("task-file").await.unwrap().is_none(),
            "A2A tasks are in-memory, so they do not outlive the store that created them",
        );

        second
            .bus()
            .publish("topic-file", Bytes::from_static(b"x"))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(NON_DELIVERY_WINDOW, subscriber.next())
                .await
                .is_err(),
            "the bus is in-memory, so a separate store's publish reaches no subscriber here",
        );
    }

    #[tokio::test]
    async fn file_backend_rejects_a_root_it_cannot_create() {
        let root = tempfile::NamedTempFile::new().unwrap();
        let config = SessionStoreConfig::File(FileSessionStoreConfig {
            root: root.path().to_path_buf(),
        });

        let Err(err) = build_session_store(&config).await else {
            panic!("a plain file cannot serve as a store root");
        };
        assert!(matches!(err, SessionStoreError::Connect { .. }), "{err:?}");
    }
}
