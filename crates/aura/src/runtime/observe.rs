//! The observers attached to a session, what each one declared, and the
//! subscription each holds while it is attached.
//!
//! An observer attaches to a session, not to a run. It declares whether it
//! collects or claims, and whether it has presence. Because the attach is
//! on the session, a client that claims a session and then starts a run is
//! the claimant from the run's first event. A session has at most one
//! claimant and any number of collecting observers.
//!
//! An observer leaves when its subscription is dropped or detached, recorded
//! as [`DetachCause::Released`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use aura_events::SessionId;
use aura_events::run::{
    DetachCause, LifecycleEvent, LivenessPolicy, Observer, ObserverId, ObserverKind,
    RunCancelReason, SessionEventPayload,
};
use futures::Stream;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio_util::sync::WaitForCancellationFutureOwned;

use crate::journal::{Cursor, JournalItem, JournalSubscription};
use crate::run_context::Attendance;
use crate::session_store::SessionStoreError;

use super::Shared;
use super::record::{RunRecord, SessionRecord};
use super::recorder::Chore;

/// How an observer attaches to a session.
#[derive(Debug, Clone, Copy)]
pub struct Attach {
    pub(super) kind: ObserverKind,
    /// Whether something that can answer a prompt is behind this observer.
    pub(super) presence: bool,
    /// Where in the session's stream the subscription starts.
    pub(super) from: Cursor,
}

impl Attach {
    /// A collecting observer, without presence, that reads only new events.
    pub fn collecting() -> Self {
        Self {
            kind: ObserverKind::Collecting,
            presence: false,
            from: Cursor::Live,
        }
    }

    /// A claiming observer, without presence, that reads only new events.
    pub fn claiming() -> Self {
        Self {
            kind: ObserverKind::Claiming,
            presence: false,
            from: Cursor::Live,
        }
    }

    #[must_use]
    pub fn present(mut self) -> Self {
        self.presence = true;
        self
    }

    #[must_use]
    pub fn from(mut self, cursor: Cursor) -> Self {
        self.from = cursor;
        self
    }
}

/// Why the runtime refused to attach an observer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AttachError {
    /// The session already has a claimant.
    #[error("observer `{by}` holds the session's claim; a session has one claimant at a time")]
    AlreadyClaimed {
        /// The current claimant.
        by: ObserverId,
    },
    #[error("the runtime is shutting down and attaches no new observers")]
    ShuttingDown,
    #[error(transparent)]
    Store(#[from] SessionStoreError),
}

/// How many observers a session has, by what each declared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ObserverCounts {
    pub collecting: usize,
    pub claiming: usize,
    /// Observers of either kind with presence.
    pub present: usize,
}

/// The observers attached to one session.
#[derive(Default)]
pub(super) struct ObserverSet {
    collecting: HashMap<ObserverId, Observer>,
    claimant: Option<Observer>,
}

impl ObserverSet {
    /// Adds `observer`. A claiming observer is refused with the current
    /// claimant's id if there is one. A collecting observer is always added.
    pub(super) fn attach(&mut self, observer: Observer) -> Result<(), ObserverId> {
        if observer.kind == ObserverKind::Claiming {
            if let Some(holder) = &self.claimant {
                return Err(holder.id.clone());
            }
            self.claimant = Some(observer);
        } else {
            self.collecting.insert(observer.id.clone(), observer);
        }
        Ok(())
    }

    /// Removes observer `id` and returns it, with whether it held the claim.
    pub(super) fn detach(&mut self, id: &ObserverId) -> Option<(Observer, bool)> {
        if self
            .claimant
            .as_ref()
            .is_some_and(|holder| holder.id == *id)
        {
            return self.claimant.take().map(|holder| (holder, true));
        }
        self.collecting.remove(id).map(|observer| (observer, false))
    }

