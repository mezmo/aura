//! What the runtime holds in memory for each session and each run. Nothing
//! here is serialized; what crosses to a caller is a snapshot of
//! [`RunFacts`] or an [`AgentState`](super::AgentState).

use std::collections::BTreeSet;
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
    /// The session's observer attaches and detaches still being written.
    observer_writes: Arc<ObserverWrites>,
}

/// Observer attaches and detaches under way on a session, not yet on its
/// stream.
#[derive(Default)]
struct ObserverWrites {
    queue: Mutex<WriteQueue>,
    /// Signals that one of them is no longer being written.
    ended: Notify,
}

/// The numbers given to a session's observer writes, and the ones still
/// unfinished.
#[derive(Default)]
struct WriteQueue {
    /// The number the next write is given.
    next: u64,
    unfinished: BTreeSet<u64>,
}

impl ObserverWrites {
    fn queue(&self) -> MutexGuard<'_, WriteQueue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One attach or detach counted among a session's observer writes.
pub(super) struct ObserverWrite {
    writes: Arc<ObserverWrites>,
    /// The write's place in the session's order.
    number: u64,
    finished: bool,
}

impl ObserverWrite {
    /// Waits until every write counted before this one is finished, so the
    /// session's attaches and detaches reach its stream in the order they
    /// were counted.
    pub(super) async fn turn(&self) {
        loop {
            let notified = self.writes.ended.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.writes.queue().unfinished.first() == Some(&self.number) {
                return;
            }
            notified.await;
        }
    }

    /// Ends this write, written or not. It leaves the queue at once, so a
    /// write that gave up keeps nothing for later writes to wait on.
    pub(super) fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.writes.queue().unfinished.remove(&self.number);
            self.writes.ended.notify_waiters();
        }
    }
}

