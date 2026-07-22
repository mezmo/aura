//! Cross-instance A2A streaming and cancel over the session-store event bus
//! (`docs/design/session-storage.md` §6.2).
//!
//! The upstream `DefaultRequestHandler` tracks live executions in a
//! process-local map, so `tasks/{id}:subscribe` only reaches executions on
//! the serving instance, and a cancel landing elsewhere finds no local cancel
//! handle. Two bus topics bridge the gap:
//!
//! - [`task_topic`] — [`BusBridgedExecutor`] publishes every execution event;
//!   an instance serving a subscribe for a task it is not executing relays
//!   them ([`relay_subscription`]).
//! - [`cancel_topic`] — a cancel publishes here; the executing instance is
//!   subscribed and drives its local cancel machinery.
//!
//! Bus delivery is fire-and-forget end to end, and the store stays the source
//! of truth: fan-out frames are sequence-numbered so a relay that misses one
//! surfaces a fell-behind error rather than silently delivering partial
//! content, a relay converges through its periodic store poll (and is
//! lifetime-bounded, so a dead executing instance cannot hang it forever),
//! and the instance that received the cancel writes the terminal status
//! itself, whether or not the routed cancel arrives. The Redis store rejects
//! updates to terminal tasks, so an execution that misses a routed cancel
//! can burn work until it finishes, but cannot overwrite the recorded
//! `Canceled`.

use std::sync::Arc;
use std::time::Duration;

use a2a::{A2AError, StreamResponse};
use a2a_server::{AgentExecutor, ExecutorContext, TaskStore};
use aura::session_store::EventBus;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio::task::AbortHandle;
use tracing::warn;

use super::SharedTaskStore;
use super::agent_executor::fail_status;

/// How often a bus relay double-checks the store, so a subscriber attached to
/// a task whose executing instance died still observes the terminal state (or
/// the record's expiry) instead of waiting forever.
const RELAY_STORE_POLL: Duration = Duration::from_secs(15);

/// Upper bound on one relay stream's lifetime. A subscriber to a task whose
/// executing instance died (store stuck non-terminal, no frames ever again)
/// gets a terminal error at this bound instead of an unbounded hang; a task
/// legitimately running longer stays subscribable — the client resubscribes
/// and gets a fresh relay.
const RELAY_MAX_LIFETIME: Duration = Duration::from_secs(900);

/// Fan-out topic carrying one task's execution events.
pub fn task_topic(task_id: &str) -> String {
    format!("a2a:task:{task_id}")
}

/// Topic routing a cancel to the instance executing the task.
pub fn cancel_topic(task_id: &str) -> String {
    format!("a2a:cancel:{task_id}")
}

/// [`AgentExecutor`] wrapper connecting an execution to the event bus:
/// every event the inner executor yields is also published to the task's
/// fan-out topic, a listener drives the inner cancel when a routed cancel
/// arrives, and `cancel` publishes the routed cancel before running the
/// inner cancel locally.
pub struct BusBridgedExecutor<E> {
    inner: Arc<E>,
    bus: Arc<dyn EventBus>,
}

impl<E: AgentExecutor> BusBridgedExecutor<E> {
    pub fn new(inner: E, bus: Arc<dyn EventBus>) -> Self {
        Self {
            inner: Arc::new(inner),
            bus,
        }
    }
}

impl<E: AgentExecutor> AgentExecutor for BusBridgedExecutor<E> {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let bus = self.bus.clone();
        let inner = self.inner.clone();
        let listener_ctx = cancel_context(&ctx);
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();
        let events = self.inner.execute(ctx);

        Box::pin(async_stream::stream! {
            let listener = spawn_cancel_listener(bus.clone(), inner, listener_ctx).await;
            let _stop_listener_on_drop = AbortOnDrop(listener);
            let mut events = publish_and_forward(bus, task_id, context_id, events);
            while let Some(item) = events.next().await {
                yield item;
            }
        })
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let bus = self.bus.clone();
        let inner = self.inner.clone();
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();

