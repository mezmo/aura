//! The run a task is working on, scoped to that task.
//!
//! Rig invokes tools as `Tool::call(args)` with no call-time context, so a tool
//! cannot be handed the run it belongs to. A task-local supplies it without any
//! component storing it: concurrent runs each see their own, and nothing has to
//! be reset between runs.
//!
//! This reaches only code running inside the run's task. MCP progress
//! notifications arrive on the transport's own task and cannot read it — they
//! are correlated by progress token instead, registered at call time from here.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio_util::sync::CancellationToken;

use futures::Stream;

use aura_events::agent::AgentEvent;
use tokio::sync::mpsc;

use aura_events::ToolCallId;

/// Events a run may buffer before its observer reads them.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// One run — what its own work needs to correlate, and where its events go.
pub struct RunContext {
    id: Arc<str>,
    tool_calls: Mutex<VecDeque<ToolCallId>>,
    events: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
}

/// Pending tool ids before warning, in case results never arrive to pop them.
const MAX_PENDING_TOOL_CALLS: usize = 256;

impl RunContext {
    /// A run and the receiver its observer reads, on a token of its own.
    pub fn channel(id: impl Into<Arc<str>>) -> (Arc<Self>, mpsc::Receiver<AgentEvent>) {
        Self::channel_on(id, CancellationToken::new())
    }

    /// A run on `cancel`, for a caller that already holds the token the run is
    /// to stop on — a child of its own caller's, so one run ending leaves the
    /// others alone.
    pub fn channel_on(
        id: impl Into<Arc<str>>,
        cancel: CancellationToken,
    ) -> (Arc<Self>, mpsc::Receiver<AgentEvent>) {
        let (events, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let run = Arc::new(Self {
            id: id.into(),
            tool_calls: Mutex::new(VecDeque::new()),
            events,
            cancel,
        });
        (run, receiver)
    }

    /// A run nobody observes, for a test that needs one to exist without
    /// reading what it emits. Production names a run it can reach an observer
    /// through, or names none.
    #[cfg(test)]
    pub fn detached(id: impl Into<Arc<str>>) -> Arc<Self> {
        Self::channel(id).0
    }

    /// Hands an event to whoever is observing the run. `false` when nothing is
    /// reading, which a producer that only wanted it logged can ignore.
    pub async fn emit(&self, event: AgentEvent) -> bool {
        let payload = std::mem::discriminant(&event.payload);
        let delivered = self.events.send(event).await.is_ok();

        if !delivered {
            tracing::debug!(
                run_id = %self.id,
                ?payload,
                "nobody is observing this run, so its event reached no consumer"
            );
        }
        delivered
    }

    pub fn id(&self) -> &Arc<str> {
        &self.id
    }

    /// The token that cancels this run.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<ToolCallId>> {
        self.tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Rig executes a run's tools one at a time, so the provider's tool ids
    /// arrive and are consumed in the same order and a queue correlates them.
    pub fn push_tool_call(&self, tool_call_id: impl Into<ToolCallId>) {
        let mut queue = self.queue();
        queue.push_back(tool_call_id.into());

        if queue.len() > MAX_PENDING_TOOL_CALLS {
            tracing::warn!(
                run_id = %self.id,
                pending = queue.len(),
                "pending tool ids are accumulating; results may not be arriving"
            );
        }
    }

    /// The id a call in flight belongs to. MCP execution reads it without
    /// consuming, because the result that pairs with it has not arrived yet.
    pub fn peek_tool_call(&self) -> Option<ToolCallId> {
        self.queue().front().cloned()
    }

    pub fn pop_tool_call(&self) -> Option<ToolCallId> {
        self.queue().pop_front()
    }
}

impl std::fmt::Debug for RunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("id", &self.id)
            .field("pending_tool_calls", &self.queue().len())
            .finish()
    }
}

/// A run held for work that runs outside its scope.
#[derive(Default)]
pub struct BoundRun(Mutex<Option<Arc<RunContext>>>);

impl BoundRun {
    /// A slot holding the run in scope, for work built inside one.
    pub fn captured() -> Self {
        Self(Mutex::new(current_run()))
    }

