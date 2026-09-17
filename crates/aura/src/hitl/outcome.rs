//! The durable approval-outcome seam: who may address a parked row, and what
//! addressing it produced.
//!
//! [`ApprovalAuthority`] is persisted on every parked row at registration —
//! `Conversational` for inline registration, `WebhookPoll` for the 207 bridge
//! — so a resolver must present the authority the row was parked under; a
//! wrong authority reads as unknown with no mutation, which blocks both local
//! override of governance and cross-agent polling of a shared store.
//!
//! [`AddressedApproval`] and [`ApprovalRead`] are the store's read-or-expire
//! vocabulary: expiry is a durable per-call `TimedOut`, never a fabricated
//! approval or denial, and an existing terminal winner is returned unchanged.
//! Missing, decode, and I/O failures stay errors at the store boundary; they
//! never masquerade as outcomes.

use serde::{Deserialize, Serialize};

use super::decision::{ResolvedDecision, Timestamp};
use super::registry::ParkedApproval;

/// Who may address a parked approval row. Persisted on each new parked row at
/// registration and checked by resolve and read-or-expire under the store's
/// serialization boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAuthority {
    /// Inline registration: the local ingress and the standalone resolver.
    Conversational,
    /// The 207 bridge: the poller's poll-and-decide path.
    WebhookPoll,
}

/// How a parked approval was addressed: a recorded decision, or a deadline
/// that passed strictly after which no decision ever landed.
///
/// `TimedOut` is durable and terminal — it is never an approval, never a
/// denial, and never re-derived from missing or unreadable evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressedApproval {
    /// A terminal decision won; carries the decision and any approver
    /// identity captured alongside it.
    Decided(ResolvedDecision),
    /// The row's own deadline passed with no decision. Carries the deadline
    /// that expired, so feedback can name the window the caller missed; a
    /// decision taken exactly at the deadline remains valid.
    TimedOut { deadline: Timestamp },
}

/// The result of one store read-or-expire: the row is gone, still pending, or
/// addressed with its terminal outcome.
///
/// The `Addressed` arm carries the approval row next to its outcome, so the
/// consumer can validate run, task, tool, and arguments against the stored
/// approval before acting on the outcome.
///
/// No `Debug`: [`ParkedApproval`] keeps credentials out of derived renderings,
/// so this vocabulary follows the same rule.
#[derive(Clone)]
pub enum ApprovalRead {
    /// No row under that id and authority: unknown, wrongly owned, or already
    /// consumed. Wrong authority fails here with no mutation.
    Missing,
    /// The row is still pending: undecided and inside its window.
    Pending(ParkedApproval),
    /// The row was addressed — decided, or timed out strictly past its
    /// deadline.
    Addressed {
        approval: ParkedApproval,
        outcome: AddressedApproval,
    },
}
