#![cfg(feature = "integration-session-store")]

//! Integration tests for the Redis/Valkey session-store backend against a
//! live server (`docs/design/session-storage.md` §12).
//!
//! Requires a reachable Redis/Valkey; point `AURA_TEST_REDIS_URL` at it
//! (default `redis://127.0.0.1:6379`). Locally:
//!
//! ```sh
//! docker run --rm -d -p 6379:6379 valkey/valkey:8
//! cargo test -p aura-web-server --features integration-session-store --test redis_session_store_test
//! ```
//!
//! Each test namespaces its keys under a unique prefix with a short TTL, so
//! tests neither collide nor leave state behind.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use a2a::{ListTasksRequest, Message, Part, Role, Task, TaskState, TaskStatus};
use aura::hitl::{ApprovalDecision, ApprovalOutcome, PendingApprovals, ResolveError};
use aura::request_cancellation::RequestCancelToken;
use aura::session_store::ParkedApprovalRecord;
use aura_config::{RedisSessionStoreConfig, SessionStoreBackend};
use aura_web_server::session_store::{RedisSessionStore, SessionStore};
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use common::make_parked;

fn redis_url() -> String {
    std::env::var("AURA_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

fn test_config(task_ttl_secs: u64) -> RedisSessionStoreConfig {
    RedisSessionStoreConfig {
        url: redis_url(),
        key_prefix: format!("aura:test:{}", uuid::Uuid::new_v4()),
        connect_timeout: std::time::Duration::from_secs(5),
        task_ttl_secs: std::num::NonZeroU64::new(task_ttl_secs),
    }
}

async fn connect(config: &RedisSessionStoreConfig) -> RedisSessionStore {
    RedisSessionStore::connect(config)
        .await
        .expect("failed to connect to redis; is it running? (see AURA_TEST_REDIS_URL)")
}

fn make_task(id: &str, ctx: &str, state: TaskState) -> Task {
    Task {
        id: id.to_string(),
        context_id: ctx.to_string(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        artifacts: None,
        history: None,
        metadata: None,
    }
}

fn list_req() -> ListTasksRequest {
    ListTasksRequest {
        context_id: None,
        status: None,
        page_size: None,
        page_token: None,
        history_length: None,
        status_timestamp_after: None,
        include_artifacts: None,
        tenant: None,
    }
}

#[tokio::test]
async fn ping_succeeds() {
    let store = connect(&test_config(60)).await;
    store.ping().await.expect("ping failed");
    assert_eq!(store.backend(), SessionStoreBackend::Redis);
}

#[tokio::test]
async fn connect_to_unreachable_backend_fails_fast() {
    let config = RedisSessionStoreConfig {
        url: "redis://127.0.0.1:1".to_string(),
        connect_timeout: std::time::Duration::from_secs(1),
        ..test_config(60)
    };
    let started = std::time::Instant::now();
    let result = RedisSessionStore::connect(&config).await;
    assert!(result.is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "connect did not fail fast: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn create_get_roundtrip_preserves_task() {
    let tasks = connect(&test_config(60)).await.tasks();

    let mut task = make_task("t1", "c1", TaskState::Submitted);
    task.history = Some(vec![
        Message::new(Role::User, vec![Part::text("hello")]),
        Message::new(Role::Agent, vec![Part::text("hi")]),
    ]);
    task.metadata = Some(std::collections::HashMap::from([(
        "k".to_string(),
        serde_json::json!("v"),
    )]));

    let version = tasks.create(task.clone()).await.unwrap();
    assert_eq!(version, 1);

    let got = tasks.get("t1").await.unwrap().expect("task not found");
    assert_eq!(got.id, "t1");
    assert_eq!(got.context_id, "c1");
    assert_eq!(got.status.state, TaskState::Submitted);
    assert_eq!(got.history.as_ref().unwrap().len(), 2);
    assert_eq!(
        got.metadata,
        Some(std::collections::HashMap::from([(
            "k".to_string(),
            serde_json::json!("v"),
        )]))
    );
}

#[tokio::test]
async fn duplicate_create_is_rejected() {
    let tasks = connect(&test_config(60)).await.tasks();
    let task = make_task("t1", "c1", TaskState::Submitted);
    tasks.create(task.clone()).await.unwrap();
    assert!(tasks.create(task).await.is_err());
}

#[tokio::test]
async fn update_bumps_version_and_replaces_task() {
    let tasks = connect(&test_config(60)).await.tasks();
    tasks
        .create(make_task("t1", "c1", TaskState::Submitted))
        .await
        .unwrap();

    let v2 = tasks
        .update(make_task("t1", "c1", TaskState::Working))
        .await
        .unwrap();
    assert_eq!(v2, 2);
    let v3 = tasks
        .update(make_task("t1", "c1", TaskState::Completed))
        .await
        .unwrap();
    assert_eq!(v3, 3);

    let got = tasks.get("t1").await.unwrap().unwrap();
    assert_eq!(got.status.state, TaskState::Completed);
}

#[tokio::test]
async fn update_of_unknown_task_is_not_found() {
    let tasks = connect(&test_config(60)).await.tasks();
    assert!(
        tasks
            .update(make_task("missing", "c1", TaskState::Working))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn get_of_unknown_task_is_none() {
    let tasks = connect(&test_config(60)).await.tasks();
    assert!(tasks.get("missing").await.unwrap().is_none());
}

#[tokio::test]
async fn list_filters_by_context_and_status() {
    let tasks = connect(&test_config(60)).await.tasks();
    tasks
        .create(make_task("t1", "c1", TaskState::Submitted))
        .await
        .unwrap();
    tasks
        .create(make_task("t2", "c2", TaskState::Working))
        .await
        .unwrap();
    tasks
        .create(make_task("t3", "c1", TaskState::Completed))
        .await
        .unwrap();

    let by_ctx = tasks
        .list(&ListTasksRequest {
            context_id: Some("c1".to_string()),
            ..list_req()
        })
        .await
        .unwrap();
    assert_eq!(by_ctx.total_size, 2);
    let ids: Vec<&str> = by_ctx.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["t1", "t3"]);

    let by_status = tasks
        .list(&ListTasksRequest {
            status: Some(TaskState::Working),
            ..list_req()
        })
        .await
        .unwrap();
    assert_eq!(by_status.total_size, 1);
    assert_eq!(by_status.tasks[0].id, "t2");

    let all = tasks.list(&list_req()).await.unwrap();
    assert_eq!(all.total_size, 3);
}

#[tokio::test]
async fn list_paginates_with_offset_tokens() {
    let tasks = connect(&test_config(60)).await.tasks();
    for i in 0..5 {
        tasks
            .create(make_task(&format!("t{i}"), "c1", TaskState::Submitted))
            .await
            .unwrap();
    }

    let page1 = tasks
        .list(&ListTasksRequest {
            page_size: Some(2),
            ..list_req()
        })
        .await
        .unwrap();
    assert_eq!(page1.tasks.len(), 2);
    assert_eq!(page1.total_size, 5);
    assert!(!page1.next_page_token.is_empty());

    let page2 = tasks
        .list(&ListTasksRequest {
            page_size: Some(2),
            page_token: Some(page1.next_page_token),
            ..list_req()
        })
        .await
        .unwrap();
    let ids: Vec<&str> = page2.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["t2", "t3"]);
}

#[tokio::test]
async fn list_truncates_history_to_newest() {
    let tasks = connect(&test_config(60)).await.tasks();
    let mut task = make_task("t1", "c1", TaskState::Working);
    task.history = Some(vec![
        Message::new(Role::User, vec![Part::text("1")]),
        Message::new(Role::Agent, vec![Part::text("2")]),
        Message::new(Role::User, vec![Part::text("3")]),
    ]);
    tasks.create(task).await.unwrap();

    let resp = tasks
        .list(&ListTasksRequest {
            history_length: Some(1),
            ..list_req()
        })
        .await
        .unwrap();
    assert_eq!(resp.tasks[0].history.as_ref().unwrap().len(), 1);
}

/// Cross-instance simulation (§12): two independent backend handles sharing one
/// Redis — a task created through "instance A" is visible to "instance B" by get, list,
/// and history-by-context, and B can update it.
#[tokio::test]
async fn task_created_on_one_instance_is_visible_and_updatable_on_another() {
    let config = test_config(60);
    let instance_a = connect(&config).await.tasks();
    let instance_b = connect(&config).await.tasks();

    let mut task = make_task("t1", "ctx-shared", TaskState::Working);
    task.history = Some(vec![Message::new(Role::User, vec![Part::text("q")])]);
    instance_a.create(task).await.unwrap();

    let got = instance_b
        .get("t1")
        .await
        .unwrap()
        .expect("instance B cannot see t1");
    assert_eq!(got.status.state, TaskState::Working);
    assert_eq!(got.history.as_ref().unwrap().len(), 1);

    let by_ctx = instance_b
        .list(&ListTasksRequest {
            context_id: Some("ctx-shared".to_string()),
            ..list_req()
        })
        .await
        .unwrap();
    assert_eq!(by_ctx.total_size, 1);

    let v2 = instance_b
        .update(make_task("t1", "ctx-shared", TaskState::Completed))
        .await
        .unwrap();
    assert_eq!(v2, 2);
    let got = instance_a.get("t1").await.unwrap().unwrap();
    assert_eq!(got.status.state, TaskState::Completed);
}

/// Expired tasks disappear from `get`, and `list` prunes their ids from the
/// index instead of erroring on the missing record.
#[tokio::test]
async fn expired_task_is_gone_and_pruned_from_list() {
    let tasks = connect(&test_config(1)).await.tasks();
    tasks
        .create(make_task("t1", "c1", TaskState::Submitted))
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(1600)).await;

    assert!(tasks.get("t1").await.unwrap().is_none());
    let resp = tasks.list(&list_req()).await.unwrap();
    assert_eq!(resp.total_size, 0);
}

// ---------------------------------------------------------------------------
// HITL approval store
// ---------------------------------------------------------------------------

// The backend-agnostic battery lives in `tests/common`; each test below
// wires it to two live Redis connections.

#[tokio::test]
async fn approval_register_get_roundtrip_preserves_record() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();
    common::register_get_roundtrip(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn approval_resolve_is_at_most_once_across_instances() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();
    common::resolve_is_at_most_once(&instance_a, &instance_b).await;
}

/// Redis-specific: the consumed ticket is gone from the store. The file
/// backend instead moves the ticket into the decision file and retains it
/// until `remove` (§2.5).
#[tokio::test]
async fn approval_resolve_removes_the_parked_record() {
    let approvals = connect(&test_config(60)).await.approvals();
    let parked = make_parked("req-consumed", Duration::from_secs(60));
    let id = parked.request.decision_id;
    approvals.register(parked).await.unwrap();

    approvals
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .unwrap();

    assert!(approvals.get(&id).await.unwrap().is_none());
}

#[tokio::test]
async fn approval_concurrent_resolves_have_exactly_one_winner() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();
    common::concurrent_resolves_have_exactly_one_winner(&instance_a, &instance_b).await;
}

/// A resolution leaves a durable decision record readable from any instance (issue #474).
#[tokio::test]
async fn approval_resolve_records_decision_readable_cross_instance() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();
    common::resolve_records_readable_decision(&instance_a, &instance_b).await;
}

/// The decision record keeps identity and decision together across
/// instances.
#[tokio::test]
async fn approval_resolve_records_identity_readable_cross_instance() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();
    common::resolve_records_identity_with_the_decision(&instance_a, &instance_b).await;
}

