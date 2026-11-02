//! Operational metrics: OpenTelemetry instruments read by a Prometheus
//! scrape endpoint and, when an OTLP endpoint is configured, pushed over
//! OTLP as well.
//!
//! Instrument names follow the OpenTelemetry GenAI and MCP semantic
//! conventions where one exists and sit under `aura.*` otherwise. Every
//! recording site goes through the functions here so the attribute
//! vocabulary has one owner. Attribute values are config-bounded (agent,
//! provider, model, MCP server key, tool name, ingress, outcome); session,
//! request, run and user identifiers never become attributes.
//!
//! Without the `otel` feature the functions compile to nothing and
//! [`render_prometheus`] reports [`MetricsError::Disabled`].

use std::time::Duration;

/// Where a run entered the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingress {
    Http,
    A2a,
    Slack,
}

impl Ingress {
    pub fn as_str(self) -> &'static str {
        match self {
            Ingress::Http => "http",
            Ingress::A2a => "a2a",
            Ingress::Slack => "slack",
        }
    }
}

/// Which agent a run measurement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// The agent the request was addressed to (single-agent run or the
    /// orchestration coordinator).
    Main,
    /// An orchestration worker.
    Worker,
}

impl AgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentRole::Main => "main",
            AgentRole::Worker => "worker",
        }
    }
}

/// One completed run, for [`record_run`].
#[derive(Debug, Clone)]
pub struct RunMeasure<'a> {
    pub agent: &'a str,
    pub ingress: Ingress,
    pub role: AgentRole,
    pub orchestration: bool,
    /// `None` for a run that finished normally.
    pub error_type: Option<&'a str>,
    pub duration: Duration,
    pub inference_calls: Option<u64>,
    pub tool_calls: Option<u64>,
}

/// Provider-billed tokens of one LLM turn, for [`record_token_usage`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenMeasure {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Kind of tool behind a `gen_ai.execute_tool` measurement
/// (`gen_ai.tool.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolType {
    /// A tool executed in-process (native and scratchpad tools).
    Function,
    /// A tool served by an MCP server.
    Extension,
}

impl ToolType {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolType::Function => "function",
            ToolType::Extension => "extension",
        }
    }
}