    /// Replaces any run already bound. One slot is enough while an agent is
    /// built per request and owns the tools it gates.
    pub fn bind(&self, run: Arc<RunContext>) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(run);
    }

    /// The bound run, or the ambient one for a caller already inside it.
    pub fn get(&self) -> Option<Arc<RunContext>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .or_else(current_run)
    }
}

tokio::task_local! {
    static RUN: Arc<RunContext>;
}

/// Hands an event to the run in scope. `false` when no run is in scope or
/// nothing is reading, which a producer that only wanted it logged can ignore.
///
/// Work that runs outside the scope — a notification on the transport task, a
/// spawned sweep — reaches its run through what it already holds and calls
/// [`RunContext::emit`] on it.
pub async fn emit(event: AgentEvent) -> bool {
    match current_run() {
        Some(run) => run.emit(event).await,
        None => {
            tracing::debug!(
                payload = ?std::mem::discriminant(&event.payload),
                "no run is in scope, so this event reached no consumer"
            );
            false
        }
    }
}

pub fn current_run() -> Option<Arc<RunContext>> {
    RUN.try_with(Arc::clone).ok()
}

pub fn current_run_id() -> Option<Arc<str>> {
    RUN.try_with(|run| Arc::clone(run.id())).ok()
}

/// Runs `f` with `run` in scope. Task-locals do not cross `tokio::spawn`, so
/// a spawned worker needs its own call rather than inheriting its parent's.
pub async fn with_run<F: Future>(run: Arc<RunContext>, f: F) -> F::Output {
    RUN.scope(run, f).await
}

/// Enters the scope on every poll, so a stream polled by a consumer outside the
/// run still executes its tool calls with the run in scope.
pub fn scope_stream<S: Stream + Unpin>(run: Arc<RunContext>, inner: S) -> ScopedStream<S> {
    ScopedStream { run, inner }
}

pub struct ScopedStream<S> {
    run: Arc<RunContext>,
    inner: S,
}

impl<S: Stream + Unpin> Stream for ScopedStream<S> {
    type Item = S::Item;

