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
//! `OTEL_EXPORTER_OTLP_PROTOCOL` selects the wire protocol: `grpc` (the
//! default) or `http/protobuf`.  Either way an `https://` endpoint is served
//! with the platform's native root certificates and `http://` connects in
//! plaintext.
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
//! span. Token counts live on the `agent.turn` spans only: the Rig fork
//! records per-call model identifiers and usage there, Phoenix prices those
//! LLM-kind spans (`openinference_exporter::infer_span_kind`), and every
//! aggregate — per worker, per phase, per trace — is Phoenix's rollup of
//! the turns. No Aura-owned span records token counts, so nothing
//! double-counts.
//!
//! ### Single-agent mode
//!
//! ```text
//! agent.stream (LLM, ROOT)          <- Phoenix root span, lives for full stream duration
//!   ├── user.id, session.id, metadata, input.value, output.value
//!   └── agent.turn (LLM)           <- from Rig fork; carries model + per-call tokens
//!       ├── execute_tool (TOOL)     <- from Rig (no error status — see below)
//!       │   └── mcp.tool_call (TOOL) <- from Aura, canonical tool span with error status
//!       └── execute_tool (TOOL)
//!           └── mcp.tool_call (TOOL)
//! ```
//!
//! ### Orchestration mode
//!
//! ```text
//! agent.stream (LLM, ROOT)
//!   └── orchestration (CHAIN)                   <- full orchestration lifecycle
//!         ├── orchestration.planning (LLM)         <- coordinator routing/planning
//!         │   └── agent.turn (LLM) → ...
//!         └── orchestration.iteration (CHAIN)    <- per plan-execute-continue cycle
//!             └── orchestration.worker (LLM)       <- per worker task
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
//! The `aura` standalone backend
//! (`crates/aura-cli/src/backend/direct.rs`) wraps its
//! `execute_completion` spawn with the same `agent.stream` instrumentation,
//! so traces emitted from `aura --standalone` produce the same shape as
//! the web server's. The CLI does not emit the `chat_completions` /
//! `streaming_completion` HTTP-infrastructure spans because it has no HTTP
//! layer; those spans live on a separate trace in the server.
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
/// LLM hosting provider (e.g. "openai", "bedrock").
pub const ATTR_LLM_PROVIDER: &str = "llm.provider";
/// Model name (e.g. "gpt-4o").
pub const ATTR_LLM_MODEL_NAME: &str = "llm.model_name";
/// Prompt / input token count.
pub const ATTR_LLM_TOKEN_PROMPT: &str = "llm.token_count.prompt";
/// Completion / output token count.
pub const ATTR_LLM_TOKEN_COMPLETION: &str = "llm.token_count.completion";
/// LLM call parameters (temperature, max_tokens, …) as a JSON object string.
pub const ATTR_LLM_INVOCATION_PARAMETERS: &str = "llm.invocation_parameters";
/// End-user identifier from the request.
pub const ATTR_USER_ID: &str = "user.id";
/// Caller-supplied request metadata as a JSON object string.
pub const ATTR_METADATA: &str = "metadata";
/// Aura release that produced the trace.
pub const ATTR_AURA_VERSION: &str = "aura.version";
/// Aura workspace version.
pub const AURA_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Serving path: "orchestration" or "single-agent".
pub const ATTR_AURA_MODE: &str = "aura.mode";
/// Prompt template with placeholders, before variable substitution.
pub const ATTR_LLM_PROMPT_TEMPLATE: &str = "llm.prompt_template.template";
/// Prompt template variables as a JSON object string.
pub const ATTR_LLM_PROMPT_TEMPLATE_VARIABLES: &str = "llm.prompt_template.variables";
/// Assembled system prompt sent to the provider (preamble + skill catalog,
/// etc.), as an OTel GenAI array of typed parts.
pub const ATTR_GEN_AI_SYSTEM_INSTRUCTIONS: &str = "gen_ai.system_instructions";

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

// HITL attributes (used by `hitl::route` and `mcp_tool_execution`)

/// Handle of the human approval decision gating a tool call — the same
/// `decision_id` the approval payload and lifecycle events carry.
pub const ATTR_DECISION_ID: &str = "decision_id";

/// Comma-separated, sorted outbound header NAMES an approver override
/// applied to a gated MCP call — never their values.
pub const ATTR_APPLIED_HEADERS: &str = "applied_headers";

