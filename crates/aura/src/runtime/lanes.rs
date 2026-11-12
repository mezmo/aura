//! Picking the lane a conversation's start lands on.

use std::sync::{Arc, MutexGuard};

use crate::conversation::{Conversation, LANES_PER_CONVERSATION, Lane};

use super::Shared;
use super::record::{SessionRecord, SessionState};
use super::start::{BusyLane, StartError};

impl Shared {
    /// Finds the conversation's first free lane and returns it with its
    /// session and the `admission` lock held. [`busy_because`] decides
    /// whether a lane is free. Refused when every lane is busy, with the
    /// reason for each lane, or when [`admit`](Self::admit) refuses a lane.
    ///
    /// Each lane is admitted once, from the home lane up, so a call opens
    /// each lane's session at most once. The lock held for the free lane is
    /// the one returned. A lane that frees up after it was checked is left
    /// for the next start.
    pub(super) async fn admit_lane(
        &self,
        conversation: &Conversation,
    ) -> Result<(MutexGuard<'_, ()>, Arc<SessionRecord>, Lane), StartError> {
        let mut busy = Vec::with_capacity(LANES_PER_CONVERSATION);
        for lane in conversation.lanes() {
            let (admitted, session) = self.admit(&lane.session_id()).await?;
            let why = busy_because(&session.lock());
            match why {
                None => return Ok((admitted, session, lane)),
                Some(why) => busy.push(why),
            }
        }
        let lanes = busy
            .into_boxed_slice()
            .try_into()
            .expect("the scan notes why each lane is busy");
        Err(StartError::ConversationBusy {
            conversation: conversation.clone(),
            lanes,
        })
    }
}

/// Why a lane whose session is in `state` cannot take a start, or `None` if
/// it can. A lane is busy while `state.live` holds a run or the session has
/// a claimant. Collecting observers do not make it busy.
fn busy_because(state: &SessionState) -> Option<BusyLane> {
    let run = state.live.as_ref().map(|live| live.id);
    let by = state.observers.claimant().cloned();
    match (run, by) {
        (None, None) => None,
        (Some(run), None) => Some(BusyLane::Running { run }),
        (None, Some(by)) => Some(BusyLane::Claimed { by }),
        (Some(run), Some(by)) => Some(BusyLane::RunningAndClaimed { run, by }),
    }
}
