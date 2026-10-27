//! What the runtime holds in memory for each session and each run. Nothing
//! here is serialized; what crosses to a caller is a snapshot of
//! [`RunFacts`] or an [`AgentState`](super::AgentState).

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use aura_events::run::{Liveness, RunCancelReason};
use aura_events::{RunId, SessionId, TokenCount, TokenUsage};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::journal::SessionJournal;
use crate::run_context::{Attendance, RunContext};
use crate::session_store::{RunFacts, RunStatus};
use crate::streaming_request_hook::UsageState;

use super::observe::ObserverSet;

/// One session the runtime holds.
pub(crate) struct SessionRecord {
    pub(super) id: SessionId,
    pub(super) journal: Arc<SessionJournal>,
    state: Mutex<SessionState>,
    /// Signals that a reopened record has replaced this one.
    pub(super) superseded: CancellationToken,
    /// Signals that an observer's attach or detach is no longer being
    /// written.
    observer_write_ended: Notify,
}

/// A session's mutable state, behind one lock.
pub(crate) struct SessionState {
    /// The run executing on the session.
    pub(super) live: Option<Arc<RunRecord>>,
    /// The id and final status of the session's most recent ended run.
    pub(super) last_run: Option<(RunId, RunStatus)>,
    /// Ended runs of the session still within the retention window.
    pub(super) retained: usize,
    /// The observers attached to the session.
    pub(super) observers: ObserverSet,
    /// Observer attaches and detaches not yet on the session's stream.
    pub(super) observer_writes: usize,
}

impl SessionState {
    /// Whether the session has no live run, no retained run, no observers,
    /// and no attach or detach still being written.
    pub(super) fn is_idle(&self) -> bool {
        self.live.is_none()
            && self.retained == 0
            && self.observers.is_empty()
            && self.observer_writes == 0
    }
}