/// The decision record's TTL keeps a margin past the parked record's.
#[tokio::test]
async fn decision_record_outlives_parked_record_ttl() {
    let approvals = connect(&test_config(60)).await.approvals();
    let parked = make_parked("req-margin", Duration::from_secs(1));
    let id = parked.request.decision_id;
    approvals.register(parked).await.unwrap();
    approvals
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(1600)).await;

    assert_eq!(
        approvals.decision(&id).await.unwrap(),
        Some(ApprovalDecision::Approved.into())
    );
}

#[tokio::test]
async fn approval_remove_makes_resolve_not_found() {
    let approvals = connect(&test_config(60)).await.approvals();
    common::remove_makes_resolve_not_found(&approvals).await;
}

#[tokio::test]
async fn approval_cancel_request_removes_only_matching() {
    let approvals = connect(&test_config(60)).await.approvals();
    common::cancel_request_removes_only_matching(&approvals).await;
}

#[tokio::test]
async fn approval_list_pending_returns_only_live_undecided() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();
    common::list_pending_returns_only_live_undecided(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn approval_list_pending_empty_store_returns_empty() {
    let approvals = connect(&test_config(60)).await.approvals();
    common::list_pending_empty_store_returns_empty(&approvals).await;
}

/// An expired record still inside its MIN_TTL_SECS floor must not be
/// listed: the post-decode filter backs the native TTL.
#[tokio::test]
async fn approval_list_pending_excludes_expired() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();
    common::list_pending_excludes_expired(&instance_a, &instance_b).await;
}

