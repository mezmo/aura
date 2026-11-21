//! A [`StreamingAgent`] for tests that need an agent without a provider.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use aura::stream_events::McpServerStatus;
use aura::streaming::AgentRun;
use aura::{Message, RunContext, StreamError, StreamItem, StreamingAgent, UsageState};
use aura::{StreamedAssistantContent, StreamedUserContent, ToolCall, ToolResult, ToolResultKind};
use futures::stream::{self, BoxStream, StreamExt};

type StartHook = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
type EffectHook = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Pause inserted after each scripted step.
///
/// `process_sse_stream_full` races the stream against the run's event channel
/// in an unbiased `tokio::select!`, so a step that lands while the
/// previous one is still queued would be ordered arbitrarily. Pausing lets the
/// consumer drain before the next step becomes ready. Under
/// `#[tokio::test(start_paused = true)]` the runtime auto-advances, so this
/// costs no wall-clock time.
const SETTLE: Duration = Duration::from_millis(10);

/// One step of a [`MockAgent`] script.
pub enum Step {
    Item(Result<StreamItem, StreamError>),
    Effect(EffectHook),
    /// Runs an effect and yields an item in the same step.
    Emitted(Result<StreamItem, StreamError>, EffectHook),
}

impl Step {
    pub fn item(item: Result<StreamItem, StreamError>) -> Self {
        Self::Item(item)
    }

    /// Passed the current call's `request_id`.
    pub fn effect<F, Fut>(effect: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::Effect(Arc::new(move |request_id| Box::pin(effect(request_id))))
    }

    /// Runs `effect` and yields `item` in the same step.
    ///
    /// This models an event that an agent puts on both its item stream and
    /// the run's event channel. `effect` sends the event on the channel, and
    /// `item` carries the same event on the stream. `effect` is passed the
    /// current call's `request_id`.
    pub fn emitted<F, Fut>(item: Result<StreamItem, StreamError>, effect: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::Emitted(
            item,
            Arc::new(move |request_id| Box::pin(effect(request_id))),
        )
    }
}

/// Stream items in the shapes a provider produces, for building scripts.
pub mod items {
    use super::*;

    pub fn text(content: &str) -> Result<StreamItem, StreamError> {
        Ok(StreamItem::StreamAssistantItem(
            StreamedAssistantContent::Text(content.to_string()),
        ))
    }

    pub fn tool_call(id: &str, name: &str, arguments: &str) -> Result<StreamItem, StreamError> {
        Ok(StreamItem::StreamAssistantItem(
            StreamedAssistantContent::ToolCall(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: arguments.to_string(),
            }),
        ))
    }

    pub fn tool_result(id: &str, result: &str) -> Result<StreamItem, StreamError> {
        tool_result_of(id, result, ToolResultKind::Text)
    }

    pub fn tool_error(id: &str, error: &str) -> Result<StreamItem, StreamError> {
        tool_result_of(id, error, ToolResultKind::Error)
    }

    fn tool_result_of(
        id: &str,
        result: &str,
        kind: ToolResultKind,
    ) -> Result<StreamItem, StreamError> {
        Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(
            ToolResult {
                id: id.to_string(),
                call_id: None,
                result: result.to_string(),
                kind,
            },
        )))
    }
}

enum Script {
    Pending,
    Steps(Mutex<Option<Vec<Step>>>),
}

pub struct MockAgent {
    on_stream_start: Option<StartHook>,
    script: Script,
    run_id: aura::RunId,
    stream_claim: aura::streaming::StreamClaim,
    /// Whether each run gets its own `RunContext`.
    run_context: bool,
    mcp_status: Vec<McpServerStatus>,
}

impl MockAgent {
    /// Yields nothing and never ends.
    pub fn pending() -> Self {
        Self {
            on_stream_start: None,
            script: Script::Pending,
            run_id: aura::RunId::mint(),
            stream_claim: aura::streaming::StreamClaim::default(),
            run_context: false,
            mcp_status: Vec::new(),
        }
    }

