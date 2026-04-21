//! Shared logging configuration for aura binaries
//!
//! This module provides a unified logging setup with three modes:
//! - Default: Application-level info logs only
//! - Verbose: Info logs with truncated tool execution details
//! - Debug: Full debug logging for all aura crates
//!
//! When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, all tracing spans are
//! automatically exported as OpenTelemetry spans via the OTLP exporter.
//!
//! ## Per-layer filtering
//!
//! The console (fmt) layer uses the same filter as before (controlled by
//! `--debug`/`--verbose` flags or `RUST_LOG`).  The OTel layer gets its own
//! permissive filter so that aura/rig spans always reach Phoenix/Jaeger even
//! when the console is in quiet default mode.
//!
//! ## Span hierarchy (streaming)
//!
//! `agent.stream` is created as a **root span** (`parent: None`) so that
//! Phoenix sees it as the trace root.  I/O attributes (`input.value`,
//! `output.value`, `user.id`, `session.id`, `metadata`) always live on this
//! span. Token counts live here in **single-agent mode only**; in
//! **orchestration mode** they live on the per-phase child spans
//! (`orchestration.planning`, `orchestration.worker`,
//! `orchestration.synthesis`, `orchestration.evaluation`) so Phoenix's
//! rollup shows the accurate aggregate without double-counting the parent.
//!
//! ### Single-agent mode
//!
//! ```text
//! agent.stream (AGENT, ROOT)        <- Phoenix root span, lives for full stream duration
//!   ├── user.id, session.id, metadata, input.value, output.value, tokens
//!   └── agent.turn (LLM)           <- from Rig fork (reuses agent.stream as parent)
//!       ├── execute_tool (TOOL)     <- from Rig (no error status — see below)
//!       │   └── mcp.tool_call (TOOL) <- from Aura, canonical tool span with error status
//!       └── execute_tool (TOOL)
//!           └── mcp.tool_call (TOOL)
//! ```
//!
//! ### Orchestration mode
//!
//! ```text
//! agent.stream (AGENT, ROOT)
//!   └── orchestration (CHAIN)                   <- full orchestration lifecycle
//!         ├── orchestration.planning (CHAIN)     <- coordinator routing/planning
//!         │   └── agent.turn (LLM) → ...
//!         └── orchestration.iteration (CHAIN)    <- per plan-execute-continue cycle
//!             └── orchestration.worker (AGENT)   <- per worker task
//!                 └── agent.turn (LLM) → execute_tool → mcp.tool_call
//! ```
//!
//! ```text
//! chat_completions (separate trace)  <- HTTP infrastructure
//!   └── streaming_completion         <- HTTP infrastructure
//! ```
//!
//! The `tokio::spawn` in `handlers.rs` is instrumented with `agent.stream`
//! so that `Span::current()` is active when rig's `send()` runs. Rig reuses
//! the caller's span instead of creating its own `invoke_agent` span,
//! keeping `agent.turn` as a direct child of `agent.stream`.
//!
//! For orchestration, the spawned task in `Orchestrator::stream()` is
//! instrumented with the `agent.stream` span so that all orchestration
//! child spans nest correctly under the trace root.
//!
//! Tool errors are only recorded on the `mcp.tool_call` child span (by
//! `mcp_tool_execution.rs`), not on Rig's `execute_tool` parent.  This is
//! intentional: `mcp.tool_call` is the canonical TOOL span for Phoenix.
//!
//! ## Content recording
//!
//! When `OTEL_RECORD_CONTENT=true`, prompt/completion text and tool
//! arguments/results are recorded as span attributes.  Truncated to
//! `OTEL_CONTENT_MAX_LENGTH` (default 1000) bytes (rounded down to a
//! UTF-8 character boundary) to avoid oversized spans.

#[cfg(feature = "otel")]
use opentelemetry::trace::TracerProvider as _;
#[cfg(feature = "otel")]
use opentelemetry_sdk::trace::TracerProvider;

#[cfg(feature = "otel")]
use std::sync::OnceLock;
use std::sync::{atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(feature = "otel")]
static TRACER_PROVIDER: OnceLock<TracerProvider> = OnceLock::new();

