//! Redis-backed HITL approval store: a parked approval is readable and
//! resolvable from any instance, and a resolution leaves a durable decision
//! record the parking instance can recover if the bus wake is lost.
//!
//! Key schema (all under the configured `key_prefix`, default `aura`):
//!
//! | Key                                    | Type                 | Purpose                        |
//! | -------------------------------------- | -------------------- | ------------------------------ |
//! | `{p}:approval:{decision_id}`           | string (record JSON) | parked approval record         |
//! | `{p}:approval:decision:{decision_id}`  | string (record JSON) | recorded decision              |
//! | `{p}:approval:req:{request_id}`        | set of decision ids  | `cancel_request` fan-out       |
//!
//! Approval records carry a TTL derived from the approval's `expires_at`, so
//! abandoned entries self-clean; the parking instance's await remains the
//! authoritative timeout. Decision records keep a margin past the parked
//! record's remaining TTL, covering the parking instance's deadline-backstop
//! read. The request index is refreshed on every register with a margin over
//! the record TTL and pruned best-effort on resolve/remove; a stale indexed id
//! only costs `cancel_request` a `DEL` of a missing key.

use std::sync::LazyLock;

use async_trait::async_trait;
use aura::hitl::{ApprovalDecision, DecisionId, ParkedApproval, ResolveError};
use aura::session_store::{ApprovalStore, DecisionRecord, ParkedApprovalRecord, SessionStoreError};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use super::request_err;

/// Floor for a record's TTL, so an approval registered at (or past) its expiry
/// still exists for the in-flight resolve or get that raced it.
const MIN_TTL_SECS: u64 = 1;
/// Margin the request index's TTL keeps over its newest record's TTL.
const REQ_INDEX_TTL_MARGIN_SECS: u64 = 60;
/// Decision TTL margin over the parked record's remaining TTL.
const DECISION_TTL_MARGIN_MS: u64 = 60_000;

/// Atomic script for the at-most-once claim and durable decision write.
static RESOLVE_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| {
    redis::Script::new(
        r#"
        local record = redis.call('GET', KEYS[1])
        if not record then
            return nil
        end
        local ttl_ms = redis.call('PTTL', KEYS[1])
        if ttl_ms < 0 then
            ttl_ms = 0
        end
        redis.call('DEL', KEYS[1])
        redis.call('SET', KEYS[2], ARGV[1], 'PX', ttl_ms + tonumber(ARGV[2]))
        return record
        "#,
    )
});

/// Atomic script for the durable resolution: first-write-wins on the decision
/// key, with the parked approval left in place.
static RESOLVE_DURABLE_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| {
    redis::Script::new(
        r#"
        if redis.call('EXISTS', KEYS[1]) == 0 then
            return nil
        end
        local ttl_ms = redis.call('PTTL', KEYS[1])
        if ttl_ms < 0 then
            ttl_ms = 0
        end
        redis.call('SET', KEYS[2], ARGV[1], 'NX', 'PX', ttl_ms + tonumber(ARGV[2]))
        return redis.call('GET', KEYS[2])
        "#,
    )
});

