//! Things that observe a run's turns and tool calls.
//!
//! Rig exposes one hook per streaming request, so anything watching a run has
//! to be folded into that single implementation or bolted on elsewhere. This
//! trait is the seam that lets them be independent; the rig-facing hook fans
//! out to all of them.

use std::sync::Arc;

use async_trait::async_trait;

pub struct ToolCall<'a> {
    pub name: &'a str,
}

/// Why a hook ended a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunCancelReason {
    /// The run outlived the bound it was started with.
    Deadline { after: std::time::Duration },
    /// Something outside the run cancelled it.
    External,
    /// The model called a tool the caller executes, so the run yields to it.
    ClientTool,
}

/// Something watching one run.
///
/// A hook belongs to the run whose [`Hooks`] holds it and is registered when
/// that run's rig-facing hook is built. State it keeps is that run's, so sharing
/// one instance across runs reports a state one of them did not reach.
#[async_trait]
pub trait AgentHook: Send + Sync {
    /// `false` ends the run rather than taking another LLM turn.
    /// `Some` ends the run before the next turn, carrying why.
    async fn before_turn(&self) -> Option<RunCancelReason> {
        None
    }

    async fn on_tool_call(&self, _tool_name: &str) {}

    /// Ends the run at the next point the agent checks.
    fn should_cancel(&self) -> Option<RunCancelReason> {
        None
    }
}

#[derive(Clone, Default)]
pub struct Hooks(Vec<Arc<dyn AgentHook>>);

impl Hooks {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub fn with(mut self, hook: Arc<dyn AgentHook>) -> Self {
        self.0.push(hook);
        self
    }
}

#[async_trait]
impl AgentHook for Hooks {
    /// One hook declining ends the run; the rest are still asked, so none
    /// silently skips the turn it was told about.
    async fn before_turn(&self) -> Option<RunCancelReason> {
        let mut stop = None;
        for hook in &self.0 {
            stop = hook.before_turn().await.or(stop);
        }
        stop
    }

    async fn on_tool_call(&self, tool_name: &str) {
        for hook in &self.0 {
            hook.on_tool_call(tool_name).await;
        }
    }

    fn should_cancel(&self) -> Option<RunCancelReason> {
        self.0.iter().find_map(|hook| hook.should_cancel())
    }
}

/// Ends a run once its deadline passes or its token is cancelled.
pub struct Deadline {
    start: std::time::Instant,
    timeout: Option<std::time::Duration>,
    cancelled: tokio_util::sync::CancellationToken,
}

impl Deadline {
    pub fn new(
        timeout: Option<std::time::Duration>,
        cancelled: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            start: std::time::Instant::now(),
            timeout,
            cancelled,
        }
    }
}

#[async_trait]
impl AgentHook for Deadline {
    fn should_cancel(&self) -> Option<RunCancelReason> {
        if self.cancelled.is_cancelled() {
            Some(RunCancelReason::External)
        } else {
            self.timeout
                .filter(|timeout| self.start.elapsed() > *timeout)
                .map(|after| RunCancelReason::Deadline { after })
        }
    }
}

/// Ends a run after the model calls a tool the client executes, so the caller
/// can run it and resume in a follow-up request.
pub struct ClientTools {
    names: std::collections::HashSet<String>,
    called: std::sync::atomic::AtomicBool,
}

impl ClientTools {
    pub fn new(names: std::collections::HashSet<String>) -> Self {
        Self {
            names,
            called: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn was_called(&self) -> bool {
        self.called.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[async_trait]
impl AgentHook for ClientTools {
    async fn before_turn(&self) -> Option<RunCancelReason> {
        self.was_called().then_some(RunCancelReason::ClientTool)
    }

    async fn on_tool_call(&self, tool_name: &str) {
        if self.names.contains(tool_name) {
            tracing::info!(
                "Client tool '{}' called — the run yields after it",
                tool_name
            );
            self.called
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counting {
        calls: AtomicUsize,
        proceed: bool,
        cancel: bool,
    }

    #[async_trait]
    impl AgentHook for Counting {
        async fn before_turn(&self) -> Option<RunCancelReason> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (!self.proceed).then_some(RunCancelReason::External)
        }

        fn should_cancel(&self) -> Option<RunCancelReason> {
            self.cancel.then_some(RunCancelReason::External)
        }
    }

    fn hook(proceed: bool, cancel: bool) -> Arc<Counting> {
        Arc::new(Counting {
            calls: AtomicUsize::new(0),
            proceed,
            cancel,
        })
    }

    #[tokio::test]
    async fn an_empty_set_proceeds_and_does_not_cancel() {
        let hooks = Hooks::new();
        assert!(hooks.before_turn().await.is_none());
        assert!(hooks.should_cancel().is_none());
    }

    #[tokio::test]
    async fn one_hook_declining_ends_the_turn() {
        let yes = hook(true, false);
        let no = hook(false, false);
        let hooks = Hooks::new().with(yes.clone()).with(no.clone());

        assert!(hooks.before_turn().await.is_some());
    }

    /// A hook that stops being asked stops being able to observe the run, so
    /// the decision is collected from all of them rather than short-circuited.
    #[tokio::test]
    async fn every_hook_is_asked_even_once_one_declines() {
        let no = hook(false, false);
        let later = hook(true, false);
        let hooks = Hooks::new().with(no.clone()).with(later.clone());

        assert!(hooks.before_turn().await.is_some());
        assert_eq!(later.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn any_hook_can_cancel() {
        let hooks = Hooks::new().with(hook(true, false)).with(hook(true, true));
        assert!(hooks.should_cancel().is_some());
    }

    fn client_tools(names: &[&str]) -> ClientTools {
        ClientTools::new(names.iter().map(|n| (*n).to_string()).collect())
    }

    #[tokio::test]
    async fn a_run_with_no_client_tools_keeps_going() {
        let tools = client_tools(&[]);
        assert!(tools.before_turn().await.is_none());
    }

    #[tokio::test]
    async fn calling_a_client_tool_ends_the_run_before_the_next_turn() {
        let tools = client_tools(&["Read"]);
        assert!(tools.before_turn().await.is_none());

        tools.on_tool_call("Read").await;

        assert!(tools.was_called());
        assert_eq!(
            tools.before_turn().await,
            Some(RunCancelReason::ClientTool),
            "the caller must run the tool, and the reason says so"
        );
    }

    #[tokio::test]
    async fn a_server_side_tool_does_not_end_the_run() {
        let tools = client_tools(&["Read"]);

        tools.on_tool_call("list_files").await;

        assert!(!tools.was_called());
        assert!(tools.before_turn().await.is_none());
    }

    #[tokio::test]
    async fn a_deadline_cancels_once_its_token_does() {
        let token = tokio_util::sync::CancellationToken::new();
        let deadline = Deadline::new(Some(std::time::Duration::from_secs(300)), token.clone());
        assert!(deadline.should_cancel().is_none());

        token.cancel();
        assert!(deadline.should_cancel().is_some());
    }
}
