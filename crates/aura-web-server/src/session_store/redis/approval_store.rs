//! Redis-backed HITL approval store: a parked approval is readable and
//! resolvable from any instance, and a resolution leaves a durable decision
//! record the parking instance can recover if the bus wake is lost.
//!
//! Key schema (all under the configured `key_prefix`, default `aura`):
//!
//! | Key                                    | Type                 | Purpose                  |
//! | -------------------------------------- | -------------------- | ------------------------ |
//! | `{p}:approval:{decision_id}`           | string (record JSON) | parked approval record   |
//! | `{p}:approval:decision:{decision_id}`  | string (record JSON) | recorded decision        |
//! | `{p}:approval:req:{request_id}`        | set of decision ids  | `cancel_request` fan-out |
//!
//! Approval records carry a TTL derived from the approval's `expires_at`, so
//! abandoned entries self-clean; the parking instance's await remains the
//! authoritative timeout. Decision records keep a margin past the parked
//! record's remaining TTL, covering the parking instance's deadline-backstop
//! read. The request index is refreshed on every register with a margin over
//! the record TTL and pruned best-effort on resolve/remove. The cancel sweep
//! prunes per id, never the whole index key; `SWEEP_TAKE_SCRIPT` states what
//! each id yields.
//! `list_pending` SCANs the parked-record keys in batches (never KEYS),
//! skipping decision and index keys by segment and wrong-typed or
//! undecodable records per key; the native TTL is the primary expiry, with
//! a post-decode filter as defense in depth.

use std::sync::LazyLock;

use async_trait::async_trait;
use aura::hitl::{DecisionId, ParkedApproval, ResolveError, ResolvedDecision};
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
const SCAN_BATCH_SIZE: usize = 200;
/// Type-guarded GET for the `list_pending` scan: a string key's value,
/// integer 0 for a wrong-typed key (the caller warns and skips), nil for a
/// key that expired or resolved between SCAN and here. A bare GET maps the
/// server's WRONGTYPE to an extension error that would fail the whole scan.
static TYPED_GET_SCRIPT: &str = r#"
if redis.call('TYPE', KEYS[1]).ok == 'string' then
    return redis.call('GET', KEYS[1])
end
if redis.call('EXISTS', KEYS[1]) == 1 then
    return 0
end
return nil
"#;
/// Key prefixes under `{p}:approval:` that are not parked records: the
/// recorded decisions and the `cancel_request` index sets. Matched against
/// the key remainder after the `{p}:approval:` prefix is stripped.
const DECISION_KEY_SEGMENT: &str = "decision:";
const REQ_KEY_SEGMENT: &str = "req:";

/// Sweep one approval key (KEYS[1]) out of its request index (KEYS[2]):
/// a string key is GETDEL'd and its id SREM'd; a wrong-typed key returns 0
/// and keeps its index entry for a later sweep; an absent key is a stale
/// index entry, SREM'd.
static SWEEP_TAKE_SCRIPT: &str = r#"
if redis.call('TYPE', KEYS[1]).ok == 'string' then
    local record = redis.call('GETDEL', KEYS[1])
    redis.call('SREM', KEYS[2], ARGV[1])
    return record
end
if redis.call('EXISTS', KEYS[1]) == 1 then
    return 0