/// Content type of the Prometheus text exposition `render_prometheus`
/// produces.
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("metrics are not available: aura was built without the `otel` feature")]
    Disabled,
    #[error("metrics are not initialised")]
    NotInitialised,
    #[cfg(feature = "otel")]
    #[error("metrics SDK error: {0}")]
    Sdk(#[from] opentelemetry_sdk::metrics::MetricError),
    #[cfg(feature = "otel")]
    #[error("prometheus encoding error: {0}")]
    Encode(#[from] prometheus::Error),
}

/// Whether this build can serve metrics at all.
pub const fn is_available() -> bool {
    cfg!(feature = "otel")
}

/// `gen_ai.provider.name` for one of aura's provider identifiers
/// (`ProviderAgent::provider_name`), using the semconv spelling where the
/// conventions define one.
pub fn provider_name(provider: &str) -> &str {
    match provider {
        "bedrock" => "aws.bedrock",
        "gemini" => "gcp.gemini",
        other => other,
    }
}

#[cfg(feature = "otel")]
pub use imp::*;

#[cfg(feature = "otel")]
mod imp {
    use std::sync::OnceLock;
    use std::time::Duration;

    use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, UpDownCounter};
    use opentelemetry::{KeyValue, global};
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
    use prometheus::Encoder;

    use super::{Ingress, MetricsError, RunMeasure, TokenMeasure, ToolType, provider_name};

    const METER_NAME: &str = "aura";

    // Semconv attribute keys.
    const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
    const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
    const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
    const GEN_AI_AGENT_NAME: &str = "gen_ai.agent.name";
    const GEN_AI_TOKEN_MODALITY: &str = "gen_ai.token.modality";
    const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
    const GEN_AI_TOOL_TYPE: &str = "gen_ai.tool.type";
    const ERROR_TYPE: &str = "error.type";
    const MCP_METHOD_NAME: &str = "mcp.method.name";
    const HTTP_ROUTE: &str = "http.route";
    const HTTP_REQUEST_METHOD: &str = "http.request.method";
    const HTTP_RESPONSE_STATUS_CODE: &str = "http.response.status_code";
    // Aura attribute keys.
    const AURA_INGRESS: &str = "aura.ingress";
    const AURA_MODE: &str = "aura.mode";
    const AURA_ROLE: &str = "aura.role";
    const AURA_MCP_SERVER: &str = "aura.mcp.server";
    const AURA_WORKER: &str = "aura.worker";
    const AURA_DECISION: &str = "aura.decision";
    const AURA_OUTCOME: &str = "aura.outcome";

    /// Buckets the HTTP semconv recommends for `http.server.request.duration`.
    const HTTP_SECONDS: &[f64] = &[
        0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
    ];
    /// Buckets the GenAI semconv recommends for client operation durations.
    const GEN_AI_SECONDS: &[f64] = &[
        0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
    ];
    /// Buckets the MCP semconv recommends for operation durations.
    const MCP_SECONDS: &[f64] = &[
        0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
    ];
    /// Whole runs and worker tasks run up to the streaming timeout (15 min
    /// by default), so the top buckets sit above it.
    const RUN_SECONDS: &[f64] = &[
        0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0,
    ];
    /// Buckets the GenAI semconv recommends for per-operation token counts.
    const TOKENS: &[f64] = &[
        1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0,
        4194304.0, 16777216.0, 67108864.0,
    ];
    const CALLS: &[f64] = &[0.0, 1.0, 2.0, 3.0, 5.0, 8.0, 13.0, 21.0, 34.0, 55.0, 89.0];
    const CYCLES: &[f64] = &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0];
    /// Approval waits are human-paced: seconds to an hour.
    const APPROVAL_SECONDS: &[f64] = &[
        1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
    ];

    struct Instruments {
        http_server_request_duration: Histogram<f64>,
        invoke_agent_duration: Histogram<f64>,
        invoke_agent_inference_calls: Histogram<u64>,
        invoke_agent_tool_calls: Histogram<u64>,
        runs_active: UpDownCounter<i64>,
        inference_duration: Histogram<f64>,
        usage_input_tokens: Counter<u64>,
        usage_output_tokens: Counter<u64>,
        usage_cache_read_tokens: Counter<u64>,
        usage_cache_write_tokens: Counter<u64>,
        operation_input_tokens: Histogram<u64>,
        operation_output_tokens: Histogram<u64>,
        mcp_operation_duration: Histogram<f64>,
        execute_tool_duration: Histogram<f64>,
        mcp_server_connected: Gauge<u64>,
        mcp_server_tools: Gauge<u64>,
        routing: Counter<u64>,
        planning_cycles: Histogram<u64>,
        task_duration: Histogram<f64>,
        hitl_pending: UpDownCounter<i64>,
        hitl_decision_duration: Histogram<f64>,
    }

    impl Instruments {
        fn new(meter: &Meter) -> Self {
            Self {
                http_server_request_duration: meter
                    .f64_histogram("http.server.request.duration")
                    .with_unit("s")
                    .with_description("Duration of HTTP server requests")
                    .with_boundaries(HTTP_SECONDS.to_vec())
                    .build(),
                invoke_agent_duration: meter
                    .f64_histogram("gen_ai.invoke_agent.duration")
                    .with_unit("s")
                    .with_description("Duration of one agent run, from request to final event")
                    .with_boundaries(RUN_SECONDS.to_vec())
                    .build(),
                invoke_agent_inference_calls: meter
                    .u64_histogram("gen_ai.invoke_agent.inference_calls")
                    .with_unit("{inference_call}")
                    .with_description("LLM calls made during one agent run")
                    .with_boundaries(CALLS.to_vec())
                    .build(),
                invoke_agent_tool_calls: meter
                    .u64_histogram("gen_ai.invoke_agent.tool_calls")
                    .with_unit("{tool_call}")
                    .with_description("Tool calls made during one agent run")
                    .with_boundaries(CALLS.to_vec())
                    .build(),
                runs_active: meter
                    .i64_up_down_counter("aura.runs.active")
                    .with_unit("{run}")
                    .with_description("Agent runs in flight")
                    .build(),
                inference_duration: meter
                    .f64_histogram("gen_ai.client.inference.duration")
                    .with_unit("s")
                    .with_description("Duration of one LLM inference call")
                    .with_boundaries(GEN_AI_SECONDS.to_vec())
                    .build(),
                usage_input_tokens: meter
                    .u64_counter("gen_ai.client.inference.usage.input_tokens")
                    .with_unit("{token}")
                    .with_description("Input tokens billed by the provider, cached tokens included")
                    .build(),
                usage_output_tokens: meter
                    .u64_counter("gen_ai.client.inference.usage.output_tokens")
                    .with_unit("{token}")
                    .with_description("Output tokens billed by the provider")
                    .build(),
                usage_cache_read_tokens: meter
                    .u64_counter("gen_ai.client.inference.usage.cache_read.input_tokens")
                    .with_unit("{token}")
                    .with_description("Input tokens served from the provider's prompt cache")
                    .build(),
                usage_cache_write_tokens: meter
                    .u64_counter("gen_ai.client.inference.usage.cache_write.input_tokens")
                    .with_unit("{token}")
                    .with_description("Input tokens written to the provider's prompt cache")
                    .build(),
                operation_input_tokens: meter
                    .u64_histogram("gen_ai.client.inference.operation.input_tokens")
                    .with_unit("{token}")
                    .with_description("Input tokens of one LLM inference call")
                    .with_boundaries(TOKENS.to_vec())
                    .build(),
                operation_output_tokens: meter
                    .u64_histogram("gen_ai.client.inference.operation.output_tokens")
                    .with_unit("{token}")
                    .with_description("Output tokens of one LLM inference call")
                    .with_boundaries(TOKENS.to_vec())
                    .build(),
                mcp_operation_duration: meter
                    .f64_histogram("mcp.client.operation.duration")
                    .with_unit("s")
                    .with_description("Duration of one MCP client operation")
                    .with_boundaries(MCP_SECONDS.to_vec())
                    .build(),
                execute_tool_duration: meter
                    .f64_histogram("gen_ai.execute_tool.duration")
                    .with_unit("s")
                    .with_description(
                        "Duration of one tool execution as the agent sees it, middleware included",
                    )
                    .with_boundaries(MCP_SECONDS.to_vec())
                    .build(),
                mcp_server_connected: meter
                    .u64_gauge("aura.mcp.server.connected")
                    .with_description("1 when the agent's connection to the MCP server is up")
                    .build(),
                mcp_server_tools: meter
                    .u64_gauge("aura.mcp.server.tools")
                    .with_unit("{tool}")
                    .with_description("Tools the agent discovered on the MCP server")
                    .build(),
                routing: meter
                    .u64_counter("aura.orchestration.routing")
                    .with_unit("{decision}")
                    .with_description("Orchestration routing decisions")
                    .build(),
                planning_cycles: meter
                    .u64_histogram("aura.orchestration.planning_cycles")
                    .with_unit("{cycle}")
                    .with_description("Planning cycles one orchestrated run took")
                    .with_boundaries(CYCLES.to_vec())
                    .build(),
                task_duration: meter
                    .f64_histogram("aura.orchestration.task.duration")
                    .with_unit("s")
                    .with_description("Duration of one orchestration worker task")
                    .with_boundaries(RUN_SECONDS.to_vec())
                    .build(),
                hitl_pending: meter
                    .i64_up_down_counter("aura.hitl.pending")
                    .with_unit("{approval}")
                    .with_description("Tool calls waiting on a human decision")
                    .build(),
                hitl_decision_duration: meter
                    .f64_histogram("aura.hitl.decision.duration")
                    .with_unit("s")
                    .with_description("Time from approval request to decision")
                    .with_boundaries(APPROVAL_SECONDS.to_vec())
                    .build(),
            }
        }
    }

    struct State {
        registry: prometheus::Registry,
        provider: SdkMeterProvider,
        instruments: Instruments,
    }

    static STATE: OnceLock<State> = OnceLock::new();

    fn instruments() -> Option<&'static Instruments> {
        STATE.get().map(|s| &s.instruments)
    }

    /// Install the meter provider: a Prometheus reader always, plus an OTLP
    /// periodic reader when `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` or
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Must run inside a Tokio runtime
    /// (the OTLP reader spawns its export task on it) and before the first
    /// recording, since instruments bind to whichever provider is global
    /// when they are created. A second call is a no-op.
    ///
    /// An OTLP exporter that fails to build is reported and skipped, so a
    /// bad collector address costs the push, not the scrape.
    pub fn init() -> Result<(), MetricsError> {
        if STATE.get().is_some() {
            return Ok(());
        }
        let registry = prometheus::Registry::new();
        let prometheus_reader = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .without_scope_info()
            .build()?;

        let service_name =
            std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "aura".to_string());
        let mut builder = SdkMeterProvider::builder()
            .with_reader(prometheus_reader)
            .with_resource(opentelemetry_sdk::Resource::new(vec![
                KeyValue::new("service.name", service_name),
                KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            ]));
        match otlp_reader() {
            Ok(Some(reader)) => builder = builder.with_reader(reader),
            Ok(None) => {}
            Err(e) => tracing::warn!(
                "OTLP metrics endpoint is set but the exporter failed to initialize: {e}. \
                 Metrics stay available on the Prometheus endpoint only."
            ),
        }
        let provider = builder.build();
        global::set_meter_provider(provider.clone());
        let instruments = Instruments::new(&global::meter(METER_NAME));
        let _ = STATE.set(State {
            registry,
            provider,
            instruments,
        });
        Ok(())
    }

    /// The OTLP metrics reader, `None` when no OTLP endpoint is configured.
    /// The signal-specific endpoint and protocol variables win over the
    /// generic ones, matching the span exporter.
    fn otlp_reader() -> Result<Option<PeriodicReader>, MetricsError> {
        use opentelemetry_otlp::{MetricExporter, WithTonicConfig};

        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"))
            .ok();
        let Some(endpoint) = endpoint else {
            return Ok(None);
        };
        let protocol =
            crate::logging::resolve_otlp_protocol_from("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL");
        let exporter = match protocol {
            crate::logging::OtlpProtocol::Grpc => {
                let mut builder = MetricExporter::builder().with_tonic();
                if crate::logging::endpoint_uses_tls(&endpoint) {
                    builder = builder.with_tls_config(
                        tonic::transport::ClientTlsConfig::new().with_native_roots(),
                    );
                }
                builder.build()?
            }
            crate::logging::OtlpProtocol::HttpProtobuf => {
                MetricExporter::builder().with_http().build()?
            }
        };
        Ok(Some(
            PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio).build(),
        ))
    }

    /// The Prometheus text exposition of every instrument, collected now.
    pub fn render_prometheus() -> Result<String, MetricsError> {
        let state = STATE.get().ok_or(MetricsError::NotInitialised)?;
        let families = state.registry.gather();
        let mut buf = Vec::new();
        prometheus::TextEncoder::new().encode(&families, &mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Flush and shut down the meter provider. Blocking under the hood (the
    /// OTLP reader waits for its final export), so it runs on the blocking
    /// pool with a bound, like `logging::shutdown_tracer`. No-op when
    /// metrics were never initialised.
    pub async fn shutdown() {
        let Some(state) = STATE.get() else {
            return;
        };
        let provider = state.provider.clone();
        match tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                if let Err(e) = provider.shutdown() {
                    tracing::warn!("metrics shutdown error: {e}");
                }
            }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("metrics shutdown task panicked: {e}"),
            Err(_) => tracing::warn!("metrics shutdown timed out after 5s"),
        }
    }

    fn kv(key: &'static str, value: &str) -> KeyValue {
        KeyValue::new(key, value.to_owned())
    }

    fn push_error(attrs: &mut Vec<KeyValue>, error_type: Option<&str>) {
        if let Some(error_type) = error_type {
            attrs.push(kv(ERROR_TYPE, error_type));
        }
    }

    pub fn record_http_request(route: &str, method: &str, status: u16, duration: Duration) {
        let Some(i) = instruments() else { return };
        i.http_server_request_duration.record(
            duration.as_secs_f64(),
            &[
                kv(HTTP_ROUTE, route),
                kv(HTTP_REQUEST_METHOD, method),
                KeyValue::new(HTTP_RESPONSE_STATUS_CODE, i64::from(status)),
            ],
        );
    }

    pub fn runs_active_add(ingress: Ingress, delta: i64) {
        let Some(i) = instruments() else { return };
        i.runs_active
            .add(delta, &[kv(AURA_INGRESS, ingress.as_str())]);
    }

    pub fn record_run(run: RunMeasure<'_>) {
        let Some(i) = instruments() else { return };
        let mode = if run.orchestration {
            "orchestration"
        } else {
            "single"
        };
        let mut attrs = vec![
            kv(GEN_AI_AGENT_NAME, run.agent),
            kv(AURA_INGRESS, run.ingress.as_str()),
            kv(AURA_ROLE, run.role.as_str()),
            kv(AURA_MODE, mode),
        ];
        push_error(&mut attrs, run.error_type);
        i.invoke_agent_duration
            .record(run.duration.as_secs_f64(), &attrs);
        let count_attrs = [
            kv(GEN_AI_AGENT_NAME, run.agent),
            kv(AURA_ROLE, run.role.as_str()),
        ];
        if let Some(n) = run.inference_calls {
            i.invoke_agent_inference_calls.record(n, &count_attrs);
        }
        if let Some(n) = run.tool_calls {
            i.invoke_agent_tool_calls.record(n, &count_attrs);
        }
    }

    fn inference_attrs(provider: &str, model: &str, agent: &str) -> Vec<KeyValue> {
        vec![
            kv(GEN_AI_OPERATION_NAME, "chat"),
            kv(GEN_AI_PROVIDER_NAME, provider_name(provider)),
            kv(GEN_AI_REQUEST_MODEL, model),
            kv(GEN_AI_AGENT_NAME, agent),
        ]
    }

    pub fn record_inference(
        provider: &str,
        model: &str,
        agent: &str,
        error_type: Option<&str>,
        duration: Duration,
    ) {
        let Some(i) = instruments() else { return };
        let mut attrs = inference_attrs(provider, model, agent);
        push_error(&mut attrs, error_type);
        i.inference_duration.record(duration.as_secs_f64(), &attrs);
    }

    pub fn record_token_usage(provider: &str, model: &str, agent: &str, tokens: TokenMeasure) {
        let Some(i) = instruments() else { return };
        let attrs = inference_attrs(provider, model, agent);
        i.operation_input_tokens.record(tokens.input, &attrs);
        i.operation_output_tokens.record(tokens.output, &attrs);
        let mut usage_attrs = attrs;
        usage_attrs.push(kv(GEN_AI_TOKEN_MODALITY, "text"));
        i.usage_input_tokens.add(tokens.input, &usage_attrs);
        i.usage_output_tokens.add(tokens.output, &usage_attrs);
        if tokens.cache_read > 0 {
            i.usage_cache_read_tokens
                .add(tokens.cache_read, &usage_attrs);
        }
        if tokens.cache_write > 0 {
            i.usage_cache_write_tokens
                .add(tokens.cache_write, &usage_attrs);
        }
    }

    pub fn record_mcp_operation(
        server: &str,
        tool: &str,
        error_type: Option<&str>,
        duration: Duration,
    ) {
        let Some(i) = instruments() else { return };
        let mut attrs = vec![
            kv(MCP_METHOD_NAME, "tools/call"),
            kv(AURA_MCP_SERVER, server),
            kv(GEN_AI_TOOL_NAME, tool),
        ];
        push_error(&mut attrs, error_type);
        i.mcp_operation_duration
            .record(duration.as_secs_f64(), &attrs);
    }

    pub fn record_tool_execution(
        tool: &str,
        agent: &str,
        tool_type: ToolType,
        error_type: Option<&str>,
        duration: Duration,
    ) {
        let Some(i) = instruments() else { return };
        let mut attrs = vec![
            kv(GEN_AI_TOOL_NAME, tool),
            kv(GEN_AI_AGENT_NAME, agent),
            kv(GEN_AI_TOOL_TYPE, tool_type.as_str()),
        ];
        push_error(&mut attrs, error_type);
        i.execute_tool_duration
            .record(duration.as_secs_f64(), &attrs);
    }

    pub fn set_mcp_server_status(agent: &str, server: &str, connected: bool, tool_count: u64) {
        let Some(i) = instruments() else { return };
        let attrs = [kv(GEN_AI_AGENT_NAME, agent), kv(AURA_MCP_SERVER, server)];
        i.mcp_server_connected.record(u64::from(connected), &attrs);
        i.mcp_server_tools.record(tool_count, &attrs);
    }

    pub fn record_routing_decision(agent: &str, decision: &str) {
        let Some(i) = instruments() else { return };
        i.routing.add(
            1,
            &[kv(GEN_AI_AGENT_NAME, agent), kv(AURA_DECISION, decision)],
        );
    }

    pub fn record_planning_cycles(agent: &str, cycles: u64) {
        let Some(i) = instruments() else { return };
        i.planning_cycles
            .record(cycles, &[kv(GEN_AI_AGENT_NAME, agent)]);
    }

    pub fn record_task(agent: &str, worker: &str, outcome: &str, duration: Duration) {
        let Some(i) = instruments() else { return };
        i.task_duration.record(
            duration.as_secs_f64(),
            &[
                kv(GEN_AI_AGENT_NAME, agent),
                kv(AURA_WORKER, worker),
                kv(AURA_OUTCOME, outcome),
            ],
        );
    }

    pub fn hitl_pending_add(agent: &str, delta: i64) {
        let Some(i) = instruments() else { return };
        i.hitl_pending.add(delta, &[kv(GEN_AI_AGENT_NAME, agent)]);
    }

    pub fn record_hitl_decision(agent: &str, decision: &str, duration: Duration) {
        let Some(i) = instruments() else { return };
        i.hitl_decision_duration.record(
            duration.as_secs_f64(),
            &[kv(GEN_AI_AGENT_NAME, agent), kv(AURA_DECISION, decision)],
        );
    }
}