    /// Entering the scope clones the handle, so the run is held behind an `Arc`
    /// rather than rebuilt on every poll.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let run = Arc::clone(&this.run);
        RUN.sync_scope(run, || Pin::new(&mut this.inner).poll_next(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn run(id: &str) -> Arc<RunContext> {
        RunContext::detached(id)
    }

    #[tokio::test]
    async fn a_scope_established_inside_a_spawn_holds() {
        let (run, _rx) = RunContext::channel("spawned");
        let seen = tokio::spawn(with_run(run, async { current_run_id() }))
            .await
            .unwrap();
        assert_eq!(seen.as_deref(), Some("spawned"));
    }

    /// A run built on the caller's token stops when the caller does. The work
    /// that awaits cancellation reads the token off the run, so a run holding a
    /// different token than the one the handler fires would leave a gated call
    /// waiting on a signal nobody sends.
    #[tokio::test]
    async fn a_run_stops_on_the_token_it_was_built_on() {
        let caller = CancellationToken::new();
        let (run, _events) = RunContext::channel_on("run_on_token", caller.clone());
        assert!(!run.cancel_token().is_cancelled());

        caller.cancel();
        assert!(run.cancel_token().is_cancelled());
    }

    /// A run given no token has one of its own, so nothing reads a token that
    /// cancels something else.
    #[tokio::test]
    async fn a_run_given_no_token_has_its_own() {
        let (a, _ea) = RunContext::channel("run_a");
        let (b, _eb) = RunContext::channel("run_b");

        a.cancel_token().cancel();
        assert!(a.cancel_token().is_cancelled());
        assert!(
            !b.cancel_token().is_cancelled(),
            "one run's token is its own"
        );
    }

    #[tokio::test]
    async fn there_is_no_run_outside_a_run() {
        assert!(current_run().is_none());
        assert_eq!(current_run_id(), None);
    }

    #[tokio::test]
    async fn a_scope_supplies_the_run() {
        let seen = with_run(run("run_1"), async { current_run_id() }).await;
        assert_eq!(seen.as_deref(), Some("run_1"));
    }

    #[tokio::test]
    async fn concurrent_runs_do_not_see_each_other() {
        let a = tokio::spawn(with_run(run("run_a"), async {
            tokio::task::yield_now().await;
            current_run_id()
        }));
        let b = tokio::spawn(with_run(run("run_b"), async {
            tokio::task::yield_now().await;
            current_run_id()
        }));

        assert_eq!(a.await.unwrap().as_deref(), Some("run_a"));
        assert_eq!(b.await.unwrap().as_deref(), Some("run_b"));
    }

    #[tokio::test]
    async fn a_scoped_stream_carries_the_run_into_each_poll() {
        let inner = futures::stream::iter(0..3).map(|_| current_run_id());
        let seen: Vec<_> = scope_stream(run("run_s"), inner).collect().await;

        assert_eq!(
            seen.iter().map(|id| id.as_deref()).collect::<Vec<_>>(),
            vec![Some("run_s"); 3]
        );
    }

    /// Orchestration drives workers with `FuturesUnordered` inside the run's
    /// task rather than spawning them, which is what lets their tool calls —
    /// and so their progress — resolve to the run.
    #[tokio::test]
    async fn workers_driven_as_futures_keep_the_run() {
        use futures::stream::FuturesUnordered;

        let seen = with_run(run("run_w"), async {
            let mut workers: FuturesUnordered<_> = (0..3)
                .map(|_| async {
                    tokio::task::yield_now().await;
                    current_run_id()
                })
                .collect();

            let mut ids = Vec::new();
            while let Some(id) = workers.next().await {
                ids.push(id);
            }
            ids
        })
        .await;

        assert_eq!(
            seen.iter().map(|id| id.as_deref()).collect::<Vec<_>>(),
            vec![Some("run_w"); 3]
        );
    }

    /// A spawned task does not inherit its parent's scope, which is why every
    /// orchestration worker establishes its own.
    #[tokio::test]
    async fn a_spawned_task_does_not_inherit_the_scope() {
        let seen = with_run(run("run_p"), async {
            tokio::spawn(async { current_run_id() }).await.unwrap()
        })
        .await;

        assert_eq!(seen, None);
    }

    #[test]
    fn a_run_with_no_calls_in_flight_has_nothing_to_report() {
        let run = run("run_empty");
        assert_eq!(run.peek_tool_call(), None);
        assert_eq!(run.pop_tool_call(), None);
    }

    /// Rig executes tools sequentially, so the order results are consumed in
    /// matches the order the calls were announced.
    #[test]
    fn tool_calls_come_back_in_the_order_they_were_pushed() {
        let run = run("run_fifo");
        run.push_tool_call("call_1");
        run.push_tool_call("call_2");

        assert_eq!(run.peek_tool_call(), Some(ToolCallId::new("call_1")));
        assert_eq!(
            run.peek_tool_call(),
            Some(ToolCallId::new("call_1")),
            "peeking leaves the call in flight, because its result has not arrived"
        );
        assert_eq!(run.pop_tool_call(), Some(ToolCallId::new("call_1")));
        assert_eq!(run.pop_tool_call(), Some(ToolCallId::new("call_2")));
        assert_eq!(run.pop_tool_call(), None);
    }

    #[test]
    fn one_runs_calls_are_invisible_to_another() {
        let a = run("run_a");
        let b = run("run_b");
        a.push_tool_call("call_a");

        assert_eq!(b.peek_tool_call(), None);
        assert_eq!(a.peek_tool_call(), Some(ToolCallId::new("call_a")));
    }

    /// The hook pushes and pops through the ambient run rather than being handed
    /// one, so a call recorded inside the scope has to be the same run's.
    #[tokio::test]
    async fn a_call_recorded_in_scope_is_visible_to_the_run() {
        let run = run("run_hook");

        with_run(Arc::clone(&run), async {
            current_run()
                .expect("work inside a run has its run")
                .push_tool_call("call_in_scope");
        })
        .await;

        assert_eq!(
            run.pop_tool_call(),
            Some(ToolCallId::new("call_in_scope")),
            "the push reached the run, not a copy of it"
        );
    }

    /// The order holds across a longer run, not only a pair.
    #[test]
    fn a_long_sequence_keeps_its_order() {
        let run = run("run_many");
        for i in 0..64 {
            run.push_tool_call(format!("call_{i}"));
        }

        for i in 0..64 {
            assert_eq!(
                run.pop_tool_call(),
                Some(ToolCallId::new(format!("call_{i}")))
            );
        }
        assert_eq!(run.pop_tool_call(), None);
    }
}
