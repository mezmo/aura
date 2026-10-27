//! The runtime owns every session this process holds, and the run each
//! session is executing.
//!
//! A caller describes a run with [`StartRun`] and gets its [`RunId`] back at
//! once. The runtime finds or creates the session, then prepares the agent
//! and streams the run on a task of its own. Everything the run emits goes to
//! the session's journal, between the run's `Started` event and its ending.
//! A session runs one run at a time.
//!
//! Callers only ever get snapshots: a run's [`RunFacts`], a session's
//! [`AgentState`], or a listing read through the [`RunStore`]. The agent, the
//! run's event channel, and its task are never handed out.

mod observe;
mod prepare;
mod record;
mod recorder;
mod start;
mod state;
mod task;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use aura_config::Config;
use aura_events::run::{
    LifecycleEvent, Liveness, LivenessPolicy, Observer, ObserverId, RunCancelReason,
    SessionEventPayload,
};
use aura_events::{RunId, SessionId};
use tokio_util::sync::CancellationToken;

use crate::hitl::PendingApprovals;
use crate::journal::{Cursor, SessionJournal};
use crate::session_store::{
    InMemoryJournalStore, InMemoryRunStore, InMemorySkillInvocationStore, JournalStore, RunFacts,
    RunFilter, RunStatus, RunStore, SessionStoreError, SkillInvocationStore,
};

pub use observe::{Attach, AttachError, ObserverCounts, Subscription};
pub use prepare::RigBuilderPreparer;
pub use start::{StartError, StartRun};
pub use state::{AgentState, BlockReason};

use prepare::AgentPreparer;
use record::{Abandon, RunRecord, SessionRecord};

/// How long [`AgentRuntime::new`] keeps a run after it ends.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(60);

/// The stores the runtime keeps its sessions and runs in.
pub struct RuntimeStores {
    pub runs: Arc<dyn RunStore>,
    pub journals: Arc<dyn JournalStore>,
    pub skills: Arc<dyn SkillInvocationStore>,
}

impl RuntimeStores {
    /// Stores that keep everything in this process.
    pub fn in_memory() -> Self {
        Self {
            runs: Arc::new(InMemoryRunStore::new()),
            journals: Arc::new(InMemoryJournalStore::new()),
            skills: Arc::new(InMemorySkillInvocationStore::new()),
        }
    }
}

/// Owns the sessions this process holds and the run each is executing.
pub struct AgentRuntime {
    shared: Arc<Shared>,
}

/// What the runtime and its run tasks share.
struct Shared {
    preparer: Arc<dyn AgentPreparer>,
    run_store: Arc<dyn RunStore>,
    journal_store: Arc<dyn JournalStore>,
    skill_store: Arc<dyn SkillInvocationStore>,
    pending_approvals: PendingApprovals,
    sessions: Mutex<HashMap<SessionId, Arc<SessionRecord>>>,
    runs: RwLock<HashMap<RunId, Arc<RunRecord>>>,
    /// Guards finding or adding a session and claiming it for a run.
    admission: Mutex<()>,
    /// How long an ended run stays held.
    retention: Duration,
    draining: AtomicBool,
    /// Cancelled once a drain has let everything go.
    shut_down: CancellationToken,
    /// The task that does the async work a `Drop` cannot.
    recorder: OnceLock<recorder::Recorder>,
}

impl AgentRuntime {
    /// A runtime preparing agents through `preparer`, keeping sessions and
    /// runs in `stores`, and holding each run for [`DEFAULT_RETENTION`] after
    /// it ends.
    pub fn new(preparer: RigBuilderPreparer, stores: RuntimeStores) -> Self {
        Self::with_retention(preparer, stores, DEFAULT_RETENTION)
    }

    /// [`new`](Self::new), holding each run for `retention` after it ends.
    pub fn with_retention(
        preparer: RigBuilderPreparer,
        stores: RuntimeStores,
        retention: Duration,
    ) -> Self {
        let pending_approvals = preparer.pending_approvals().clone();
        Self::with_preparer(Arc::new(preparer), stores, pending_approvals, retention)
    }

