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

use std::time::Duration;

use a2a::{ListTasksRequest, Message, Part, Role, Task, TaskState, TaskStatus};
use aura::hitl::{
    AgentScope, ApprovalDecision, ApprovalItem, ApprovalOrigin, ApprovalOutcome, ApprovalRequest,
    DecisionId, PROTOCOL_VERSION, ParkedApproval, PendingApprovals, ResolveError,
};
use aura::request_cancellation::RequestCancelToken;
use aura::session_store::ParkedApprovalRecord;
use aura_config::{RedisSessionStoreConfig, SessionStoreBackend};
use aura_web_server::session_store::{RedisSessionStore, SessionStore};
use bytes::Bytes;
use futures_util::StreamExt;

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

fn make_parked(request_id: &str, ttl: Duration) -> ParkedApproval {
    let now = chrono::Utc::now();
    ParkedApproval {
        request: ApprovalRequest {
            version: PROTOCOL_VERSION,
            decision_id: DecisionId::generate(),
            request_id: request_id.to_string(),
            scope: AgentScope::Single { session_id: None },
            origin: ApprovalOrigin::ConfigGate {
                matched_pattern: "kubectl_*".to_string(),
            },
            items: vec![ApprovalItem {
                tool_name: "kubectl_delete".to_string(),
                arguments: serde_json::json!({"pod": "web-1"}),
            }],
        },
        registered_at: now,
        expires_at: now + chrono::Duration::from_std(ttl).unwrap(),
    }
}

#[tokio::test]
async fn approval_register_get_roundtrip_preserves_record() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();

    let parked = make_parked("req-1", Duration::from_secs(60));
    let id = parked.request.decision_id;
    let expected = ParkedApprovalRecord::from(&parked);
    instance_a.register(parked).await.unwrap();

    let restored = instance_b
        .get(&id)
        .await
        .unwrap()
        .expect("instance B sees approval");
    assert_eq!(ParkedApprovalRecord::from(&restored), expected);
}

#[tokio::test]
async fn approval_resolve_is_at_most_once_across_instances() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();

    let parked = make_parked("req-2", Duration::from_secs(60));
    let id = parked.request.decision_id;
    instance_a.register(parked).await.unwrap();

    instance_b
        .resolve(&id, ApprovalDecision::Approved)
        .await
        .expect("first resolve wins");
    assert_eq!(
        instance_a.resolve(&id, ApprovalDecision::Approved).await,
        Err(ResolveError::NotFound)
    );
    assert!(instance_a.get(&id).await.unwrap().is_none());
}

#[tokio::test]
async fn approval_concurrent_resolves_have_exactly_one_winner() {
    let config = test_config(60);
    let instance_a = connect(&config).await.approvals();
    let instance_b = connect(&config).await.approvals();

    let parked = make_parked("req-3", Duration::from_secs(60));
    let id = parked.request.decision_id;
    instance_a.register(parked).await.unwrap();

    let (a, b) = tokio::join!(
        instance_a.resolve(&id, ApprovalDecision::Approved),
        instance_b.resolve(&id, ApprovalDecision::Approved),
    );
    assert_eq!(
        [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(),
        1,
        "exactly one resolver must win: {a:?} / {b:?}"
    );
}

#[tokio::test]
async fn approval_remove_makes_resolve_not_found() {
    let approvals = connect(&test_config(60)).await.approvals();
    let parked = make_parked("req-4", Duration::from_secs(60));
    let id = parked.request.decision_id;
    approvals.register(parked).await.unwrap();

    approvals.remove(&id).await.unwrap();

    assert_eq!(
        approvals.resolve(&id, ApprovalDecision::Approved).await,
        Err(ResolveError::NotFound)
    );
}

#[tokio::test]
async fn approval_cancel_request_removes_only_matching() {
    let approvals = connect(&test_config(60)).await.approvals();
    let cancel = make_parked("req-cancel", Duration::from_secs(60));
    let keep = make_parked("req-keep", Duration::from_secs(60));
    let cancel_id = cancel.request.decision_id;
    let keep_id = keep.request.decision_id;
    approvals.register(cancel).await.unwrap();
    approvals.register(keep).await.unwrap();

    approvals.cancel_request("req-cancel").await.unwrap();

    assert!(approvals.get(&cancel_id).await.unwrap().is_none());
    assert!(approvals.get(&keep_id).await.unwrap().is_some());
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
        approvals.resolve(&id, ApprovalDecision::Approved).await,
        Err(ResolveError::NotFound)
    );
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
        .resolve(&id, ApprovalDecision::Approved)
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
}
