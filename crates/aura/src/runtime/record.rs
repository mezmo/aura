//! What the runtime holds in memory for each session and each run. Nothing
//! here is serialized; what crosses to a caller is a snapshot of
//! [`RunFacts`] or an [`AgentState`](super::AgentState).

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use aura_events::run::RunCancelReason;
use aura_events::{RunId, SessionId, TokenCount, TokenUsage};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::journal::SessionJournal;
use crate::session_store::{RunFacts, RunStatus};
use crate::streaming_request_hook::UsageState;

/// One session the runtime holds.
pub(crate) struct SessionRecord {
    pub(super) id: SessionId,
    pub(super) journal: Arc<SessionJournal>,
    state: Mutex<SessionState>,
}

/// A session's mutable state, behind one lock.
pub(crate) struct SessionState {
    /// The run executing on the session.
    pub(super) live: Option<Arc<RunRecord>>,
    /// The id and final status of the session's most recent ended run.
    pub(super) last_run: Option<(RunId, RunStatus)>,
    /// Ended runs of the session still within the retention window.
    pub(super) retained: usize,
}

impl SessionState {
    /// Whether the session holds nothing: no live run and no ended run still
    /// retained.
    pub(super) fn is_idle(&self) -> bool {
        self.live.is_none() && self.retained == 0
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
            }),
        }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// A drain has abandoned the run.
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
        }
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