impl Drop for ObserverWrite {
    fn drop(&mut self) {
        self.finish();
    }
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
            }),
            superseded: CancellationToken::new(),
            observer_writes: Arc::default(),
        }
    }

    /// A record for `fenced`'s session over a new journal. It takes over the
    /// fenced record's last run and observers, and shares its count of
    /// observer writes under way, since those writes now go to the new
    /// journal and the session must not be let go before they finish.
    pub(super) fn reopening(fenced: &Self, journal: SessionJournal) -> Self {
        let mut from = fenced.lock();
        Self {
            id: fenced.id.clone(),
            journal: Arc::new(journal),
            state: Mutex::new(SessionState {
                live: None,
                last_run: from.last_run,
                retained: 0,
                observers: std::mem::take(&mut from.observers),
            }),
            superseded: CancellationToken::new(),
            observer_writes: Arc::clone(&fenced.observer_writes),
        }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the session holds nothing: no live run, no retained run, no
    /// observers, and no attach or detach still being written.
    pub(super) fn is_idle(&self) -> bool {
        let state = self.lock();
        state.live.is_none()
            && state.retained == 0
            && state.observers.is_empty()
            && self.observer_writes.queue().unfinished.is_empty()
    }

    /// Counts one more attach or detach as being written, and returns it
    /// numbered after every write counted before it. Called with the
    /// session's state locked, alongside the change it records.
    pub(super) fn begin_observer_write(&self) -> ObserverWrite {
        let mut queue = self.observer_writes.queue();
        let number = queue.next;
        queue.next += 1;
        queue.unfinished.insert(number);
        drop(queue);
        ObserverWrite {
            writes: Arc::clone(&self.observer_writes),
            number,
            finished: false,
        }
    }

    /// Waits until every attach and detach counted before the wait began is
    /// finished. Later ones are not waited for: each counts its write before
    /// checking whether a run is still going, so none of them can name a run
    /// already past stopping.
    pub(super) async fn observer_writes_done(&self) {
        let until = self.observer_writes.queue().next;
        loop {
            let notified = self.observer_writes.ended.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let done = self
                .observer_writes
                .queue()
                .unfinished
                .range(..until)
                .next()
                .is_none();
            if done {
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
    task: Mutex<Option<JoinHandle<()>>>,
    ending: watch::Sender<Ending>,
    /// The run's context.
    context: Mutex<Option<Arc<RunContext>>>,
    /// How many of the run's stream items have shown activity.
    activity: watch::Sender<u64>,
}

/// Follows one run's activity.
pub struct RunActivity(watch::Receiver<u64>);

impl RunActivity {
    /// Resolves once the run has shown activity since the last wait, or, on
    /// the first wait, since the run began. Activity that comes while nothing
    /// waits is reported once, by the next wait. Never resolves once the
    /// runtime has let go of the run.
    pub async fn next(&mut self) {
        if self.0.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
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
    /// The run's ending is already someone else's to write, or written.
    Taken,
    /// The caller writes the run's ending. Holds the run's task, already
    /// aborted.
    Abandoned(Option<JoinHandle<()>>),
}

/// Who writes a run's ending, and whether it is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// No one has claimed it.
    Unclaimed,
    /// The run's task writes it.
    Task,
    /// A drain aborted the run's task and writes it.
    Drain,
    /// Nothing more will be written.
    Settled,
}

impl RunRecord {
    pub(super) fn new(facts: RunFacts) -> Self {
        Self {
            id: facts.run_id,
            cancel: CancellationToken::new(),
            stops: Mutex::new(Stops::default()),
            usage: OnceLock::new(),
            facts: watch::Sender::new(facts),
            task: Mutex::new(None),
            ending: watch::Sender::new(Ending::Unclaimed),
            context: Mutex::new(None),
            activity: watch::Sender::new(0),
        }
    }

    /// Records that the run showed activity.
    pub(super) fn note_activity(&self) {
        self.activity
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    pub(super) fn activity(&self) -> RunActivity {
        let mut activity = self.activity.subscribe();
        if *activity.borrow() > 0 {
            activity.mark_changed();
        }
        RunActivity(activity)
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

    /// A receiver that sees every change to the run's facts.
    #[cfg(test)]
    pub(super) fn watch(&self) -> watch::Receiver<RunFacts> {
        self.facts.subscribe()
    }

    /// Keeps the run's usage counters, which the run writes as it goes.
    pub(super) fn track_usage(&self, usage: UsageState) {
        let _ = self.usage.set(usage);
    }

    /// The run's usage counters. `None` until the agent starts streaming the
    /// run.
    pub(super) fn counters(&self) -> Option<&UsageState> {
        self.usage.get()
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

    fn task(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        self.task.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn hold_task(&self, handle: JoinHandle<()>) {
        *self.task() = Some(handle);
    }

    /// Moves the ending from `Unclaimed` to `claimant`, and returns whether
    /// it did. The move happens under the channel's lock, so two claimants
    /// cannot both succeed.
    #[must_use]
    fn claim_ending(&self, claimant: Ending) -> bool {
        self.ending.send_if_modified(|ending| {
            let claimed = *ending == Ending::Unclaimed;
            if claimed {
                *ending = claimant;
            }
            claimed
        })
    }

    /// Claims the right to write the run's ending for its task. Returns
    /// `false` if a drain abandoned the run first, in which case the drain
    /// writes the ending.
    #[must_use]
    pub(super) fn begin_ending(&self) -> bool {
        self.claim_ending(Ending::Task)
    }

    /// Aborts the run's task and claims the right to write its ending, unless
    /// the task has already begun writing it, an earlier call abandoned the
    /// run, or the ending is done.
    #[must_use]
    pub(super) fn abandon(&self) -> Abandon {
        if !self.claim_ending(Ending::Drain) {
            return Abandon::Taken;
        }
        let handle = self.task().take();
        if let Some(handle) = &handle {
            handle.abort();
        }
        Abandon::Abandoned(handle)
    }

    /// Marks the run's ending done: nothing more will be written for it.
    pub(super) fn settle(&self) {
        self.ending.send_replace(Ending::Settled);
    }

    pub(super) fn is_settled(&self) -> bool {
        *self.ending.borrow() == Ending::Settled
    }

    /// Resolves once the run's ending is settled.
    pub(super) async fn settled(&self) {
        let mut ending = self.ending.subscribe();
        let _ = ending.wait_for(|ending| *ending == Ending::Settled).await;
    }

    /// Aborts the run's task, whatever it is doing.
    pub(super) fn abort_task(&self) {
        if let Some(handle) = &*self.task() {
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
