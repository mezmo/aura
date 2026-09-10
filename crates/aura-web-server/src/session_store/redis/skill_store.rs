//! Redis-backed skill-invocation store: a skill loaded through one instance
//! rehydrates on whichever instance serves the session's next turn.
//!
//! Key schema (all under the configured `key_prefix`, default `aura`):
//!
//! | Key                       | Type                                  | Purpose                    |
//! | ------------------------- | ------------------------------------- | -------------------------- |
//! | `{p}:skills:{session_id}` | hash: dedup key → record JSON         | session's invocation log   |
//!
//! Writes use `HSETNX`, so the first record for a dedup key wins — the
//! store-level idempotency the [`SkillInvocationStore`] contract requires —
//! and a new key is refused once the hash holds
//! [`MAX_SKILL_RECORDS_PER_SESSION`] entries. Every write refreshes the hash
//! TTL, so an active session's log lives as long as the session keeps
//! invoking skills plus the configured TTL, and an abandoned session
//! self-cleans. Entries that fail to decode, including records on another
//! schema version, are skipped with a warning rather than failing the
//! listing, tolerating skew during a rolling deploy.

use std::num::NonZeroU64;

use async_trait::async_trait;
use aura::config::SessionId;
use aura::session_store::{
    MAX_SKILL_RECORDS_PER_SESSION, SessionStoreError, SkillInvocationRecord, SkillInvocationStore,
};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use super::request_err;

pub struct RedisSkillInvocationStore {
    conn: ConnectionManager,
    key_prefix: String,
    ttl_secs: Option<NonZeroU64>,
}

impl RedisSkillInvocationStore {
    pub fn new(conn: ConnectionManager, key_prefix: &str, ttl_secs: Option<NonZeroU64>) -> Self {
        Self {
            conn,
            key_prefix: key_prefix.to_string(),
            ttl_secs,
        }
    }

    fn session_key(&self, session_id: &SessionId) -> String {
        format!("{}:skills:{}", self.key_prefix, session_id.as_str())
    }
}

#[async_trait]
impl SkillInvocationStore for RedisSkillInvocationStore {
    async fn record(
        &self,
        session_id: &SessionId,
        record: SkillInvocationRecord,
    ) -> Result<(), SessionStoreError> {
        let payload =
            serde_json::to_string(&record).expect("skill invocation record serializes to JSON");
        let key = self.session_key(session_id);
        let dedup_key = record.invocation.dedup_key();
        let mut conn = self.conn.clone();

        // The cap is a soft bound: two instances admitting a new key at the
        // cap in the same instant can both pass this read. A dedup key the
        // hash already holds is never refused, so a re-invocation at the cap
        // still refreshes the TTL.
        let (len, exists): (usize, bool) = redis::pipe()
            .hlen(&key)
            .hexists(&key, &dedup_key)
            .query_async(&mut conn)
            .await
            .map_err(request_err)?;
        if !exists && len >= MAX_SKILL_RECORDS_PER_SESSION {
            tracing::warn!(
                session_id = session_id.as_str(),
                cap = MAX_SKILL_RECORDS_PER_SESSION,
                "skill invocation store at per-session capacity; dropping new record"
            );
            return Ok(());
        }

        let mut pipe = redis::pipe();
        pipe.hset_nx(&key, dedup_key, payload).ignore();
        if let Some(ttl) = self.ttl_secs {
            pipe.expire(&key, ttl.get() as i64).ignore();
        }
        pipe.query_async::<()>(&mut conn).await.map_err(request_err)
    }

    async fn list(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SkillInvocationRecord>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let payloads: Vec<String> = conn
            .hvals(self.session_key(session_id))
            .await
            .map_err(request_err)?;

        let mut records: Vec<SkillInvocationRecord> = payloads
            .iter()
            .filter_map(|json| match SkillInvocationRecord::decode(json) {
                Ok(record) => Some(record),
                Err(e) => {
                    tracing::warn!(
                        session_id = session_id.as_str(),
                        "skipping skill invocation record: {e}"
                    );
                    None
                }
            })
            .collect();
        records.sort_by_key(|r| (r.anchor, r.seq));
        Ok(records)
    }
}