#[cfg(not(feature = "otel"))]
pub use stub::*;

/// The same surface as `imp`, compiled to nothing.
#[cfg(not(feature = "otel"))]
mod stub {
    use std::time::Duration;

    use super::{Ingress, MetricsError, RunMeasure, TokenMeasure, ToolType};

    pub fn init() -> Result<(), MetricsError> {
        Err(MetricsError::Disabled)
    }
    pub fn render_prometheus() -> Result<String, MetricsError> {
        Err(MetricsError::Disabled)
    }
    pub async fn shutdown() {}
    pub fn record_http_request(_: &str, _: &str, _: u16, _: Duration) {}
    pub fn runs_active_add(_: Ingress, _: i64) {}
    pub fn record_run(_: RunMeasure<'_>) {}
    pub fn record_inference(_: &str, _: &str, _: &str, _: Option<&str>, _: Duration) {}
    pub fn record_token_usage(_: &str, _: &str, _: &str, _: TokenMeasure) {}
    pub fn record_mcp_operation(_: &str, _: &str, _: Option<&str>, _: Duration) {}
    pub fn record_tool_execution(_: &str, _: &str, _: ToolType, _: Option<&str>, _: Duration) {}
    pub fn set_mcp_server_status(_: &str, _: &str, _: bool, _: u64) {}
    pub fn record_routing_decision(_: &str, _: &str) {}
    pub fn record_planning_cycles(_: &str, _: u64) {}
    pub fn record_task(_: &str, _: &str, _: &str, _: Duration) {}
    pub fn hitl_pending_add(_: &str, _: i64) {}
    pub fn record_hitl_decision(_: &str, _: &str, _: Duration) {}
}

#[cfg(all(test, feature = "otel"))]
mod tests {
    use super::*;