    pub(super) fn attendance(&self) -> Attendance {
        Attendance {
            present: self.all().any(|observer| observer.presence),
            claimed: self.claimant.is_some(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.claimant.is_none() && self.collecting.is_empty()
    }

    pub(super) fn counts(&self) -> ObserverCounts {
        ObserverCounts {
            collecting: self.collecting.len(),
            claiming: usize::from(self.claimant.is_some()),
            present: self.all().filter(|observer| observer.presence).count(),
        }
    }

    fn all(&self) -> impl Iterator<Item = &Observer> {
        self.collecting.values().chain(self.claimant.iter())
    }
}

/// An observer's attachment to a session, and the session's events it reads.
#[must_use = "dropping the subscription detaches the observer"]
pub struct Subscription {
    observer: Observer,
    events: JournalSubscription,
    /// Where `events` would start again: after the last item it delivered.
    next: Cursor,
    /// The signal that the session record `events` reads from was replaced.
    superseded: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
    attachment: Option<Attachment>,
}

impl Subscription {
    /// `events` reads `record`'s journal from `from`.
    pub(super) fn new(
        observer: Observer,
        events: JournalSubscription,
        from: Cursor,
        record: &SessionRecord,
        shared: Weak<Shared>,
    ) -> Self {
        let attachment = Attachment {
            shared,
            session: record.id.clone(),
            observer: observer.id.clone(),
        };
        Self {
            observer,
            events,
            next: from,
            superseded: Some(Box::pin(record.superseded.clone().cancelled_owned())),
            attachment: Some(attachment),
        }
    }

    /// Moves the stream to the journal of the record that replaced the one
    /// it was reading, starting after the last item it delivered. The store
    /// holds everything in between, including what the other writer
    /// appended. When this process no longer holds the session there is
    /// nothing to move to, and the old stream ends with its record.
    fn follow_reopen(&mut self) {
        let current = self.attachment.as_ref().and_then(|attachment| {
            let shared = attachment.shared.upgrade()?;
            shared.lock_sessions().get(&attachment.session).cloned()
        });
        self.superseded = current.map(|record| {
            self.events = record.journal.subscribe(self.next);
            Box::pin(record.superseded.clone().cancelled_owned())
        });
    }

    pub fn observer(&self) -> &Observer {
        &self.observer
    }

    /// Detaches the observer and waits until the session's stream records
    /// it. Dropping the subscription also detaches, but records it in the
    /// background.
    pub async fn detach(mut self) {
        if let Some(attachment) = self.attachment.take() {
            attachment.detach().await;
        }
    }
}

impl Stream for Subscription {
    type Item = JournalItem;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        while let Some(superseded) = &mut this.superseded
            && superseded.as_mut().poll(cx).is_ready()
        {
            this.follow_reopen();
        }
        let item = std::task::ready!(this.events.as_mut().poll_next(cx));
        if let Some(item) = &item {
            this.next = match item {
                JournalItem::Event(event) => Cursor::After(event.seq),
                JournalItem::Gap { resumed_at } => Cursor::From(*resumed_at),
                JournalItem::Unknown { seq, .. } => Cursor::After(*seq),
            };
        }
        Poll::Ready(item)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(attachment) = self.attachment.take() {
            drop(attachment.release());
        }
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("observer", &self.observer)
            .finish_non_exhaustive()
    }
}

/// The half of a subscription that detaches its observer.
struct Attachment {
    shared: Weak<Shared>,
    session: SessionId,
    observer: ObserverId,
}

impl Attachment {
    /// Removes the observer from its session and acts on a lost claim, both
    /// now, then hands the detach to the recorder to write as `Released`.
    /// The returned receiver resolves once the detach is written, or once
    /// the recorder drops it. Stopping the run does not wait for the
    /// journal: the run's ending waits for the detach instead, so the stream
    /// still shows the claim going first.
    fn release(self) -> Option<oneshot::Receiver<()>> {
        let shared = self.shared.upgrade()?;
        let detached = shared.unregister(&self.session, &self.observer)?;
        detached.react_unclaimed();
        let (recorded, written) = oneshot::channel();
        shared.hand_off(Chore::Detach { detached, recorded });
        Some(written)
    }

