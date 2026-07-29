//! Re-arming inactivity deadline for streaming loops.
//!
//! Detects a provider that has gone silent mid-stream: the deadline re-arms on
//! every sign of provider liveness and fires only after a full window of
//! silence. Suspension exempts tool execution, where provider silence is
//! expected (tool hangs are a separate concern, tracked by #187).

use crate::provider_agent::{StreamItem, StreamedAssistantContent, StreamedUserContent};
use std::time::Duration;
use tokio::time::Instant;

/// Sentinel prefix of an inactivity-stall error message; deliberately
/// neutral about blame.
pub const STALL_MESSAGE: &str = "no stream progress for";

/// How a stream item bears on the inactivity deadline.
#[derive(Debug, Clone, Copy)]
pub enum Liveness {
    Activity,
    ToolStarted,
    ToolFinished,
}

/// Tool execution happens inside the following `stream.next()` (rig yields
/// `ToolCall` before executing and `ToolResult` after), so `ToolStarted`
/// suspends across exactly the window where provider silence is expected.
/// Pairing relies on rig's sequential tool execution ("Critical Assumption",
/// CLAUDE.md).
pub fn liveness_of<E>(item: &Result<StreamItem, E>) -> Liveness {
    match item {
        Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall(_))) => {
            Liveness::ToolStarted
        }
        Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(_))) => {
            Liveness::ToolFinished
        }
        _ => Liveness::Activity,
    }
}

/// Re-arming deadline that fires after a window of provider silence.
#[derive(Debug)]
pub struct InactivityDeadline {
    window: Duration,
    state: State,
}

#[derive(Debug)]
enum State {
    Disabled,
    Idle,
    Armed { deadline: Instant },
    Suspended,
}