    /// Yields the given items, then ends the stream.
    pub fn yielding<I>(items: I) -> Self
    where
        I: IntoIterator<Item = Result<StreamItem, StreamError>>,
    {
        Self::scripted(items.into_iter().map(Step::Item).collect())
    }

    /// Runs the given steps in order, then ends the stream.
    pub fn scripted(steps: Vec<Step>) -> Self {
        Self {
            on_stream_start: None,
            script: Script::Steps(Mutex::new(Some(steps))),
            run_id: aura::RunId::mint(),
            stream_claim: aura::streaming::StreamClaim::default(),
            run_context: false,
            mcp_status: Vec::new(),
        }
    }

    /// Streams under `run_id` instead of the id minted when the agent was built.
    pub fn for_run(mut self, run_id: aura::RunId) -> Self {
        self.run_id = run_id;
        self
    }

    /// Gives each run its own `RunContext`, built on the cancel token from
    /// the run's options. The context is in scope while the script runs, so
    /// an effect step can send events through `aura::run_context::emit`. The
    /// returned `AgentRun` holds the context (`run_context()`) and the
    /// receiver of its event channel (`take_agent_events()`).
    pub fn with_run_context(mut self) -> Self {
        self.run_context = true;
        self
    }

    /// Reports `servers` as the status of its MCP servers.
    pub fn with_mcp_status(mut self, servers: Vec<McpServerStatus>) -> Self {
        self.mcp_status = servers;
        self
    }

    /// Awaited before `stream` produces its stream, and passed that
    /// call's `request_id`.
    pub fn on_stream_start<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_stream_start = Some(Arc::new(move |request_id| Box::pin(hook(request_id))));
        self
    }

    async fn start(&self, request_id: &str) -> BoxStream<'static, Result<StreamItem, StreamError>> {
        if let Some(hook) = &self.on_stream_start {
            hook(request_id.to_string()).await;
        }

        let stream: BoxStream<'static, Result<StreamItem, StreamError>> = match &self.script {
            Script::Pending => Box::pin(stream::pending()),
            Script::Steps(steps) => {
                let steps = steps
                    .lock()
                    .expect("script lock")
                    .take()
                    .unwrap_or_default();
                let request_id = request_id.to_string();
                Box::pin(
                    stream::iter(steps)
                        .then(move |step| {
                            let request_id = request_id.clone();
                            async move {
                                match step {
                                    Step::Item(item) => {
                                        tokio::time::sleep(SETTLE).await;
                                        Some(item)
                                    }
                                    Step::Effect(effect) => {
                                        effect(request_id).await;
                                        tokio::time::sleep(SETTLE).await;
                                        None
                                    }
                                    // Settle first, so that the effect's event and
                                    // the item arrive with no pause between them.
                                    Step::Emitted(item, effect) => {
                                        tokio::time::sleep(SETTLE).await;
                                        effect(request_id).await;
                                        Some(item)
                                    }
                                }
                            }
                        })
                        .filter_map(std::future::ready),
                )
            }
        };
        stream
    }
}

#[async_trait]
impl StreamingAgent for MockAgent {
    fn get_provider_info(&self) -> (&str, &str) {
        ("test", "fake")
    }

    fn run_id(&self) -> aura::RunId {
        self.run_id
    }

    async fn stream(
        &self,
        _query: &str,
        _chat_history: Vec<Message>,
        options: aura::streaming::RunOptions,
        request_id: &str,
    ) -> AgentRun {
        // Held to the contract a real agent keeps, so a test that names the
        // wrong run or streams twice fails the way production would.
        if let Err(refused) = self.stream_claim.claim(self.run_id, request_id) {
            return AgentRun::refused(refused);
        }
        // Carries a caller-supplied token so `cancel_token()` returns the one the
        // caller named. The scripts do not race it, so cancelling does not end a
        // mock stream.
        let cancel = options.into_parts().1.unwrap_or_default();
        let stream = self.start(request_id).await;
        if !self.run_context {
            return AgentRun::new(stream, cancel, UsageState::new());
        }
        let (run, events) = RunContext::channel_on(self.run_id, cancel.clone());
        let scoped = aura::run_context::scope_stream(Arc::clone(&run), stream);
        AgentRun::new(Box::pin(scoped), cancel, UsageState::new())
            .observed_by(events)
            .within(run)
    }