// --- Content recording configuration ---

static RECORD_CONTENT: AtomicBool = AtomicBool::new(false);
static CONTENT_MAX_LENGTH: AtomicUsize = AtomicUsize::new(1000);

/// Whether prompt/completion content should be recorded as span attributes.
///
/// Controlled by `OTEL_RECORD_CONTENT` env var (default `false`).
pub fn should_record_content() -> bool {
    RECORD_CONTENT.load(Ordering::Relaxed)
}

/// Force content recording on/off for a test, returning the previous value
/// so the test can restore it.
#[cfg(test)]
pub(crate) fn set_record_content_for_tests(v: bool) -> bool {
    RECORD_CONTENT.swap(v, Ordering::Relaxed)
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
pub fn init_content_config() {
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

/// Ensure aura_config warnings are always visible regardless of RUST_LOG setting.
///
/// This is important for operational warnings like duplicate skill detection
/// that should never be silently filtered.
fn ensure_aura_config_warnings(filter: EnvFilter) -> EnvFilter {
    filter.add_directive("aura_config=warn".parse().unwrap())
}

// ---------------------------------------------------------------------------
// OTel provider / layer / filter (only when feature = "otel")
// ---------------------------------------------------------------------------

/// Whether an OTLP endpoint URL selects TLS.
///
/// Scheme comparison is case-insensitive because URI schemes are.
#[cfg(feature = "otel")]
fn endpoint_uses_tls(endpoint: &str) -> bool {
    endpoint.trim().to_ascii_lowercase().starts_with("https://")
}

/// Wire protocol carrying spans to the collector.
#[cfg(feature = "otel")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OtlpProtocol {
    Grpc,
    HttpProtobuf,
}

/// Parse an OTLP protocol from its spec spelling.
///
/// `http/json` is deliberately unrecognised: the exporter is built without
/// the `http-json` feature, so accepting the name would promise a transport
/// that cannot be constructed.
#[cfg(feature = "otel")]
fn parse_otlp_protocol(value: &str) -> Option<OtlpProtocol> {
    match value.trim().to_ascii_lowercase().as_str() {
        "grpc" => Some(OtlpProtocol::Grpc),
        "http/protobuf" => Some(OtlpProtocol::HttpProtobuf),
        _ => None,
    }
}

/// Resolve the export protocol from the environment, defaulting to gRPC.
///
/// `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` wins over the generic
/// `OTEL_EXPORTER_OTLP_PROTOCOL`. An unrecognised value warns and falls back
/// rather than dropping traces silently.
#[cfg(feature = "otel")]
fn resolve_otlp_protocol() -> OtlpProtocol {
    let configured = std::env::var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL"))
        .ok();

    match configured {
        None => OtlpProtocol::Grpc,
        Some(value) => parse_otlp_protocol(&value).unwrap_or_else(|| {
            eprintln!(
                "WARNING: unsupported OTLP protocol '{value}' — falling back to grpc. \
                 Supported values: grpc, http/protobuf."
            );
            OtlpProtocol::Grpc
        }),
    }
}

/// Build the gRPC span exporter.
///
/// An `https://` endpoint is given a `ClientTlsConfig` using the platform's
/// native root store; tonic refuses to connect to an `https://` URI when no
/// TLS config was attached, so this must be set before `build()`.
#[cfg(feature = "otel")]
fn build_grpc_span_exporter(
    endpoint: &str,
) -> Result<opentelemetry_otlp::SpanExporter, opentelemetry::trace::TraceError> {
    use opentelemetry_otlp::WithTonicConfig;

    let mut builder = opentelemetry_otlp::SpanExporter::builder().with_tonic();
    if endpoint_uses_tls(endpoint) {
        builder =
            builder.with_tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots());
    }
    builder.build()
}

/// Build the HTTP/protobuf span exporter.
///
/// TLS needs no configuration here — the reqwest client the exporter
/// defaults to carries its own rustls stack and root certificates. The
/// exporter appends `/v1/traces` to a generic `OTEL_EXPORTER_OTLP_ENDPOINT`
/// and uses `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` verbatim.
#[cfg(feature = "otel")]
fn build_http_span_exporter()
-> Result<opentelemetry_otlp::SpanExporter, opentelemetry::trace::TraceError> {
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
}