    pub(crate) fn with_preparer(
        preparer: Arc<dyn AgentPreparer>,
        stores: RuntimeStores,
        pending_approvals: PendingApprovals,
        retention: Duration,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                preparer,
                run_store: stores.runs,
                journal_store: stores.journals,
                skill_store: stores.skills,
                pending_approvals,
                sessions: Mutex::new(HashMap::new()),
                runs: RwLock::new(HashMap::new()),
                admission: Mutex::new(()),
                retention,
                draining: AtomicBool::new(false),
                shut_down: CancellationToken::new(),
                recorder: OnceLock::new(),
            }),
        }
    }

    /// Starts a run and returns its id as soon as the run is recorded, in
    /// [`RunStatus::Preparing`]. The run's task writes the run to the run
    /// store, prepares the agent, and streams the run; `Started` follows on
    /// the session's journal once it is streaming.
    ///
    /// A start that names no session gets a new one. A start that names no
    /// agent runs the default agent. A session runs one run at a time, so a
    /// start on a session with a live run is refused with that run's id; the
    /// agent may change from one run to the next. Once [`drain`](Self::drain)
    /// has begun, every start is refused.
    pub async fn start(&self, start: StartRun) -> Result<RunId, StartError> {
        let config = self.shared.preparer.config(start.agent.as_deref())?;
        if !honours(&config, start.liveness) {
            return Err(StartError::PolicyUnsupported {
                policy: start.liveness.policy,
                agent: config.agent_id().to_owned(),
            });
        }
        let session_id = start.session.clone().unwrap_or_else(new_session_id);

        // `admission` is held from here until the run's task is spawned, so
        // two starts on one session cannot both find it idle. Nothing below
        // awaits before the spawn. So a caller that drops this future cannot
        // leave the session holding a run that no task will finish.
        let (_admitted, session) = self.shared.admit(&session_id).await?;
        let run_id = RunId::mint();
        let record = {
            let mut state = session.lock();
            if let Some(live) = &state.live {
                return Err(StartError::RunInProgress(live.id));
            }
            let record = Arc::new(RunRecord::new(RunFacts {
                run_id,
                session_id: session_id.clone(),
                agent: config.agent_id().to_owned(),
                status: RunStatus::Preparing,
                liveness: start.liveness,
                started_at: aura_events::Timestamp::now(),
                ended_at: None,
                usage: aura_events::TokenUsage::default(),
                first_seq: None,
                latest_seq: None,
                pending_approvals: Vec::new(),
                checkpoint: None,
                continues: start.continues,
            }));
            state.live = Some(Arc::clone(&record));
            record
        };
        self.shared
            .runs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run_id, Arc::clone(&record));

        let task = tokio::spawn(task::run(
            Arc::clone(&self.shared),
            session,
            Arc::clone(&record),
            config,
            start,
        ));
        record.hold_task(task);
        Ok(run_id)
    }

    /// Stops the run and returns whether it was stopped. Returns `false` for
    /// a run that has ended or that this process does not hold. The run's
    /// task writes the `Cancelled` ending, with `reason` and `message`.
    pub fn cancel(&self, run: &RunId, reason: RunCancelReason, message: Option<String>) -> bool {
        self.shared
            .run(run)
            .is_some_and(|record| record.stop(reason, message))
    }

    /// Shuts the runtime down, in four steps:
    ///
    /// 1. Refuses every later start with [`StartError::ShuttingDown`].
    /// 2. Stops every live run for [`RunCancelReason::Shutdown`], and waits
    ///    up to `grace` for them to end.
    /// 3. Abandons any run still going: aborts its task and writes its
    ///    `Cancelled` ending for it. Writing those endings, and waiting for
    ///    runs that were already writing their own, gets one more `grace` in
    ///    total.
    /// 4. Lets go of every run and session, and the sessions' journals with
    ///    them. Nothing starts after a drain, so nothing needs them.
    pub async fn drain(&self, grace: Duration) {
        let live: Vec<Arc<RunRecord>> = {
            // With the admission lock held, every start has either finished
            // admitting its run, which is then stopped below, or will see
            // `draining` and be refused.
            let _admitted = self.shared.lock_admission();
            self.shared.draining.store(true, Ordering::Release);
            self.shared
                .runs
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .values()
                .filter(|record| !record.status().is_terminal())
                .cloned()
                .collect()
        };
        for record in &live {
            record.stop(RunCancelReason::Shutdown, None);
        }
        if tokio::time::timeout(grace, all_ended(&live)).await.is_err() {
            tracing::warn!(
                ?grace,
                "runs were still going when the shutdown grace period ran out; abandoning them"
            );
        }

        let mut taken = Vec::new();
        let mut abandoned = Vec::new();
        for record in live.iter().filter(|record| !record.status().is_terminal()) {
            match record.abandon() {
                Abandon::Taken => taken.push(Arc::clone(record)),
                Abandon::Abandoned(task) => abandoned.push((Arc::clone(record), task)),
            }
        }
        // One more grace period covers both the endings written here and the
        // ones runs are already writing, so a stalled store cannot hold up
        // shutdown for longer than that.
        let written =
            futures::future::join_all(abandoned.into_iter().map(|(record, task)| async move {
                if let Some(task) = task {
                    let _ = task.await;
                }
                let session_id = record.facts().session_id;
                let session = self.shared.lock_sessions().get(&session_id).cloned();
                if let Some(session) = session {
                    task::end_abandoned(&self.shared, &session, &record).await;
                }
            }));
        let endings = futures::future::join(written, all_ended(&taken));
        if tokio::time::timeout(grace, endings).await.is_err() {
            tracing::warn!(
                ?grace,
                "endings were still being written when the shutdown grace period ran out; letting the runs go without them"
            );
        }

        let runs = std::mem::take(
            &mut *self
                .shared
                .runs
                .write()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for record in runs.values() {
            record.abort_task();
        }
        self.shared.stop_recorder();
        self.shared.shut_down.cancel();
        self.shared.lock_sessions().clear();
    }

    /// Attaches an observer to `session` and returns its `Subscription`: the
    /// session's event stream, read from the cursor the observer asked for.
    /// A session this process does not hold is opened first.
    ///
    /// The subscription joins the stream before the attach is appended to
    /// it, so a subscriber starting live finds its own `ObserverAttached`
    /// among its first events. An event the live run was already appending
    /// at that moment arrives ahead of it.
    ///
    /// What the observer declares applies to the session's live run, which
    /// the attach event names, and to every run that starts while the
    /// observer stays attached. A claim is refused with the holder's id while
    /// the session already has a claimant. Once [`drain`](Self::drain) has
    /// begun, every attach is refused.
    pub async fn attach(
        &self,
        session: &SessionId,
        how: Attach,
    ) -> Result<Subscription, AttachError> {
        let observer = Observer {
            id: new_observer_id(),
            kind: how.kind,
            presence: how.presence,
        };
        self.shared.start_recorder();
        let mut registered = self.shared.register(session, &observer).await?;
        let record = Arc::clone(&registered.record);
        // A live cursor is pinned to the journal's newest number, so the
        // subscription can pick up from the same place on a reopened journal.
        let from = match how.from {
            Cursor::Live => record.journal.latest().map_or(Cursor::Start, Cursor::After),
            from => from,
        };
        let events = record.journal.subscribe(from);
        let attached = LifecycleEvent::ObserverAttached {
            observer: observer.clone(),
        };
        let append = record
            .journal
            .append(registered.run_id, SessionEventPayload::Lifecycle(attached));
        // An attach still writing when a drain has let everything go gives
        // up, so it does not keep the session's journal alive past shutdown.
        tokio::select! {
            appended = append => appended?,
            () = self.shared.shut_down.cancelled() => return Err(AttachError::ShuttingDown),
        };
        registered.recorded = true;
        drop(registered);
        Ok(Subscription::new(
            observer,
            events,
            from,
            &record,
            Arc::downgrade(&self.shared),
        ))
    }

    /// How many observers `session` has, or `None` for a session this
    /// process does not hold.
    pub fn observer_counts(&self, session: &SessionId) -> Option<ObserverCounts> {
        let session = self.shared.lock_sessions().get(session).cloned()?;
        let counts = session.lock().observers.counts();
        Some(counts)
    }

    /// The run's facts, or `None` once the runtime has let the run go, a
    /// retention window after it ends. A live run's usage is current as of
    /// the call.
    pub fn facts(&self, run: &RunId) -> Option<RunFacts> {
        self.shared.run(run).map(|record| record.facts())
    }

    /// Where the session stands, or `None` for a session this process does
    /// not hold. The first case that applies wins:
    ///
    /// - a live run whose agent is being prepared: `Preparing`
    /// - a live run waiting on approvals: `Blocked`
    /// - any other live run: `Running`
    /// - no live run, and the last run parked: `Parked`
    /// - otherwise: `Done`
    pub fn session_state(&self, session: &SessionId) -> Option<AgentState> {
        let session = self.shared.lock_sessions().get(session).cloned()?;
        let state = session.lock();
        if let Some(live) = &state.live {
            let RunFacts {
                status,
                pending_approvals,
                ..
            } = live.facts();
            return Some(match status {
                RunStatus::Preparing => AgentState::Preparing,
                _ if !pending_approvals.is_empty() => AgentState::Blocked {
                    on: BlockReason::Approval {
                        decisions: pending_approvals,
                    },
                },
                _ => AgentState::Running,
            });
        }
        Some(match state.last_run {
            Some((_, RunStatus::Parked)) => AgentState::Parked,
            _ => AgentState::Done,
        })
    }

    /// The runs `filter` selects, read through the run store.
    pub async fn list(&self, filter: &RunFilter) -> Result<Vec<RunFacts>, SessionStoreError> {
        self.shared.run_store.list(filter).await
    }
}

