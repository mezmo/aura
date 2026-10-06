//! The signal that stops a run, as the work awaiting it sees it.
//!
//! A run's token lives on its [`crate::run_context::RunContext`]; this is the
//! handle work awaits it through.

use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

pub type RequestId = String;

/// A run's cancellation signal. Fired by the handler on client disconnect,
/// timeout, or server shutdown. Observed by MCP notifications, HITL approval
/// gates, and resource cleanup.
#[derive(Clone)]
pub struct RequestCancelToken(CancellationToken);

impl RequestCancelToken {
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.0.cancelled()
    }

    pub fn cancel(&self) {
        self.0.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// A token that is never cancelled. Used when no request-level
    /// cancellation is registered (e.g. CLI standalone mode).
    pub fn unbound() -> Self {
        Self(CancellationToken::new())
    }

    /// The wrapped token, for call sites that drive the generic streaming
    /// pipeline — it speaks the underlying token type.
    pub fn as_token(&self) -> &CancellationToken {
        &self.0
    }
}

impl From<CancellationToken> for RequestCancelToken {
    fn from(token: CancellationToken) -> Self {
        Self(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run's token is the one its work awaits, so the handle has to observe
    /// the same signal rather than a copy of it.
    #[test]
    fn a_token_observes_the_run_it_was_made_from() {
        let run = CancellationToken::new();
        let observed = RequestCancelToken::from(run.clone());
        assert!(!observed.is_cancelled());

        run.cancel();
        assert!(observed.is_cancelled());
    }

    /// Work with no run to await — the CLI, a test — must not report cancelled.
    #[test]
    fn an_unbound_token_is_never_cancelled() {
        assert!(!RequestCancelToken::unbound().is_cancelled());
    }
}