// ---------------------------------------------------------------------------
// OpenInference / gen_ai attribute key constants
// ---------------------------------------------------------------------------
// These are shared between the helper functions below and
// `openinference_exporter::transform_span`. Keep both in sync.

/// LLM provider identifier (e.g. "openai", "anthropic").
pub const ATTR_LLM_SYSTEM: &str = "llm.system";
/// Model name (e.g. "gpt-4o").
pub const ATTR_LLM_MODEL_NAME: &str = "llm.model_name";
/// Prompt / input token count.
pub const ATTR_LLM_TOKEN_PROMPT: &str = "llm.token_count.prompt";
/// Completion / output token count.
pub const ATTR_LLM_TOKEN_COMPLETION: &str = "llm.token_count.completion";
/// Total token count (prompt + completion).
pub const ATTR_LLM_TOKEN_TOTAL: &str = "llm.token_count.total";
/// Completion tokens spent generating tool call JSON (subset of completion).
pub const ATTR_LLM_TOKEN_TOOL_COMPLETION: &str = "llm.token_count.tool_completion";

pub const ATTR_INPUT_MIME_TYPE: &str = "input.mime_type";
pub const ATTR_INPUT_LENGTH: &str = "input.length";
pub const ATTR_INPUT_VALUE: &str = "input.value";
pub const ATTR_OUTPUT_MIME_TYPE: &str = "output.mime_type";
pub const ATTR_OUTPUT_LENGTH: &str = "output.length";
pub const ATTR_OUTPUT_VALUE: &str = "output.value";

// Tool-level attributes (used by `mcp_tool_execution.rs` and `openinference_exporter.rs`)
pub const ATTR_TOOL_NAME: &str = "tool.name";
pub const ATTR_TOOL_PARAMETERS: &str = "tool.parameters";
pub const ATTR_TOOL_PARAMETERS_COUNT: &str = "tool.parameters.count";
pub const ATTR_TOOL_RESULT: &str = "tool.result";
pub const ATTR_TOOL_RESULT_LENGTH: &str = "tool.result.length";
pub const ATTR_TOOL_CANCELLED: &str = "tool.cancelled";

// --- Content recording configuration ---

static RECORD_CONTENT: AtomicBool = AtomicBool::new(false);
static CONTENT_MAX_LENGTH: AtomicUsize = AtomicUsize::new(1000);

/// Whether prompt/completion content should be recorded as span attributes.
///
/// Controlled by `OTEL_RECORD_CONTENT` env var (default `false`).
pub fn should_record_content() -> bool {
    RECORD_CONTENT.load(Ordering::Relaxed)
}

/// Maximum byte length for content span attributes.
///
/// Controlled by `OTEL_CONTENT_MAX_LENGTH` env var (default `1000`).
/// Truncation respects UTF-8 character boundaries.
pub fn content_max_length() -> usize {
    CONTENT_MAX_LENGTH.load(Ordering::Relaxed)
}

/// Truncate a string for OTel span attributes, respecting `OTEL_CONTENT_MAX_LENGTH`.
pub fn truncate_for_otel(s: &str) -> String {
    let max = content_max_length();
    if s.len() <= max {
        return s.to_string();
    }
    let boundary = s.floor_char_boundary(max);
    format!("{}...", &s[..boundary])
}

/// Read content-recording env vars. Called once at the top of `init_logging`.
fn init_content_config() {
    RECORD_CONTENT.store(
        crate::env_flags::bool_env("OTEL_RECORD_CONTENT", false),
        Ordering::Relaxed,
    );
    if let Ok(val) = std::env::var("OTEL_CONTENT_MAX_LENGTH")
        && let Ok(n) = val.parse::<usize>()
    {
        CONTENT_MAX_LENGTH.store(n, Ordering::Relaxed);
    }
}

