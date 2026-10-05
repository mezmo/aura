//! The durable approval-outcome seam: who may address a parked row.
//!
//! [`ApprovalAuthority`] is persisted on every parked row at registration —
//! `Conversational` for inline registration, `WebhookPoll` for the park arm
//! under poll delivery — so a later resolver must present the authority the
//! row was parked under. New rows always write the field; the one
//! transitional exception is the stored-record decode, where
//! interactive rows shipped before the stamp existed carry no `authority`
//! and read as `Conversational` — the only channel those rows ever had —
//! while an explicit unknown value stays a decode failure. That default
//! lives at the storage decode site in `session_store::record`, never on
//! this type.

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
