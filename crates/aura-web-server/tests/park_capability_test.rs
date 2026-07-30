#![cfg(feature = "integration-session-store")]

//! Staged: durable parking on a backend without the run-store capability.
//!
//! Requires a reachable Redis/Valkey; point `AURA_TEST_REDIS_URL` at it
//! (default `redis://127.0.0.1:6379`), same as `redis_session_store_test`.

use aura_config::{RedisSessionStoreConfig, SessionStoreBackend};
use aura_web_server::session_store::{RedisSessionStore, SessionStore, run_store_for_parking};

fn redis_url() -> String {
    std::env::var("AURA_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

fn test_config() -> RedisSessionStoreConfig {
    RedisSessionStoreConfig {
        url: redis_url(),
        key_prefix: format!("aura:test:{}", uuid::Uuid::new_v4()),
        connect_timeout: std::time::Duration::from_secs(5),
        task_ttl_secs: std::num::NonZeroU64::new(60),
    }
}

/// Staged: the redis backend hands out no run store (its run-store
/// implementation is #325), so a redis-configured deployment must refuse
/// durable parking cleanly, naming the backend and the missing capability —
/// never fall back to in-request parking, never fail with an unrelated error.
#[tokio::test]
async fn redis_backend_without_run_store_refuses_durable_parking_by_name() {
    let store = RedisSessionStore::connect(&test_config())
        .await
        .expect("failed to connect to redis; is it running? (see AURA_TEST_REDIS_URL)");

    assert!(
        store.runs().is_none(),
        "the redis backend must not claim a run-store capability it does not have",
    );

    let refusal = match run_store_for_parking(&store) {
        Err(refusal) => refusal,
        Ok(_) => panic!("durable parking must refuse without the run-store capability"),
    };
    assert_eq!(refusal.backend, SessionStoreBackend::Redis);
    let message = refusal.to_string();
    assert!(
        message.contains("run-store"),
        "the refusal must name the missing capability: {message}",
    );
    assert!(
        message.contains("redis"),
        "the refusal must name the backend: {message}",
    );
}