end
redis.call('SREM', KEYS[2], ARGV[1])
return nil
"#;

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

    /// `GETDEL` a record and prune its request index best-effort, returning
    /// the raw payload; `None` means no live entry existed. An unparseable
    /// payload keeps its index entry until the index key's TTL.
    async fn take_json(&self, id: &str) -> Result<Option<String>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let payload: Option<String> = redis::cmd("GETDEL")
            .arg(self.approval_key(id))
            .query_async(&mut conn)
            .await
            .map_err(request_err)?;
        if let Some(json) = &payload {
            self.prune_req_index(id, json).await;
        }
        Ok(payload)
    }

    /// Drop a taken record's id from its request index, best-effort.
    async fn prune_req_index(&self, id: &str, record_json: &str) {
        if let Ok(record) = serde_json::from_str::<ParkedApprovalRecord>(record_json) {
            let mut conn = self.conn.clone();
            let _: Result<(), _> = conn.srem(self.req_key(&record.request_id), id).await;
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
        decision: ResolvedDecision,
    ) -> Result<(), ResolveError> {
        // The script's atomic take is the at-most-once guarantee: exactly one
        // resolver gets the record; everyone else (and every later attempt)
        // sees `NotFound`. The same step writes the decision record — the
        // serialized record carries the decision AND any captured identity,
        // so one atomic SET keeps the pair together under concurrency.
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
        self.prune_req_index(&id.to_string(), &json).await;
        Ok(())
    }

    async fn decision(
        &self,
        id: &DecisionId,
    ) -> Result<Option<ResolvedDecision>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let payload: Option<String> = conn
            .get(self.decision_key(&id.to_string()))
            .await
            .map_err(request_err)?;
        payload
            .map(|json| {
                serde_json::from_str::<DecisionRecord>(&json)
                    .map_err(|e| SessionStoreError::Decode {
                        reason: e.to_string(),
                    })
                    .and_then(|record| {
                        ResolvedDecision::try_from(record).map_err(|e| SessionStoreError::Decode {
                            reason: e.to_string(),
                        })
                    })
            })
            .transpose()
    }

    async fn remove(&self, id: &DecisionId) -> Result<(), SessionStoreError> {
        // No decode: a corrupt payload must not fail a removal already done.
        self.take_json(&id.to_string()).await.map(|_| ())
    }

    async fn cancel_request(
        &self,
        request_id: &str,
    ) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let req_key = self.req_key(request_id);
        let mut conn = self.conn.clone();
        let ids: Vec<String> = conn.smembers(&req_key).await.map_err(request_err)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // One atomic pipe: a mid-sweep failure cannot drop an
        // already-cleared prefix. The script owns index membership and only
        // sweeps the ids this SMEMBERS saw, so a registration the index
        // gained after it stays discoverable by a later sweep.
        let mut pipe = redis::pipe();
        pipe.atomic();
        for id in &ids {
            pipe.cmd("EVAL")
                .arg(SWEEP_TAKE_SCRIPT)
                .arg(2)
                .arg(self.approval_key(id))
                .arg(&req_key)
                .arg(id);
        }
        let payloads: Vec<Option<redis::Value>> =
            pipe.query_async(&mut conn).await.map_err(request_err)?;

        let mut cleared = Vec::new();
        for (id, payload) in ids.into_iter().zip(payloads) {
            match payload {
                None => {}
                Some(redis::Value::Int(0)) => tracing::warn!(
                    decision_id = %id,
                    "wrong-typed approval key left in place; index entry kept"
                ),
                Some(value) => {
                    let Some(json) = swept_record_json(&id, value) else {
                        continue;
                    };
                    match decode(&json) {
                        Ok(parked) => cleared.push(parked),
                        Err(err) => tracing::warn!(
                            decision_id = %id, error = %err,
                            "undecodable approval record skipped by cancel_request"
                        ),
                    }
                }
            }
        }
        Ok(cleared)
    }

    async fn list_pending(&self) -> Result<Vec<ParkedApproval>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let pattern = format!("{}:approval:*", self.key_prefix);

        // SCAN in batches, never KEYS: a scan must not block the server.
        // A mutating keyspace can hand a key back twice; dedupe before GET.
        let mut cursor: u64 = 0;
        let mut keys = std::collections::HashSet::new();
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .cursor_arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(SCAN_BATCH_SIZE)
                .query_async(&mut conn)
                .await
                .map_err(request_err)?;
            keys.extend(batch);
            cursor = next;
            if cursor == 0 {
                break;
            }
        }

        let now = chrono::Utc::now();
        let mut pending = Vec::new();
        for key in keys {
            // Strip the configured prefix before the subspace test: a prefix
            // containing ":decision:" or ":req:" must not exclude every key.
            let Some(rest) = key.strip_prefix(format!("{}:approval:", self.key_prefix).as_str())
            else {
                continue;
            };
            if rest.starts_with(DECISION_KEY_SEGMENT) || rest.starts_with(REQ_KEY_SEGMENT) {
                continue;
            }
            let value = redis::cmd("EVAL")
                .arg(TYPED_GET_SCRIPT)
                .arg(1)
                .arg(&key)
                .query_async::<redis::Value>(&mut conn)
                .await
                .map_err(request_err)?;
            let json = match value {
                // Expired or resolved between SCAN and GET: nothing to list.
                redis::Value::Nil => continue,
                redis::Value::Int(0) => {
                    tracing::warn!(key = %key, "wrong-typed approval key skipped by list_pending");
                    continue;
                }
                redis::Value::BulkString(bytes) => match String::from_utf8(bytes) {
                    Ok(json) => json,
                    Err(err) => {
                        tracing::warn!(
                            key = %key, error = %err,
                            "non-UTF8 approval record skipped by list_pending"
                        );
                        continue;
                    }
                },
                _ => {
                    tracing::warn!(key = %key, "unexpected approval key value skipped by list_pending");
                    continue;
                }
            };
            let parked = match decode(&json) {
                Ok(parked) => parked,
                Err(err) => {
                    tracing::warn!(
                        key = %key, error = %err,
                        "undecodable approval record skipped by list_pending"
                    );
                    continue;
                }
            };
            // Native TTL is the primary expiry; the contract filter repeats
            // here so a record inside its MIN_TTL_SECS floor past
            // `expires_at` is never listed.
            if parked.expires_at > now {
                pending.push(parked);
            }
        }
        Ok(pending)
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

/// Decode one swept reply into record JSON, skipping entries the cancel can
/// no longer deliver: non-UTF-8 bytes (that record is consumed
/// unrecoverably, like a decode failure) or an unexpected reply kind.
fn swept_record_json(id: &str, value: redis::Value) -> Option<String> {
    match value {
        redis::Value::BulkString(bytes) => match String::from_utf8(bytes) {
            Ok(json) => Some(json),
            Err(err) => {
                tracing::warn!(
                    decision_id = %id, error = %err,
                    "non-UTF-8 approval record skipped by cancel_request"
                );
                None
            }
        },
        _other => {
            tracing::warn!(
                decision_id = %id,
                "unexpected approval record value kind skipped by cancel_request"
            );
            None
        }
    }
}
