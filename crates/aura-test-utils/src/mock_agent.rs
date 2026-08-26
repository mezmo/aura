//! A [`StreamingAgent`] for tests that need an agent without a provider.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura::{Message, StreamError, StreamItem, StreamingAgent, UsageState};
use futures::stream::{self, BoxStream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

type StartHook = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Clone)]
enum Script {
    Pending,
    Items(Vec<StreamItem>),
}

pub struct MockAgent {
    script: Script,
    on_stream_start: Option<StartHook>,
}

impl MockAgent {
    /// Yields nothing and never ends.
    pub fn pending() -> Self {
        Self::with_script(Script::Pending)
    }

    /// Yields `items` in order, then ends.
    pub fn scripted(items: impl IntoIterator<Item = StreamItem>) -> Self {
        Self::with_script(Script::Items(items.into_iter().collect()))
    }

    fn with_script(script: Script) -> Self {
        Self {
            script,
            on_stream_start: None,
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

        match &self.script {
            Script::Pending => Box::pin(stream::pending()) as BoxStream<'_, _>,
            Script::Items(items) => Box::pin(stream::iter(
                items.clone().into_iter().map(Ok::<_, StreamError>),
            )),
        }
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
        _cancel_token: CancellationToken,
        request_id: &str,
    ) -> Result<BoxStream<'static, Result<StreamItem, StreamError>>, StreamError> {
        Ok(self.start(request_id).await)
    }

    async fn stream_with_timeout(
        &self,
        _query: &str,
        _chat_history: Vec<Message>,
        _timeout: Duration,
        request_id: &str,
    ) -> (
        BoxStream<'static, Result<StreamItem, StreamError>>,
        watch::Sender<bool>,
        UsageState,
    ) {
        let stream = self.start(request_id).await;
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        (stream, cancel_tx, UsageState::new())
    }

    async fn cancel_and_close_mcp(&self, _request_id: &str, _reason: &str) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura::StreamedAssistantContent;
    use futures::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn text(s: &str) -> StreamItem {
        StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(s.to_string()))
    }

    async fn collect_text(agent: &MockAgent) -> Vec<String> {
        let stream = agent
            .stream("q", vec![], CancellationToken::new(), "req_1")
            .await
            .expect("mock stream should start");
        stream
            .filter_map(|item| async move {
                match item {
                    Ok(StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                        Some(t)
                    }
                    _ => None,
                }
            })
            .collect()
            .await
    }

    #[tokio::test]
    async fn scripted_items_replay_in_order_then_end() {
        let agent = MockAgent::scripted([text("a"), text("b")]);
        assert_eq!(collect_text(&agent).await, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn the_script_replays_on_every_stream() {
        let agent = MockAgent::scripted([text("a")]);
        assert_eq!(collect_text(&agent).await, vec!["a"]);
        assert_eq!(collect_text(&agent).await, vec!["a"]);
    }

    #[tokio::test]
    async fn a_pending_agent_never_yields() {
        let agent = MockAgent::pending();
        let mut stream = agent
            .stream("q", vec![], CancellationToken::new(), "req_1")
            .await
            .expect("mock stream should start");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stream.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_start_hook_runs_before_both_entry_points_stream() {
        for entry_point in ["stream", "stream_with_timeout"] {
            let ran = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&ran);
            let agent = MockAgent::pending().on_stream_start(move |request_id| {
                let flag = Arc::clone(&flag);
                async move {
                    assert_eq!(request_id, "req_1");
                    flag.store(true, Ordering::SeqCst);
                }
            });

            if entry_point == "stream" {
                let _ = agent
                    .stream("q", vec![], CancellationToken::new(), "req_1")
                    .await
                    .expect("mock stream should start");
            } else {
                let _ = agent
                    .stream_with_timeout("q", vec![], Duration::from_secs(1), "req_1")
                    .await;
            }

            assert!(
                ran.load(Ordering::SeqCst),
                "hook should run for {entry_point}"
            );
        }
    }
}