#[tokio::test]
async fn approval_list_pending_skips_wrong_typed_and_undecodable_keys() {
    let config = test_config(60);
    let approvals = connect(&config).await.approvals();
    let parked = make_parked("req-list-skips", std::time::Duration::from_secs(60));
    let live_id = parked.request.decision_id;
    approvals.register(parked).await.unwrap();

    let client = redis::Client::open(redis_url()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let prefix = &config.key_prefix;
    let wrong_typed = format!("{prefix}:approval:{}", uuid::Uuid::new_v4());
    redis::cmd("SADD")
        .arg(&wrong_typed)
        .arg("x")
        .query_async::<()>(&mut conn)
        .await
        .unwrap();
    let undecodable = format!("{prefix}:approval:{}", uuid::Uuid::new_v4());
    redis::cmd("SET")
        .arg(&undecodable)
        .arg("not a parked approval")
        .query_async::<()>(&mut conn)
        .await
        .unwrap();

    let pending = approvals.list_pending().await.unwrap();

    let ids: Vec<_> = pending.iter().map(|p| p.request.decision_id).collect();
    assert_eq!(ids, [live_id], "only the genuine record is listed");
}

/// `cancel_request` returns exactly the records it cleared; a decided
/// sibling of the same owner is absent, and a cleared ticket refuses a later
/// resolve.
#[tokio::test]
async fn approval_cancel_request_returns_cleared_set() {
    let config = test_config(60);
    let approvals = connect(&config).await.approvals();
    let undecided = make_parked("req-cancel-return", Duration::from_secs(60));
    let undecided_id = undecided.request.decision_id;
    let cleared_record = ParkedApprovalRecord::from(&undecided);
    let decided = make_parked("req-cancel-return", Duration::from_secs(60));
    let decided_id = decided.request.decision_id;
    let keep = make_parked("req-cancel-return-keep", Duration::from_secs(60));
    let keep_id = keep.request.decision_id;
    approvals.register(undecided).await.unwrap();
    approvals.register(decided).await.unwrap();
    approvals.register(keep).await.unwrap();
    approvals
        .resolve(&decided_id, ApprovalDecision::Approved.into())
        .await
        .unwrap();

    let cleared = approvals.cancel_request("req-cancel-return").await.unwrap();

    assert_eq!(cleared.len(), 1, "only the undecided ticket is cleared");
    assert_eq!(
        ParkedApprovalRecord::from(&cleared[0]),
        cleared_record,
        "the cleared record is returned unchanged"
    );
    assert!(approvals.get(&keep_id).await.unwrap().is_some());
    assert_eq!(
        approvals
            .resolve(&undecided_id, ApprovalDecision::Approved.into())
            .await,
        Err(ResolveError::NotFound),
        "a cleared ticket resolves NotFound"
    );

    // Sequential discoverability, not a race pin: a registration added
    // after the cancel returned keeps its index entry, and a second cancel
    // still discovers and takes it. The true SMEMBERS-to-EXEC interleave is
    // pinned by cancel_request_leaves_a_mid_sweep_registration_indexed.
    let late = make_parked("req-cancel-return", Duration::from_secs(60));
    let late_id = late.request.decision_id;
    let late_record = ParkedApprovalRecord::from(&late);
    approvals.register(late).await.unwrap();
    let client = redis::Client::open(redis_url()).unwrap();
    let mut raw = client.get_multiplexed_async_connection().await.unwrap();
    let still_indexed: bool = redis::cmd("SISMEMBER")
        .arg(format!(
            "{}:approval:req:req-cancel-return",
            config.key_prefix
        ))
        .arg(late_id.to_string())
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(
        still_indexed,
        "the first cancel's SREM left the late registration's index entry"
    );

    let cleared_late = approvals.cancel_request("req-cancel-return").await.unwrap();

    assert_eq!(
        cleared_late.len(),
        1,
        "the post-cancel registration is discoverable"
    );
    assert_eq!(
        ParkedApprovalRecord::from(&cleared_late[0]),
        late_record,
        "the late record is returned unchanged"
    );
    assert_eq!(
        approvals
            .resolve(&late_id, ApprovalDecision::Approved.into())
            .await,
        Err(ResolveError::NotFound),
        "the second cancel GETDEL'd the late ticket"
    );
}

/// `remove` drops a record it cannot decode: the entry is already GETDEL'd,
/// so a corrupt payload must not fail the removal with a `Decode` error.
#[tokio::test]
async fn remove_tolerates_an_undecodable_record() {
    let config = test_config(60);
    let approvals = connect(&config).await.approvals();
    let parked = make_parked("req-remove-corrupt", Duration::from_secs(60));
    let id = parked.request.decision_id;
    approvals.register(parked).await.unwrap();

    // Corrupt the record in place, TTL-bounded; its index entry dies with
    // the index key's own TTL.
    let client = redis::Client::open(redis_url()).unwrap();
    let mut raw = client.get_multiplexed_async_connection().await.unwrap();
    redis::cmd("SET")
        .arg(format!("{}:approval:{id}", config.key_prefix))
        .arg("{not valid json")
        .arg("EX")
        .arg(60)
        .query_async::<()>(&mut raw)
        .await
        .unwrap();

    approvals
        .remove(&id)
        .await
        .expect("a corrupt payload must not fail the removal");

    assert!(approvals.get(&id).await.unwrap().is_none());
}

/// One corrupt record must not fail `cancel_request` for its whole request:
/// with a list planted at one approval key the sweep still returns the valid
/// sibling and sweeps its id from the index, while the wrong-typed key and
/// its index entry stay in place for a later sweep to retry; the same holds
/// when the planted record is non-UTF-8 bytes, which the sweep consumes.
#[tokio::test]
async fn cancel_request_skips_a_wrong_type_value_and_returns_valid_records() {
    let config = test_config(60);
    let approvals = connect(&config).await.approvals();
    let req_index_key = format!("{}:approval:req:req-wrong-type", config.key_prefix);
    let client = redis::Client::open(redis_url()).unwrap();
    let mut raw = client.get_multiplexed_async_connection().await.unwrap();

    let valid = make_parked("req-wrong-type", Duration::from_secs(60));
    let valid_id = valid.request.decision_id;
    let valid_record = ParkedApprovalRecord::from(&valid);
    let corrupt = make_parked("req-wrong-type", Duration::from_secs(60));
    let corrupt_id = corrupt.request.decision_id;
    approvals.register(valid).await.unwrap();
    approvals.register(corrupt).await.unwrap();
    let corrupt_key = format!("{}:approval:{corrupt_id}", config.key_prefix);
    redis::pipe()
        .atomic()
        .del(&corrupt_key)
        .ignore()
        .rpush(&corrupt_key, "planted list, not a record")
        .ignore()
        .expire(&corrupt_key, 60)
        .ignore()
        .query_async::<()>(&mut raw)
        .await
        .unwrap();

    let cleared = approvals
        .cancel_request("req-wrong-type")
        .await
        .expect("a wrong-typed record must not fail the sweep");

    assert_eq!(cleared.len(), 1, "only the valid record is cleared");
    assert_eq!(
        ParkedApprovalRecord::from(&cleared[0]),
        valid_record,
        "the valid record is returned unchanged"
    );
    assert!(approvals.get(&valid_id).await.unwrap().is_none());
    assert!(
        !redis::cmd("SISMEMBER")
            .arg(&req_index_key)
            .arg(valid_id.to_string())
            .query_async::<bool>(&mut raw)
            .await
            .unwrap(),
        "the swept record's index entry went with it"
    );
    assert!(
        redis::cmd("SISMEMBER")
            .arg(&req_index_key)
            .arg(corrupt_id.to_string())
            .query_async::<bool>(&mut raw)
            .await
            .unwrap(),
        "the wrong-typed id keeps its index entry"
    );
    assert_eq!(
        redis::cmd("TYPE")
            .arg(&corrupt_key)
            .query_async::<String>(&mut raw)
            .await
            .unwrap(),
        "list",
        "the wrong-typed key is left in place"
    );
    assert_eq!(
        redis::cmd("LRANGE")
            .arg(&corrupt_key)
            .arg(0)
            .arg(-1)
            .query_async::<Vec<String>>(&mut raw)
            .await
            .unwrap(),
        ["planted list, not a record"],
        "the planted list survives the sweep"
    );

    let cleared_again = approvals
        .cancel_request("req-wrong-type")
        .await
        .expect("a retry over a wrong-typed key must not fail the sweep");
    assert!(
        cleared_again.is_empty(),
        "a wrong-typed key alone clears nothing"
    );
    assert!(
        redis::cmd("SISMEMBER")
            .arg(&req_index_key)
            .arg(corrupt_id.to_string())
            .query_async::<bool>(&mut raw)
            .await
            .unwrap(),
        "the retry keeps the wrong-typed id indexed"
    );

    let second_valid = make_parked("req-wrong-type", Duration::from_secs(60));
    let second_record = ParkedApprovalRecord::from(&second_valid);
    let non_utf8 = make_parked("req-wrong-type", Duration::from_secs(60));
    let non_utf8_id = non_utf8.request.decision_id;
    approvals.register(second_valid).await.unwrap();
    approvals.register(non_utf8).await.unwrap();
    redis::cmd("SET")
        .arg(format!("{}:approval:{non_utf8_id}", config.key_prefix))
        .arg(vec![0xff_u8, 0xfe, b'{'])
        .arg("EX")
        .arg(60)
        .query_async::<()>(&mut raw)
        .await
        .unwrap();

    let cleared = approvals
        .cancel_request("req-wrong-type")
        .await
        .expect("a non-UTF-8 record must not fail the sweep");

    assert_eq!(cleared.len(), 1, "only the valid record is cleared");
    assert_eq!(
        ParkedApprovalRecord::from(&cleared[0]),
        second_record,
        "the valid record is returned unchanged"
    );
    assert!(
        approvals.get(&non_utf8_id).await.unwrap().is_none(),
        "the non-UTF-8 record was consumed"
    );
    assert_eq!(
        redis::cmd("SMEMBERS")
            .arg(&req_index_key)
            .query_async::<Vec<String>>(&mut raw)
            .await
            .unwrap(),
        vec![corrupt_id.to_string()],
        "the non-UTF-8 record's index entry was swept; the wrong-typed one remains"
    );
}

#[tokio::test]
async fn approval_expires_with_its_record_ttl() {
    let approvals = connect(&test_config(60)).await.approvals();
    let parked = make_parked("req-ttl", Duration::from_secs(1));
    let id = parked.request.decision_id;
    approvals.register(parked).await.unwrap();

    tokio::time::sleep(Duration::from_millis(1600)).await;

    assert!(approvals.get(&id).await.unwrap().is_none());
    assert_eq!(
        approvals
            .resolve(&id, ApprovalDecision::Approved.into())
            .await,
        Err(ResolveError::NotFound)
    );
}

/// Native TTL expiry removes the record outright: the `list_pending` scan
/// sees neither the key nor a residue.
#[tokio::test]
async fn approval_list_pending_excludes_a_natively_expired_record() {
    let approvals = connect(&test_config(60)).await.approvals();
    let parked = make_parked("req-poll-ttl", Duration::from_secs(1));
    approvals.register(parked).await.unwrap();

    tokio::time::sleep(Duration::from_millis(1600)).await;

    assert!(approvals.list_pending().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// SMEMBERS-hold proxy: pin the sweep's SMEMBERS-to-pipe window
// ---------------------------------------------------------------------------

/// What a pump reports: the held SMEMBERS reply is in hand, or it failed.
#[derive(Debug)]
enum ProxyEvent {
    Captured,
    Failed(String),
}

/// A lockstep TCP proxy in front of the test server: each request frame is
/// forwarded, then its response frame, so pipelined and MULTI/EXEC traffic
/// stays ordered without interpreting payloads. Once armed, the first
/// SMEMBERS on `needle` has its response withheld until `release` fires: a
/// hard barrier with no sleeps. The store's pub/sub connection creates no
/// subscriptions here, so no unsolicited frame breaks the lockstep. Every
/// task dies with the test runtime.
struct SmembersHoldProxy {
    addr: std::net::SocketAddr,
    armed: AtomicBool,
    release: tokio::sync::Notify,
    events: tokio::sync::mpsc::Sender<ProxyEvent>,
}

impl SmembersHoldProxy {
    async fn start(
        needle: String,
    ) -> (
        std::sync::Arc<Self>,
        tokio::sync::mpsc::Receiver<ProxyEvent>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (events, rx) = tokio::sync::mpsc::channel(4);
        let proxy = std::sync::Arc::new(Self {
            addr: listener.local_addr().unwrap(),
            armed: AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
            events,
        });
        tokio::spawn({
            let proxy = proxy.clone();
            async move {
                while let Ok((client, _)) = listener.accept().await {
                    tokio::spawn(proxy.clone().pump(client, needle.clone()));
                }
            }
        });
        (proxy, rx)
    }

    /// The test server URL with only its `host:port` swapped for the proxy's,
    /// so credentials, database, and options survive byte for byte.
    fn url(&self) -> String {
        let url = redis_url();
        let lower = url.to_ascii_lowercase();
        assert!(
            !lower.contains("protocol=resp3") && !lower.contains("protocol=3"),
            "the pin proxy forwards RESP2 only"
        );
        let endpoint = test_server_endpoint();
        assert!(
            url.contains(&endpoint),
            "the test server URL must spell {endpoint}"
        );
        url.replacen(&endpoint, &self.addr.to_string(), 1)
    }

    async fn pump(self: std::sync::Arc<Self>, client: tokio::net::TcpStream, needle: String) {
        use tokio::io::AsyncBufReadExt;
        let run = async {
            let server = tokio::net::TcpStream::connect(test_server_endpoint()).await?;
            let (client_read, mut client_write) = client.into_split();
            let (server_read, mut server_write) = server.into_split();
            let mut client_read = tokio::io::BufReader::new(client_read);
            let mut server_read = tokio::io::BufReader::new(server_read);
            let (mut request, mut response) = (Vec::new(), Vec::new());
            // EOF between frames is normal teardown; mid-frame it is a defect.
            while !client_read.fill_buf().await?.is_empty() {
                request.clear();
                let args = read_frame(&mut client_read, &mut request).await?;
                let hold = args.len() >= 2
                    && args[0].eq_ignore_ascii_case(b"SMEMBERS")
                    && args[1] == needle.as_bytes()
                    && self.armed.swap(false, Ordering::AcqRel);
                server_write.write_all(&request).await?;
                response.clear();
                read_frame(&mut server_read, &mut response).await?;
                if hold {
                    // The store's own response timeout fails the sweep
                    // loudly if the release never comes.
                    let _ = self.events.send(ProxyEvent::Captured).await;
                    self.release.notified().await;
                }
                client_write.write_all(&response).await?;
            }
            Ok::<(), std::io::Error>(())
        };
        if let Err(err) = run.await {
            let _ = self.events.send(ProxyEvent::Failed(err.to_string())).await;
        }
    }
}

/// The test server's `host:port`, resolved by the client's own URL parser.
fn test_server_endpoint() -> String {
    let info = redis::Client::open(redis_url())
        .unwrap()
        .get_connection_info()
        .clone();
    match info.addr {
        redis::ConnectionAddr::Tcp(host, port) => format!("{host}:{port}"),
        other => panic!("the pin proxy expects a plain TCP test server, got {other:?}"),
    }
}

/// Append one RESP2 frame to `raw`, returning its bulk strings: for a
/// command, its arguments. Arrays are consumed by element count, so nested
/// replies (an `EXEC` result) stay one frame.
async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    raw: &mut Vec<u8>,
) -> std::io::Result<Vec<Vec<u8>>> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad RESP frame");
    let mut bulks = Vec::new();
    let mut pending = 1usize;
    while pending > 0 {
        pending -= 1;
        let start = raw.len();
        if reader.read_until(b'\n', raw).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let line = &raw[start..];
        let count = || -> std::io::Result<isize> {
            std::str::from_utf8(&line[1..line.len().saturating_sub(2)])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(invalid)
        };
        match line[0] {
            b'+' | b'-' | b':' => {}
            b'$' => {
                let n = count()?;
                if n >= 0 {
                    let at = raw.len();
                    raw.resize(at + n as usize + 2, 0);
                    reader.read_exact(&mut raw[at..]).await?;
                    bulks.push(raw[at..at + n as usize].to_vec());
                }
            }
            b'*' => pending += count()?.max(0) as usize,
            _ => return Err(invalid()),
        }
    }
    Ok(bulks)
}

/// The sweep's SMEMBERS-to-pipe window, pinned end to end: a registration
/// that lands after the sweep read the index but before its pipe executes
/// keeps its index entry and stays discoverable by a later cancel. Under
/// the old whole-index `DEL` this fails at the SISMEMBER assertion.
#[tokio::test]
async fn cancel_request_leaves_a_mid_sweep_registration_indexed() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let config = test_config(60);
        let req_index_key = format!("{}:approval:req:req-mid-sweep", config.key_prefix);
        let (proxy, mut events) = SmembersHoldProxy::start(req_index_key.clone()).await;
        let proxied = RedisSessionStoreConfig {
            url: proxy.url(),
            ..config.clone()
        };
        let approvals = connect(&proxied).await.approvals();

        let first = make_parked("req-mid-sweep", Duration::from_secs(60));
        let first_id = first.request.decision_id;
        approvals.register(first).await.unwrap();

        // Arm after the fixture registration, so the held SMEMBERS is the
        // sweep's own.
        proxy.armed.store(true, Ordering::Release);
        let sweep = tokio::spawn({
            let approvals = approvals.clone();
            async move { approvals.cancel_request("req-mid-sweep").await }
        });
        match events.recv().await {
            Some(ProxyEvent::Captured) => {}
            Some(ProxyEvent::Failed(err)) => panic!("the pin proxy failed: {err}"),
            None => panic!("the pin proxy went away"),
        }

        // The racing registration completes end to end on a direct
        // connection while the sweep is parked ahead of its pipe.
        let late = make_parked("req-mid-sweep", Duration::from_secs(60));
        let late_id = late.request.decision_id;
        connect(&config)
            .await
            .approvals()
            .register(late)
            .await
            .unwrap();
        proxy.release.notify_one();

        let cleared = sweep.await.unwrap().unwrap();
        assert_eq!(
            cleared.len(),
            1,
            "the sweep clears only what its SMEMBERS saw"
        );
        assert_eq!(cleared[0].request.decision_id, first_id);

        let client = redis::Client::open(redis_url()).unwrap();
        let mut raw = client.get_multiplexed_async_connection().await.unwrap();
        let indexed: bool = redis::cmd("SISMEMBER")
            .arg(&req_index_key)
            .arg(late_id.to_string())
            .query_async(&mut raw)
            .await
            .unwrap();
        assert!(indexed, "the mid-sweep registration kept its index entry");

        let cleared_late = approvals.cancel_request("req-mid-sweep").await.unwrap();
        assert_eq!(
            cleared_late.len(),
            1,
            "the mid-sweep registration stays discoverable"
        );
        assert_eq!(cleared_late[0].request.decision_id, late_id);
        assert!(
            events.try_recv().is_err(),
            "the pin proxy reported a failure"
        );
    })
    .await
    .expect("the interleave pin completed within its budget");
}

