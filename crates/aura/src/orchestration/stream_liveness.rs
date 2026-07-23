//! Stream liveness policy: inactivity timeout for provider hangs.
//!
//! `per_call_timeout_secs` wraps a worker's WHOLE ReAct loop, so it cannot
//! distinguish a hung provider from a busy worker (mezmo/aura#394). This
//! module models the alternative: an inactivity deadline that re-arms on
//! every stream item — text delta, reasoning delta, tool call, tool result
//! all count as liveness — and fails the stream only after N consecutive
//! silent seconds.
//!
//! The "0 disables" decision is parsed once at construction
//! ([`StreamLiveness::from_secs`]); downstream code never re-checks a raw
//! integer.

use std::num::NonZeroU64;
use std::time::Duration;

use futures::{Stream, StreamExt};

/// A positive inactivity window between consecutive stream items.
///
/// Business rule: the window bounds silence, not total duration. Any stream
/// item resets the deadline.
///
/// Forbidden invalid state: a zero window ("time out immediately") is
/// unrepresentable; zero means disabled and is parsed into
/// [`StreamLiveness::Unbounded`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InactivityWindow(NonZeroU64);

impl InactivityWindow {
    pub fn as_duration(&self) -> Duration {
        Duration::from_secs(self.0.get())
    }

    pub fn as_secs(&self) -> u64 {
        self.0.get()
    }
}

/// Liveness policy for a worker stream, decided once from config.
///
/// Business rule: `inactivity_timeout_secs = 0` disables the bound; a
/// positive value fails the stream after that many silent seconds between
/// items.
///
/// Forbidden invalid state: an "enabled with zero seconds" policy — the
/// enabled/disabled decision and the window are one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamLiveness {
    /// No inactivity bound; awaiting the next item can block indefinitely.
    Unbounded,
    /// Fail the stream after this many consecutive silent seconds.
    Bounded(InactivityWindow),
}

impl StreamLiveness {
    /// Parse the config value: 0 disables, any positive value bounds silence.
    pub fn from_secs(secs: u64) -> Self {
        match NonZeroU64::new(secs) {
            None => Self::Unbounded,
            Some(secs) => Self::Bounded(InactivityWindow(secs)),
        }
    }

    /// Await the next stream item under this policy.
    ///
    /// The deadline re-arms on every call, so each yielded item counts as
    /// liveness: a stream may run arbitrarily long as long as no single gap
    /// between items exceeds the window.
    pub async fn next_item<S>(&self, stream: &mut S) -> Result<Option<S::Item>, InactivityElapsed>
    where
        S: Stream + Unpin,
    {
        match self {
            Self::Unbounded => Ok(stream.next().await),
            Self::Bounded(window) => tokio::time::timeout(window.as_duration(), stream.next())
                .await
                .map_err(|_| InactivityElapsed { window: *window }),
        }
    }
}

/// The stream went silent for longer than the configured inactivity window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InactivityElapsed {
    window: InactivityWindow,
}

impl std::fmt::Display for InactivityElapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stream produced no items for {}s (inactivity_timeout_secs={}) — the LLM provider appears hung",
            self.window.as_secs(),
            self.window.as_secs(),
        )
    }
}

impl std::error::Error for InactivityElapsed {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_secs_parses_to_unbounded() {
        assert_eq!(StreamLiveness::from_secs(0), StreamLiveness::Unbounded);
    }

    #[test]
    fn positive_secs_parses_to_bounded_window() {
        match StreamLiveness::from_secs(90) {
            StreamLiveness::Bounded(window) => {
                assert_eq!(window.as_secs(), 90);
                assert_eq!(window.as_duration(), Duration::from_secs(90));
            }
            StreamLiveness::Unbounded => panic!("90s must parse to Bounded"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn silent_stream_past_window_fails_with_inactivity_error() {
        let liveness = StreamLiveness::from_secs(30);
        let mut stream = futures::stream::pending::<u32>();

        let err = liveness
            .next_item(&mut stream)
            .await
            .expect_err("a silent stream must trip the inactivity window");
        assert!(err.to_string().contains("inactivity_timeout_secs=30"));
    }

    #[tokio::test(start_paused = true)]
    async fn items_within_window_pass_even_when_total_exceeds_window() {
        // Window: 10s. Five items arrive 8s apart: every gap is under the
        // window, but the total duration (40s) exceeds it.
        let liveness = StreamLiveness::from_secs(10);
        let mut stream = Box::pin(futures::stream::unfold(0u32, |n| async move {
            if n >= 5 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(8)).await;
            Some((n, n + 1))
        }));

        let mut items = Vec::new();
        while let Some(item) = liveness
            .next_item(&mut stream)
            .await
            .expect("gaps under the window must never trip the deadline")
        {
            items.push(item);
        }
        assert_eq!(items, vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_stream_fails_when_one_gap_exceeds_window() {
        // Window: 10s. The second gap (12s) exceeds it even though the first
        // item arrived promptly.
        let liveness = StreamLiveness::from_secs(10);
        let mut stream = Box::pin(futures::stream::unfold(0u32, |n| async move {
            let gap = if n == 0 { 1 } else { 12 };
            tokio::time::sleep(Duration::from_secs(gap)).await;
            Some((n, n + 1))
        }));

        let first = liveness.next_item(&mut stream).await;
        assert_eq!(first, Ok(Some(0)));
        let second = liveness.next_item(&mut stream).await;
        assert!(second.is_err(), "a 12s gap must trip a 10s window");
    }

    #[tokio::test(start_paused = true)]
    async fn unbounded_preserves_todays_behavior() {
        // Window 0 (disabled): a 1-hour gap between items must not fail.
        let liveness = StreamLiveness::from_secs(0);
        let mut stream = Box::pin(futures::stream::unfold(0u32, |n| async move {
            if n >= 1 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Some((n, n + 1))
        }));

        assert_eq!(liveness.next_item(&mut stream).await, Ok(Some(0)));
        assert_eq!(liveness.next_item(&mut stream).await, Ok(None));
    }
}