impl Shared {
    fn lock_admission(&self) -> MutexGuard<'_, ()> {
        self.admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<SessionId, Arc<SessionRecord>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run(&self, run: &RunId) -> Option<Arc<RunRecord>> {
        self.runs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run)
            .cloned()
    }

    /// Waits out the retention window, then lets the ended run go, and the
    /// session too if it is then idle.
    async fn retire(&self, session: &Arc<SessionRecord>, record: &RunRecord) {
        tokio::time::sleep(self.retention).await;
        let _admitted = self.lock_admission();
        self.runs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&record.id);
        {
            let mut state = session.lock();
            state.retained = state.retained.saturating_sub(1);
        }
        self.let_go_if_idle_admitted(session);
    }

    /// Takes `admission`, then calls
    /// [`let_go_if_idle_admitted`](Self::let_go_if_idle_admitted).
    fn let_go_if_idle(&self, session: &Arc<SessionRecord>) {
        let _admitted = self.lock_admission();
        self.let_go_if_idle_admitted(session);
    }

    /// Removes the session if it is idle (see `SessionState::is_idle`). Its
    /// journal goes with it, which ends its subscriptions. The store keeps
    /// the session's events and newest sequence number, so a later start
    /// continues the sequence. Only this exact record is removed: if the
    /// session was reopened since, the newer record stays. The caller holds
    /// `admission`.
    fn let_go_if_idle_admitted(&self, session: &Arc<SessionRecord>) {
        if !session.lock().is_idle() {
            return;
        }
        let mut sessions = self.lock_sessions();
        if sessions
            .get(&session.id)
            .is_some_and(|held| Arc::ptr_eq(held, session))
        {
            sessions.remove(&session.id);
        }
    }

    /// Locks `admission` and returns the guard with the session `id`. Fails
    /// once a drain has begun.
    ///
    /// The session needs a new journal when this process does not hold it,
    /// or holds it idle with a journal another writer has fenced. Opening a
    /// journal reads the store, so the lock is released while it opens.
    /// Otherwise a slow store would hold up a drain, and every start on
    /// other sessions.
    ///
    /// After opening, the lock is taken again and both checks run again. A
    /// drain may have begun in the meantime. Another start may also have
    /// opened the session, and then its journal is used and this one is
    /// dropped.
    ///
    /// A reopened session keeps its last run and its observers. Their
    /// subscriptions move to the new journal, and a later detach comes off
    /// the new record. The records are swapped under the sessions lock, so a
    /// failed open changes nothing and a concurrent detach lands on exactly
    /// one record.
    async fn admit(
        &self,
        id: &SessionId,
    ) -> Result<(MutexGuard<'_, ()>, Arc<SessionRecord>), AdmitError> {
        let admitted = self.lock_admission();
        if self.draining.load(Ordering::Acquire) {
            return Err(AdmitError::ShuttingDown);
        }
        if let Some(session) = self.usable_session(id) {
            return Ok((admitted, session));
        }
        drop(admitted);

        let journal = SessionJournal::open(id.clone(), Arc::clone(&self.journal_store)).await?;

        let admitted = self.lock_admission();
        if self.draining.load(Ordering::Acquire) {
            return Err(AdmitError::ShuttingDown);
        }
        if let Some(session) = self.usable_session(id) {
            return Ok((admitted, session));
        }
        let session = Arc::new(SessionRecord::new(id.clone(), journal));
        let mut sessions = self.lock_sessions();
        let fenced = sessions.get(id).cloned();
        if let Some(fenced) = &fenced {
            tracing::info!(session_id = %id, "another writer fenced the session's journal; reopening it");
            let mut from = fenced.lock();
            let mut to = session.lock();
            to.last_run = from.last_run;
            to.observers = std::mem::take(&mut from.observers);
        }
        sessions.insert(id.clone(), Arc::clone(&session));
        drop(sessions);
        if let Some(fenced) = fenced {
            fenced.superseded.cancel();
        }
        Ok((admitted, session))
    }

    /// The session `id`, if this process holds it and can keep using its
    /// journal. An idle session whose journal another writer has fenced needs
    /// a new journal, so it is not returned. A fenced session that still has
    /// a live run is returned anyway, so the start is refused for that run
    /// instead of replacing the session.
    fn usable_session(&self, id: &SessionId) -> Option<Arc<SessionRecord>> {
        let held = self.lock_sessions().get(id).cloned()?;
        let usable = !held.journal.is_fenced() || held.lock().live.is_some();
        usable.then_some(held)
    }
}