// ---------------------------------------------------------------------------
// Event bus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bus_delivers_across_instances() {
    let config = test_config(60);
    let instance_a = connect(&config).await.bus();
    let instance_b = connect(&config).await.bus();

    let mut sub = instance_a.subscribe("topic-x").await.unwrap();
    instance_b
        .publish("topic-x", Bytes::from_static(b"hello"))
        .await
        .unwrap();

    let payload = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("delivery within 5s")
        .expect("stream open");
    assert_eq!(payload, Bytes::from_static(b"hello"));
}

#[tokio::test]
async fn bus_fans_out_to_all_subscribers() {
    let config = test_config(60);
    let bus = connect(&config).await.bus();

    let mut sub_a = bus.subscribe("topic-fan").await.unwrap();
    let mut sub_b = bus.subscribe("topic-fan").await.unwrap();
    bus.publish("topic-fan", Bytes::from_static(b"payload"))
        .await
        .unwrap();

    for sub in [&mut sub_a, &mut sub_b] {
        let payload = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("delivery within 5s")
            .expect("stream open");
        assert_eq!(payload, Bytes::from_static(b"payload"));
    }
}

#[tokio::test]
async fn bus_topics_are_independent() {
    let config = test_config(60);
    let bus = connect(&config).await.bus();

    let mut sub_a = bus.subscribe("topic-a").await.unwrap();
    let mut sub_b = bus.subscribe("topic-b").await.unwrap();
    bus.publish("topic-a", Bytes::from_static(b"for-a"))
        .await
        .unwrap();
    bus.publish("topic-b", Bytes::from_static(b"for-b"))
        .await
        .unwrap();

    let a = tokio::time::timeout(Duration::from_secs(5), sub_a.next())
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(Duration::from_secs(5), sub_b.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a, Bytes::from_static(b"for-a"));
    assert_eq!(b, Bytes::from_static(b"for-b"));
}

