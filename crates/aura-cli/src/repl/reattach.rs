//! The bounded reattach policy for parked runs.
//!
//! After a targeted `RunParked` ends the current stream, the client
//! reattaches through the run-resource POST only — never governance.
//! The wait is bounded three ways: retryable `parked`/`running` rows
//! poll on a one-second cadence growing to five seconds; five
//! consecutive transient transport or 503 failures give up after the
//! 1, 2, 4, 8, and 10 second delays; and the advertised retention
//! deadline caps the total wait. After a resume POST has been accepted
//! (a 200 stream), an ambiguously interrupted stream stops all
//! automatic retries.

use std::time::{Duration, SystemTime};

/// The park state recorded from a `RunParked` event: the reattach
/// target and its advertised retention deadline.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "wired by the W6c fill commit")]
pub(crate) struct ParkedRun {
    pub run_id: String,
    pub retention_expires_at: String,
    pub decision_ids: Vec<String>,
}

/// How a reattach wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code, reason = "wired by the W6c fill commit")]
pub(crate) enum ReattachEnd {
    /// A resumed segment completed with this final text.
    Completed(String),
    /// The user cancelled the wait; whatever partial text was received
    /// is preserved, no failure message is added.
    Cancelled,
    /// A resume POST was accepted but its stream ended ambiguously:
    /// automatic retries stop after saving the received partial.
    AmbiguousStream,
    /// A terminal 409 row (interrupted, config_changed, mismatch,
    /// expired).
    Terminal,
    /// The run is absent.
    NotFound,
    /// A 500 `reify_failed` (or an unknown/undecodable row, which fails
    /// closed to terminal).
    ReifyFailed,
    /// Five consecutive transient transport or 503 failures.
    TransientBudget,
    /// The advertised retention deadline passed.
    RetentionCap,
}

/// The reattach timing policy.
///
/// Pure scheduling state: the driver feeds it outcomes and sleeps the
/// delays it returns. A successful 200 resets the transient budget (the
/// failures must be consecutive); a fresh park resets the retryable
/// cadence (each gate waits afresh).
#[allow(dead_code, reason = "wired by the W6c fill commit")]
pub(crate) struct ReattachSchedule {
    retryable_step: u32,
    transient_failures: u32,
}

#[allow(dead_code, reason = "wired by the W6c fill commit")]
impl ReattachSchedule {
    pub(crate) fn new() -> Self {
        Self {
            retryable_step: 0,
            transient_failures: 0,
        }
    }

    /// The delay before the next poll after a retryable `parked` or
    /// `running` row: one second growing to five, then steady five.
    pub(crate) fn next_retryable_delay(&mut self) -> Duration {
        let _ = &mut self.retryable_step;
        todo!("filled with the retryable cadence in the W6c fill commit")
    }

    /// Record one transient transport or 503 failure. Returns the delay
    /// to wait before the next attempt (`1, 2, 4, 8, 10` seconds), or
    /// `None` once five consecutive failures have accumulated.
    pub(crate) fn record_transient(&mut self) -> Option<Duration> {
        let _ = &mut self.transient_failures;
        todo!("filled with the transient budget in the W6c fill commit")
    }

    /// A 200 stream was accepted: the transient failures are no longer
    /// consecutive.
    pub(crate) fn reset_transient(&mut self) {
        self.transient_failures = 0;
    }
}

impl Default for ReattachSchedule {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the park's advertised retention deadline into an absolute cap.
/// An undecodable stamp yields no cap: the deadline is an RFC 3339
/// instant by construction on the wire, so garbage means a broken event,
/// and the honest bound is the schedule alone.
#[allow(dead_code, reason = "wired by the W6c fill commit")]
pub(crate) fn retention_cap_from(expires_at: &str) -> Option<SystemTime> {
    let _ = expires_at;
    todo!("filled with the deadline parse in the W6c fill commit")
}

/// The session's latest observed park, recorded by the `RunParked`
/// rendering and consumed by the automatic reattach (the in-turn
/// driver) and the manual `/resume-run` re-arm.
#[allow(dead_code, reason = "wired by the W6c fill commit")]
pub(crate) fn latest_park_slot() -> &'static std::sync::Mutex<Option<ParkedRun>> {
    static SLOT: std::sync::Mutex<Option<ParkedRun>> = std::sync::Mutex::new(None);
    &SLOT
}

/// A manual `/resume-run` request staged by the command layer for the
/// main REPL loop, which owns the turn machinery the reattach needs.
#[allow(dead_code, reason = "wired by the W6c fill commit")]
pub(crate) fn pending_manual_resume() -> &'static std::sync::Mutex<Option<String>> {
    static SLOT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    &SLOT
}

/// The one fixed message a complete reify failure appends to chat
/// history, per the client failure contract.
#[allow(dead_code, reason = "wired by the W6c fill commit")]
pub(crate) const REIFY_FAILED_MESSAGE: &str =
    "I could not resume the paused run. You can continue the conversation.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_cadence_grows_one_to_five_then_steady() {
        let mut schedule = ReattachSchedule::new();
        let mut delays = Vec::new();
        for _ in 0..10 {
            delays.push(schedule.next_retryable_delay());
        }
        let expected = [
            1, 2, 3, 4, 5, 5, 5, 5, 5, 5,
        ]
        .map(Duration::from_secs);
        assert_eq!(delays, expected);
    }

    #[test]
    fn transient_budget_walks_the_five_delays_then_exhausts() {
        let mut schedule = ReattachSchedule::new();
        let delays: Vec<Duration> =
            std::iter::from_fn(|| schedule.record_transient()).collect();
        let expected = [1, 2, 4, 8, 10].map(Duration::from_secs);
        assert_eq!(delays, expected);
        // The budget stays exhausted on further calls.
        assert!(schedule.record_transient().is_none());
    }

    #[test]
    fn transient_budget_resets_after_an_accepted_stream() {
        let mut schedule = ReattachSchedule::new();
        assert_eq!(schedule.record_transient(), Some(Duration::from_secs(1)));
        assert_eq!(schedule.record_transient(), Some(Duration::from_secs(2)));
        schedule.reset_transient();
        assert_eq!(schedule.record_transient(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn retention_cap_parses_an_rfc3339_stamp() {
        let cap = retention_cap_from("2026-10-01T12:00:00Z").expect("valid stamp parses");
        let as_unix = cap
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after the epoch");
        assert_eq!(as_unix.as_secs(), 1_790_177_600);
    }

    #[test]
    fn retention_cap_rejects_garbage_rather_than_guessing() {
        assert!(retention_cap_from("not a timestamp").is_none());
        assert!(retention_cap_from("").is_none());
    }
}