        Box::pin(async_stream::stream! {
            // Route the cancel to wherever the execution lives. When it is
            // local, the routed copy and the inner cancel below race to the
            // same cancel handle and the loser is a no-op.
            if let Err(err) = bus
                .publish(&cancel_topic(&task_id), Bytes::from_static(b"cancel"))
                .await
            {
                warn!(
                    task_id, error = %err,
                    "a2a cancel publish failed; a cross-instance execution may keep running",
                );
            }

            // The terminal status this yields is applied to the shared store
            // by the request handler on this instance; publishing it also
            // ends any cross-instance relays.
            let events = inner.cancel(ctx);
            let mut events = publish_and_forward(bus, task_id, context_id, events);
            while let Some(item) = events.next().await {
                yield item;
            }
        })
    }
}

/// One frame on the task fan-out topic. `seq` restarts at 1 for each
/// publishing stream and increases by 1 per frame, letting a relay detect
/// frames the lossy bus dropped for it.
#[derive(Serialize)]
struct TaskFrameRef<'a> {
    seq: u64,
    event: &'a StreamResponse,
}

/// Owned deserialization counterpart of [`TaskFrameRef`].
#[derive(Deserialize)]
struct TaskFrame {
    seq: u64,
    event: StreamResponse,
}

/// Forward `events`, publishing each to the task's fan-out topic before
/// yielding it — the request handler stops polling after a terminal event,
/// so a post-yield publish would never run for the frame remote subscribers
/// need most. An `Err` item (which the handler delivers only to local
/// subscribers and never persists) is published as a synthetic terminal
/// `Failed` status so cross-instance relays terminate too.
fn publish_and_forward(
    bus: Arc<dyn EventBus>,
    task_id: String,
    context_id: String,
    mut events: BoxStream<'static, Result<StreamResponse, A2AError>>,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    let topic = task_topic(&task_id);
    Box::pin(async_stream::stream! {
        let mut seq: u64 = 0;
        while let Some(item) = events.next().await {
            seq += 1;
            match &item {
                Ok(event) => publish_frame(bus.as_ref(), &topic, seq, event).await,
                Err(err) => {
                    let failed = fail_status(&task_id, &context_id, &err.to_string());
                    publish_frame(bus.as_ref(), &topic, seq, &failed).await;
                }
            }
            yield item;
        }
    })
}