/// Try to build an OpenTelemetry `TracerProvider` when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
///
/// Stores the provider in `TRACER_PROVIDER` for later shutdown and returns it.
/// Returns `None` when the env var is absent.
///
/// The signal-specific `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` takes precedence
/// when deciding whether the gRPC transport needs TLS, matching the
/// precedence the exporter itself applies when it resolves the endpoint.
///
/// Public so binaries that compose their own subscriber stack (e.g.
/// `aura` in standalone mode) can register the provider before attaching
/// an `OpenTelemetryLayer`. Subsequent calls reuse the cached provider.
#[cfg(feature = "otel")]
pub fn init_otel_provider() -> Option<&'static TracerProvider> {
    // Presence check only — the OTLP exporter reads the endpoint value itself
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;

    let signal_endpoint = std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok();
    let effective_endpoint = signal_endpoint.as_deref().unwrap_or(&endpoint);

    let protocol = resolve_otlp_protocol();
    let build_result = match protocol {
        OtlpProtocol::Grpc => build_grpc_span_exporter(effective_endpoint),
        OtlpProtocol::HttpProtobuf => build_http_span_exporter(),
    };

    let otlp_exporter = match build_result {
        Ok(exporter) => exporter,
        Err(e) => {
            eprintln!(
                "WARNING: OTEL_EXPORTER_OTLP_ENDPOINT is set ({endpoint}) but the OTLP \
                 {protocol:?} exporter failed to initialize: {e}. Traces will NOT be exported."
            );
            return None;
        }
    };
    let exporter = crate::openinference_exporter::OpenInferenceExporter::new(otlp_exporter);

    let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "aura".to_string());
    // Phoenix routes traces to a project solely by the
    // `openinference.project.name` resource attribute (its OTLP receiver
    // ignores `service.name`); without it, traces land in Phoenix's
    // default project.
    let project_name =
        std::env::var("PHOENIX_PROJECT_NAME").unwrap_or_else(|_| service_name.clone());

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(opentelemetry_sdk::Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", service_name),
            opentelemetry::KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            opentelemetry::KeyValue::new("openinference.project.name", project_name),
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
pub fn otel_layer<S>(
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
pub fn otel_filter(binary_name: &str) -> EnvFilter {
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

    // Read attribute value length limit from env
    #[cfg(feature = "otel")]
    crate::openinference_exporter::init_attribute_value_length_limit();

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
        let console_filter = ensure_aura_config_warnings(console_filter);

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

/// Mark the span status as error with a message, and attach an OTel
/// `exception` span event carrying `exception.message` — the shape Phoenix
/// renders on failed spans. No-op when the `otel` feature is disabled.
#[cfg(feature = "otel")]
pub fn set_span_error(span: &tracing::Span, msg: impl Into<String>) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let msg = msg.into();
    // The tracing-opentelemetry layer rewrites an `error`-only event (no
    // message field) into a span event named `exception`. Fired before
    // `set_status` so the clean message below overrides the Debug-formatted
    // status the layer derives from the event.
    {
        let _guard = span.enter();
        tracing::error!(error = msg.as_str());
    }
    span.set_status(opentelemetry::trace::Status::error(msg));
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
///
/// Sets both `llm.system` and `llm.provider` to the same provider string:
/// Phoenix cost tracking matches `llm.provider` + `llm.model_name` against
/// its pricing table, while `llm.system` is what its UI displays.
#[cfg(feature = "otel")]
pub fn set_llm_identifiers(span: &tracing::Span, provider: &str, model: &str) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_attribute(ATTR_LLM_SYSTEM, provider.to_string());
    span.set_attribute(ATTR_LLM_PROVIDER, provider.to_string());
    span.set_attribute(ATTR_LLM_MODEL_NAME, model.to_string());
}

#[cfg(not(feature = "otel"))]
pub fn set_llm_identifiers(span: &tracing::Span, _provider: &str, _model: &str) {
    let _ = span;
}

/// Build the `llm.invocation_parameters` JSON object string for an LLM config
/// (temperature, max_tokens, and any additional params). Returns `None` when
/// the config sets no call parameters.
pub fn llm_invocation_parameters(llm: &aura_config::LlmConfig) -> Option<String> {
    let mut map = serde_json::Map::new();
    if let Some(t) = llm.temperature() {
        map.insert("temperature".into(), t.into());
    }
    if let Some(m) = llm.max_tokens() {
        map.insert("max_tokens".into(), m.into());
    }
    if let Some(serde_json::Value::Object(extra)) = llm.additional_params() {
        map.extend(extra);
    }
    if map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(map).to_string())
    }
}

