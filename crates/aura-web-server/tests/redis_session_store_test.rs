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

use a2a::{ListTasksRequest, Message, Part, Role, Task, TaskState, TaskStatus};
use aura_config::{RedisSessionStoreConfig, SessionStoreBackend};
use aura_web_server::session_store::{RedisSessionStore, SessionStore};

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

/// Cross-pod simulation (§12): two independent backend handles sharing one
/// Redis — a task created through "pod A" is visible to "pod B" by get, list,
/// and history-by-context, and B can update it.
#[tokio::test]
async fn task_created_on_one_pod_is_visible_and_updatable_on_another() {
    let config = test_config(60);
    let pod_a = connect(&config).await.tasks();
    let pod_b = connect(&config).await.tasks();

    let mut task = make_task("t1", "ctx-shared", TaskState::Working);
    task.history = Some(vec![Message::new(Role::User, vec![Part::text("q")])]);
    pod_a.create(task).await.unwrap();

    let got = pod_b.get("t1").await.unwrap().expect("pod B cannot see t1");
    assert_eq!(got.status.state, TaskState::Working);
    assert_eq!(got.history.as_ref().unwrap().len(), 1);

    let by_ctx = pod_b
        .list(&ListTasksRequest {
            context_id: Some("ctx-shared".to_string()),
            ..list_req()
        })
        .await
        .unwrap();
    assert_eq!(by_ctx.total_size, 1);

    let v2 = pod_b
        .update(make_task("t1", "ctx-shared", TaskState::Completed))
        .await
        .unwrap();
    assert_eq!(v2, 2);
    let got = pod_a.get("t1").await.unwrap().unwrap();
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