    #[test]
    fn provider_names_follow_semconv() {
        assert_eq!(provider_name("bedrock"), "aws.bedrock");
        assert_eq!(provider_name("gemini"), "gcp.gemini");
        assert_eq!(provider_name("openai"), "openai");
        assert_eq!(provider_name("ollama"), "ollama");
    }

    /// One process-wide provider, so every assertion about the rendered
    /// exposition lives in this test.
    #[tokio::test]
    async fn renders_recorded_instruments_in_prometheus_text() {
        init().expect("metrics init");
        init().expect("second init is a no-op");

        record_http_request(
            "/v1/chat/completions",
            "POST",
            200,
            Duration::from_millis(12),
        );
        runs_active_add(Ingress::Http, 1);
        record_run(RunMeasure {
            agent: "demo",
            ingress: Ingress::Http,
            role: AgentRole::Main,
            orchestration: false,
            error_type: Some("timeout"),
            duration: Duration::from_secs(3),
            inference_calls: Some(2),
            tool_calls: Some(1),
        });
        record_inference(
            "bedrock",
            "claude",
            "demo",
            None,
            Duration::from_millis(800),
        );
        record_token_usage(
            "bedrock",
            "claude",
            "demo",
            TokenMeasure {
                input: 100,
                output: 20,
                cache_read: 40,
                cache_write: 0,
            },
        );
        record_mcp_operation(
            "mezmo",
            "search_logs",
            Some("tool_error"),
            Duration::from_secs(1),
        );
        record_tool_execution(
            "search_logs",
            "demo",
            ToolType::Extension,
            None,
            Duration::from_secs(1),
        );
        set_mcp_server_status("demo", "mezmo", true, 17);
        record_routing_decision("demo", "orchestrated");
        record_planning_cycles("demo", 2);
        record_task("demo", "logs", "completed", Duration::from_secs(4));
        hitl_pending_add("demo", 1);
        record_hitl_decision("demo", "approved", Duration::from_secs(30));

        let text = render_prometheus().expect("render");

        for needle in [
            r#"http_server_request_duration_seconds_count{http_request_method="POST",http_response_status_code="200",http_route="/v1/chat/completions"} 1"#,
            r#"aura_runs_active{aura_ingress="http"} 1"#,
            r#"gen_ai_invoke_agent_duration_seconds_count{aura_ingress="http",aura_mode="single",aura_role="main",error_type="timeout",gen_ai_agent_name="demo"} 1"#,
            r#"gen_ai_invoke_agent_inference_calls_count{aura_role="main",gen_ai_agent_name="demo"} 1"#,
            r#"gen_ai_client_inference_duration_seconds_count{gen_ai_agent_name="demo",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="claude"} 1"#,
            r#"gen_ai_client_inference_usage_input_tokens_total{gen_ai_agent_name="demo",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="claude",gen_ai_token_modality="text"} 100"#,
            r#"gen_ai_client_inference_usage_cache_read_input_tokens_total{gen_ai_agent_name="demo",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="claude",gen_ai_token_modality="text"} 40"#,
            r#"gen_ai_client_inference_operation_input_tokens_sum{gen_ai_agent_name="demo",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="claude"} 100"#,
            r#"mcp_client_operation_duration_seconds_count{aura_mcp_server="mezmo",error_type="tool_error",gen_ai_tool_name="search_logs",mcp_method_name="tools/call"} 1"#,
            r#"gen_ai_execute_tool_duration_seconds_count{gen_ai_agent_name="demo",gen_ai_tool_name="search_logs",gen_ai_tool_type="extension"} 1"#,
            r#"aura_mcp_server_connected{aura_mcp_server="mezmo",gen_ai_agent_name="demo"} 1"#,
            r#"aura_mcp_server_tools{aura_mcp_server="mezmo",gen_ai_agent_name="demo"} 17"#,
            r#"aura_orchestration_routing_total{aura_decision="orchestrated",gen_ai_agent_name="demo"} 1"#,
            r#"aura_orchestration_planning_cycles_sum{gen_ai_agent_name="demo"} 2"#,
            r#"aura_orchestration_task_duration_seconds_count{aura_outcome="completed",aura_worker="logs",gen_ai_agent_name="demo"} 1"#,
            r#"aura_hitl_pending{gen_ai_agent_name="demo"} 1"#,
            r#"aura_hitl_decision_duration_seconds_count{aura_decision="approved",gen_ai_agent_name="demo"} 1"#,
            r#"target_info{service_name="aura""#,
        ] {
            assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
        }
        // Cache-write tokens were zero, so that counter has no series.
        assert!(!text.contains("gen_ai_client_inference_usage_cache_write_input_tokens_total{"));
        assert!(!text.contains("otel_scope_name"), "scope info is dropped");
    }
}
