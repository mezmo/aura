//! A [`StreamingAgent`] for tests that need an agent without a provider.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use aura::streaming::AgentRun;
use aura::{Message, StreamError, StreamItem, StreamingAgent, UsageState};
use aura::{StreamedAssistantContent, StreamedUserContent, ToolCall, ToolResult};
use futures::stream::{self, BoxStream, StreamExt};
use tokio_util::sync::CancellationToken;

type StartHook = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
type EffectHook = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Pause inserted after each scripted step.
///
/// `process_sse_stream_full` races the stream against the tool/progress/approval
/// channels in an unbiased `tokio::select!`, so a step that lands while the
/// previous one is still queued would be ordered arbitrarily. Pausing lets the
/// consumer drain before the next step becomes ready. Under
/// `#[tokio::test(start_paused = true)]` the runtime auto-advances, so this
/// costs no wall-clock time.
const SETTLE: Duration = Duration::from_millis(10);

/// One step of a [`MockAgent`] script.
pub enum Step {
    Item(Result<StreamItem, StreamError>),
    /// Typically a publish to a request-scoped broker.
    Effect(EffectHook),
}

impl Step {
    pub fn item(item: Result<StreamItem, StreamError>) -> Self {
        Self::Item(item)
    }

    /// Passed the current call's `request_id`, so brokers keyed by request can
    /// be published to from inside the script.
    pub fn effect<F, Fut>(effect: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::Effect(Arc::new(move |request_id| Box::pin(effect(request_id))))
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
        Ok(StreamItem::StreamUserItem(StreamedUserContent::ToolResult(
            ToolResult {
                id: id.to_string(),
                call_id: None,
                result: result.to_string(),
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
}

impl MockAgent {
    /// Yields nothing and never ends.
    pub fn pending() -> Self {
        Self {
            on_stream_start: None,
            script: Script::Pending,
        }
    }

    /// Yields the given items, then ends the stream.
    pub fn yielding<I>(items: I) -> Self
    where
        I: IntoIterator<Item = Result<StreamItem, StreamError>>,
    {
        Self::scripted(items.into_iter().map(Step::Item).collect())
    }

    /// Runs the given steps in order, then ends the stream. The script is
    /// consumed by the first `stream`/`stream_with_timeout` call; a second call
    /// on the same agent yields an empty stream.
    pub fn scripted(steps: Vec<Step>) -> Self {
        Self {
            on_stream_start: None,
            script: Script::Steps(Mutex::new(Some(steps))),
        }
    }

    /// Awaited before either entry point produces its stream, and passed that
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
                                let item = match step {
                                    Step::Item(item) => Some(item),
                                    Step::Effect(effect) => {
                                        effect(request_id).await;
                                        None
                                    }
                                };
                                tokio::time::sleep(SETTLE).await;
                                item
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

    async fn stream(
        &self,
        _query: &str,
        _chat_history: Vec<Message>,
        _timeout: Option<Duration>,
        request_id: &str,
    ) -> AgentRun {
        let stream = self.start(request_id).await;
        AgentRun::new(stream, CancellationToken::new(), UsageState::new())
    }

    async fn cancel_and_close_mcp(&self, _request_id: &str, _reason: &str) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test]
    async fn a_pending_agent_never_yields() {
        let agent = MockAgent::pending();
        let mut stream = agent.stream("q", vec![], None, "req_1").await.into_events();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stream.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_start_hook_runs_before_the_stream() {
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let agent = MockAgent::pending().on_stream_start(move |request_id| {
            let flag = Arc::clone(&flag);
            async move {
                assert_eq!(request_id, "req_1");
                flag.store(true, Ordering::SeqCst);
            }
        });

        let _ = agent.stream("q", vec![], None, "req_1").await.into_events();

        assert!(ran.load(Ordering::SeqCst), "hook should run");
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
            .stream("q", vec![], None, "req_42")
            .await
            .into_events();
        let items: Vec<_> = stream.collect().await;

        assert_eq!(order.lock().expect("order lock").as_slice(), ["req_42"]);
        assert_eq!(items.len(), 1, "effects do not yield stream items");
    }

    #[tokio::test(start_paused = true)]
    async fn a_script_is_consumed_by_the_first_stream_call() {
        let agent = MockAgent::yielding([items::text("once")]);

        let first: Vec<_> = agent
            .stream("q", vec![], None, "req_1")
            .await
            .into_events()
            .collect()
            .await;
        let second: Vec<_> = agent
            .stream("q", vec![], None, "req_1")
            .await
            .into_events()
            .collect()
            .await;

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
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

        let mut stream = agent.stream("q", vec![], None, "req_1").await.into_events();
        while stream.next().await.is_some() {
            seen.fetch_add(1, Ordering::SeqCst);
        }

        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }
}