#[tokio::test]
async fn bus_publish_without_subscribers_is_ok() {
    let bus = connect(&test_config(60)).await.bus();
    bus.publish("nobody-home", Bytes::from_static(b"x"))
        .await
        .unwrap();
}

/// Deployments with different key prefixes sharing one Redis must not hear
/// each other's topics.
#[tokio::test]
async fn bus_prefixes_isolate_deployments() {
    let config_a = test_config(60);
    let config_b = test_config(60);
    let instance_a = connect(&config_a).await.bus();
    let instance_b = connect(&config_b).await.bus();

    let mut sub = instance_a.subscribe("topic-shared").await.unwrap();
    instance_b
        .publish("topic-shared", Bytes::from_static(b"other-deployment"))
        .await
        .unwrap();
    instance_a
        .publish("topic-shared", Bytes::from_static(b"same-deployment"))
        .await
        .unwrap();

    let payload = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("delivery within 5s")
        .expect("stream open");
    assert_eq!(payload, Bytes::from_static(b"same-deployment"));
}

// ---------------------------------------------------------------------------
// Cross-instance approval wake (full stack: §6.1)
// ---------------------------------------------------------------------------

/// The §6.1 flow over live Redis: an approval parked through one instance's
/// registry is resolved through another instance's registry, and the parking instance's
/// suspended await wakes with the decision.
#[tokio::test]
async fn approval_parked_on_one_instance_wakes_when_resolved_on_another() {
    let config = test_config(60);
    let store_a = connect(&config).await;
    let store_b = connect(&config).await;
    let instance_a = PendingApprovals::with_backend(store_a.approvals(), store_a.bus());
    let instance_b = PendingApprovals::with_backend(store_b.approvals(), store_b.bus());
    let cancel = RequestCancelToken::unbound();

    let parked = make_parked("req-cross", Duration::from_secs(30));
    let request = parked.request;
    let id = request.decision_id;
    let handle = instance_a.register(request, Duration::from_secs(30)).await;

    instance_b
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .expect("resolve through the other instance succeeds");

    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.outcome(&cancel))
        .await
        .expect("wake must arrive well before the approval timeout");
    assert_eq!(
        outcome,
        ApprovalOutcome::Decided(ApprovalDecision::Approved)
    );
}