/// Serve `tasks/{id}:subscribe` for a task this instance is not executing:
/// a store snapshot first, then live frames from the fan-out topic until a
/// terminal event — the same shape as the upstream local subscription. A
/// missing or already-terminal task is `task_not_found`, matching the
/// upstream handler's response once an execution is gone.
pub async fn relay_subscription(
    bus: Arc<dyn EventBus>,
    store: SharedTaskStore,
    task_id: &str,
) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
    // Subscribe before the snapshot read so frames arriving in between are
    // buffered rather than lost. A frame whose effect is already in the
    // snapshot is delivered twice in that window; the store copy (written
    // once, by the executing instance's handler) stays authoritative.
    let mut frames = bus
        .subscribe(&task_topic(task_id))
        .await
        .map_err(|e| A2AError::internal(format!("task event subscription failed: {e}")))?;

    let snapshot = store
        .get(task_id)
        .await?
        .filter(|task| !task.status.state.is_terminal())
        .ok_or_else(|| A2AError::task_not_found(task_id))?;

    let task_id = task_id.to_string();
    Ok(Box::pin(async_stream::stream! {
        yield Ok(StreamResponse::Task(snapshot));

        let deadline = tokio::time::Instant::now() + RELAY_MAX_LIFETIME;
        let mut poll = tokio::time::interval(RELAY_STORE_POLL);
        poll.reset();
        let mut last_seq: Option<u64> = None;
        loop {
            tokio::select! {
                frame = frames.next() => {
                    let Some(payload) = frame else { break };
                    match serde_json::from_slice::<TaskFrame>(&payload) {
                        Ok(frame) => {
                            // A sequence gap means the lossy bus dropped
                            // frames for this slow subscriber: surface it,
                            // mirroring the upstream local subscription's
                            // fell-behind error, instead of silently
                            // delivering partial content. A seq at or below
                            // the last seen one is a new publishing stream
                            // (e.g. the cancel path) and resets the baseline.
                            if last_seq.is_some_and(|last| frame.seq > last + 1) {
                                yield Err(A2AError::internal(
                                    "subscription fell behind the task's event stream; resubscribe",
                                ));
                                break;
                            }
                            last_seq = Some(frame.seq);
                            let terminal = is_terminal_event(&frame.event);
                            yield Ok(frame.event);
                            if terminal {
                                break;
                            }
                        }
                        Err(err) => {
                            warn!(task_id, error = %err, "undecodable a2a task frame ignored");
                        }
                    }
                }
                _ = poll.tick() => {
                    match store.get(&task_id).await {
                        Ok(Some(task)) if task.status.state.is_terminal() => {
                            yield Ok(StreamResponse::Task(task));
                            break;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(err) => {
                            yield Err(err);
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    yield Err(A2AError::internal(
                        "subscription ended without a terminal event; resubscribe",
                    ));
                    break;
                }
            }
        }
    }))
}

/// Terminal-event test, mirroring the upstream handler's (private) version.
fn is_terminal_event(event: &StreamResponse) -> bool {
    match event {
        StreamResponse::Task(task) => task.status.state.is_terminal(),
        StreamResponse::StatusUpdate(update) => update.status.state.is_terminal(),
        _ => false,
    }
}

/// Publish one execution event to the fan-out topic. Failures are logged and
/// skipped — the store write is the durable record; relays converge via
/// their store poll and sequence-gap detection.
async fn publish_frame(bus: &dyn EventBus, topic: &str, seq: u64, event: &StreamResponse) {
    match serde_json::to_vec(&TaskFrameRef { seq, event }) {
        Ok(payload) => {
            if let Err(err) = bus.publish(topic, Bytes::from(payload)).await {
                warn!(topic, error = %err, "a2a event publish failed; cross-instance subscribers may miss this frame");
            }
        }
        Err(err) => warn!(topic, error = %err, "a2a event serialization failed"),
    }
}

/// Subscribe to the task's cancel topic and, on the first routed cancel,
/// drive the inner executor's cancel to completion. Its events are discarded:
/// the instance that received the cancel request writes the terminal status
/// to the shared store, so this side only has to stop the execution.
async fn spawn_cancel_listener<E: AgentExecutor>(
    bus: Arc<dyn EventBus>,
    inner: Arc<E>,
    ctx: ExecutorContext,
) -> Option<AbortHandle> {
    let topic = cancel_topic(&ctx.task_id);
    match bus.subscribe(&topic).await {
        Ok(mut requests) => Some(
            tokio::spawn(async move {
                if requests.next().await.is_some() {
                    let mut cancelled = inner.cancel(ctx);
                    while cancelled.next().await.is_some() {}
                }
            })
            .abort_handle(),
        ),
        Err(err) => {
            warn!(
                task_id = ctx.task_id, error = %err,
                "a2a cancel subscription failed; cross-instance cancel unavailable for this execution",
            );
            None
        }
    }
}

/// The context a routed cancel hands to the inner executor's cancel — the
/// same shape the upstream handler builds for a direct cancel.
fn cancel_context(ctx: &ExecutorContext) -> ExecutorContext {
    ExecutorContext {
        message: None,
        task_id: ctx.task_id.clone(),
        stored_task: ctx.stored_task.clone(),
        context_id: ctx.context_id.clone(),
        metadata: None,
        user: None,
        service_params: ctx.service_params.clone(),
        tenant: ctx.tenant.clone(),
    }
}

/// Aborts the cancel listener when the execution stream is dropped or ends.
struct AbortOnDrop(Option<AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use a2a::{
        Artifact, CancelTaskRequest, Message, Part, Role, SendMessageRequest,
        SubscribeToTaskRequest, TaskArtifactUpdateEvent, TaskState, TaskStatus,
        TaskStatusUpdateEvent,
    };
    use a2a_server::middleware::ServiceParams;
    use a2a_server::{InMemoryTaskStore, RequestHandler};
    use aura::session_store::InMemoryEventBus;
    use tokio::sync::Notify;

    use super::*;
    use crate::a2a::AuraRequestHandler;

    /// Scripted executor standing in for one instance's `AuraAgentExecutor`:
    /// `execute` emits `Working` and then waits — `release` continues to an
    /// artifact plus `Completed`, `stop` (fired by `cancel`) ends the stream
    /// with no terminal event, mirroring the real executor's routed-cancel
    /// path. `cancel` records the task id it was invoked for.
    struct FakeExecutor {
        release: Arc<Notify>,
        stop: Arc<Notify>,
        cancelled: Arc<Mutex<Vec<String>>>,
    }

    struct FakeHandles {
        release: Arc<Notify>,
        cancelled: Arc<Mutex<Vec<String>>>,
    }

    fn fake_executor() -> (FakeExecutor, FakeHandles) {
        let release = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        (
            FakeExecutor {
                release: release.clone(),
                stop: stop.clone(),
                cancelled: cancelled.clone(),
            },
            FakeHandles { release, cancelled },
        )
    }

    fn working_status(task_id: &str, context_id: &str, state: TaskState) -> StreamResponse {
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
                let task_id = ctx.task_id.clone();
                let context_id = ctx.context_id.clone();
                yield Ok(working_status(&task_id, &context_id, TaskState::Working));
                tokio::select! {
                    _ = stop.notified() => return,
                    _ = release.notified() => {}
                }
                yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    task_id: task_id.clone(),
                    context_id: context_id.clone(),
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
                yield Ok(working_status(&task_id, &context_id, TaskState::Completed));
            })
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            self.cancelled.lock().unwrap().push(ctx.task_id.clone());
            self.stop.notify_one();
            Box::pin(futures_util::stream::once(async move {
                Ok(working_status(
                    &ctx.task_id,
                    &ctx.context_id,
                    TaskState::Canceled,
                ))
            }))
        }
    }

    /// One simulated instance: a full `AuraRequestHandler` whose executor is
    /// bus-bridged, over the shared store and bus.
    fn instance(
        store: &SharedTaskStore,
        bus: &Arc<InMemoryEventBus>,
    ) -> (AuraRequestHandler, FakeHandles) {
        let (executor, handles) = fake_executor();
        let bus: Arc<dyn EventBus> = bus.clone();
        let handler = AuraRequestHandler::new(
            BusBridgedExecutor::new(executor, bus.clone()),
            store.clone(),
            bus,
        );
        (handler, handles)
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

    async fn next_frame(
        stream: &mut BoxStream<'static, Result<StreamResponse, A2AError>>,
    ) -> StreamResponse {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("frame within 5s")
            .expect("stream open")
            .expect("frame ok")
    }

    #[tokio::test]
    async fn subscribe_on_other_instance_relays_execution_events() {
        let store = SharedTaskStore::from_store(Arc::new(InMemoryTaskStore::new()));
        let bus = Arc::new(InMemoryEventBus::new());
        let (instance_a, handles_a) = instance(&store, &bus);
        let (instance_b, _handles_b) = instance(&store, &bus);
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

        // Snapshot first, mirroring the local subscription shape.
        match next_frame(&mut relay).await {
            StreamResponse::Task(task) => {
                assert_eq!(task.id, "t1");
                assert!(!task.status.state.is_terminal());
            }
            other => panic!("expected snapshot Task frame, got {other:?}"),
        }

        handles_a.release.notify_one();

        let mut saw_artifact = false;
        loop {
            match next_frame(&mut relay).await {
                StreamResponse::ArtifactUpdate(update) => {
                    assert_eq!(update.artifact.parts.len(), 1);
                    saw_artifact = true;
                }
                StreamResponse::StatusUpdate(update)
                    if update.status.state == TaskState::Completed =>
                {
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_artifact, "artifact frame must reach the relay");
        assert!(
            tokio::time::timeout(Duration::from_secs(5), relay.next())
                .await
                .expect("stream ends after terminal frame")
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancel_on_other_instance_stops_the_executing_instance() {
        let store = SharedTaskStore::from_store(Arc::new(InMemoryTaskStore::new()));
        let bus = Arc::new(InMemoryEventBus::new());
        let (instance_a, handles_a) = instance(&store, &bus);
        let (instance_b, _handles_b) = instance(&store, &bus);
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

        // The routed cancel must reach instance A's executor.
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
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("routed cancel reaches the executing instance");

        let stored = store.get("t2").await.unwrap().expect("task in store");
        assert_eq!(stored.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn relay_subscription_unknown_task_is_not_found() {
        let store = SharedTaskStore::from_store(Arc::new(InMemoryTaskStore::new()));
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        let Err(err) = relay_subscription(bus, store, "missing").await else {
            panic!("unknown task must not be subscribable");
        };
        assert!(err.to_string().contains("missing"));
    }

    #[tokio::test]
    async fn relay_subscription_terminal_task_is_not_found() {
        let store = SharedTaskStore::from_store(Arc::new(InMemoryTaskStore::new()));
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        let task = a2a::Task {
            id: "t3".to_string(),
            context_id: "c3".to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        store.create(task).await.unwrap();

        assert!(relay_subscription(bus, store, "t3").await.is_err());
    }

    /// The executing instance dying leaves no terminal frame on the bus; the
    /// relay's periodic store poll observes the terminal state instead.
    #[tokio::test(start_paused = true)]
    async fn relay_store_poll_converges_without_bus_frames() {
        let store = SharedTaskStore::from_store(Arc::new(InMemoryTaskStore::new()));
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        let mut task = a2a::Task {
            id: "t4".to_string(),
            context_id: "c4".to_string(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        store.create(task.clone()).await.unwrap();

        let mut relay = relay_subscription(bus, store.clone(), "t4").await.unwrap();
        match next_frame(&mut relay).await {
            StreamResponse::Task(snapshot) => assert_eq!(snapshot.status.state, TaskState::Working),
            other => panic!("expected snapshot, got {other:?}"),
        }

        task.status.state = TaskState::Completed;
        store.update(task).await.unwrap();

        // Longer than RELAY_STORE_POLL so (paused) time reaches the tick.
        let converged = tokio::time::timeout(RELAY_STORE_POLL * 4, relay.next())
            .await
            .expect("poll converges")
            .expect("stream open")
            .expect("frame ok");
        match converged {
            StreamResponse::Task(converged) => {
                assert_eq!(converged.status.state, TaskState::Completed);
            }
            other => panic!("expected terminal store snapshot, got {other:?}"),
        }
        assert!(relay.next().await.is_none());
    }

    /// Executor whose execution fails mid-flight: `Working`, then (once
    /// released) an `Err` item — the shape upstream never persists.
    struct FailingExecutor {
        release: Arc<Notify>,
    }

    impl AgentExecutor for FailingExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let release = self.release.clone();
            Box::pin(async_stream::stream! {
                yield Ok(working_status(&ctx.task_id, &ctx.context_id, TaskState::Working));
                release.notified().await;
                yield Err(A2AError::internal("provider exploded"));
            })
        }

        fn cancel(
            &self,
            _ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            Box::pin(futures_util::stream::empty())
        }
    }

    #[tokio::test]
    async fn failing_execution_terminates_cross_instance_relays() {
        let store = SharedTaskStore::from_store(Arc::new(InMemoryTaskStore::new()));
        let bus = Arc::new(InMemoryEventBus::new());
        let release = Arc::new(Notify::new());
        let bus_dyn: Arc<dyn EventBus> = bus.clone();
        let instance_a = AuraRequestHandler::new(
            BusBridgedExecutor::new(
                FailingExecutor {
                    release: release.clone(),
                },
                bus_dyn.clone(),
            ),
            store.clone(),
            bus_dyn,
        );
        let (instance_b, _handles_b) = instance(&store, &bus);
        let params = ServiceParams::new();

        instance_a
            .send_message(&params, send_request("t5", "c5"))
            .await
            .expect("send succeeds");

        let mut relay = instance_b
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    id: "t5".to_string(),
                    tenant: None,
                },
            )
            .await
            .expect("subscribe succeeds");
        match next_frame(&mut relay).await {
            StreamResponse::Task(task) => assert!(!task.status.state.is_terminal()),
            other => panic!("expected snapshot, got {other:?}"),
        }

        release.notify_one();

        // The executor's Err is published as a synthetic terminal Failed
        // status, so the relay ends instead of hanging.
        match next_frame(&mut relay).await {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.status.state, TaskState::Failed);
            }
            other => panic!("expected Failed status, got {other:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(5), relay.next())
                .await
                .expect("stream ends after terminal frame")
                .is_none()
        );
    }

    #[tokio::test]
    async fn relay_errors_on_sequence_gap() {
        let store = SharedTaskStore::from_store(Arc::new(InMemoryTaskStore::new()));
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        store
            .create(a2a::Task {
                id: "t6".to_string(),
                context_id: "c6".to_string(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .await
            .unwrap();
        let mut relay = relay_subscription(bus.clone(), store, "t6").await.unwrap();
        next_frame(&mut relay).await; // snapshot

        let publish = |seq: u64| {
            let payload = serde_json::to_vec(&TaskFrameRef {
                seq,
                event: &working_status("t6", "c6", TaskState::Working),
            })
            .unwrap();
            let bus = bus.clone();
            async move { bus.publish(&task_topic("t6"), Bytes::from(payload)).await }
        };
        publish(1).await.unwrap();
        next_frame(&mut relay).await; // seq 1 delivered

        publish(3).await.unwrap(); // seq 2 lost
        let item = tokio::time::timeout(Duration::from_secs(5), relay.next())
            .await
            .expect("gap surfaces within 5s")
            .expect("stream open");
        let err = item.expect_err("a sequence gap must surface as an error");
        assert!(err.to_string().contains("fell behind"), "{err}");
        assert!(relay.next().await.is_none());
    }

    /// A dead executing instance never writes a terminal state; the relay
    /// must end with an error at its lifetime bound instead of hanging.
    #[tokio::test(start_paused = true)]
    async fn relay_errors_at_max_lifetime_without_terminal() {
        let store = SharedTaskStore::from_store(Arc::new(InMemoryTaskStore::new()));
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new());
        store
            .create(a2a::Task {
                id: "t7".to_string(),
                context_id: "c7".to_string(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .await
            .unwrap();
        let mut relay = relay_subscription(bus, store, "t7").await.unwrap();
        next_frame(&mut relay).await; // snapshot

        let item = tokio::time::timeout(RELAY_MAX_LIFETIME * 2, relay.next())
            .await
            .expect("lifetime bound fires")
            .expect("stream open");
        let err = item.expect_err("the lifetime bound must surface as an error");
        assert!(
            err.to_string().contains("without a terminal event"),
            "{err}"
        );
        assert!(relay.next().await.is_none());
    }
}
