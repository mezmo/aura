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

use aura_events::{RunId, ToolCallId};

use crate::scratchpad::ContextBudget;
use crate::skill_tool::SkillInvocationRecorder;
use crate::turn_nudge::TurnNudgeState;

/// Events a run may buffer before its observer reads them.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// One run — what its own work needs to correlate, where its events go, and
/// the state an agent's tools keep for it.
pub struct RunContext {
    id: RunId,
    tool_calls: Mutex<VecDeque<ToolCallId>>,
    events: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    /// The run's context budget.
    scratchpad_budget: Option<ContextBudget>,
    /// The run's turn-limit tracking.
    turn_nudge: Option<Arc<TurnNudgeState>>,
    /// Where the run's skill-tool invocations are recorded.
    skill_recorder: Option<Arc<SkillInvocationRecorder>>,
}

/// Pending tool ids before warning, in case results never arrive to pop them.
const MAX_PENDING_TOOL_CALLS: usize = 256;

impl RunContext {
    /// A run and the receiver its observer reads, on a token of its own.
    pub fn channel(id: RunId) -> (Arc<Self>, mpsc::Receiver<AgentEvent>) {
        Self::channel_on(id, CancellationToken::new())
    }

    /// A run on `cancel`, for a caller that already holds the token the run is
    /// to stop on — a child of its own caller's, so one run ending leaves the
    /// others alone.
    pub fn channel_on(
        id: RunId,
        cancel: CancellationToken,
    ) -> (Arc<Self>, mpsc::Receiver<AgentEvent>) {
        Self::channel_for_agent(id, cancel, None, None, None)
    }

    /// A run on `cancel` carrying the state a prepared agent's tools keep for
    /// it, and the receiver its observer reads.
    pub fn channel_for_agent(
        id: RunId,
        cancel: CancellationToken,
        scratchpad_budget: Option<ContextBudget>,
        turn_nudge: Option<Arc<TurnNudgeState>>,
        skill_recorder: Option<Arc<SkillInvocationRecorder>>,
    ) -> (Arc<Self>, mpsc::Receiver<AgentEvent>) {
        let (events, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let run = Arc::new(Self {
            id,
            tool_calls: Mutex::new(VecDeque::new()),
            events,
            cancel,
            scratchpad_budget,
            turn_nudge,
            skill_recorder,
        });
        (run, receiver)
    }

    /// A run within `parent` for one agent of it, an orchestration worker or
    /// coordinator: the same id, observer and cancellation, with tool state
    /// of its own. Its tool-call queue stays empty, an agent within a run
    /// streaming under a key of its own rather than the run's id.
    pub fn child(
        parent: &Arc<Self>,
        scratchpad_budget: Option<ContextBudget>,
        turn_nudge: Option<Arc<TurnNudgeState>>,
        skill_recorder: Option<Arc<SkillInvocationRecorder>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: parent.id,
            tool_calls: Mutex::new(VecDeque::new()),
            events: parent.events.clone(),
            cancel: parent.cancel.clone(),
            scratchpad_budget,
            turn_nudge,
            skill_recorder,
        })
    }

    /// A run nobody observes, for a test that needs one to exist without
    /// reading what it emits. Production names a run it can reach an observer
    /// through, or names none.
    #[cfg(test)]
    pub fn detached(id: RunId) -> Arc<Self> {
        Self::channel(id).0
    }

    /// [`detached`](Self::detached), carrying tool state.
    #[cfg(test)]
    pub(crate) fn detached_with(
        id: RunId,
        scratchpad_budget: Option<ContextBudget>,
        turn_nudge: Option<Arc<TurnNudgeState>>,
    ) -> Arc<Self> {
        Self::channel_for_agent(
            id,
            CancellationToken::new(),
            scratchpad_budget,
            turn_nudge,
            None,
        )
        .0
    }

    /// The run's context budget.
    pub fn scratchpad_budget(&self) -> Option<&ContextBudget> {
        self.scratchpad_budget.as_ref()
    }

    /// The run's turn-limit tracking.
    pub fn turn_nudge(&self) -> Option<&Arc<TurnNudgeState>> {
        self.turn_nudge.as_ref()
    }

    /// Where the run's skill-tool invocations are recorded.
    pub fn skill_recorder(&self) -> Option<&Arc<SkillInvocationRecorder>> {
        self.skill_recorder.as_ref()
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

    pub fn id(&self) -> RunId {
        self.id
    }

    /// Whether `id` is this run's id as the run spells it — the form its
    /// `Display` gives, which is the key every request-keyed registry holds.
    /// The same UUID spelled another way names no run, and neither does a
    /// string that is not one.
    pub fn has_id(&self, id: &str) -> bool {
        let mut spelled = uuid::Uuid::encode_buffer();
        *self.id.as_uuid().hyphenated().encode_lower(&mut spelled) == *id
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

    /// A slot holding `run`.
    pub fn holding(run: Arc<RunContext>) -> Self {
        Self(Mutex::new(Some(run)))
    }

    /// Replaces any run already bound. One slot is enough because a prepared
    /// agent serves one run at a time; `PreparedAgent::begin_run` is what
    /// refuses a second while the first is alive.
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

    /// The run's id, or empty outside a run so an approval raised then still
    /// carries a well-formed id even though nothing routes it.
    pub fn id_or_empty(&self) -> String {
        self.get()
            .map(|run| run.id().to_string())
            .unwrap_or_default()
    }

    /// The run's context budget.
    pub fn scratchpad_budget(&self) -> Option<ContextBudget> {
        self.get().and_then(|run| run.scratchpad_budget().cloned())
    }

    /// The run's turn-limit tracking.
    pub fn turn_nudge(&self) -> Option<Arc<TurnNudgeState>> {
        self.get().and_then(|run| run.turn_nudge().cloned())
    }

    /// Where the run's skill-tool invocations are recorded.
    pub fn skill_recorder(&self) -> Option<Arc<SkillInvocationRecorder>> {
        self.get().and_then(|run| run.skill_recorder().cloned())
    }

    /// A slot holding an unobserved run that carries only `budget`.
    #[cfg(test)]
    pub(crate) fn pinned_budget(budget: ContextBudget) -> Self {
        Self::holding(RunContext::detached_with(RunId::mint(), Some(budget), None))
    }

    /// A slot holding an unobserved run that carries only `turn_nudge`.
    #[cfg(test)]
    pub(crate) fn pinned_nudge(turn_nudge: Arc<TurnNudgeState>) -> Self {
        Self::holding(RunContext::detached_with(
            RunId::mint(),
            None,
            Some(turn_nudge),
        ))
    }
}

impl std::fmt::Debug for BoundRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundRun")
            .field("run_id", &self.get().map(|run| run.id().to_string()))
            .finish()
    }
}