/// Custom formatter that truncates long log lines to prevent overwhelming output
struct TruncatingFormatter {
    max_length: usize,
}

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for TruncatingFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        use std::fmt::Write as FmtWrite;

        // Build the complete log message into a string buffer
        let mut buf = String::new();

        // Add timestamp
        let now = chrono::Local::now();
        write!(&mut buf, "{} ", now.format("%Y-%m-%dT%H:%M:%S%.3fZ"))?;

        // Add level
        let level = *event.metadata().level();
        write!(&mut buf, "{level:5} ")?;

        // Add target
        write!(&mut buf, "{}: ", event.metadata().target())?;

        // Format the fields to the buffer
        let mut field_writer = Writer::new(&mut buf);
        ctx.field_format()
            .format_fields(field_writer.by_ref(), event)?;

        // Check length and truncate if needed
        if buf.len() > self.max_length {
            writeln!(
                writer,
                "{}... ({} chars)",
                &buf[..self.max_length],
                buf.len()
            )?;
        } else {
            writeln!(writer, "{buf}")?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// OTel provider / layer / filter (only when feature = "otel")
// ---------------------------------------------------------------------------

/// Try to build an OpenTelemetry `TracerProvider` when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
///
/// Stores the provider in `TRACER_PROVIDER` for later shutdown and returns it.
/// Returns `None` when the env var is absent.
#[cfg(feature = "otel")]
fn init_otel_provider() -> Option<&'static TracerProvider> {
    // Presence check only — the OTLP exporter reads the endpoint value itself
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;

    let otlp_exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
    {
        Ok(exporter) => exporter,
        Err(e) => {
            eprintln!(
                "WARNING: OTEL_EXPORTER_OTLP_ENDPOINT is set ({endpoint}) but the OTLP \
                 exporter failed to initialize: {e}. Traces will NOT be exported."
            );
            return None;
        }
    };
    let exporter = crate::openinference_exporter::OpenInferenceExporter::new(otlp_exporter);

    let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "aura".to_string());

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(opentelemetry_sdk::Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", service_name),
        ]))
        .build();

    let _ = TRACER_PROVIDER.set(provider);
    TRACER_PROVIDER.get()
}

/// Build an `OpenTelemetryLayer` for a given subscriber type `S`.
///
/// Called per-branch so the layer's generic `S` parameter matches the
/// concrete subscriber stack in that branch.
#[cfg(feature = "otel")]
fn otel_layer<S>(
    provider: &TracerProvider,
) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let tracer = provider.tracer("aura");
    tracing_opentelemetry::layer().with_tracer(tracer)
}

/// Build the permissive OTel filter.
///
/// Captures all aura + rig spans for Phoenix/Jaeger regardless of console verbosity.
/// Override with `OTEL_LOG_LEVEL` env var.
#[cfg(feature = "otel")]
fn otel_filter(binary_name: &str) -> EnvFilter {
    EnvFilter::try_from_env("OTEL_LOG_LEVEL").unwrap_or_else(|_| {
        format!(
            "warn,aura=trace,aura_config=info,{binary_name}=info,rig::agent::prompt_request=info,rig::completions=info"
        )
        .into()
    })
}

