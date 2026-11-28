//! The runtime owns every session this process holds, and the run each
//! session is executing.
//!
//! A caller describes a run with [`StartRun`] and gets its [`RunId`] back at
//! once. The runtime finds or creates the session, then prepares the agent
//! and streams the run on a task of its own. Everything the run emits goes to
//! the session's journal, between the run's `Started` event and its ending.
//! A session runs one run at a time. A conversation, which is a chat id and
//! the agent answering it, has up to
//! [`LANES_PER_CONVERSATION`](crate::conversation::LANES_PER_CONVERSATION)
//! sessions called lanes, so it can run several at once.
//!
//! Callers get snapshots: a run's [`RunFacts`], a session's [`AgentState`],
//! or a listing read through the [`RunStore`]. The one live handle is the
//! [`Subscription`] that [`attach`](AgentRuntime::attach) returns, which reads
//! a session's events. The agent, the run's event channel, and its task are
//! never handed out.

mod lanes;
mod observe;
mod otel;
mod prepare;
mod record;
mod recorder;
mod snapshot;
mod start;
mod state;
mod task;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use aura_config::Config;
use aura_events::run::{
    CancelCause, LifecycleEvent, Liveness, LivenessPolicy, Observer, ObserverId,
    SessionEventPayload,
};
use aura_events::{RunId, SequenceNumber, SessionId};
use tokio_util::sync::CancellationToken;

use crate::conversation::Conversation;
use crate::hitl::PendingApprovals;
use crate::journal::{Cursor, SessionJournal};
use crate::session_store::{
    InMemoryJournalStore, InMemoryRunStore, InMemorySkillInvocationStore, JournalEntry,
    JournalStore, RunDetails, RunFacts, RunFilter, RunStatus, RunStatuses, RunStore, SessionFilter,
    SessionRuns, SessionStoreError, SkillInvocationStore,
};

pub use crate::conversation::RunSession;
pub use observe::{Attach, AttachError, ObserverCounts, Subscription};
pub use otel::record_ending;
#[cfg(feature = "test-support")]
pub use prepare::AgentForRun;
pub use prepare::RigBuilderPreparer;
pub use record::RunActivity;
pub use snapshot::{
    EventRange, PageLimit, RunPage, RunQuery, RunSnapshot, SessionPage, SessionQuery,
    SessionSnapshot, StopOutcome,
};
pub use start::{Admitted, BusyLane, Caller, RunTarget, StartError, StartRun};
pub use state::{AgentState, AgentStateKind, BlockReason};