/// Store poll recovers the decision well before the approval timeout.
#[tokio::test]
async fn store_only_resolve_wakes_parking_instance_via_poll() {
    let config = test_config(60);
    let store_a = connect(&config).await;
    let store_b = connect(&config).await;
    let parker = PendingApprovals::with_backend(store_a.approvals(), store_a.bus());
    let cancel = RequestCancelToken::unbound();

    let parked = make_parked("req-poll", Duration::from_secs(60));
    let request = parked.request;
    let id = request.decision_id;
    let handle = parker.register(request, Duration::from_secs(60)).await;

    // Resolve against the store alone — no registry, no publish.
    store_b
        .approvals()
        .resolve(&id, ApprovalDecision::Approved.into())
        .await
        .expect("store resolve succeeds");

    let outcome = tokio::time::timeout(Duration::from_secs(15), handle.outcome(&cancel))
        .await
        .expect("store poll must deliver the decision well before the 60s timeout");
    assert_eq!(
        outcome,
        ApprovalOutcome::Decided(ApprovalDecision::Approved)
    );
}

// ---------------------------------------------------------------------------
// A2A streaming/cancel over the bus (full stack: §6.2)
// ---------------------------------------------------------------------------

mod a2a_bridge {
    use std::sync::{Arc, Mutex};