/// Initialize logging based on debug and verbose flags
///
/// # Arguments
/// * `debug` - Enable debug-level logging for all aura crates
/// * `verbose` - Enable info-level logging with filtered output
/// * `binary_name` - Name of the binary for targeted logging (e.g., "aura_web_server")
pub fn init_logging(debug: bool, verbose: bool, binary_name: &str) {
    // Read content-recording config once
    init_content_config();

    // Initialise OTel provider once; each branch builds its own typed layer from it
    #[cfg(feature = "otel")]
    let provider = init_otel_provider();

    if debug {
        // Console filter: debug for aura crates
        let console_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            format!(
                "warn,aura_config=debug,aura=debug,{binary_name}=info,rig::agent::prompt_request=info,rig::providers::openai=debug"
            )
            .into()
        });

        let registry =
            tracing_subscriber::registry().with(fmt::layer().with_filter(console_filter));
        #[cfg(feature = "otel")]
        let registry =
            registry.with(provider.map(|p| otel_layer(p).with_filter(otel_filter(binary_name))));
        registry.init();
    } else if verbose {
        // Console filter: info for aura crates
        let console_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            format!(
                "warn,aura_config=info,aura=info,{binary_name}=info,rig::agent::prompt_request=info,rig::providers::openai=info"
            )
            .into()
        });

        // Create a custom formatting layer that truncates very long lines (e.g., API payloads)
        // Block execute_tool spans (and their events) from rig to avoid duplication
        // Our aura::mcp_dynamic logs provide better tool execution visibility with truncation
        let fmt_layer = fmt::layer()
            .event_format(TruncatingFormatter { max_length: 500 })
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                // Block execute_tool spans from rig::agent::prompt_request to prevent duplicate logs
                // Our aura::mcp_dynamic provides tool execution logs with proper truncation
                // This also blocks events within the execute_tool span (like "executed tool X with args Y")
                if metadata.target().starts_with("rig::agent::prompt_request")
                    && metadata.is_span()
                    && metadata.name() == "execute_tool"
                {
                    return false;
                }
                true
            }))
            .with_filter(console_filter);

        let registry = tracing_subscriber::registry().with(fmt_layer);
        #[cfg(feature = "otel")]
        let registry =
            registry.with(provider.map(|p| otel_layer(p).with_filter(otel_filter(binary_name))));
        registry.init();
    } else {
        // Default: Only binary-specific info level logging on console
        let console_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| format!("{binary_name}=info").into());

        let registry =
            tracing_subscriber::registry().with(fmt::layer().with_filter(console_filter));
        #[cfg(feature = "otel")]
        let registry =
            registry.with(provider.map(|p| otel_layer(p).with_filter(otel_filter(binary_name))));
        registry.init();
    }
}

// ---------------------------------------------------------------------------
// OTel span attribute helpers — public wrappers with no-op fallbacks
// ---------------------------------------------------------------------------

/// Set an OTel attribute on a span. No-op when the `otel` feature is disabled.
#[cfg(feature = "otel")]
pub fn set_span_attribute(
    span: &tracing::Span,
    key: impl Into<opentelemetry::Key>,
    value: impl Into<opentelemetry::Value>,
) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_attribute(key, value);
}

/// Set an OTel attribute on a span. No-op when the `otel` feature is disabled.
#[cfg(not(feature = "otel"))]
pub fn set_span_attribute<V>(span: &tracing::Span, _key: &str, _value: V) {
    let _ = span;
}

/// Mark the span status as OK. No-op when the `otel` feature is disabled.
#[cfg(feature = "otel")]
pub fn set_span_ok(span: &tracing::Span) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_status(opentelemetry::trace::Status::Ok);
}

/// Mark the span status as OK. No-op when the `otel` feature is disabled.
#[cfg(not(feature = "otel"))]
pub fn set_span_ok(span: &tracing::Span) {
    let _ = span;
}

/// Mark the span status as error with a message. No-op when the `otel` feature is disabled.
#[cfg(feature = "otel")]
pub fn set_span_error(span: &tracing::Span, msg: impl Into<String>) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_status(opentelemetry::trace::Status::error(msg.into()));
}

/// Mark the span status as error with a message. No-op when the `otel` feature is disabled.
#[cfg(not(feature = "otel"))]
pub fn set_span_error(span: &tracing::Span, _msg: impl Into<String>) {
    let _ = span;
}

// ---------------------------------------------------------------------------
// Higher-level OTel helpers (dual-impl)
// ---------------------------------------------------------------------------

/// Record LLM provider and model identifiers on a span.
#[cfg(feature = "otel")]
pub fn set_llm_identifiers(span: &tracing::Span, provider: &str, model: &str) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_attribute(ATTR_LLM_SYSTEM, provider.to_string());
    span.set_attribute(ATTR_LLM_MODEL_NAME, model.to_string());
}

#[cfg(not(feature = "otel"))]
pub fn set_llm_identifiers(span: &tracing::Span, _provider: &str, _model: &str) {
    let _ = span;
}