use prepare::AgentPreparer;
use record::{Abandon, ObserverWrite, RunRecord, SessionRecord, SessionState};

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
    /// How many sessions have a live run.
    live_runs: tokio::sync::watch::Sender<usize>,
    /// Guards finding or adding a session and claiming it for a run.
    admission: Mutex<()>,
    /// How long an ended run stays held.
    retention: Duration,
    draining: AtomicBool,
    /// The signal that the runtime has shut down.
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
                live_runs: tokio::sync::watch::Sender::new(0),
                admission: Mutex::new(()),
                retention,
                draining: AtomicBool::new(false),
                shut_down: CancellationToken::new(),
                recorder: OnceLock::new(),
            }),
        }
    }

    /// A runtime that gets each run's agent from `agents` instead of building
    /// it from `configs`. It is for tests that drive the runtime with a
    /// scripted agent.
    ///
    /// `agents` is given the resolved config and the run's id. It returns the
    /// agent for the run and the agent its events are attributed to, or the
    /// error that preparing the agent would have failed with. Agent names
    /// resolve as they do in [`RigBuilderPreparer`]. Approvals are kept in
    /// memory, and ended runs are held for [`DEFAULT_RETENTION`].
    #[cfg(feature = "test-support")]
    pub fn over_agents<F>(
        configs: Arc<Vec<Config>>,
        default_agent: Option<String>,
        stores: RuntimeStores,
        agents: F,
    ) -> Self
    where
        F: Fn(&Config, RunId) -> Result<AgentForRun, crate::error::BuilderError>
            + Send
            + Sync
            + 'static,
    {
        let preparer = prepare::AgentFactoryPreparer::new(configs, default_agent, Box::new(agents));
        Self::with_preparer(
            Arc::new(preparer),
            stores,
            PendingApprovals::new(),
            DEFAULT_RETENTION,
        )
    }

    /// Starts a run and returns its id as soon as the run is recorded, in
    /// [`RunStatus::Preparing`]. The run's task writes the run to the run
    /// store, prepares the agent, and streams the run; `Started` follows on
    /// the session's journal once it is streaming.
    ///
    /// A start that names no agent runs the default agent. A fresh start
    /// gets a new session. A session runs one run at a time, so a start on a
    /// named session with a live run is refused with that run's id; the
    /// agent may change from one run to the next. A start on a conversation
    /// takes one of its lanes, picked by `Shared::admit_lane`. Once
    /// [`drain`](Self::drain) has begun, every start is refused.
    pub async fn start(&self, start: StartRun) -> Result<RunId, StartError> {
        let launched = self.launch(start, |_, _| Ok(())).await?;
        Ok(launched.record.id)
    }

    /// Starts a run as [`start`](Self::start) does, attaches an observer as
    /// `how` says to the session the run is on, and returns the run's id,
    /// its session, and the observer's subscription.
    ///
    /// The observer is added in the same hold of `admission` that picks the
    /// session, so no other start or claim can get in between. A claim on a
    /// named session that already has a claimant is refused with that
    /// claimant's id. The observer's attach event comes before the run's
    /// `Started` on the session's stream.
    ///
    /// If the attach event cannot be written, or the caller stops waiting for
    /// it, the start fails and `Registered`'s `Drop` stops its run.
    pub async fn start_attached(
        &self,
        start: StartRun,
        how: Attach,
    ) -> Result<Admitted, StartError> {
        let observer = Observer {
            id: new_observer_id(),
            kind: how.kind,
            presence: how.presence,
        };
        self.shared.start_recorder();
        let Launched {
            record,
            session,
            run_session,
            joined: write,
        } = self
            .launch(start, |state, session| {
                state
                    .observers
                    .attach(observer.clone())
                    .map_err(|by| StartError::AlreadyClaimed { by })?;
                Ok(session.begin_observer_write())
            })
            .await?;
        let run = record.id;
        let registered = Registered {
            shared: Arc::clone(&self.shared),
            record: session,
            write,
            observer: observer.id.clone(),
            run: Some(AttachedRun::StartedWith(record)),
            recorded: false,
        };
        let subscription = registered.write_attach(observer, how.from).await?;
        Ok(Admitted {
            run,
            session: run_session,
            subscription,
        })
    }

    /// Admits `start` on the session its target resolves to, records the run
    /// as `Preparing`, and spawns its task, all under one hold of
    /// `admission`.
    ///
    /// `join` runs with the session's state locked, after the check for a
    /// live run and before the run is recorded, so anything it adds to the
    /// session is there before the run. If `join` returns an error, the start
    /// is refused with it.
    async fn launch<T>(
        &self,
        start: StartRun,
        join: impl FnOnce(&mut SessionState, &SessionRecord) -> Result<T, StartError>,
    ) -> Result<Launched<T>, StartError> {
        let config = self.shared.preparer.config(start.agent.as_deref())?;
        if !honours(&config, start.liveness) {
            return Err(StartError::PolicyUnsupported {
                policy: start.liveness.policy,
                agent: config.agent_id().to_owned(),
            });
        }

        // `admission` is held from here until the run's task is spawned, so
        // two starts on one session cannot both find it idle. Nothing below
        // awaits before the spawn. So a caller that drops this future cannot
        // leave the session holding a run that no task will finish.
        let (_admitted, session, run_session) = match &start.target {
            RunTarget::Fresh => {
                let id = new_session_id();
                let (admitted, session) = self.shared.admit(&id).await?;
                (admitted, session, RunSession::Plain(id))
            }
            // The id is admitted as it is, even when it is a lane's session
            // id such as `c1:a:0`. The run then runs on that lane's session,
            // but its facts record a plain session, with no lane.
            RunTarget::Session(id) => {
                let (admitted, session) = self.shared.admit(id).await?;
                (admitted, session, RunSession::Plain(id.clone()))
            }
            RunTarget::Conversation { chat } => {
                let conversation = Conversation::new(chat.clone(), config.agent_id());
                let (admitted, session, lane) = self.shared.admit_lane(&conversation).await?;
                (admitted, session, RunSession::Lane(lane))
            }
        };
        let run_id = RunId::mint();
        let (record, joined) = {
            let mut state = session.lock();
            if let Some(live) = &state.live {
                return Err(StartError::RunInProgress(live.id));
            }
            let joined = join(&mut state, &session)?;
            // Read before the run's task is spawned, so every event the run
            // appends has a later number.
            let preceding_event = session.journal.latest();
            let record = Arc::new(RunRecord::new(
                RunFacts {
                    session: run_session.clone(),
                    run: RunDetails {
                        run_id,
                        agent: config.agent_id().to_owned(),
                        status: RunStatus::Preparing,
                        liveness: start.liveness,
                        started_at: aura_events::Timestamp::now(),
                        ended_at: None,
                        usage: aura_events::TokenUsage::default(),
                        first_event: None,
                        latest_event: None,
                        pending_approvals: Vec::new(),
                        checkpoint: None,
                        continues: start.continues,
                    },
                },
                preceding_event,
            ));
            state.live = Some(Arc::clone(&record));
            self.shared.live_runs.send_modify(|live| *live += 1);
            (record, joined)
        };
        self.shared
            .runs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run_id, Arc::clone(&record));

        // The task prepares the agent under the caller's span.
        let task = tokio::spawn(task::run(
            Arc::clone(&self.shared),
            Arc::clone(&session),
            Arc::clone(&record),
            config,
            start,
            tracing::Span::current(),
        ));
        record.hold_task(task);
        Ok(Launched {
            record,
            session,
            run_session,
            joined,
        })
    }

    /// Stops the run and returns whether it was stopped. Returns `false` for
    /// a run that has ended or that this process does not hold. The run's
    /// task writes the `Cancelled` ending, with `cause` and `reason`.
    pub fn cancel(&self, run: &RunId, cause: CancelCause, reason: Option<String>) -> bool {
        self.shared
            .run(run)
            .is_some_and(|record| record.stop(cause, reason))
    }

    /// Follows the activity of `run`, or `None` for a run this process does
    /// not hold.
    pub fn activity(&self, run: &RunId) -> Option<RunActivity> {
        self.shared.run(run).map(|record| record.activity())
    }

    /// Shuts the runtime down, in four steps:
    ///
    /// 1. Refuses every later start with [`StartError::ShuttingDown`].
    /// 2. Stops every live run for [`CancelCause::Shutdown`], and waits
    ///    up to `grace` for them to end.
    /// 3. Abandons any run still going: aborts its task and writes its
    ///    `Cancelled` ending for it. Writing those endings, and waiting for
    ///    runs that were already writing their own, gets one more `grace` in
    ///    total.
    /// 4. Lets go of every run and session, and the sessions' journals with
    ///    them. Nothing starts after a drain, so nothing needs them. It also
    ///    stops the recorder and cancels `shut_down`, so no attach or detach
    ///    still being written keeps a journal alive past the drain.
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
                .filter(|record| !record.is_settled())
                .cloned()
                .collect()
        };
        for record in &live {
            record.stop(CancelCause::Shutdown, None);
        }
        if tokio::time::timeout(grace, all_settled(&live))
            .await
            .is_err()
        {
            tracing::warn!(
                ?grace,
                "runs were still going when the shutdown grace period ran out; abandoning them"
            );
        }

        let mut taken = Vec::new();
        let mut abandoned = Vec::new();
        for record in live.iter().filter(|record| !record.is_settled()) {
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
                let session_id = record.facts().session_id();
                let session = self.shared.lock_sessions().get(&session_id).cloned();
                if let Some(session) = session {
                    task::end_abandoned(&self.shared, &session, &record).await;
                }
            }));
        let endings = futures::future::join(written, all_settled(&taken));
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

    /// Resolves once no run is live on this runtime, meaning no session has
    /// a run preparing or streaming, or once a drain has let every run go. A
    /// run stops being live when its ending frees its session. That happens
    /// before the run store has the run's final facts.
    pub async fn idle(&self) {
        let mut live = self.shared.live_runs.subscribe();
        tokio::select! {
            _ = live.wait_for(|live| *live == 0) => {}
            () = self.shared.shut_down.cancelled() => {}
        }
    }

    /// Attaches an observer to `session` and returns its `Subscription`: the
    /// session's event stream, read from the cursor the observer asked for.
    /// A session this process does not hold is opened first.
    ///
    /// The subscription joins the stream before the attach is appended to
    /// it, so a subscriber starting live finds its own `ObserverAttached`
    /// among its first events. Attaches and detaches counted before it, and
    /// events the live run appends while it waits its turn, arrive ahead of
    /// it.
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
        let registered = self.shared.register(session, &observer).await?;
        Ok(registered.write_attach(observer, how.from).await?)
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
    /// - a live run that was stopped: `Stopping`
    /// - a live run whose agent is being prepared: `Preparing`
    /// - a live run waiting on approvals: `Blocked`
    /// - any other live run: `Running`
    /// - no live run, and the last run parked: `Parked`
    /// - otherwise: `Done`
    pub fn session_state(&self, session: &SessionId) -> Option<AgentState> {
        let session = self.shared.lock_sessions().get(session).cloned()?;
        let state = session.lock();
        if let Some(live) = &state.live {
            let RunDetails {
                status,
                pending_approvals,
                ..
            } = live.facts().run;
            return Some(match status {
                _ if live.stopped().is_some() => AgentState::Stopping,
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

    /// One page of the sessions `query` selects, in session-id order: those
    /// the run store has a run of, and those the runtime holds a run of. A
    /// session is listed for as long as either keeps one of its runs, so a
    /// session the runtime has let go is still listed, and so is one whose
    /// rows the store let go while the runtime holds its run. A session that
    /// never had a run is not.
    ///
    /// The query's agent, chat and state are read off each session's newest
    /// run. For a session the runtime holds, that is the runtime's newest
    /// run, as [`session`](Self::session) reads it, and the store supplies
    /// the runs before it. When the store's rows for the page have no row
    /// for such a session, its rows are read from the store on their own,
    /// once the page is cut, for the sessions left on it.
    ///
    /// The run store has no status for a run that is stopping. Only the
    /// runtime knows that a run the runtime holds is stopping. A query for
    /// `Stopping` is answered from the sessions the runtime holds, not from
    /// the store. Those sessions are paged in session-id order with `after`,
    /// `limit` and `next` as any other page is. A query for any other state
    /// leaves out a session whose newest run is stopping, even when the
    /// store's row for that session matches.
    ///
    /// A page holds no session past the last one the store returned, when it
    /// returned more than the page holds, since the store has not been read
    /// past it. The page's `next` is the id of the last session on the page
    /// when more matched, and otherwise that of the last session the store
    /// returned, even when that session was left off the page. A session
    /// whose newest run changes while the page is read, to one the query
    /// does not select, is left off it the same way, so a page can come back
    /// short, and `next` still moves past every session it passed over.
    pub async fn sessions(&self, query: &SessionQuery) -> Result<SessionPage, SessionStoreError> {
        let (statuses, pending_approval) = match query.state {
            Some(AgentStateKind::Stopping) => return self.stopping_sessions(query).await,
            None => (None, None),
            Some(AgentStateKind::Preparing) => {
                (Some(RunStatuses::just(RunStatus::Preparing)), None)
            }
            Some(AgentStateKind::Running) => {
                (Some(RunStatuses::just(RunStatus::Running)), Some(false))
            }
            Some(AgentStateKind::Blocked) => {
                (Some(RunStatuses::just(RunStatus::Running)), Some(true))
            }
            Some(AgentStateKind::Parked) => (Some(RunStatuses::just(RunStatus::Parked)), None),
            Some(AgentStateKind::Done) => (
                Some(
                    RunStatuses::just(RunStatus::Finished)
                        .and(RunStatus::Failed)
                        .and(RunStatus::Cancelled),
                ),
                None,
            ),
        };
        let filter = SessionFilter {
            session: None,
            chat: query.chat.clone(),
            agent: query.agent.clone(),
            statuses,
            pending_approval,
            limit: query.limit.get() + 1,
            after: query.after.clone(),
        };
        let stored = self.shared.run_store.newest_per_session(&filter).await?;
        let bounds = PageBounds::new(filter.after.clone(), &stored, query.limit, |row| {
            row.newest().session_id()
        });
        let mut stored: BTreeMap<SessionId, SessionRuns> = stored
            .into_iter()
            .map(|runs| (runs.newest().session_id(), runs))
            .collect();
        let mut page: BTreeMap<SessionId, PageSession> = BTreeMap::new();
        let open_sessions: Vec<Arc<SessionRecord>> =
            self.shared.lock_sessions().values().cloned().collect();
        for open_session in open_sessions {
            let id = &open_session.id;
            if !bounds.contains(id) {
                continue;
            }
            let Some(newest_run) = self.newest_run(&open_session) else {
                continue;
            };
            // With no row on the store's page, the session's newest run did
            // not match there, or the store has no run of it.
            let candidate = match stored.remove(id) {
                Some(runs) => {
                    PageSession::Runs(session_runs_with_newest_run(Some(runs), newest_run))
                }
                None => PageSession::NewestRun(newest_run),
            };
            if filter.matches_newest_run(candidate.newest_run()) {
                page.insert(id.clone(), candidate);
            }
        }
        page.extend(
            stored
                .into_iter()
                .map(|(id, runs)| (id, PageSession::Runs(runs))),
        );
        let mut page: Vec<PageSession> = page.into_values().collect();
        let next = bounds.cut(&mut page, query.limit, |candidate| {
            candidate.newest_run().session_id()
        });
        let mut sessions = Vec::with_capacity(page.len());
        for candidate in page {
            let row = match candidate {
                PageSession::Runs(runs) => runs,
                PageSession::NewestRun(newest_run) => {
                    let stored = self
                        .session_runs_from_store(&newest_run.session_id())
                        .await?;
                    let runs = session_runs_with_newest_run(stored, newest_run);
                    // A newer run of the session can reach the store after
                    // the page's first read, and it may not match.
                    if !filter.matches(&runs) {
                        continue;
                    }
                    runs
                }
            };
            let snapshot = self.session_snapshot(row).await?;
            if query
                .state
                .is_none_or(|kind| snapshot.state().kind() == kind)
            {
                sessions.push(snapshot);
            }
        }
        Ok(SessionPage::new(sessions, next))
    }

    /// The page of a `Stopping` query: the sessions this process holds whose
    /// live run was stopped and matches the query's agent and chat.
    async fn stopping_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<SessionPage, SessionStoreError> {
        let open_sessions: Vec<Arc<SessionRecord>> =
            self.shared.lock_sessions().values().cloned().collect();
        let mut ids: Vec<SessionId> = open_sessions
            .iter()
            .filter(|session| query.after.as_ref().is_none_or(|after| session.id > *after))
            .filter(|session| {
                let state = session.lock();
                state.live.as_ref().is_some_and(|live| {
                    let facts = live.facts();
                    live.stopped().is_some()
                        && query
                            .agent
                            .as_ref()
                            .is_none_or(|agent| *agent == facts.run.agent)
                        && query.chat.as_ref().is_none_or(|chat| {
                            facts
                                .lane()
                                .is_some_and(|lane| lane.conversation().chat() == chat)
                        })
                })
            })
            .map(|session| session.id.clone())
            .collect();
        ids.sort();
        let next = cut_to_page(&mut ids, query.limit, SessionId::clone);
        let mut sessions = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(snapshot) = self.session(id).await?
                && snapshot.state().kind() == AgentStateKind::Stopping
            {
                sessions.push(snapshot);
            }
        }
        Ok(SessionPage::new(sessions, next))
    }

    /// The session `id`, or `None` for a session this process does not hold
    /// and the run store has no run of.
    ///
    /// For a session the runtime holds, its newest run is the runtime's:
    /// the live run, or else the last run that ended while the runtime
    /// still keeps it. The run store's rows supply the runs before it, and
    /// past its bound the store can have let some or all of them go. For a
    /// session the runtime does not hold, everything comes from the store's
    /// rows. The runtime adds what only it knows about a session it holds:
    /// the newest run's current facts and whether it was stopped, the last
    /// run that ended, the observer counts, and the journal's newest number.
    /// For a session it does not hold, the newest number is read from the
    /// journal store.
    pub async fn session(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionSnapshot>, SessionStoreError> {
        let stored = self.session_runs_from_store(id).await?;
        let open_session = self.shared.lock_sessions().get(id).cloned();
        let newest_run = open_session.and_then(|session| self.newest_run(&session));
        let row = match (stored, newest_run) {
            (stored, Some(newest_run)) => session_runs_with_newest_run(stored, newest_run),
            (Some(row), None) => row,
            (None, None) => return Ok(None),
        };
        Ok(Some(self.session_snapshot(row).await?))
    }

    /// The runs of the session `id` as the run store describes them, or
    /// `None` when it has none.
    async fn session_runs_from_store(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionRuns>, SessionStoreError> {
        let filter = SessionFilter {
            session: Some(id.clone()),
            limit: 1,
            ..SessionFilter::default()
        };
        Ok(self
            .shared
            .run_store
            .newest_per_session(&filter)
            .await?
            .pop())
    }

    /// The facts of the newest run the runtime holds of `session`: its live
    /// run, or else the last run that ended, while the runtime still keeps
    /// it.
    fn newest_run(&self, session: &SessionRecord) -> Option<RunFacts> {
        let last_ended = {
            let state = session.lock();
            if let Some(live) = &state.live {
                return Some(live.facts());
            }
            state.last_run.map(|(run, _)| run)
        };
        self.shared.run(&last_ended?).map(|record| record.facts())
    }

    /// The newest number on the session's stream, or `None` for a session
    /// neither this process nor the journal store knows. It comes from the
    /// journal the runtime holds, and from the journal store for a session
    /// it does not hold. The run store plays no part.
    pub async fn latest_event(
        &self,
        id: &SessionId,
    ) -> Result<Option<SequenceNumber>, SessionStoreError> {
        let open_session = self.shared.lock_sessions().get(id).cloned();
        match open_session {
            Some(session) => Ok(session.journal.latest()),
            None => self.shared.journal_store.latest(id).await,
        }
    }

    /// The entry numbered `seq` on the session's stream, or `None` when the
    /// journal store holds none: the stream has not reached `seq`, the store
    /// let the entry go, or it forgot the session. It is read from the
    /// journal store, which keeps a session's entries whether this process
    /// holds the session or not. The run store plays no part, so a session
    /// whose runs the run store has dropped still has its entries.
    pub async fn journal_entry(
        &self,
        id: &SessionId,
        seq: SequenceNumber,
    ) -> Result<Option<JournalEntry>, SessionStoreError> {
        let entries = self.shared.journal_store.read(id, seq, seq, 1).await?;
        Ok(entries.into_iter().find(|entry| entry.seq() == seq))
    }

    /// The snapshot of the session `row` describes, with what the runtime
    /// holds for the session in place of the store's view where it has it.
    async fn session_snapshot(
        &self,
        row: SessionRuns,
    ) -> Result<SessionSnapshot, SessionStoreError> {
        let (newest, stored_previous) = row.into_parts();
        let record = self.shared.run(&newest.run.run_id);
        let stopped = record
            .as_ref()
            .is_some_and(|record| record.stopped().is_some());
        let newest = record.map_or(newest, |record| record.facts());
        let id = newest.session_id();
        let open_session = self.shared.lock_sessions().get(&id).cloned();
        let (latest_event, observers, last_ended_run) = match open_session {
            Some(session) => {
                let state = session.lock();
                (
                    session.journal.latest(),
                    state.observers.counts(),
                    state.last_run.map(|(run, _)| run),
                )
            }
            None => (
                self.shared.journal_store.latest(&id).await?,
                ObserverCounts::default(),
                None,
            ),
        };
        // The session's own last run, unless that is the newest run itself.
        let previous_ended = last_ended_run
            .filter(|last| *last != newest.run.run_id)
            .or(stored_previous);
        Ok(SessionSnapshot::of(
            newest,
            stopped,
            previous_ended,
            latest_event,
            observers,
        ))
    }

    /// One page of the runs `query` selects, in run-id order: those the run
    /// store lists, and those the runtime holds. A run the runtime holds is
    /// matched and shown by its record's facts in place of the store's row,
    /// so a live run's usage and newest number are current, and the page has
    /// the run whatever the store kept of it. The page ends, and its `next`
    /// is worked out, as on [`sessions`](Self::sessions).
    pub async fn runs(&self, query: &RunQuery) -> Result<RunPage, SessionStoreError> {
        let filter = RunFilter {
            session: query.session.clone(),
            conversation: None,
            chat: query.chat.clone(),
            status: query.status,
            pending_approval: query.pending_approval,
            limit: query.limit.get() + 1,
            after: query.after,
        };
        let stored = self.shared.run_store.list(&filter).await?;
        let bounds = PageBounds::new(filter.after, &stored, query.limit, |facts| facts.run.run_id);
        let mut runs: BTreeMap<RunId, RunSnapshot> = stored
            .into_iter()
            .map(|facts| (facts.run.run_id, self.snapshot_from_store(facts)))
            .collect();
        for record in self.shared.run_records() {
            if !bounds.contains(&record.id) {
                continue;
            }
            let snapshot = self.snapshot_from_record(&record);
            if filter.matches(snapshot.facts()) {
                runs.insert(record.id, snapshot);
            } else {
                runs.remove(&record.id);
            }
        }
        let mut runs: Vec<RunSnapshot> = runs.into_values().collect();
        let next = bounds.cut(&mut runs, query.limit, |run| run.facts().run.run_id);
        Ok(RunPage::new(runs, next))
    }

    /// The run, from its record while the runtime holds it and from the run
    /// store after that. `None` for a run neither has. A read can wait, as
    /// `ending_written` says.
    pub async fn run(&self, run: &RunId) -> Result<Option<RunSnapshot>, SessionStoreError> {
        if let Some(record) = self.shared.run(run) {
            self.ending_written(&record).await;
            return Ok(Some(self.snapshot_from_record(&record)));
        }
        let facts = self.shared.run_store.get(run).await?;
        Ok(facts.map(|facts| self.snapshot_from_store(facts)))
    }

    /// Whether the run's facts show its ending, once any ending already
    /// being written is written. A run's ending goes on the session's stream
    /// before its facts, so a reader can see the run's terminal event while
    /// the facts still say it is live. Once the run's stream has ended, this
    /// waits for the facts, or for a drain, which can let the run go without
    /// an ending; a run whose stream is still going is not waited for.
    async fn ending_written(&self, record: &RunRecord) -> bool {
        if !record.stoppable() {
            tokio::select! {
                biased;
                () = record.ended() => {}
                () = self.shared.shut_down.cancelled() => {}
            }
        }
        record.facts().run.status.is_terminal()
    }

    /// The snapshot of a run the runtime holds, with its session's observers.
    fn snapshot_from_record(&self, record: &RunRecord) -> RunSnapshot {
        let observers = self
            .observer_counts(&record.facts().session_id())
            .unwrap_or_default();
        RunSnapshot::from_record(record, observers)
    }

    /// The snapshot of a run read from the run store, with its session's
    /// observers.
    fn snapshot_from_store(&self, facts: RunFacts) -> RunSnapshot {
        let observers = self
            .observer_counts(&facts.session_id())
            .unwrap_or_default();
        RunSnapshot::from_store(facts, observers)
    }

    /// Stops the run as [`cancel`](Self::cancel) does, and says what came of
    /// it:
    ///
    /// - [`StopOutcome::Stopping`] when the run was stopped by this call or
    ///   an earlier one and has not ended. The first cause given is the one
    ///   the run ends with. The snapshot is taken before the stop, so it
    ///   never shows the run as ended.
    /// - [`StopOutcome::AlreadyEnded`] when the run's stream had already
    ///   ended, or the run store holds the run as ended. The run keeps the
    ///   ending it has. A run whose ending is still being written is waited
    ///   for, so the snapshot shows how it ended.
    /// - [`StopOutcome::NotHere`] when the runtime does not hold the run, or
    ///   a drain lets the run go before its ending is written, and the run
    ///   store has no ended run with this id.
    pub async fn stop(
        &self,
        run: &RunId,
        cause: CancelCause,
        reason: Option<String>,
    ) -> Result<StopOutcome, SessionStoreError> {
        if let Some(record) = self.shared.run(run) {
            let before = self.snapshot_from_record(&record);
            if record.stop(cause, reason) {
                return Ok(StopOutcome::Stopping(before));
            }
            // A refused stop means the stream has ended. A drain can let the
            // run go without its ending, and then the run is read from the
            // store.
            if self.ending_written(&record).await {
                return Ok(StopOutcome::AlreadyEnded(
                    self.snapshot_from_record(&record),
                ));
            }
        }
        Ok(match self.shared.run_store.get(run).await? {
            Some(facts) if facts.run.status.is_terminal() => {
                StopOutcome::AlreadyEnded(self.snapshot_from_store(facts))
            }
            Some(_) | None => StopOutcome::NotHere,
        })
    }
}

/// Cuts `rows` down to `limit`, and returns the key the next page starts
/// after: that of the last row kept, when there were more rows than that.
fn cut_to_page<T, K>(rows: &mut Vec<T>, limit: PageLimit, key: impl FnOnce(&T) -> K) -> Option<K> {
    if rows.len() <= limit.get() {
        return None;
    }
    rows.truncate(limit.get());
    rows.last().map(key)
}

/// The keys a page read from the store, with what the runtime holds added,
/// can hold. They come after `after`, the last key of the previous page.
/// When the store returned more rows than the page holds, they go no
/// further than `stored_end`, the key of the last row it returned: the
/// store was not read past it, so a row the runtime adds past it could come
/// after rows the store has not returned yet.
struct PageBounds<K> {
    after: Option<K>,
    stored_end: Option<K>,
}

impl<K: Ord> PageBounds<K> {
    fn new<T>(after: Option<K>, stored: &[T], limit: PageLimit, key: impl FnOnce(&T) -> K) -> Self {
        let stored_end = if stored.len() > limit.get() {
            stored.last().map(key)
        } else {
            None
        };
        Self { after, stored_end }
    }

    fn contains(&self, key: &K) -> bool {
        self.after.as_ref().is_none_or(|after| key > after)
            && self.stored_end.as_ref().is_none_or(|end| key <= end)
    }

    /// Cuts `rows`, in key order, down to `limit`, and returns the page's
    /// `next`: the key of the last row kept when there were more, or else
    /// `stored_end`, since the store may have more rows past it.
    fn cut<T>(self, rows: &mut Vec<T>, limit: PageLimit, key: impl FnOnce(&T) -> K) -> Option<K> {
        cut_to_page(rows, limit, key).or(self.stored_end)
    }
}

/// A session on a page of [`AgentRuntime::sessions`] before the page is cut.
enum PageSession {
    /// The session's runs.
    Runs(SessionRuns),
    /// The newest run of a session the runtime holds, without the store's
    /// rows for the session.
    NewestRun(RunFacts),
}

impl PageSession {
    fn newest_run(&self) -> &RunFacts {
        match self {
            Self::Runs(runs) => runs.newest(),
            Self::NewestRun(newest_run) => newest_run,
        }
    }
}

/// The runs of a session the runtime holds: `stored`, the run store's view
/// of them if it has one, with `newest_run`, the runtime's newest run of the
/// session, as the newest. `push` takes only a later run, so the store's
/// newest run stands when it is the later one: when the store's rows were
/// read after `newest_run`, or another process writes to the same store.
fn session_runs_with_newest_run(stored: Option<SessionRuns>, newest_run: RunFacts) -> SessionRuns {
    let Some(mut runs) = stored else {
        return SessionRuns::new(newest_run);
    };
    if newest_run.run.run_id >= runs.newest().run.run_id {
        runs.push(newest_run);
    }
    runs
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

    /// The records of every run the runtime holds.
    fn run_records(&self) -> Vec<Arc<RunRecord>> {
        self.runs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
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

    /// Takes `admission`, then lets the session `id` go if the record this
    /// process holds for it is idle. The record is looked up here rather
    /// than passed in, because the session may have been reopened since the
    /// caller's write began, and then the replacement is what to let go.
    fn let_go_if_idle(&self, id: &SessionId) {
        let _admitted = self.lock_admission();
        // Bound first, so the sessions lock is released before
        // `let_go_if_idle_admitted` takes it again.
        let current = self.lock_sessions().get(id).cloned();
        if let Some(current) = current {
            self.let_go_if_idle_admitted(&current);
        }
    }

    /// Removes the session if it is idle (see `SessionRecord::is_idle`). Its
    /// journal goes with it, which ends its subscriptions. The store keeps
    /// the session's events and newest sequence number, so a later start
    /// continues the sequence. Only this exact record is removed: if the
    /// session was reopened since, the newer record stays. The caller holds
    /// `admission`.
    fn let_go_if_idle_admitted(&self, session: &Arc<SessionRecord>) {
        if !session.is_idle() {
            return;
        }
        let mut sessions = self.lock_sessions();
        if sessions
            .get(&session.id)
            .is_some_and(|open_session| Arc::ptr_eq(open_session, session))
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
    /// A reopened record takes over the fenced one's state (see
    /// `SessionRecord::reopening`), and cancelling the fenced record's
    /// `superseded` moves its subscriptions to the new journal. The records
    /// are swapped under the sessions lock, so a failed open changes nothing
    /// and a concurrent detach lands on exactly one record.
    async fn admit(
        &self,
        id: &SessionId,
    ) -> Result<(MutexGuard<'_, ()>, Arc<SessionRecord>), Refused> {
        let admitted = self.lock_admission();
        if self.draining.load(Ordering::Acquire) {
            return Err(Refused::ShuttingDown);
        }
        if let Some(session) = self.usable_session(id) {
            return Ok((admitted, session));
        }
        drop(admitted);

        let journal = SessionJournal::open(id.clone(), Arc::clone(&self.journal_store)).await?;

        let admitted = self.lock_admission();
        if self.draining.load(Ordering::Acquire) {
            return Err(Refused::ShuttingDown);
        }
        if let Some(session) = self.usable_session(id) {
            return Ok((admitted, session));
        }
        let mut sessions = self.lock_sessions();
        let fenced = sessions.get(id).cloned();
        let session = Arc::new(match &fenced {
            Some(fenced) => {
                tracing::info!(session_id = %id, "another writer fenced the session's journal; reopening it");
                SessionRecord::reopening(fenced, journal)
            }
            None => SessionRecord::new(id.clone(), journal),
        });
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
        let session = self.lock_sessions().get(id).cloned()?;
        let usable = !session.journal.is_fenced() || session.lock().live.is_some();
        usable.then_some(session)
    }
}

/// Why admitting a session, or writing an observer's attach, failed.
enum Refused {
    ShuttingDown,
    Store(SessionStoreError),
}

impl From<SessionStoreError> for Refused {
    fn from(error: SessionStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<Refused> for StartError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::ShuttingDown => Self::ShuttingDown,
            Refused::Store(error) => Self::Store(error),
        }
    }
}

impl From<Refused> for AttachError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::ShuttingDown => Self::ShuttingDown,
            Refused::Store(error) => Self::Store(error),
        }
    }
}

/// A run [`AgentRuntime::launch`] admitted, with its task spawned.
struct Launched<T> {
    record: Arc<RunRecord>,
    session: Arc<SessionRecord>,
    run_session: RunSession,
    /// What the start's `join` returned.
    joined: T,
}

/// An observer whose attach is not yet on the session's stream.
struct Registered {
    shared: Arc<Shared>,
    record: Arc<SessionRecord>,
    write: ObserverWrite,
    observer: ObserverId,
    /// The run that goes with the observer's attach.
    run: Option<AttachedRun>,
    /// Whether the attach reached the session's stream.
    recorded: bool,
}

/// The run that goes with an observer's attach.
enum AttachedRun {
    /// The session's live run.
    Live(RunId),
    /// The run started together with the observer.
    StartedWith(Arc<RunRecord>),
}

impl Registered {
    /// Subscribes to the session's stream from `from`, appends the
    /// observer's attach event when its turn comes, and returns the
    /// subscription.
    async fn write_attach(
        mut self,
        observer: Observer,
        from: Cursor,
    ) -> Result<Subscription, Refused> {
        let record = Arc::clone(&self.record);
        // A live cursor is pinned to the journal's newest number, so the
        // subscription can pick up from the same place on a reopened journal.
        let from = match from {
            Cursor::Live => record.journal.latest().map_or(Cursor::Start, Cursor::After),
            from => from,
        };
        let events = record.journal.subscribe(from);
        let attached = LifecycleEvent::ObserverAttached {
            observer: observer.clone(),
        };
        // A run started together with the observer did not exist when the
        // observer was added, so the attach has no run id, the same as an
        // attach made before `start`.
        let run_id = match &self.run {
            Some(AttachedRun::Live(id)) => Some(*id),
            Some(AttachedRun::StartedWith(_)) | None => None,
        };
        // Behind every observer write counted before it, so a claim given up
        // just before shows on the stream as given up first.
        let write = async {
            self.write.turn().await;
            record
                .journal
                .append(run_id, SessionEventPayload::Lifecycle(attached))
                .await
        };
        // An attach still writing when a drain has let everything go gives
        // up, so it does not keep the session's journal alive past shutdown.
        tokio::select! {
            appended = write => appended.map_err(Refused::Store)?,
            () = self.shared.shut_down.cancelled() => return Err(Refused::ShuttingDown),
        };
        self.recorded = true;
        let shared = Arc::downgrade(&self.shared);
        drop(self);
        Ok(Subscription::new(observer, events, from, &record, shared))
    }
}

impl Shared {
    /// Registers `observer` on the session `id`, opening the session when
    /// this process does not hold it, and returns the guard that takes the
    /// observer off again unless its attach reaches the stream. Holds
    /// `admission` only while it does this, so an attach and a start cannot
    /// both open the session, and a stalled append afterwards holds up only
    /// this session. Until the guard is dropped, the session is not let go.
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
        let write = record.begin_observer_write();
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
            write,
            observer: observer.id.clone(),
            run: run_id.map(AttachedRun::Live),
            recorded: false,
        })
    }
}

impl Drop for Registered {
    /// Finishes the attach's observer write. If the attach event never
    /// reached the stream, it also takes the observer off the session without
    /// writing a detach, stops the run started together with it, and lets the
    /// session go if it then holds nothing.
    fn drop(&mut self) {
        if !self.recorded {
            drop(self.shared.unregister(&self.record.id, &self.observer));
            // The run is stopped as `External` whatever its liveness policy,
            // since nobody would read it and the caller never got its id. It
            // is stopped before the write is finished, because the run's task
            // checks for a stop once that write is done.
            if let Some(AttachedRun::StartedWith(run)) = &self.run {
                run.stop(
                    CancelCause::External,
                    Some("the start failed before its observer was attached".to_owned()),
                );
            }
        }
        // Finished before the hand-off, so the let-go finds no write of this
        // attach still pending.
        self.write.finish();
        if !self.recorded {
            self.shared
                .hand_off(recorder::Chore::LetGo(self.record.id.clone()));
        }
    }
}

/// Resolves once the ending of every run in `records` is settled.
async fn all_settled(records: &[Arc<RunRecord>]) {
    futures::future::join_all(records.iter().map(|record| record.settled())).await;
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