/// A run in progress on a prepared agent.
pub struct RunLease {
    run: Arc<RunContext>,
}

impl RunLease {
    pub(crate) fn new(run: Arc<RunContext>) -> Self {
        Self { run }
    }

    pub fn run(&self) -> &Arc<RunContext> {
        &self.run
    }
}

/// A prepared agent was asked to begin a run while it still serves another.
#[derive(Debug, thiserror::Error)]
#[error("prepared agent already serves run `{active}`; it serves one run at a time")]
pub struct RunInProgress {
    /// Id of the run holding the agent.
    pub active: RunId,
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

pub fn current_run_id() -> Option<RunId> {
    RUN.try_with(|run| run.id()).ok()
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

/// Runs `f` with a fresh run in scope and returns what it emitted, for a test
/// that asserts on a run's events without standing up an observer.
#[cfg(test)]
pub(crate) async fn observing<F: Future>(name: &str, f: F) -> (F::Output, Vec<AgentEvent>) {
    let (run, mut events) = RunContext::channel(named_run_id(name));
    let out = with_run(run, f).await;

    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    (out, seen)
}

/// The same run id for the same `name` every time, so a test can name a run
/// and compare against that name later.
#[cfg(test)]
pub(crate) fn named_run_id(name: &str) -> RunId {
    RunId::try_from(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        name.as_bytes(),
    ))
    .expect("a v5 UUID is never nil")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn run(name: &str) -> Arc<RunContext> {
        RunContext::detached(named_run_id(name))
    }

    #[tokio::test]
    async fn a_scope_established_inside_a_spawn_holds() {
        let (run, _rx) = RunContext::channel(named_run_id("spawned"));
        let seen = tokio::spawn(with_run(run, async { current_run_id() }))
            .await
            .unwrap();
        assert_eq!(seen, Some(named_run_id("spawned")));
    }

    /// A run built on the caller's token stops when the caller does. The work
    /// that awaits cancellation reads the token off the run, so a run holding a
    /// different token than the one the handler fires would leave a gated call
    /// waiting on a signal nobody sends.
    #[tokio::test]
    async fn a_run_stops_on_the_token_it_was_built_on() {
        let caller = CancellationToken::new();
        let (run, _events) = RunContext::channel_on(named_run_id("run_on_token"), caller.clone());
        assert!(!run.cancel_token().is_cancelled());

        caller.cancel();
        assert!(run.cancel_token().is_cancelled());
    }

    /// A run given no token has one of its own, so nothing reads a token that
    /// cancels something else.
    #[tokio::test]
    async fn a_run_given_no_token_has_its_own() {
        let (a, _ea) = RunContext::channel(named_run_id("run_a"));
        let (b, _eb) = RunContext::channel(named_run_id("run_b"));

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

    /// Registries key a run by the string its id displays as, so that is the
    /// one spelling a run answers to: not the same UUID in another case or
    /// form, and not a string that is no run id at all.
    #[test]
    fn a_run_answers_only_to_its_id_as_it_spells_it() {
        let run = run("run_spelled");
        let spelled = run.id().to_string();

        assert!(run.has_id(&spelled));
        assert!(!run.has_id(&spelled.to_uppercase()));
        assert!(!run.has_id(&run.id().as_uuid().simple().to_string()));
        assert!(!run.has_id(&format!("urn:uuid:{spelled}")));
        assert!(!run.has_id("req_1"));
        assert!(!run.has_id(&named_run_id("run_other").to_string()));
    }

    #[tokio::test]
    async fn a_scope_supplies_the_run() {
        let seen = with_run(run("run_1"), async { current_run_id() }).await;
        assert_eq!(seen, Some(named_run_id("run_1")));
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

        assert_eq!(a.await.unwrap(), Some(named_run_id("run_a")));
        assert_eq!(b.await.unwrap(), Some(named_run_id("run_b")));
    }

    #[tokio::test]
    async fn a_scoped_stream_carries_the_run_into_each_poll() {
        let inner = futures::stream::iter(0..3).map(|_| current_run_id());
        let seen: Vec<_> = scope_stream(run("run_s"), inner).collect().await;

        assert_eq!(seen, vec![Some(named_run_id("run_s")); 3]);
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

        assert_eq!(seen, vec![Some(named_run_id("run_w")); 3]);
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
    fn an_unbound_slot_outside_a_run_resolves_nothing() {
        let slot = BoundRun::default();
        assert!(slot.get().is_none());
        assert_eq!(slot.id_or_empty(), "");
        assert!(slot.scratchpad_budget().is_none());
        assert!(slot.turn_nudge().is_none());
    }

    /// The state a prepared agent's tools keep for a run reaches them through
    /// the slot they were built with, and follows whichever run is bound.
    #[test]
    fn tool_state_reaches_the_slot_from_the_bound_run() {
        use crate::scratchpad::TiktokenCounter;

        let budget =
            ContextBudget::new(1_000, 0.0, 0, Arc::new(TiktokenCounter::default_counter()));
        let nudge = TurnNudgeState::new(true, None, 2).unwrap();
        let slot = BoundRun::default();
        slot.bind(RunContext::detached_with(
            named_run_id("req_a"),
            Some(budget.clone()),
            Some(Arc::clone(&nudge)),
        ));

        assert_eq!(slot.id_or_empty(), named_run_id("req_a").to_string());
        slot.scratchpad_budget().unwrap().record_intercepted(7);
        assert_eq!(
            budget.scratchpad_usage().0,
            7,
            "the slot hands out the run's own budget, counters shared",
        );
        assert!(Arc::ptr_eq(&slot.turn_nudge().unwrap(), &nudge));

        slot.bind(RunContext::detached(named_run_id("req_b")));
        assert_eq!(slot.id_or_empty(), named_run_id("req_b").to_string());
        assert!(slot.scratchpad_budget().is_none());
    }

    /// A worker's run is the orchestration run as its observer and its
    /// cancellation see it, with the worker's own tool state.
    #[tokio::test]
    async fn a_child_shares_its_parents_identity_and_keeps_its_own_state() {
        let (parent, mut events) = RunContext::channel(named_run_id("req_parent"));
        let nudge = TurnNudgeState::new(true, None, 2).unwrap();
        let child = RunContext::child(&parent, None, Some(Arc::clone(&nudge)), None);

        assert_eq!(child.id(), parent.id());
        assert!(Arc::ptr_eq(child.turn_nudge().unwrap(), &nudge));
        assert!(parent.turn_nudge().is_none(), "the parent keeps none of it");

        child
            .emit(AgentEvent::new(
                aura_events::AgentContext::single_agent(),
                aura_events::agent::AgentEventPayload::TextDelta {
                    content: "hi".into(),
                },
            ))
            .await;
        assert!(
            events.try_recv().is_ok(),
            "what the child emits reaches the parent's observer",
        );

        parent.cancel_token().cancel();
        assert!(
            child.cancel_token().is_cancelled(),
            "stopping the run stops the worker",
        );
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