    /// Like [`release`](Self::release), but waits for the detach to be
    /// written. A caller that stops waiting leaves the write to finish.
    async fn detach(self) {
        if let Some(written) = self.release() {
            let _ = written.await;
        }
    }
}

/// An observer removed from its session.
pub(super) struct Detached {
    session: Arc<SessionRecord>,
    observer: Observer,
    /// The session's live run.
    live: Option<Arc<RunRecord>>,
    /// Whether the observer held the session's claim.
    claimant: bool,
}

impl Drop for Detached {
    /// Ends the detach's count in the session's `observer_writes`, written
    /// or not.
    fn drop(&mut self) {
        self.session.finish_observer_write();
    }
}

impl Detached {
    /// Stops the live run as `Unclaimed` if this observer held the claim and
    /// the run's liveness policy is Cancel. Under other policies the run
    /// goes on. A run nobody claimed, or one a collecting observer left, is
    /// not touched.
    pub(super) fn react_unclaimed(&self) {
        if !self.claimant {
            return;
        }
        if let Some(live) = &self.live
            && live.liveness().policy == LivenessPolicy::Cancel
        {
            live.stop(RunCancelReason::Unclaimed, None);
        }
    }
}

impl Shared {
    /// Removes the observer from its session and updates the live run's
    /// attendance. Returns `None` if this process does not hold the session
    /// or the observer is not attached to it. Holds the sessions lock
    /// throughout, so the observer comes off the current record and not one
    /// a reopen is replacing. The detach counts in the session's
    /// `observer_writes` until the returned `Detached` is dropped.
    pub(super) fn unregister(
        &self,
        session: &SessionId,
        observer: &ObserverId,
    ) -> Option<Detached> {
        let sessions = self.lock_sessions();
        let session = Arc::clone(sessions.get(session)?);
        let mut state = session.lock();
        let (observer, claimant) = state.observers.detach(observer)?;
        // A run whose stream has ended already has its ending decided, so
        // the detach does not name it and losing the claim does not reach it.
        let live = state.live.clone().filter(|live| live.stoppable());
        if let Some(live) = &live {
            live.mirror(state.observers.attendance());
        }
        state.observer_writes += 1;
        drop(state);
        drop(sessions);
        Some(Detached {
            session,
            observer,
            live,
            claimant,
        })
    }

    /// Appends `ObserverDetached` to the session's stream, then
    /// `ClaimsExhausted` if the observer held the claim of a run that was
    /// still going, and lets the session go if it is then idle. The appends
    /// go to the session's current journal, which is a new one if the
    /// session was reopened after a fence.
    pub(super) async fn record_detach(&self, detached: Detached, because: DetachCause) {
        let journal = self.lock_sessions().get(&detached.session.id).map_or_else(
            || Arc::clone(&detached.session.journal),
            |held| Arc::clone(&held.journal),
        );
        let live = detached.live.as_ref();
        let run_id = live.map(|live| live.id);
        let event = LifecycleEvent::ObserverDetached {
            observer: detached.observer.clone(),
            because,
        };
        if let Err(error) = journal
            .append(run_id, SessionEventPayload::Lifecycle(event))
            .await
        {
            tracing::warn!(session_id = %detached.session.id, %error, "the session journal refused the observer's detach");
        }
        if detached.claimant
            && let Some(live) = live
            && let Err(error) = journal
                .append(
                    Some(live.id),
                    SessionEventPayload::Lifecycle(LifecycleEvent::ClaimsExhausted),
                )
                .await
        {
            tracing::warn!(run_id = %live.id, %error, "the session journal refused the run's ClaimsExhausted event");
        }
        let session = Arc::clone(&detached.session);
        drop(detached);
        self.let_go_if_idle(&session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observer(id: &str, kind: ObserverKind, presence: bool) -> Observer {
        Observer {
            id: ObserverId::new(id).unwrap(),
            kind,
            presence,
        }
    }

    /// One claimant at a time, any number of collectors, and presence
    /// counted across both.
    #[test]
    fn the_set_holds_one_claimant_and_counts_presence_across_kinds() {
        let mut set = ObserverSet::default();
        assert!(set.is_empty());

        set.attach(observer("c1", ObserverKind::Collecting, false))
            .unwrap();
        set.attach(observer("c2", ObserverKind::Collecting, true))
            .unwrap();
        set.attach(observer("k1", ObserverKind::Claiming, false))
            .unwrap();
        assert_eq!(
            set.attach(observer("k2", ObserverKind::Claiming, true)),
            Err(ObserverId::new("k1").unwrap())
        );
        assert_eq!(
            set.counts(),
            ObserverCounts {
                collecting: 2,
                claiming: 1,
                present: 1,
            }
        );
        assert_eq!(
            set.attendance(),
            Attendance {
                present: true,
                claimed: true,
            }
        );

        let (gone, held_claim) = set.detach(&ObserverId::new("k1").unwrap()).unwrap();
        assert_eq!(gone.id, ObserverId::new("k1").unwrap());
        assert!(held_claim);
        assert!(!set.attendance().claimed);
        assert_eq!(set.detach(&ObserverId::new("k1").unwrap()), None);

        let (_, held_claim) = set.detach(&ObserverId::new("c2").unwrap()).unwrap();
        assert!(!held_claim);
        assert!(!set.attendance().present);
        assert!(!set.is_empty());
    }
}
