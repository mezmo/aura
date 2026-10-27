//! The runtime's recorder: one task that does the async work a `Drop` cannot
//! do itself. A dropped subscription hands it the observer's detach to write,
//! and a dropped attach hands it the session to let go. Each chore runs on
//! its own, so a write that stalls on one session holds up no other.

use std::sync::{Arc, Weak};

use aura_events::run::DetachCause;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

use super::Shared;
use super::observe::Detached;
use super::record::SessionRecord;

/// Work handed to the recorder.
pub(super) enum Chore {
    /// An observer's detach, to write to its session's stream.
    Detach {
        detached: Detached,
        recorded: oneshot::Sender<()>,
    },
    /// A session to let go if it holds nothing.
    LetGo(Arc<SessionRecord>),
}

impl Chore {
    async fn run(self, shared: Arc<Shared>) {
        match self {
            Self::Detach { detached, recorded } => {
                shared.record_detach(detached, DetachCause::Released).await;
                let _ = recorded.send(());
            }
            Self::LetGo(session) => shared.let_go_if_idle(&session),
        }
    }
}

/// The recorder's queue and its task.
pub(super) struct Recorder {
    chores: mpsc::UnboundedSender<Chore>,
    task: AbortHandle,
}

impl Recorder {
    /// Starts the recorder on the current tokio runtime. It holds `shared`
    /// weakly, since `shared` holds the recorder, and ends once `shared` is
    /// gone.
    fn start(shared: Weak<Shared>) -> Self {
        let (chores, mut queue) = mpsc::unbounded_channel::<Chore>();
        let task = tokio::spawn(async move {
            let mut running = JoinSet::new();
            loop {
                tokio::select! {
                    chore = queue.recv() => {
                        let Some(chore) = chore else { break };
                        let Some(shared) = shared.upgrade() else { break };
                        running.spawn(chore.run(shared));
                    }
                    Some(_) = running.join_next(), if !running.is_empty() => {}
                }
            }
            while running.join_next().await.is_some() {}
        });
        Self {
            chores,
            task: task.abort_handle(),
        }
    }
}

impl Shared {
    /// Starts the recorder if it is not running yet. Must run on a tokio
    /// runtime.
    pub(super) fn start_recorder(self: &Arc<Self>) {
        self.recorder
            .get_or_init(|| Recorder::start(Arc::downgrade(self)));
    }

    /// Hands `chore` to the recorder. A chore the recorder will not run is
    /// dropped here instead, which releases what it holds; that happens
    /// only once a drain has stopped the recorder, or the runtime is gone.
    pub(super) fn hand_off(&self, chore: Chore) {
        if let Some(recorder) = self.recorder.get() {
            let _ = recorder.chores.send(chore);
        }
    }

    /// Stops the recorder and every chore it is running.
    pub(super) fn stop_recorder(&self) {
        if let Some(recorder) = self.recorder.get() {
            recorder.task.abort();
        }
    }
}
