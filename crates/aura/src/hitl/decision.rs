//! Core HITL domain types: the decision handle, the terminal outcome, who is
//! asking, why, and the typestate for a parked approval.
//!
//! This is the domain core. The webhook wire payload lives in [`super::protocol`];
//! the SSE wire mirrors live in `aura-events` and are converted in
//! [`super::events`]. Parse-time config types live in `aura-config`.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::warn;
use uuid::Uuid;

use crate::request_cancellation::RequestCancelToken;

use crate::config::SessionId;
use crate::orchestration::{RunId, TaskIdentity};

/// Wall-clock timestamp.
///
/// Wall-clock (not [`std::time::Instant`]) so a [`ParkedApproval`] stays
/// meaningful across a process restart once durable parking lands (#209).
///
/// [`ParkedApproval`]: super::registry::ParkedApproval
pub type Timestamp = chrono::DateTime<chrono::Utc>;

/// Resolvable handle for one approval decision.
///
/// Private field; construct with [`DecisionId::generate`] or
/// [`DecisionId::parse`]. Backed by a UUID v7, so the registry's `BTreeMap`
/// iterates pending approvals oldest-first. This is where #191's durable
/// consumption/expiry semantics attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecisionId(Uuid);

impl DecisionId {
    /// Mint a fresh, time-ordered decision id.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// Parse a decision id from its canonical string form (ingress routing).
    pub fn parse(s: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(s).map(Self)
    }
}

impl fmt::Display for DecisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The terminal human (or webhook) decision on a gated call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    Approved,
    Denied { reason: Option<String> },
}

/// Why a parked approval was cancelled rather than decided or timed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CancelReason {
    ClientDisconnected,
    Shutdown,
    SenderDropped,
}

/// The terminal outcome of a parked approval.
///
/// Three variants, four logical terminal states: `Decided` carries the
/// `Approved`/`Denied` split. Only `Decided(Approved)` runs the gated call;
/// `Denied`, `TimedOut`, and `Cancelled` all deny, so fail-closed is the shape
/// of the type rather than a policy check. Pending is deliberately not a
/// variant: the pending state is the suspended await itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Decided(ApprovalDecision),
    TimedOut { waited: Duration },
    Cancelled(CancelReason),
}

/// Who is requesting the approval. Embedded in events and the webhook payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentScope {
    Single {
        session_id: Option<SessionId>,
    },
    Worker {
        run_id: RunId,
        task: TaskIdentity,
        session_id: Option<SessionId>,
    },
    /// Future coordinator-mediated surface, declared now, constructed by no
    /// current code path.
    Coordinator {
        run_id: RunId,
    },
}

/// Why this approval exists.
///
/// Replaces the spike's `RequestType` plus the `matched_pattern: Option<String>`
/// field whose `Some`/`None` tracked the surface by convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOrigin {
    /// A configured glob matched the tool call; carries the display form of the
    /// glob that fired.
    ConfigGate { matched_pattern: String },
    /// The agent called `request_approval` itself.
    AgentRequested { reason: String },
}

/// Typestate for a parked call awaiting its decision.
///
/// [`AwaitingDecision::outcome`] consumes `self`, so a registration is awaited
/// at most once. The await resolves on whichever of decision / deadline /
/// cancellation fires first.
pub struct AwaitingDecision {
    id: DecisionId,
    rx: oneshot::Receiver<ApprovalDecision>,
    deadline: Instant,
}

impl AwaitingDecision {
    /// Construct the handle. Called by
    /// [`super::registry::PendingApprovals::register`], which holds the paired
    /// `oneshot::Sender`.
    pub(crate) fn new(
        id: DecisionId,
        rx: oneshot::Receiver<ApprovalDecision>,
        deadline: Instant,
    ) -> Self {
        Self { id, rx, deadline }
    }

    /// The id a decision must be posted against to resolve this await.
    #[must_use]
    pub fn id(&self) -> DecisionId {
        self.id
    }

    /// Await the terminal outcome: a posted decision, the deadline, or
    /// cancellation (client disconnect / shutdown), whichever fires first.
    pub async fn outcome(self, cancel: &RequestCancelToken) -> ApprovalOutcome {
        let start = Instant::now();
        tokio::select! {
            // Fail closed: once the request is cancelled, do not let an already
            // buffered approval race the disconnected stream into execution.
            biased;
            _ = cancel.cancelled() => {
                warn!(decision_id = %self.id, "approval cancelled: client disconnected");
                ApprovalOutcome::Cancelled(CancelReason::ClientDisconnected)
            }
            result = self.rx => match result {
                Ok(decision) => ApprovalOutcome::Decided(decision),
                Err(_) => {
                    warn!(decision_id = %self.id, "approval cancelled: decision sender dropped");
                    ApprovalOutcome::Cancelled(CancelReason::SenderDropped)
                }
            },
            _ = tokio::time::sleep_until(self.deadline) => ApprovalOutcome::TimedOut {
                waited: start.elapsed(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time;

    #[tokio::test(start_paused = true)]
    async fn outcome_decided_when_rx_receives() {
        let (tx, rx) = oneshot::channel();
        let deadline = Instant::now() + Duration::from_secs(60);
        let awaiting = AwaitingDecision::new(DecisionId::generate(), rx, deadline);
        let cancel = RequestCancelToken::unbound();

        tx.send(ApprovalDecision::Approved).unwrap();

        assert_eq!(
            awaiting.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Approved)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn outcome_denied_when_rx_receives_denied() {
        let (tx, rx) = oneshot::channel();
        let deadline = Instant::now() + Duration::from_secs(60);
        let awaiting = AwaitingDecision::new(DecisionId::generate(), rx, deadline);
        let cancel = RequestCancelToken::unbound();

        tx.send(ApprovalDecision::Denied {
            reason: Some("no".to_owned()),
        })
        .unwrap();

        assert_eq!(
            awaiting.outcome(&cancel).await,
            ApprovalOutcome::Decided(ApprovalDecision::Denied {
                reason: Some("no".to_owned())
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn outcome_cancelled_when_sender_dropped() {
        let (tx, rx) = oneshot::channel();
        let deadline = Instant::now() + Duration::from_secs(60);
        let awaiting = AwaitingDecision::new(DecisionId::generate(), rx, deadline);
        let cancel = RequestCancelToken::unbound();

        drop(tx);

        assert_eq!(
            awaiting.outcome(&cancel).await,
            ApprovalOutcome::Cancelled(CancelReason::SenderDropped)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn outcome_timed_out_when_deadline_fires() {
        let (_tx, rx) = oneshot::channel();
        let deadline = Instant::now() + Duration::from_secs(5);
        let awaiting = AwaitingDecision::new(DecisionId::generate(), rx, deadline);
        let cancel = RequestCancelToken::unbound();

        let outcome = awaiting.outcome(&cancel);
        let advance = time::advance(Duration::from_secs(6));
        let (result, _) = tokio::join!(outcome, advance);

        match result {
            ApprovalOutcome::TimedOut { waited } => {
                assert!(waited >= Duration::from_secs(5), "waited: {:?}", waited);
            }
            other => panic!("expected TimedOut, got {:?}", other),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn outcome_cancelled_when_cancel_fires() {
        let (_tx, rx) = oneshot::channel();
        let deadline = Instant::now() + Duration::from_secs(60);
        let awaiting = AwaitingDecision::new(DecisionId::generate(), rx, deadline);
        let cancel = RequestCancelToken::unbound();

        cancel.cancel();

        assert_eq!(
            awaiting.outcome(&cancel).await,
            ApprovalOutcome::Cancelled(CancelReason::ClientDisconnected)
        );
    }
}