impl SessionRecord {
    pub(super) fn new(id: SessionId, journal: SessionJournal) -> Self {
        Self {
            id,
            journal: Arc::new(journal),
            state: Mutex::new(SessionState {
                live: None,
                last_run: None,
                retained: 0,
                observers: ObserverSet::default(),
                observer_writes: 0,
            }),
            superseded: CancellationToken::new(),
            observer_write_ended: Notify::new(),
        }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends one attach or detach counted in `observer_writes`, written or
    /// not.
    pub(super) fn finish_observer_write(&self) {
        let mut state = self.lock();
        state.observer_writes = state.observer_writes.saturating_sub(1);
        drop(state);
        self.observer_write_ended.notify_waiters();
    }

    /// Waits until no observer attach or detach is being written.
    pub(super) async fn observer_writes_done(&self) {
        loop {
            let notified = self.observer_write_ended.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lock().observer_writes == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// One run the runtime holds.
pub(crate) struct RunRecord {
    pub(super) id: RunId,
    /// The token that stops the run.
    pub(super) cancel: CancellationToken,
    stops: Mutex<Stops>,
    /// The run's live usage counters.
    usage: OnceLock<UsageState>,
    facts: watch::Sender<RunFacts>,
    task: Mutex<TaskSlot>,
    /// The run's context.
    context: Mutex<Option<Arc<RunContext>>>,
}

/// Whether and why a run was stopped.
#[derive(Default)]
struct Stops {
    /// The reason and message the run was stopped with.
    reason: Option<(RunCancelReason, Option<String>)>,
    /// Whether the run can no longer be stopped.
    closed: bool,
}

/// The result of [`RunRecord::abandon`].
pub(crate) enum Abandon {
    /// Someone else writes the run's ending: the run's own task, or an
    /// earlier call to `abandon`.
    Taken,
    /// The caller writes the run's ending. Holds the run's task, already
    /// aborted.
    Abandoned(Option<JoinHandle<()>>),
}

/// The run's task, and who has claimed the right to write the run's ending.
#[derive(Default)]
struct TaskSlot {
    handle: Option<JoinHandle<()>>,
    /// The task has begun writing the ending.
    ending: bool,
    /// A drain aborted the task and writes the ending.
    abandoned: bool,
}

impl RunRecord {
    pub(super) fn new(facts: RunFacts) -> Self {
        Self {
            id: facts.run_id,
            cancel: CancellationToken::new(),
            stops: Mutex::new(Stops::default()),
            usage: OnceLock::new(),
            facts: watch::Sender::new(facts),
            task: Mutex::new(TaskSlot::default()),
            context: Mutex::new(None),
        }
    }

    fn context_slot(&self) -> MutexGuard<'_, Option<Arc<RunContext>>> {
        self.context.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores the run's context, writing `attendance` onto it first.
    pub(super) fn bind_context(&self, context: Arc<RunContext>, attendance: Attendance) {
        context.attend(attendance);
        *self.context_slot() = Some(context);
    }

    pub(super) fn unbind_context(&self) {
        *self.context_slot() = None;
    }

    /// The run's context, while the run streams.
    #[cfg(test)]
    pub(super) fn context(&self) -> Option<Arc<RunContext>> {
        self.context_slot().clone()
    }

    /// Writes `attendance` onto the run's context, when one is bound.
    pub(super) fn mirror(&self, attendance: Attendance) {
        if let Some(context) = &*self.context_slot() {
            context.attend(attendance);
        }
    }

    pub(super) fn liveness(&self) -> Liveness {
        self.facts.borrow().liveness
    }

    /// The run's facts. A live run's usage is read from its counters now,
    /// since the stored facts only get usage once the run ends.
    pub(super) fn facts(&self) -> RunFacts {
        let mut facts = self.facts.borrow().clone();
        if !facts.status.is_terminal()
            && let Some(usage) = self.usage.get()
        {
            facts.usage = token_usage(usage);
        }
        facts
    }

    /// Changes the run's facts and wakes whoever watches them.
    pub(super) fn update(&self, change: impl FnOnce(&mut RunFacts)) {
        self.facts.send_modify(change);
    }

    pub(super) fn status(&self) -> RunStatus {
        self.facts.borrow().status
    }

    /// A receiver that sees every change to the run's facts.
    pub(super) fn watch(&self) -> watch::Receiver<RunFacts> {
        self.facts.subscribe()
    }

    /// Keeps the run's usage counters, which the run writes as it goes.
    pub(super) fn track_usage(&self, usage: UsageState) {
        let _ = self.usage.set(usage);
    }

    fn stops(&self) -> MutexGuard<'_, Stops> {
        self.stops.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stops the run for `reason` and returns whether it did. The first
    /// reason given is the one the run ends with, and a later stop does not
    /// change it. Returns `false` once the run's stream has ended, because
    /// by then the run's ending is already decided.
    pub(super) fn stop(&self, reason: RunCancelReason, message: Option<String>) -> bool {
        let mut stops = self.stops();
        if stops.closed {
            return false;
        }
        stops.reason.get_or_insert((reason, message));
        self.cancel.cancel();
        true
    }

    /// Whether a stop would still be taken.
    pub(super) fn stoppable(&self) -> bool {
        !self.stops().closed
    }

    /// Refuses every later stop.
    pub(super) fn close_to_stops(&self) {
        self.stops().closed = true;
    }

    /// The reason and message the run was stopped with, if it was stopped.
    pub(super) fn stopped(&self) -> Option<(RunCancelReason, Option<String>)> {
        self.stops().reason.clone()
    }

    fn task(&self) -> MutexGuard<'_, TaskSlot> {
        self.task.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn hold_task(&self, handle: JoinHandle<()>) {
        self.task().handle = Some(handle);
    }

    /// Claims the right to write the run's ending for its task. Returns
    /// `false` if a drain abandoned the run first, in which case the drain
    /// writes the ending.
    pub(super) fn begin_ending(&self) -> bool {
        let mut task = self.task();
        if task.abandoned {
            return false;
        }
        task.ending = true;
        true
    }

    /// Aborts the run's task and claims the right to write its ending, unless
    /// the task has already begun writing it or an earlier call abandoned
    /// the run.
    pub(super) fn abandon(&self) -> Abandon {
        let mut task = self.task();
        if task.ending || task.abandoned {
            return Abandon::Taken;
        }
        task.abandoned = true;
        let handle = task.handle.take();
        if let Some(handle) = &handle {
            handle.abort();
        }
        Abandon::Abandoned(handle)
    }

    /// Aborts the run's task, whatever it is doing.
    pub(super) fn abort_task(&self) {
        if let Some(handle) = &self.task().handle {
            handle.abort();
        }
    }

    /// The run's usage so far.
    pub(super) fn usage(&self) -> TokenUsage {
        self.usage.get().map(token_usage).unwrap_or_default()
    }
}

/// The cumulative provider-billed token totals from a run's usage counters.
fn token_usage(usage: &UsageState) -> TokenUsage {
    let (prompt, completion, total) = usage.get_final_usage();
    TokenUsage {
        prompt_tokens: TokenCount::new(prompt),
        completion_tokens: TokenCount::new(completion),
        total_tokens: TokenCount::new(total),
    }
}