    use a2a::{
        A2AError, Artifact, CancelTaskRequest, Message, Part, Role, SendMessageRequest,
        StreamResponse, SubscribeToTaskRequest, TaskArtifactUpdateEvent, TaskState, TaskStatus,
        TaskStatusUpdateEvent,
    };
    use a2a_server::middleware::ServiceParams;
    use a2a_server::{AgentExecutor, ExecutorContext, RequestHandler};
    use aura_web_server::a2a::{AuraRequestHandler, BusBridgedExecutor, SharedTaskStore};
    use futures_util::StreamExt;
    use futures_util::stream::BoxStream;
    use tokio::sync::Notify;

    use super::{Duration, connect, test_config};

    /// Scripted stand-in for one instance's executor: `execute` emits
    /// `Working`, waits for `release` (or ends silently on `stop`, the
    /// routed-cancel shape), then emits an artifact and `Completed`.
    struct FakeExecutor {
        release: Arc<Notify>,
        stop: Arc<Notify>,
        cancelled: Arc<Mutex<Vec<String>>>,
    }

    struct Handles {
        release: Arc<Notify>,
        cancelled: Arc<Mutex<Vec<String>>>,
    }

    fn status(task_id: &str, context_id: &str, state: TaskState) -> StreamResponse {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: Some(chrono::Utc::now()),
            },
            metadata: None,
        })
    }

    impl AgentExecutor for FakeExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let release = self.release.clone();
            let stop = self.stop.clone();
            Box::pin(async_stream::stream! {
                yield Ok(status(&ctx.task_id, &ctx.context_id, TaskState::Working));
                tokio::select! {
                    _ = stop.notified() => return,
                    _ = release.notified() => {}
                }
                yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    task_id: ctx.task_id.clone(),
                    context_id: ctx.context_id.clone(),
                    artifact: Artifact {
                        artifact_id: "response".to_string(),
                        name: None,
                        description: None,
                        parts: vec![Part::text("out")],
                        metadata: None,
                        extensions: None,
                    },
                    append: Some(false),
                    last_chunk: Some(true),
                    metadata: None,
                }));
                yield Ok(status(&ctx.task_id, &ctx.context_id, TaskState::Completed));
            })
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            self.cancelled.lock().unwrap().push(ctx.task_id.clone());
            self.stop.notify_one();
            Box::pin(futures_util::stream::once(async move {
                Ok(status(&ctx.task_id, &ctx.context_id, TaskState::Canceled))
            }))
        }
    }

    /// One simulated instance: its own Redis connection, wrapped executor,
    /// and request handler.
    async fn instance(
        config: &aura_config::RedisSessionStoreConfig,
    ) -> (AuraRequestHandler, Handles) {
        let session_store = connect(config).await;
        let release = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let executor = FakeExecutor {
            release: release.clone(),
            stop,
            cancelled: cancelled.clone(),
        };
        use aura_web_server::session_store::SessionStore;
        let task_store = SharedTaskStore::from_store(session_store.tasks());
        let handler = AuraRequestHandler::new(
            BusBridgedExecutor::new(executor, session_store.bus()),
            task_store,
            session_store.bus(),
        );
        (handler, Handles { release, cancelled })
    }

    fn send_request(task_id: &str, context_id: &str) -> SendMessageRequest {
        let mut message = Message::new(Role::User, vec![Part::text("hi")]);
        message.task_id = Some(task_id.to_string());
        message.context_id = Some(context_id.to_string());
        SendMessageRequest {
            message,
            configuration: None,
            metadata: None,
            tenant: None,
        }
    }

    #[tokio::test]
    async fn subscribe_on_other_instance_relays_execution_events() {
        let config = test_config(60);
        let (instance_a, handles_a) = instance(&config).await;
        let (instance_b, _handles_b) = instance(&config).await;
        let params = ServiceParams::new();

        instance_a
            .send_message(&params, send_request("t1", "c1"))
            .await
            .expect("send succeeds");

        let mut relay = instance_b
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    id: "t1".to_string(),
                    tenant: None,
                },
            )
            .await
            .expect("subscribe on the non-executing instance succeeds");

        let first = tokio::time::timeout(Duration::from_secs(5), relay.next())
            .await
            .expect("snapshot within 5s")
            .expect("stream open")
            .expect("frame ok");
        match first {
            StreamResponse::Task(task) => assert!(!task.status.state.is_terminal()),
            other => panic!("expected snapshot Task frame, got {other:?}"),
        }

        handles_a.release.notify_one();

        let mut saw_artifact = false;
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), relay.next())
                .await
                .expect("frame within 5s")
                .expect("stream open")
                .expect("frame ok");
            match frame {
                StreamResponse::ArtifactUpdate(_) => saw_artifact = true,
                StreamResponse::StatusUpdate(update)
                    if update.status.state == TaskState::Completed =>
                {
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_artifact, "artifact frame must reach the relay");
    }

    #[tokio::test]
    async fn cancel_on_other_instance_stops_the_executing_instance() {
        let config = test_config(60);
        let (instance_a, handles_a) = instance(&config).await;
        let (instance_b, _handles_b) = instance(&config).await;
        let params = ServiceParams::new();

        instance_a
            .send_message(&params, send_request("t2", "c2"))
            .await
            .expect("send succeeds");

        let task = instance_b
            .cancel_task(
                &params,
                CancelTaskRequest {
                    id: "t2".to_string(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .expect("cancel on the non-executing instance succeeds");
        assert_eq!(task.status.state, TaskState::Canceled);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if handles_a
                    .cancelled
                    .lock()
                    .unwrap()
                    .contains(&"t2".to_string())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("routed cancel reaches the executing instance");

        use aura_web_server::session_store::SessionStore as _;
        let stored = super::connect(&config)
            .await
            .tasks()
            .get("t2")
            .await
            .unwrap()
            .expect("task in store");
        assert_eq!(stored.status.state, TaskState::Canceled);
    }

    /// Subscribers on the executing instance learn the outcome of a routed cancel.
    #[tokio::test]
    async fn cancel_on_other_instance_terminates_subscribers_on_the_executing_one() {
        let config = test_config(60);
        let (instance_a, _handles_a) = instance(&config).await;
        let (instance_b, _handles_b) = instance(&config).await;
        let params = ServiceParams::new();

        instance_a
            .send_message(&params, send_request("t3", "c3"))
            .await
            .expect("send succeeds");

        let mut local = instance_a
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    id: "t3".to_string(),
                    tenant: None,
                },
            )
            .await
            .expect("subscribe on the executing instance succeeds");

        instance_b
            .cancel_task(
                &params,
                CancelTaskRequest {
                    id: "t3".to_string(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .expect("cancel on the non-executing instance succeeds");

        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), local.next())
                .await
                .expect("frame within 5s")
                .expect("stream stays open until a terminal frame")
                .expect("frame ok");
            match frame {
                StreamResponse::StatusUpdate(update) if update.status.state.is_terminal() => {
                    assert_eq!(update.status.state, TaskState::Canceled);
                    break;
                }
                StreamResponse::Task(task) => assert!(!task.status.state.is_terminal()),
                _ => {}
            }
        }
    }
}