    async fn cancel_and_close_mcp(&self, _request_id: &str, _reason: &str) -> usize {
        0
    }

    fn mcp_server_status(&self) -> Vec<McpServerStatus> {
        self.mcp_status.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test]
    async fn a_pending_agent_never_yields() {
        let agent = MockAgent::pending();
        let mut stream = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &agent.run_id().to_string(),
            )
            .await
            .into_events();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stream.next())
                .await
                .is_err()
        );
    }

    /// `into_events` wraps the stream, so the order is asserted rather than the
    /// count — a wrapper that buffered or reordered would keep the count.
    #[tokio::test(start_paused = true)]
    async fn a_yielding_agent_produces_its_items_then_ends() {
        let agent = MockAgent::yielding(vec![items::text("hello "), items::text("world")]);
        let mut stream = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &agent.run_id().to_string(),
            )
            .await
            .into_events();

        let mut texts = Vec::new();
        while let Some(item) = stream.next().await {
            if let Ok(aura::StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) =
                item
            {
                texts.push(t);
            }
        }
        assert_eq!(texts, ["hello ", "world"]);
    }

    #[tokio::test]
    async fn the_start_hook_runs_before_the_stream() {
        let ran = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(None::<String>));
        let (flag, seen_by_hook) = (Arc::clone(&ran), Arc::clone(&seen));
        let agent = MockAgent::pending().on_stream_start(move |request_id| {
            let (flag, seen) = (Arc::clone(&flag), Arc::clone(&seen_by_hook));
            async move {
                *seen.lock().expect("seen lock") = Some(request_id);
                flag.store(true, Ordering::SeqCst);
            }
        });

        let _ = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &agent.run_id().to_string(),
            )
            .await
            .into_events();

        assert!(ran.load(Ordering::SeqCst), "hook should run");
        assert_eq!(
            seen.lock().expect("seen lock").as_deref(),
            Some(agent.run_id().to_string().as_str())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn effects_run_in_script_order_and_see_the_request_id() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let effect_order = Arc::clone(&order);
        let agent = MockAgent::scripted(vec![
            Step::effect(move |request_id| {
                let order = Arc::clone(&effect_order);
                async move {
                    order.lock().expect("order lock").push(request_id);
                }
            }),
            Step::item(items::text("after")),
        ]);

        let stream = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &agent.run_id().to_string(),
            )
            .await
            .into_events();
        let items: Vec<_> = stream.collect().await;

        assert_eq!(
            order.lock().expect("order lock").as_slice(),
            [agent.run_id().to_string()]
        );
        assert_eq!(items.len(), 1, "effects do not yield stream items");
    }

    /// The mock refuses what a real agent refuses: a stream naming another
    /// run, which never reaches the start hook.
    #[tokio::test]
    async fn a_stream_naming_another_run_is_refused_before_the_hook() {
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let agent = MockAgent::yielding([items::text("never")]).on_stream_start(move |_| {
            let flag = Arc::clone(&flag);
            async move { flag.store(true, Ordering::SeqCst) }
        });

        let items: Vec<_> = agent
            .stream("q", vec![], aura::streaming::RunOptions::default(), "req_1")
            .await
            .into_events()
            .collect()
            .await;

        let [Err(error)] = items.as_slice() else {
            panic!("expected one refusal, got {items:?}");
        };
        assert!(matches!(
            error.downcast_ref::<aura::streaming::StreamRefused>(),
            Some(aura::streaming::StreamRefused::OtherId { .. })
        ));
        assert!(!ran.load(Ordering::SeqCst), "a refused stream runs nothing");
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_stream_is_refused() {
        let agent = MockAgent::yielding([items::text("once")]);

        let first: Vec<_> = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &agent.run_id().to_string(),
            )
            .await
            .into_events()
            .collect()
            .await;
        let second: Vec<_> = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &agent.run_id().to_string(),
            )
            .await
            .into_events()
            .collect()
            .await;

        assert_eq!(first.len(), 1);
        let [Err(error)] = second.as_slice() else {
            panic!("expected one refusal, got {second:?}");
        };
        assert!(matches!(
            error.downcast_ref::<aura::streaming::StreamRefused>(),
            Some(aura::streaming::StreamRefused::AlreadyStreamed { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_for_a_named_run_streams_under_that_run() {
        let run = aura::RunId::mint();
        let agent = MockAgent::yielding([items::text("hi")]).for_run(run);
        assert_eq!(agent.run_id(), run);

        let items: Vec<_> = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &run.to_string(),
            )
            .await
            .into_events()
            .collect()
            .await;
        assert!(matches!(items.as_slice(), [Ok(_)]), "{items:?}");
    }

    /// The context has the run's id and the caller's cancel token, and it is
    /// in scope for both effect steps and emitted steps.
    #[tokio::test(start_paused = true)]
    async fn a_run_context_is_on_the_callers_token_and_in_scope_for_each_step() {
        let run = aura::RunId::mint();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (effect_seen, emitted_seen) = (Arc::clone(&seen), Arc::clone(&seen));
        let agent = MockAgent::scripted(vec![
            Step::effect(move |_| {
                let seen = Arc::clone(&effect_seen);
                async move {
                    seen.lock()
                        .expect("seen lock")
                        .push(aura::run_context::current_run_id())
                }
            }),
            Step::emitted(items::text("carried"), move |_| {
                let seen = Arc::clone(&emitted_seen);
                async move {
                    seen.lock()
                        .expect("seen lock")
                        .push(aura::run_context::current_run_id())
                }
            }),
        ])
        .for_run(run)
        .with_run_context();
        let token = tokio_util::sync::CancellationToken::new();

        let mut handle = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::on_token(None, token.clone()),
                &run.to_string(),
            )
            .await;
        let context = Arc::clone(handle.run_context().expect("the run has a context"));
        assert_eq!(context.id(), run);
        assert!(
            handle.take_agent_events().is_some(),
            "the observer gets the channel"
        );
        token.cancel();
        assert!(
            context.cancel_token().is_cancelled(),
            "the context is on the caller's token"
        );

        let items: Vec<_> = handle.into_events().collect().await;
        assert!(
            matches!(
                items.as_slice(),
                [Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text)))]
                    if text == "carried"
            ),
            "an emitted step yields its item: {items:?}"
        );
        assert_eq!(
            seen.lock().expect("seen lock").as_slice(),
            [Some(run), Some(run)],
            "both effects ran with the run in scope"
        );
    }

    #[tokio::test]
    async fn without_a_run_context_the_run_has_none() {
        let agent = MockAgent::pending();
        let mut handle = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &agent.run_id().to_string(),
            )
            .await;
        assert!(handle.run_context().is_none());
        assert!(handle.take_agent_events().is_none());
    }

    #[test]
    fn the_mcp_status_is_what_the_agent_was_given() {
        let servers = vec![McpServerStatus {
            server_name: "files".to_string(),
            transport: "stdio".to_string(),
            status: "connected".to_string(),
            tools_count: 2,
            reason: None,
        }];
        let agent = MockAgent::pending().with_mcp_status(servers.clone());
        assert_eq!(agent.mcp_server_status(), servers);
        assert!(MockAgent::pending().mcp_server_status().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn steps_settle_between_items_so_a_consumer_can_interleave() {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let agent = MockAgent::scripted(vec![
            Step::item(items::text("a")),
            Step::effect(move |_| {
                let counter = Arc::clone(&counter);
                async move {
                    // Runs only after the consumer has taken the first item.
                    assert_eq!(counter.load(Ordering::SeqCst), 1);
                }
            }),
            Step::item(items::text("b")),
        ]);

        let mut stream = agent
            .stream(
                "q",
                vec![],
                aura::streaming::RunOptions::default(),
                &agent.run_id().to_string(),
            )
            .await
            .into_events();
        while stream.next().await.is_some() {
            seen.fetch_add(1, Ordering::SeqCst);
        }

        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }
}