/// Record token usage counters on a span.
///
/// `tool_completion` is the subset of `completion` spent generating tool call JSON.
/// Only recorded when non-zero to avoid noise on single-turn (no-tool) interactions.
#[cfg(feature = "otel")]
pub fn set_token_usage(
    span: &tracing::Span,
    prompt: u64,
    completion: u64,
    total: u64,
    tool_completion: u64,
) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_attribute(ATTR_LLM_TOKEN_PROMPT, prompt as i64);
    span.set_attribute(ATTR_LLM_TOKEN_COMPLETION, completion as i64);
    span.set_attribute(ATTR_LLM_TOKEN_TOTAL, total as i64);
    if tool_completion > 0 {
        span.set_attribute(ATTR_LLM_TOKEN_TOOL_COMPLETION, tool_completion as i64);
    }
}

#[cfg(not(feature = "otel"))]
pub fn set_token_usage(
    span: &tracing::Span,
    _prompt: u64,
    _completion: u64,
    _total: u64,
    _tool_completion: u64,
) {
    let _ = span;
}

/// Record input text attributes on a span (length, mime type, and optionally content).
#[cfg(feature = "otel")]
pub fn set_input_attributes(span: &tracing::Span, text: &str) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_attribute(ATTR_INPUT_MIME_TYPE, "text/plain");
    span.set_attribute(ATTR_INPUT_LENGTH, text.len() as i64);
    if should_record_content() {
        span.set_attribute(ATTR_INPUT_VALUE, truncate_for_otel(text));
    }
}

#[cfg(not(feature = "otel"))]
pub fn set_input_attributes(span: &tracing::Span, _text: &str) {
    let _ = span;
}

/// Record output text attributes on a span (length, mime type, and optionally content).
#[cfg(feature = "otel")]
pub fn set_output_attributes(span: &tracing::Span, text: &str) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_attribute(ATTR_OUTPUT_MIME_TYPE, "text/plain");
    span.set_attribute(ATTR_OUTPUT_LENGTH, text.len() as i64);
    if should_record_content() {
        span.set_attribute(ATTR_OUTPUT_VALUE, truncate_for_otel(text));
    }
}

#[cfg(not(feature = "otel"))]
pub fn set_output_attributes(span: &tracing::Span, _text: &str) {
    let _ = span;
}

/// Force-flush all pending spans to the OTLP exporter.
///
/// The `BatchSpanProcessor` buffers spans and exports on a timer (default 5 s)
/// or when the batch fills.  In practice the timer only fires when the worker
/// task is polled, which may not happen reliably between requests.  Call this
/// at the end of each request to guarantee spans are exported promptly.
///
/// No-op when OTel was not initialised or the `otel` feature is disabled.
#[cfg(feature = "otel")]
pub fn flush_tracer() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        for result in provider.force_flush() {
            if let Err(e) = result {
                tracing::warn!("OpenTelemetry force_flush error: {e}");
            }
        }
    }
}

#[cfg(not(feature = "otel"))]
pub fn flush_tracer() {}

/// Flush and shut down the OpenTelemetry tracer provider.
///
/// No-op when OTel was not initialised (i.e. `OTEL_EXPORTER_OTLP_ENDPOINT` was not set)
/// or the `otel` feature is disabled.
/// Call this before process exit to ensure all pending spans are exported.
///
/// Uses `spawn_blocking` so the tokio runtime stays alive while
/// `TracerProvider::shutdown()` blocks waiting for the `BatchSpanProcessor`
/// background task to flush. This avoids the deadlock that occurs on
/// single-threaded runtimes (e.g. actix-web's `current_thread`) when the
/// calling thread blocks synchronously — preventing the runtime from polling
/// the batch processor task it's waiting on.
#[cfg(feature = "otel")]
pub async fn shutdown_tracer() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        let provider = provider.clone();
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                if let Err(e) = provider.shutdown() {
                    eprintln!("OpenTelemetry shutdown error: {e}");
                }
            }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("OpenTelemetry shutdown task panicked: {e}"),
            Err(_) => eprintln!("OpenTelemetry shutdown timed out after 5s"),
        }
    }
}

#[cfg(not(feature = "otel"))]
pub async fn shutdown_tracer() {}
