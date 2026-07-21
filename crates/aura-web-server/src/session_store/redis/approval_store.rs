//! Redis-backed HITL approval store: a parked approval is readable and
//! resolvable from any instance.
//!
//! Key schema (all under the configured `key_prefix`, default `aura`):
//!
//! | Key                            | Type                 | Purpose                  |
//! | ------------------------------ | -------------------- | ------------------------ |
//! | `{p}:approval:{decision_id}`   | string (record JSON) | parked approval record   |
//! | `{p}:approval:req:{request_id}`| set of decision ids  | `cancel_request` fan-out |
//!
//! Approval records carry a TTL derived from the approval's `expires_at`, so
//! abandoned entries self-clean; the parking instance's await remains the
//! authoritative timeout. The request index is refreshed on every register
//! with a margin over the record TTL and pruned best-effort on resolve/remove;
//! a stale indexed id only costs `cancel_request` a `DEL` of a missing key.

use async_trait::async_trait;
use aura::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};
use aura::session_store::{ApprovalStore, ParkedApprovalRecord, SessionStoreError};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use super::request_err;

/// Floor for a record's TTL, so an approval registered at (or past) its expiry
/// still exists for the in-flight resolve or get that raced it.
const MIN_TTL_SECS: u64 = 1;
/// Margin the request index's TTL keeps over its newest record's TTL.
const REQ_INDEX_TTL_MARGIN_SECS: u64 = 60;

pub struct RedisApprovalStore {
    conn: ConnectionManager,
    key_prefix: String,
}

impl RedisApprovalStore {
    pub fn new(conn: ConnectionManager, key_prefix: &str) -> Self {
        Self {
            conn,
            key_prefix: key_prefix.to_string(),
        }
    }

    fn approval_key(&self, decision_id: &str) -> String {
        format!("{}:approval:{decision_id}", self.key_prefix)
    }

    fn req_key(&self, request_id: &str) -> String {
        format!("{}:approval:req:{request_id}", self.key_prefix)
    }

    /// Atomically take the record (`GETDEL`), pruning the request index
    /// best-effort. `None` means no live entry existed.
    async fn take(&self, id: &DecisionId) -> Result<Option<()>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let payload: Option<String> = redis::cmd("GETDEL")
            .arg(self.approval_key(&id.to_string()))
            .query_async(&mut conn)
            .await
            .map_err(request_err)?;
        let Some(json) = payload else {
            return Ok(None);
        };
        if let Ok(record) = serde_json::from_str::<ParkedApprovalRecord>(&json) {
            let _: Result<(), _> = conn
                .srem(self.req_key(&record.request_id), id.to_string())
                .await;
        }
        Ok(Some(()))
    }
}

#[async_trait]
impl ApprovalStore for RedisApprovalStore {
    async fn register(&self, parked: ParkedApproval) -> Result<(), SessionStoreError> {
        let record = ParkedApprovalRecord::from(&parked);
        let payload = serde_json::to_string(&record).expect("approval record serializes to JSON");
        let ttl = record_ttl_secs(&parked);
        let approval_key = self.approval_key(&record.decision_id.to_string());
        let req_key = self.req_key(&record.request_id);

        let mut conn = self.conn.clone();
        let mut pipe = redis::pipe();
        pipe.set_ex(&approval_key, payload, ttl).ignore();
        pipe.sadd(&req_key, record.decision_id.to_string()).ignore();
        pipe.expire(&req_key, (ttl + REQ_INDEX_TTL_MARGIN_SECS) as i64)
            .ignore();
        pipe.query_async::<()>(&mut conn).await.map_err(request_err)
    }

    async fn get(&self, id: &DecisionId) -> Result<Option<ParkedApproval>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let payload: Option<String> = conn
            .get(self.approval_key(&id.to_string()))
            .await
            .map_err(request_err)?;
        payload.map(|json| decode(&json)).transpose()
    }

    async fn resolve(
        &self,
        id: &DecisionId,
        _decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        // The atomic take is the at-most-once guarantee: exactly one resolver
        // gets the record; everyone else (and every later attempt) sees
        // `NotFound`.
        match self.take(id).await.map_err(ResolveError::Store)? {
            Some(()) => Ok(()),
            None => Err(ResolveError::NotFound),
        }
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        self.take(id).await.map(|_| ())
    }

    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError> {
        let req_key = self.req_key(request_id);
        let mut conn = self.conn.clone();
        let ids: Vec<String> = conn.smembers(&req_key).await.map_err(request_err)?;

        let mut pipe = redis::pipe();
        for id in &ids {
            pipe.del(self.approval_key(id)).ignore();
        }
        pipe.del(&req_key).ignore();
        pipe.query_async::<()>(&mut conn).await.map_err(request_err)
    }
}

/// Seconds until the approval expires, floored at [`MIN_TTL_SECS`].
fn record_ttl_secs(parked: &ParkedApproval) -> u64 {
    let remaining = (parked.expires_at - chrono::Utc::now()).num_seconds();
    u64::try_from(remaining).unwrap_or(0).max(MIN_TTL_SECS)
}

fn decode(json: &str) -> Result<ParkedApproval, SessionStoreError> {
    let record: ParkedApprovalRecord =
        serde_json::from_str(json).map_err(|e| SessionStoreError::Decode {
            reason: e.to_string(),
        })?;
    ParkedApproval::try_from(record).map_err(|e| SessionStoreError::Decode {
        reason: e.to_string(),
    })
}
