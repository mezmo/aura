//! In-memory OTel span capture for tests that assert on exported span
//! attributes. Shared by the `decision_id_span` suites (`hitl::gate`,
//! `hitl::tool`), the `applied_headers_span` suite (`mcp_tool_execution`),
//! and, under the `test-support` feature, the web server's parity test.
//!
//! Gated on `otel`, and on `test` or `test-support`. Without `otel` there
//! is no span data to assert against, and nothing outside tests needs a span
//! sink.

use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::BoxFuture;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
use opentelemetry_sdk::trace::TracerProvider;
use tracing_subscriber::layer::SubscriberExt;

/// Spans the test subscriber has exported.
#[derive(Debug, Clone, Default)]
pub struct CapturedSpans(Arc<Mutex<Vec<SpanData>>>);

impl CapturedSpans {
    /// Sets a subscriber that exports to these captured spans as the current
    /// thread's default, until the returned guard drops. Every span that
    /// closes in the meantime is exported here.
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        let provider = TracerProvider::builder()
            .with_simple_exporter(self.clone())
            .build();
        tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test"))),
        )
    }

    pub fn spans(&self) -> MutexGuard<'_, Vec<SpanData>> {
        self.0.lock().expect("captured spans mutex")
    }

    pub fn contains(&self, name: &str) -> bool {
        self.spans().iter().any(|span| span.name == name)
    }

    /// The single `key` attribute on the span named `name`. Panics if
    /// the span carries more than one — a double-stamp regression would
    /// otherwise pass with only the first entry.
    pub fn attribute(&self, name: &str, key: &str) -> Option<String> {
        let spans = self.spans();
        let span = spans.iter().find(|span| span.name == name)?;
        let matches: Vec<_> = span
            .attributes
            .iter()
            .filter(|kv| kv.key.as_str() == key)
            .collect();
        assert!(
            matches.len() <= 1,
            "span {name:?} must carry at most one {key} attribute, found {}: \
             a regression is double-stamping the same span",
            matches.len(),
        );
        matches.first().map(|kv| kv.value.to_string())
    }
}

impl SpanExporter for CapturedSpans {
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
        self.spans().extend(batch);
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Run `body` inside an `execute_tool` span (the span Rig opens around a tool call) under a subscriber that exports to memory, returning the body's output and the `decision_id` the exported span carries.
#[cfg(test)]
pub(crate) async fn traced_as_execute_tool<T>(
    body: impl std::future::Future<Output = T>,
) -> (T, Option<String>) {
    use tracing::Instrument;

    let captured = CapturedSpans::default();
    let _guard = captured.install();

    let output = body.instrument(tracing::info_span!("execute_tool")).await;

    // The registry instruments a parked approval's wake task with the
    // same span, so the export lands once that task has released it too.
    for _ in 0..1_000 {
        if captured.contains("execute_tool") {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        captured.contains("execute_tool"),
        "the execute_tool span was never exported",
    );

    (
        output,
        captured.attribute("execute_tool", crate::logging::ATTR_DECISION_ID),
    )
}