/// Atomic removal of an approval together with the decision recorded for it,
/// returning the approval payload so the request index can be pruned. Two
/// commands would leave a window — and a crash between them a permanent
/// state — where a decision is readable for an approval that is gone.
static REMOVE_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| {
    redis::Script::new(
        r#"
        local record = redis.call('GET', KEYS[1])
        redis.call('DEL', KEYS[1], KEYS[2])
        return record
        "#,
    )
});

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

    fn decision_key(&self, decision_id: &str) -> String {
        format!("{}:approval:decision:{decision_id}", self.key_prefix)
    }

    fn req_key(&self, request_id: &str) -> String {
        format!("{}:approval:req:{request_id}", self.key_prefix)
    }

    /// Drop a taken record's id from its request index, best-effort.
    async fn prune_req_index(&self, id: &DecisionId, record_json: &str) {
        if let Ok(record) = serde_json::from_str::<ParkedApprovalRecord>(record_json) {
            let mut conn = self.conn.clone();
            let _: Result<(), _> = conn
                .srem(self.req_key(&record.request_id), id.to_string())
                .await;
        }
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
        decision: ApprovalDecision,
    ) -> Result<(), ResolveError> {
        // The script's atomic take is the at-most-once guarantee: exactly one
        // resolver gets the record; everyone else (and every later attempt)
        // sees `NotFound`. The same step writes the decision record, so a
        // consumed parked entry always leaves a recoverable decision.
        let payload = serde_json::to_string(&DecisionRecord::from(&decision))
            .expect("decision record serializes to JSON");
        let mut conn = self.conn.clone();
        let taken: Option<String> = RESOLVE_SCRIPT
            .key(self.approval_key(&id.to_string()))
            .key(self.decision_key(&id.to_string()))
            .arg(payload)
            .arg(DECISION_TTL_MARGIN_MS)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| ResolveError::Store(request_err(e)))?;
        let Some(json) = taken else {
            return Err(ResolveError::NotFound);
        };
        self.prune_req_index(id, &json).await;
        Ok(())
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ApprovalDecision>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let payload: Option<String> = conn
            .get(self.decision_key(&id.to_string()))
            .await
            .map_err(request_err)?;
        payload
            .map(|json| {
                serde_json::from_str::<DecisionRecord>(&json)
                    .map(ApprovalDecision::from)
                    .map_err(|e| SessionStoreError::Decode {
                        reason: e.to_string(),
                    })
            })
            .transpose()
    }

    async fn resolve_durable(
        &self,
        id: &DecisionId,
        decision: ApprovalDecision,
    ) -> Result<aura::orchestration::park::WakeReason, ResolveError> {
        use aura::orchestration::park::WakeReason;

        let payload = serde_json::to_string(&DecisionRecord::from(&decision))
            .expect("decision record serializes to JSON");
        let mut conn = self.conn.clone();
        let stored: Option<String> = RESOLVE_DURABLE_SCRIPT
            .key(self.approval_key(&id.to_string()))
            .key(self.decision_key(&id.to_string()))
            .arg(payload)
            .arg(DECISION_TTL_MARGIN_MS)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| ResolveError::Store(request_err(e)))?;
        let Some(json) = stored else {
            return Err(ResolveError::NotFound);
        };

        // The payload is whatever the key holds after the script's `NX`
        // write, so deriving the wake from it — rather than from this call's
        // own clock — is what makes a later caller agree with the first.
        let record: DecisionRecord = serde_json::from_str(&json).map_err(|e| {
            ResolveError::Store(SessionStoreError::Decode {
                reason: e.to_string(),
            })
        })?;
        Ok(WakeReason::DecisionResolved {
            decision_id: *id,
            resolved_at: record.decided_at,
        })
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        let mut conn = self.conn.clone();
        let payload: Option<String> = REMOVE_SCRIPT
            .key(self.approval_key(&id.to_string()))
            .key(self.decision_key(&id.to_string()))
            .invoke_async(&mut conn)
            .await
            .map_err(request_err)?;
        if let Some(json) = payload {
            self.prune_req_index(id, &json).await;
        }
        Ok(())
    }

    async fn cancel_request(&self, request_id: &str) -> Result<(), SessionStoreError> {
        let req_key = self.req_key(request_id);
        let mut conn = self.conn.clone();
        let ids: Vec<String> = conn.smembers(&req_key).await.map_err(request_err)?;

        // MULTI/EXEC, so each approval and its decision go in the same
        // transaction: nothing observes, and no crash strands, a decision
        // whose approval the same cancellation already deleted.
        let mut pipe = redis::pipe();
        pipe.atomic();
        for id in &ids {
            pipe.del(self.approval_key(id)).ignore();
            pipe.del(self.decision_key(id)).ignore();
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