impl InactivityDeadline {
    /// Armed from the start. A zero window disables the deadline entirely.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        let state = if window.is_zero() {
            State::Disabled
        } else {
            State::Armed {
                deadline: Instant::now() + window,
            }
        };
        Self { window, state }
    }

    /// Idle until the first `touch` arms it. A zero window disables the
    /// deadline entirely.
    #[must_use]
    pub fn new_disarmed(window: Duration) -> Self {
        let state = if window.is_zero() {
            State::Disabled
        } else {
            State::Idle
        };
        Self { window, state }
    }

    /// Arm (or re-arm) to a full window from now. Ignored while suspended or
    /// disabled: a touch during tool execution must not shorten or revive the
    /// countdown.
    pub fn touch(&mut self) {
        if matches!(self.state, State::Idle | State::Armed { .. }) {
            self.state = State::Armed {
                deadline: Instant::now() + self.window,
            };
        }
    }

    /// Stop the countdown for the duration of a tool execution. Idempotent.
    pub fn suspend(&mut self) {
        if matches!(self.state, State::Idle | State::Armed { .. }) {
            self.state = State::Suspended;
        }
    }

    /// Restart the countdown with a full window. Idempotent: resuming while
    /// armed is a touch.
    pub fn resume(&mut self) {
        if !matches!(self.state, State::Disabled) {
            self.state = State::Armed {
                deadline: Instant::now() + self.window,
            };
        }
    }

    /// Resolves when the window elapses without a touch; pending forever while
    /// disabled or suspended.
    ///
    /// Safe in a `tokio::select!` arm whose siblings call
    /// `touch`/`suspend`/`resume`; construct a fresh future each loop
    /// iteration.
    pub async fn expired(&self) {
        match self.state {
            State::Disabled | State::Idle | State::Suspended => std::future::pending().await,
            State::Armed { deadline } => tokio::time::sleep_until(deadline).await,
        }
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// Error for a fired deadline, logged and carrying [`STALL_MESSAGE`].
    pub fn stall_error(&self, phase: &str) -> Box<dyn std::error::Error + Send + Sync> {
        let secs = self.window.as_secs();
        tracing::warn!("{phase}: {STALL_MESSAGE} {secs}s (stream_inactivity_timeout_secs={secs})");
        format!("{phase}: {STALL_MESSAGE} {secs}s").into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(30);
    /// Long enough that only a deadline that can never fire survives it.
    const NEVER: Duration = Duration::from_secs(3600);

    /// Polls `expired` against a short timeout without advancing past the
    /// deadline, so "still pending" is observable under paused time.
    async fn fires_within(deadline: &InactivityDeadline, dur: Duration) -> bool {
        tokio::time::timeout(dur, deadline.expired()).await.is_ok()
    }

    /// Asserts the deadline holds for a full window from now, then fires.
    async fn assert_full_window_ahead(deadline: &InactivityDeadline) {
        assert!(!fires_within(deadline, WINDOW - Duration::from_secs(1)).await);
        assert!(fires_within(deadline, Duration::from_secs(2)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn fires_after_window_of_silence() {
        let deadline = InactivityDeadline::new(WINDOW);
        assert_full_window_ahead(&deadline).await;
    }

    #[tokio::test(start_paused = true)]
    async fn touch_rearms_full_window() {
        let mut deadline = InactivityDeadline::new(WINDOW);
        tokio::time::advance(WINDOW - Duration::from_secs(1)).await;
        deadline.touch();
        assert_full_window_ahead(&deadline).await;
    }

    #[tokio::test(start_paused = true)]
    async fn suspended_never_fires() {
        let mut deadline = InactivityDeadline::new(WINDOW);
        deadline.suspend();
        assert!(!fires_within(&deadline, NEVER).await);
    }

    #[tokio::test(start_paused = true)]
    async fn touch_while_suspended_stays_suspended() {
        let mut deadline = InactivityDeadline::new(WINDOW);
        deadline.suspend();
        deadline.touch();
        assert!(!fires_within(&deadline, NEVER).await);
    }

    #[tokio::test(start_paused = true)]
    async fn resume_restarts_full_window() {
        let mut deadline = InactivityDeadline::new(WINDOW);
        deadline.suspend();
        tokio::time::advance(WINDOW * 3).await;
        deadline.resume();
        assert_full_window_ahead(&deadline).await;
    }

    #[tokio::test(start_paused = true)]
    async fn already_past_deadline_fires_immediately() {
        let deadline = InactivityDeadline::new(WINDOW);
        tokio::time::advance(WINDOW * 2).await;
        assert!(fires_within(&deadline, Duration::from_millis(1)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn disarmed_is_idle_until_touched() {
        let mut deadline = InactivityDeadline::new_disarmed(WINDOW);
        assert!(!fires_within(&deadline, NEVER).await);
        deadline.touch();
        assert_full_window_ahead(&deadline).await;
    }

    #[tokio::test(start_paused = true)]
    async fn disarmed_zero_window_never_fires() {
        let mut deadline = InactivityDeadline::new_disarmed(Duration::ZERO);
        deadline.touch();
        assert!(!fires_within(&deadline, NEVER).await);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_window_never_fires() {
        let mut deadline = InactivityDeadline::new(Duration::ZERO);
        assert!(!fires_within(&deadline, NEVER).await);
        deadline.suspend();
        deadline.resume();
        deadline.touch();
        assert!(!fires_within(&deadline, NEVER).await);
    }

    #[tokio::test(start_paused = true)]
    async fn select_arm_borrow_pattern_compiles_and_touches() {
        let mut deadline = InactivityDeadline::new(WINDOW);
        let mut fired = false;
        for _ in 0..3 {
            tokio::select! {
                _ = deadline.expired() => { fired = true; }
                _ = tokio::time::sleep(Duration::from_secs(10)) => { deadline.touch(); }
            }
        }
        assert!(!fired);
        tokio::select! {
            _ = deadline.expired() => { fired = true; }
            _ = tokio::time::sleep(WINDOW * 2) => {}
        }
        assert!(fired);
    }
}
