//! The durable approval-outcome seam: who may address a parked row.
//!
//! [`ApprovalAuthority`] is persisted on every parked row at registration —
//! `Conversational` for inline registration, `WebhookPoll` for the park arm
//! under poll delivery — so a later resolver must present the authority the
//! row was parked under. The field is required on every stored row: absence
//! is a decode failure, never a guessed authority.

use serde::{Deserialize, Serialize};

/// Who may address a parked approval row. Persisted on each new parked row at
/// registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAuthority {
    /// Inline registration: the local ingress and the standalone resolver.
    Conversational,
    /// The park arm under poll delivery: the reconciler's notify-and-poll
    /// path owns the row.
    WebhookPoll,
}