/// Why [`Shared::admit`] refused.
enum AdmitError {
    ShuttingDown,
    Store(SessionStoreError),
}

impl From<SessionStoreError> for AdmitError {
    fn from(error: SessionStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<AdmitError> for StartError {
    fn from(refused: AdmitError) -> Self {
        match refused {
            AdmitError::ShuttingDown => Self::ShuttingDown,
            AdmitError::Store(error) => Self::Store(error),
        }
    }
}

impl From<AdmitError> for AttachError {
    fn from(refused: AdmitError) -> Self {
        match refused {
            AdmitError::ShuttingDown => Self::ShuttingDown,
            AdmitError::Store(error) => Self::Store(error),
        }
    }
}

/// An observer whose attach is not yet on the session's stream.
struct Registered {
    shared: Arc<Shared>,
    record: Arc<SessionRecord>,
    observer: ObserverId,
    /// The live run the attach names.
    run_id: Option<RunId>,
    /// Whether the attach reached the session's stream.
    recorded: bool,
}

impl Shared {
    /// Registers `observer` on the session `id`, opening the session when
    /// this process does not hold it, and returns the guard that takes the
    /// observer off again unless its attach reaches the stream. Holds
    /// `admission` only while it does this, so an attach and a start cannot
    /// both open the session, and a stalled append afterwards holds up only
    /// this session. Until the guard is dropped, the session is not let go,
    /// and its runs append neither `Started` nor an ending.
    async fn register(
        self: &Arc<Self>,
        id: &SessionId,
        observer: &Observer,
    ) -> Result<Registered, AttachError> {
        let (_admitted, record) = self.admit(id).await?;
        let mut state = record.lock();
        state
            .observers
            .attach(observer.clone())
            .map_err(|by| AttachError::AlreadyClaimed { by })?;
        state.observer_writes += 1;
        // A run whose stream has ended is past what the observer declares.
        let run_id = state
            .live
            .as_ref()
            .filter(|live| live.stoppable())
            .map(|live| {
                live.mirror(state.observers.attendance());
                live.id
            });
        drop(state);
        Ok(Registered {
            shared: Arc::clone(self),
            record,
            observer: observer.id.clone(),
            run_id,
            recorded: false,
        })
    }
}

impl Drop for Registered {
    /// Ends the attach's count in `observer_writes`. An attach that never
    /// reached the stream also takes its observer off again, without
    /// recording or reacting to it, and lets the session go if it then holds
    /// nothing.
    fn drop(&mut self) {
        if !self.recorded {
            drop(self.shared.unregister(&self.record.id, &self.observer));
        }
        self.record.finish_observer_write();
        if !self.recorded {
            self.shared
                .hand_off(recorder::Chore::LetGo(Arc::clone(&self.record)));
        }
    }
}

/// Resolves once every run in `records` has ended.
async fn all_ended(records: &[Arc<RunRecord>]) {
    futures::future::join_all(records.iter().map(|record| async {
        let mut facts = record.watch();
        let _ = facts.wait_for(|facts| facts.status.is_terminal()).await;
    }))
    .await;
}

/// Whether an agent configured as `config` can honour `liveness`. Any agent
/// can cancel or continue. Only an orchestrated agent with `[hitl.park]`
/// enabled can park. A policy this build does not know is never honoured.
fn honours(config: &Config, liveness: Liveness) -> bool {
    match liveness.policy {
        LivenessPolicy::Cancel | LivenessPolicy::Continue => true,
        LivenessPolicy::Park => {
            config.orchestration_enabled()
                && config.hitl.as_ref().is_some_and(|hitl| hitl.park.enabled)
        }
        _ => false,
    }
}

/// A new id: `prefix`, an underscore, and a random UUID.
fn mint_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// A new observer id.
fn new_observer_id() -> ObserverId {
    ObserverId::new(mint_id("ob")).expect("a minted id starts with its prefix, so it is not blank")
}

/// A new session id, in the form the server gives a chat session it mints.
fn new_session_id() -> SessionId {
    SessionId::new(mint_id("cs")).expect("a minted id starts with its prefix, so it is not blank")
}