/// Terminal states are immutable in the store: once a task is recorded
/// terminal, a racing writer (e.g. an execution completing after a routed
/// cancel recorded `Canceled`) cannot overwrite it.
#[tokio::test]
async fn update_of_terminal_task_is_rejected() {
    let tasks = connect(&test_config(60)).await.tasks();
    tasks
        .create(make_task("t1", "c1", TaskState::Submitted))
        .await
        .unwrap();
    tasks
        .update(make_task("t1", "c1", TaskState::Canceled))
        .await
        .unwrap();

    let err = tasks
        .update(make_task("t1", "c1", TaskState::Completed))
        .await
        .expect_err("terminal task must reject updates");
    assert!(err.to_string().contains("terminal"), "{err}");

    let got = tasks.get("t1").await.unwrap().unwrap();
    assert_eq!(got.status.state, TaskState::Canceled);
}

/// Immutability rejects a *different* terminal state, not a second write of the same state.
#[tokio::test]
async fn rewriting_the_recorded_terminal_state_is_accepted() {
    let tasks = connect(&test_config(60)).await.tasks();
    tasks
        .create(make_task("t1", "c1", TaskState::Submitted))
        .await
        .unwrap();
    let version = tasks
        .update(make_task("t1", "c1", TaskState::Canceled))
        .await
        .unwrap();

    assert_eq!(
        tasks
            .update(make_task("t1", "c1", TaskState::Canceled))
            .await
            .expect("re-recording the stored terminal state must succeed"),
        version,
        "the record is left as it was written"
    );

    let got = tasks.get("t1").await.unwrap().unwrap();
    assert_eq!(got.status.state, TaskState::Canceled);
}

/// One undecodable stored record must not fail `list` for everyone.
#[tokio::test]
async fn corrupt_record_is_skipped_from_list() {
    let config = test_config(60);
    let tasks = connect(&config).await.tasks();
    tasks
        .create(make_task("t1", "c1", TaskState::Submitted))
        .await
        .unwrap();

    // Plant a record whose task payload no instance can deserialize.
    let client = redis::Client::open(redis_url()).unwrap();
    let mut raw = client.get_multiplexed_async_connection().await.unwrap();
    redis::pipe()
        .hset_multiple(
            format!("{}:a2a:task:corrupt", config.key_prefix),
            &[("version", "1"), ("task", "not json"), ("terminal", "0")],
        )
        .sadd(format!("{}:a2a:tasks", config.key_prefix), "corrupt")
        .query_async::<()>(&mut raw)
        .await
        .unwrap();

    let resp = tasks.list(&list_req()).await.unwrap();
    assert_eq!(
        resp.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        ["t1"]
    );
}