/// Record LLM invocation parameters (a JSON object string) on a span.
#[cfg(feature = "otel")]
pub fn set_llm_invocation_parameters(span: &tracing::Span, params_json: &str) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_attribute(ATTR_LLM_INVOCATION_PARAMETERS, params_json.to_string());
}

#[cfg(not(feature = "otel"))]
pub fn set_llm_invocation_parameters(span: &tracing::Span, _params_json: &str) {
    let _ = span;
}

/// Record the prompt template and its variables on a span.
///
/// The template (static text with `%%VAR%%` placeholders, no request
/// content) is always recorded; the variables carry request content, so
/// they follow the content-recording rules (`OTEL_RECORD_CONTENT` gate
/// plus truncation), mirroring `input.value`.
#[cfg(feature = "otel")]
pub fn set_llm_prompt_template(span: &tracing::Span, template: &str, variables: &[(&str, &str)]) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.set_attribute(ATTR_LLM_PROMPT_TEMPLATE, template.to_string());
    if should_record_content() {
        let map: serde_json::Map<String, serde_json::Value> = variables
            .iter()
            .map(|(k, v)| ((*k).to_string(), truncate_for_otel(v).into()))
            .collect();
        span.set_attribute(
            ATTR_LLM_PROMPT_TEMPLATE_VARIABLES,
            serde_json::Value::Object(map).to_string(),
        );
    }
}

#[cfg(not(feature = "otel"))]
pub fn set_llm_prompt_template(span: &tracing::Span, _template: &str, _variables: &[(&str, &str)]) {
    let _ = span;
}

/// Record the tool schemas advertised to the model as OpenInference
/// `llm.tools.{i}.tool.json_schema` attributes.
#[cfg(feature = "otel")]
pub fn set_llm_tools(span: &tracing::Span, schemas: &[String]) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    for (i, schema) in schemas.iter().enumerate() {
        span.set_attribute(format!("llm.tools.{i}.tool.json_schema"), schema.clone());
    }
}

#[cfg(not(feature = "otel"))]
pub fn set_llm_tools(span: &tracing::Span, _schemas: &[String]) {
    let _ = span;
}

/// A retrieved document for [`set_retrieval_documents`].
pub struct RetrievedDocument<'a> {
    pub content: &'a str,
    pub score: f64,
    pub metadata: Option<&'a serde_json::Value>,
}

/// Record retrieved documents as OpenInference `retrieval.documents.{i}.*`
/// attributes. Scores are always recorded; content and metadata only when
/// content recording is enabled (`OTEL_RECORD_CONTENT`).
#[cfg(feature = "otel")]
pub fn set_retrieval_documents(span: &tracing::Span, docs: &[RetrievedDocument<'_>]) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    for (i, doc) in docs.iter().enumerate() {
        span.set_attribute(format!("retrieval.documents.{i}.document.score"), doc.score);
        if should_record_content() {
            span.set_attribute(
                format!("retrieval.documents.{i}.document.content"),
                truncate_for_otel(doc.content),
            );
            if let Some(meta) = doc.metadata {
                span.set_attribute(
                    format!("retrieval.documents.{i}.document.metadata"),
                    meta.to_string(),
                );
            }
        }
    }
}

