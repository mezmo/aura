//! Redis/Valkey session-store backend.
//!
//! The A2A task store ([`task_store`]), the HITL approval store
//! ([`approval_store`]), and the event bus ([`event_bus`]) are all
//! Redis-backed, so A2A send/poll/list, conversational approvals, and —
//! through `crate::a2a`'s bus bridge — A2A streaming/subscribe/cancel all
//! work across instances. Each submodule documents its own key schema under
//! the configured `key_prefix`. See `docs/design/session-storage.md`.

mod approval_store;
mod event_bus;
mod task_store;

use std::sync::Arc;

use a2a_server::TaskStore;
use async_trait::async_trait;
use aura::session_store::{ApprovalStore, EventBus, SessionStoreError};
use aura_config::{RedisSessionStoreConfig, SessionStoreBackend};
use redis::Client;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

use super::SessionStore;
use approval_store::RedisApprovalStore;
use event_bus::RedisEventBus;
use task_store::RedisTaskStore;

pub struct RedisSessionStore {
    conn: ConnectionManager,
    tasks: Arc<RedisTaskStore>,
    approvals: Arc<RedisApprovalStore>,
    bus: Arc<RedisEventBus>,
}

impl RedisSessionStore {
    /// Connect and eagerly verify reachability (bounded by the configured
    /// connect timeout) so a misconfigured backend fails at startup, not on
    /// the first request.
    pub async fn connect(config: &RedisSessionStoreConfig) -> Result<Self, SessionStoreError> {
        let client =
            Client::open(config.url.as_str()).map_err(|e| SessionStoreError::InvalidUrl {
                reason: e.to_string(),
            })?;

        let timeout = config.connect_timeout;
        let manager_config = ConnectionManagerConfig::new()
            .set_connection_timeout(timeout)
            .set_response_timeout(timeout);
        let conn = tokio::time::timeout(
            timeout,
            ConnectionManager::new_with_config(client.clone(), manager_config),
        )
        .await
        .map_err(|_| SessionStoreError::ConnectTimeout { timeout })?
        .map_err(|e| SessionStoreError::Connect {
            reason: e.to_string(),
        })?;

        Ok(Self {
            tasks: Arc::new(RedisTaskStore::new(
                conn.clone(),
                &config.key_prefix,
                config.task_ttl_secs,
            )),
            approvals: Arc::new(RedisApprovalStore::new(conn.clone(), &config.key_prefix)),
            bus: Arc::new(RedisEventBus::new(
                client,
                conn.clone(),
                &config.key_prefix,
                timeout,
            )),
            conn,
        })
    }
}

#[async_trait]
impl SessionStore for RedisSessionStore {
    fn backend(&self) -> SessionStoreBackend {
        SessionStoreBackend::Redis
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
        let mut conn = self.conn.clone();
        redis::cmd("PING")
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| SessionStoreError::Request {
                reason: format!("ping failed: {e}"),
            })
    }
}

/// Map a redis failure on an established connection to the store error.
fn request_err(e: redis::RedisError) -> SessionStoreError {
    SessionStoreError::Request {
        reason: e.to_string(),
    }
}