#[cfg(not(feature = "otel"))]
pub fn set_retrieval_documents(span: &tracing::Span, _docs: &[RetrievedDocument<'_>]) {
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

/// Serialize a system prompt as a single-part OTel GenAI system-instructions
/// array.
///
/// Truncation applies to the prompt text *before* serialization: truncating
/// the finished JSON would cut mid-string and leave a value the exporter
/// cannot parse back into a message.
#[cfg(any(feature = "otel", test))]
fn system_instructions_json(text: &str) -> String {
    serde_json::json!([{ "type": "text", "content": truncate_for_otel(text) }]).to_string()
}

/// Record the assembled system prompt on a span as an OTel GenAI
/// system-instructions parts array, subject to the same content gate and
/// truncation as every other captured content attribute.
#[cfg(feature = "otel")]
pub fn set_system_prompt_attribute(span: &tracing::Span, text: &str) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    if should_record_content() {
        span.set_attribute(
            ATTR_GEN_AI_SYSTEM_INSTRUCTIONS,
            system_instructions_json(text),
        );
    }
}

#[cfg(not(feature = "otel"))]
pub fn set_system_prompt_attribute(span: &tracing::Span, _text: &str) {
    let _ = span;
}

/// Force-flush all pending spans to the OTLP exporter.
///
/// The `BatchSpanProcessor` buffers spans and exports on a timer (default 5 s)
/// or when the batch fills.  In practice the timer only fires when the worker
/// task is polled, which may not happen reliably between requests.  Call this
/// at the end of each request to guarantee spans are exported promptly.
///
/// Uses `spawn_blocking` so the tokio runtime stays alive while
/// `TracerProvider::force_flush()` blocks waiting for the `BatchSpanProcessor`
/// background task. Calling `force_flush()` directly from a tokio worker
/// thread deadlocks the batch processor (which itself runs on the runtime),
/// permanently stalling subsequent exports.
///
/// No-op when OTel was not initialised or the `otel` feature is disabled.
#[cfg(feature = "otel")]
pub async fn flush_tracer() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        let provider = provider.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for result in provider.force_flush() {
                if let Err(e) = result {
                    eprintln!("OpenTelemetry force_flush error: {e}");
                }
            }
        })
        .await;
    }
}

#[cfg(not(feature = "otel"))]
pub async fn flush_tracer() {}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A prompt longer than `OTEL_CONTENT_MAX_LENGTH` must still serialize to
    /// valid JSON — truncating the finished JSON instead of the content would
    /// cut mid-string and leave the exporter unable to parse it back.
    #[test]
    fn system_instructions_json_stays_valid_when_truncated() {
        let long_prompt = "x".repeat(content_max_length() * 2);
        let json = system_instructions_json(&long_prompt);

        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("truncated system instructions must be valid JSON");

        let parts = parsed.as_array().expect("expected a parts array");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");

        let content = parts[0]["content"].as_str().unwrap();
        assert!(
            content.len() < long_prompt.len(),
            "content was not truncated"
        );
        assert!(content.starts_with("xxx"));
    }

    /// Content shorter than the limit round-trips unchanged.
    #[test]
    fn system_instructions_json_preserves_short_content() {
        let json = system_instructions_json("You are a helpful assistant.");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["content"], "You are a helpful assistant.");
    }

    #[test]
    fn llm_invocation_parameters_serializes_call_params() {
        let llm: aura_config::LlmConfig = serde_json::from_value(serde_json::json!({
            "provider": "openai",
            "api_key": "k",
            "model": "gpt-4o",
            "temperature": 0.2,
            "max_tokens": 512,
        }))
        .unwrap();
        let params = llm_invocation_parameters(&llm).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&params).unwrap();
        assert_eq!(parsed["temperature"], 0.2);
        assert_eq!(parsed["max_tokens"], 512);
    }

    #[test]
    fn llm_invocation_parameters_none_when_unset() {
        let llm: aura_config::LlmConfig = serde_json::from_value(serde_json::json!({
            "provider": "openai",
            "api_key": "k",
            "model": "gpt-4o",
        }))
        .unwrap();
        assert!(llm_invocation_parameters(&llm).is_none());
    }

    #[cfg(feature = "otel")]
    #[test]
    fn tls_selected_only_for_https_endpoints() {
        assert!(endpoint_uses_tls("https://otel.example.com:443"));
        assert!(endpoint_uses_tls("  HTTPS://otel.example.com:443  "));
        assert!(!endpoint_uses_tls("http://localhost:4317"));
        assert!(!endpoint_uses_tls("localhost:4317"));
    }

    #[cfg(feature = "otel")]
    #[test]
    fn protocol_names_parse() {
        assert_eq!(parse_otlp_protocol("grpc"), Some(OtlpProtocol::Grpc));
        assert_eq!(
            parse_otlp_protocol(" HTTP/PROTOBUF "),
            Some(OtlpProtocol::HttpProtobuf)
        );
        // Unsupported: no http-json feature, so the name must not be accepted.
        assert_eq!(parse_otlp_protocol("http/json"), None);
        assert_eq!(parse_otlp_protocol("https"), None);
    }
}
