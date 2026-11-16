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

use std::sync::Arc;
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

/// An agent as its metrics name it: the loaded config's `[agent].name` and
/// the agent's own name, which under orchestration is a worker's.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "otel"), allow(dead_code))]
pub struct AgentLabels {
    config: Arc<str>,
    agent: Arc<str>,
}

impl AgentLabels {
    pub fn new(config: &str, agent: &str) -> Self {
        Self {
            config: config.into(),
            agent: agent.into(),
        }
    }

    /// A main agent: the config's own name on both labels.
    pub fn main(name: &str) -> Self {
        Self::new(name, name)
    }
}

/// The agent and model behind a run's LLM turns, as the inference metrics
/// label them.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "otel"), allow(dead_code))]
pub struct InferenceLabels {
    agent: AgentLabels,
    provider: Arc<str>,
    model: Arc<str>,
}

impl InferenceLabels {
    /// `provider` is aura's provider identifier; the semconv spelling is
    /// applied when the labels are recorded.
    pub fn new(agent: &AgentLabels, provider: &str, model: &str) -> Self {
        Self {
            agent: agent.clone(),
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// Provider-billed tokens of one LLM turn, for [`record_token_usage`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenMeasure {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
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

/// The `aura.decision` values of `aura.orchestration.routing`.
pub mod decision {
    pub const DIRECT_ANSWER: &str = "direct_answer";
    pub const CLARIFICATION: &str = "clarification";
    pub const ORCHESTRATED: &str = "orchestrated";
    pub const COORDINATOR_TIMEOUT: &str = "coordinator_timeout";
    pub const COORDINATOR_TURN_BUDGET_EXHAUSTED: &str = "coordinator_turn_budget_exhausted";
    pub const COORDINATOR_ERROR: &str = "coordinator_error";
    pub const ALL: [&str; 6] = [
        DIRECT_ANSWER,
        CLARIFICATION,
        ORCHESTRATED,
        COORDINATOR_TIMEOUT,
        COORDINATOR_TURN_BUDGET_EXHAUSTED,
        COORDINATOR_ERROR,
    ];
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
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use opentelemetry::metrics::{
        Counter, Gauge, Histogram, Meter, MeterProvider as _, UpDownCounter,
    };
    use opentelemetry::{KeyValue, global};
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
    use prometheus::Encoder;

    use super::{
        AgentLabels, InferenceLabels, Ingress, MetricsError, RunMeasure, TokenMeasure,
        provider_name,
    };

    const METER_NAME: &str = "aura";

    // Semconv attribute keys.
    const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
    const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
    const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
    const GEN_AI_AGENT_NAME: &str = "gen_ai.agent.name";
    const AURA_AGENT: &str = "aura.agent";
    const GEN_AI_TOKEN_MODALITY: &str = "gen_ai.token.modality";
    const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
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
        scratchpad_intercepted_tokens: Histogram<u64>,
        scratchpad_extracted_tokens: Histogram<u64>,
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
                scratchpad_intercepted_tokens: meter
                    .u64_histogram("aura.scratchpad.intercepted_tokens")
                    .with_unit("{token}")
                    .with_description(
                        "Tokens of one tool output the scratchpad kept out of the context window",
                    )
                    .with_boundaries(TOKENS.to_vec())
                    .build(),
                scratchpad_extracted_tokens: meter
                    .u64_histogram("aura.scratchpad.extracted_tokens")
                    .with_unit("{token}")
                    .with_description("Tokens one scratchpad read returned to the context window")
                    .with_boundaries(TOKENS.to_vec())
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
    /// Held for the whole of `init`, so one caller builds the state and the
    /// rest wait for it rather than racing it.
    static INIT: Mutex<()> = Mutex::new(());

    fn instruments() -> Option<&'static Instruments> {
        STATE.get().map(|s| &s.instruments)
    }

    /// Install the meter provider: a Prometheus reader always, plus, when
    /// `otlp_push` is set, an OTLP periodic reader if
    /// `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT`
    /// is set. Must run inside a Tokio runtime (the OTLP reader spawns its
    /// export task on it) and before the first recording, since instruments
    /// bind to the provider at creation. A second call is a no-op, and a
    /// concurrent one waits for the first to finish; the instruments come
    /// from the provider built here, not the global one, so a caller that
    /// loses the race still records into the registry this serves.
    ///
    /// An OTLP exporter that fails to build is reported and skipped, so a
    /// bad collector address costs the push, not the scrape.
    pub fn init(otlp_push: bool) -> Result<(), MetricsError> {
        let _serial = INIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
        match otlp_endpoint() {
            None => {}
            Some(endpoint) if !otlp_push => tracing::info!(
                "OTLP metrics push disabled (AURA_METRICS_OTLP=false); {endpoint} receives traces only"
            ),
            Some(endpoint) => match otlp_reader(&endpoint) {
                Ok(reader) => {
                    tracing::info!(
                        "Pushing metrics over OTLP to {endpoint} as well as serving /metrics"
                    );
                    builder = builder.with_reader(reader);
                }
                Err(e) => tracing::warn!(
                    "OTLP metrics endpoint {endpoint} is set but the exporter failed to initialize: {e}. \
                     Metrics stay available on the Prometheus endpoint only."
                ),
            },
        }
        let provider = builder.build();
        let instruments = Instruments::new(&provider.meter(METER_NAME));
        global::set_meter_provider(provider.clone());
        let _ = STATE.set(State {
            registry,
            provider,
            instruments,
        });
        Ok(())
    }

    /// The OTLP endpoint metrics would push to: the signal-specific variable
    /// wins over the generic one, as the OTLP spec and the span exporter
    /// have it.
    fn otlp_endpoint() -> Option<String> {
        std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"))
            .ok()
    }

    /// The OTLP metrics reader for `endpoint`. The exporter reads the
    /// endpoint itself; `endpoint` only decides TLS. The protocol follows
    /// `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL`, then the generic variable.
    fn otlp_reader(endpoint: &str) -> Result<PeriodicReader, MetricsError> {
        use opentelemetry_otlp::{MetricExporter, WithTonicConfig};

        let protocol =
            crate::logging::resolve_otlp_protocol_from("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL");
        let exporter = match protocol {
            crate::logging::OtlpProtocol::Grpc => {
                let mut builder = MetricExporter::builder().with_tonic();
                if crate::logging::endpoint_uses_tls(endpoint) {
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
        Ok(PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio).build())
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
        let mut attrs = main_agent_attrs(run.agent).to_vec();
        attrs.extend([
            kv(AURA_INGRESS, run.ingress.as_str()),
            kv(AURA_ROLE, run.role.as_str()),
            kv(AURA_MODE, mode),
        ]);
        push_error(&mut attrs, run.error_type);
        i.invoke_agent_duration
            .record(run.duration.as_secs_f64(), &attrs);
        let mut count_attrs = main_agent_attrs(run.agent).to_vec();
        count_attrs.push(kv(AURA_ROLE, run.role.as_str()));
        if let Some(n) = run.inference_calls {
            i.invoke_agent_inference_calls.record(n, &count_attrs);
        }
        if let Some(n) = run.tool_calls {
            i.invoke_agent_tool_calls.record(n, &count_attrs);
        }
    }

    impl AgentLabels {
        /// `gen_ai.agent.name` and `aura.agent` for this agent.
        fn attrs(&self) -> [KeyValue; 2] {
            [
                kv(GEN_AI_AGENT_NAME, &self.agent),
                kv(AURA_AGENT, &self.config),
            ]
        }
    }

    /// Both agent labels of a main agent, whose config is itself.
    fn main_agent_attrs(agent: &str) -> [KeyValue; 2] {
        [kv(GEN_AI_AGENT_NAME, agent), kv(AURA_AGENT, agent)]
    }

    fn inference_attrs(labels: &InferenceLabels) -> Vec<KeyValue> {
        let mut attrs = vec![
            kv(GEN_AI_OPERATION_NAME, "chat"),
            kv(GEN_AI_PROVIDER_NAME, provider_name(&labels.provider)),
            kv(GEN_AI_REQUEST_MODEL, &labels.model),
        ];
        attrs.extend(labels.agent.attrs());
        attrs
    }

    pub fn record_inference(
        labels: &InferenceLabels,
        error_type: Option<&str>,
        duration: Duration,
    ) {
        let Some(i) = instruments() else { return };
        let mut attrs = inference_attrs(labels);
        push_error(&mut attrs, error_type);
        i.inference_duration.record(duration.as_secs_f64(), &attrs);
    }

    pub fn record_token_usage(labels: &InferenceLabels, tokens: TokenMeasure) {
        let Some(i) = instruments() else { return };
        let attrs = inference_attrs(labels);
        // A provider that reports no usage for a turn yields all zeros; the
        // distributions skip it rather than count an empty sample.
        if tokens.input > 0 || tokens.output > 0 {
            i.operation_input_tokens.record(tokens.input, &attrs);
            i.operation_output_tokens.record(tokens.output, &attrs);
        }
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

    /// Create the counter series a loaded config's agents will increment, at
    /// zero, so their first increment is a change `rate()` can see rather
    /// than the birth of a series. Token usage for the agent and for each
    /// worker under its own `llm`, and the routing decisions of an
    /// orchestrating agent; histograms have no zero to record and start at
    /// their first sample.
    pub fn declare_config(config: &aura_config::Config) {
        let Some(i) = instruments() else { return };
        let main = AgentLabels::main(&config.agent.name);
        let (provider, model) = config.agent.llm.model_info();
        declare_inference(i, &InferenceLabels::new(&main, provider, model));
        let Some(orchestration) = config.orchestration.as_ref().filter(|o| o.enabled) else {
            return;
        };
        for (name, worker) in &orchestration.workers {
            let llm = worker.llm.as_ref().unwrap_or(&config.agent.llm);
            let (provider, model) = llm.model_info();
            let labels = AgentLabels::new(&config.agent.name, name);
            declare_inference(i, &InferenceLabels::new(&labels, provider, model));
        }
        for decision in super::decision::ALL {
            let mut attrs = main_agent_attrs(&config.agent.name).to_vec();
            attrs.push(kv(AURA_DECISION, decision));
            i.routing.add(0, &attrs);
        }
    }

    fn declare_inference(i: &Instruments, labels: &InferenceLabels) {
        let mut attrs = inference_attrs(labels);
        attrs.push(kv(GEN_AI_TOKEN_MODALITY, "text"));
        i.usage_input_tokens.add(0, &attrs);
        i.usage_output_tokens.add(0, &attrs);
    }

    pub fn record_mcp_operation(
        agent: &AgentLabels,
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
        attrs.extend(agent.attrs());
        push_error(&mut attrs, error_type);
        i.mcp_operation_duration
            .record(duration.as_secs_f64(), &attrs);
    }

    pub fn record_tool_execution(
        agent: &AgentLabels,
        tool: &str,
        error_type: Option<&str>,
        duration: Duration,
    ) {
        let Some(i) = instruments() else { return };
        let mut attrs = vec![kv(GEN_AI_TOOL_NAME, tool)];
        attrs.extend(agent.attrs());
        push_error(&mut attrs, error_type);
        i.execute_tool_duration
            .record(duration.as_secs_f64(), &attrs);
    }

    pub fn record_scratchpad_intercept(agent: &AgentLabels, tool: &str, tokens: u64) {
        let Some(i) = instruments() else { return };
        let mut attrs = vec![kv(GEN_AI_TOOL_NAME, tool)];
        attrs.extend(agent.attrs());
        i.scratchpad_intercepted_tokens.record(tokens, &attrs);
    }

    pub fn record_scratchpad_extract(agent: &AgentLabels, tool: &str, tokens: u64) {
        let Some(i) = instruments() else { return };
        let mut attrs = vec![kv(GEN_AI_TOOL_NAME, tool)];
        attrs.extend(agent.attrs());
        i.scratchpad_extracted_tokens.record(tokens, &attrs);
    }

    pub fn set_mcp_server_status(agent: &str, server: &str, connected: bool, tool_count: u64) {
        let Some(i) = instruments() else { return };
        let mut attrs = main_agent_attrs(agent).to_vec();
        attrs.push(kv(AURA_MCP_SERVER, server));
        i.mcp_server_connected.record(u64::from(connected), &attrs);
        i.mcp_server_tools.record(tool_count, &attrs);
    }

    pub fn record_routing_decision(agent: &str, decision: &str) {
        let Some(i) = instruments() else { return };
        let mut attrs = main_agent_attrs(agent).to_vec();
        attrs.push(kv(AURA_DECISION, decision));
        i.routing.add(1, &attrs);
    }

    pub fn record_planning_cycles(agent: &str, cycles: u64) {
        let Some(i) = instruments() else { return };
        i.planning_cycles.record(cycles, &main_agent_attrs(agent));
    }

    pub fn record_task(agent: &str, worker: &str, outcome: &str, duration: Duration) {
        let Some(i) = instruments() else { return };
        let mut attrs = main_agent_attrs(agent).to_vec();
        attrs.extend([kv(AURA_WORKER, worker), kv(AURA_OUTCOME, outcome)]);
        i.task_duration.record(duration.as_secs_f64(), &attrs);
    }

    /// One approval awaiting a decision, counted in `aura.hitl.pending`
    /// until dropped.
    pub struct HitlPending {
        agent: AgentLabels,
    }

    impl Drop for HitlPending {
        fn drop(&mut self) {
            hitl_pending_add(&self.agent, -1);
        }
    }

    /// Count an approval as pending until the returned guard drops, which a
    /// decision, a cancellation and a dropped future all do.
    pub fn hitl_pending(agent: &AgentLabels) -> HitlPending {
        hitl_pending_add(agent, 1);
        HitlPending {
            agent: agent.clone(),
        }
    }

    fn hitl_pending_add(agent: &AgentLabels, delta: i64) {
        let Some(i) = instruments() else { return };
        i.hitl_pending.add(delta, &agent.attrs());
    }

    pub fn record_hitl_decision(agent: &AgentLabels, decision: &str, duration: Duration) {
        let Some(i) = instruments() else { return };
        let mut attrs = agent.attrs().to_vec();
        attrs.push(kv(AURA_DECISION, decision));
        i.hitl_decision_duration
            .record(duration.as_secs_f64(), &attrs);
    }
}

#[cfg(not(feature = "otel"))]
pub use stub::*;

/// The same surface as `imp`, compiled to nothing.
#[cfg(not(feature = "otel"))]
mod stub {
    use std::time::Duration;

    use super::{AgentLabels, InferenceLabels, Ingress, MetricsError, RunMeasure, TokenMeasure};

    pub fn init(_: bool) -> Result<(), MetricsError> {
        Err(MetricsError::Disabled)
    }
    pub fn render_prometheus() -> Result<String, MetricsError> {
        Err(MetricsError::Disabled)
    }
    pub async fn shutdown() {}
    pub fn record_http_request(_: &str, _: &str, _: u16, _: Duration) {}
    pub fn runs_active_add(_: Ingress, _: i64) {}
    pub fn record_run(_: RunMeasure<'_>) {}
    pub fn record_inference(_: &InferenceLabels, _: Option<&str>, _: Duration) {}
    pub fn record_token_usage(_: &InferenceLabels, _: TokenMeasure) {}
    pub fn declare_config(_: &aura_config::Config) {}
    pub fn record_mcp_operation(_: &AgentLabels, _: &str, _: &str, _: Option<&str>, _: Duration) {}
    pub fn record_tool_execution(_: &AgentLabels, _: &str, _: Option<&str>, _: Duration) {}
    pub fn record_scratchpad_intercept(_: &AgentLabels, _: &str, _: u64) {}
    pub fn record_scratchpad_extract(_: &AgentLabels, _: &str, _: u64) {}
    pub fn set_mcp_server_status(_: &str, _: &str, _: bool, _: u64) {}
    pub fn record_routing_decision(_: &str, _: &str) {}
    pub fn record_planning_cycles(_: &str, _: u64) {}
    pub fn record_task(_: &str, _: &str, _: &str, _: Duration) {}
    pub struct HitlPending;
    impl Drop for HitlPending {
        fn drop(&mut self) {}
    }
    pub fn hitl_pending(_: &AgentLabels) -> HitlPending {
        HitlPending
    }
    pub fn record_hitl_decision(_: &AgentLabels, _: &str, _: Duration) {}
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
        init(false).expect("metrics init");
        init(false).expect("second init is a no-op");

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
        let labels = InferenceLabels::new(&AgentLabels::main("demo"), "bedrock", "claude");
        record_inference(&labels, None, Duration::from_millis(800));
        record_token_usage(
            &labels,
            TokenMeasure {
                input: 100,
                output: 20,
                cache_read: 40,
                cache_write: 0,
            },
        );
        record_mcp_operation(
            &AgentLabels::main("demo"),
            "mezmo",
            "search_logs",
            Some("tool_error"),
            Duration::from_secs(1),
        );
        record_tool_execution(
            &AgentLabels::main("demo"),
            "search_logs",
            None,
            Duration::from_secs(1),
        );
        record_scratchpad_intercept(&AgentLabels::main("demo"), "search_logs", 9000);
        record_scratchpad_extract(&AgentLabels::main("demo"), "head", 300);
        set_mcp_server_status("demo", "mezmo", true, 17);
        record_routing_decision("demo", "orchestrated");
        record_planning_cycles("demo", 2);
        record_task("demo", "logs", "completed", Duration::from_secs(4));
        let pending = hitl_pending(&AgentLabels::main("demo"));
        record_hitl_decision(
            &AgentLabels::main("demo"),
            "approved",
            Duration::from_secs(30),
        );

        let text = render_prometheus().expect("render");

        for needle in [
            r#"http_server_request_duration_seconds_count{http_request_method="POST",http_response_status_code="200",http_route="/v1/chat/completions"} 1"#,
            r#"aura_runs_active{aura_ingress="http"} 1"#,
            r#"gen_ai_invoke_agent_duration_seconds_count{aura_agent="demo",aura_ingress="http",aura_mode="single",aura_role="main",error_type="timeout",gen_ai_agent_name="demo"} 1"#,
            r#"gen_ai_invoke_agent_inference_calls_count{aura_agent="demo",aura_role="main",gen_ai_agent_name="demo"} 1"#,
            r#"gen_ai_client_inference_duration_seconds_count{aura_agent="demo",gen_ai_agent_name="demo",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="claude"} 1"#,
            r#"gen_ai_client_inference_usage_input_tokens_total{aura_agent="demo",gen_ai_agent_name="demo",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="claude",gen_ai_token_modality="text"} 100"#,
            r#"gen_ai_client_inference_usage_cache_read_input_tokens_total{aura_agent="demo",gen_ai_agent_name="demo",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="claude",gen_ai_token_modality="text"} 40"#,
            r#"gen_ai_client_inference_operation_input_tokens_sum{aura_agent="demo",gen_ai_agent_name="demo",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="claude"} 100"#,
            r#"mcp_client_operation_duration_seconds_count{aura_agent="demo",aura_mcp_server="mezmo",error_type="tool_error",gen_ai_agent_name="demo",gen_ai_tool_name="search_logs",mcp_method_name="tools/call"} 1"#,
            r#"gen_ai_execute_tool_duration_seconds_count{aura_agent="demo",gen_ai_agent_name="demo",gen_ai_tool_name="search_logs"} 1"#,
            r#"aura_scratchpad_intercepted_tokens_sum{aura_agent="demo",gen_ai_agent_name="demo",gen_ai_tool_name="search_logs"} 9000"#,
            r#"aura_scratchpad_extracted_tokens_count{aura_agent="demo",gen_ai_agent_name="demo",gen_ai_tool_name="head"} 1"#,
            r#"aura_mcp_server_connected{aura_agent="demo",aura_mcp_server="mezmo",gen_ai_agent_name="demo"} 1"#,
            r#"aura_mcp_server_tools{aura_agent="demo",aura_mcp_server="mezmo",gen_ai_agent_name="demo"} 17"#,
            r#"aura_orchestration_routing_total{aura_agent="demo",aura_decision="orchestrated",gen_ai_agent_name="demo"} 1"#,
            r#"aura_orchestration_planning_cycles_sum{aura_agent="demo",gen_ai_agent_name="demo"} 2"#,
            r#"aura_orchestration_task_duration_seconds_count{aura_agent="demo",aura_outcome="completed",aura_worker="logs",gen_ai_agent_name="demo"} 1"#,
            r#"aura_hitl_pending{aura_agent="demo",gen_ai_agent_name="demo"} 1"#,
            r#"aura_hitl_decision_duration_seconds_count{aura_agent="demo",aura_decision="approved",gen_ai_agent_name="demo"} 1"#,
            r#"target_info{service_name="aura""#,
        ] {
            assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
        }
        // Cache-write tokens were zero, so that counter has no series.
        assert!(!text.contains("gen_ai_client_inference_usage_cache_write_input_tokens_total{"));
        assert!(!text.contains("otel_scope_name"), "scope info is dropped");

        // The pending approval leaves the gauge when its guard drops.
        drop(pending);
        let text = render_prometheus().expect("render");
        assert!(
            text.contains(r#"aura_hitl_pending{aura_agent="demo",gen_ai_agent_name="demo"} 0"#),
            "pending gauge must return to zero in:\n{text}"
        );
    }

    /// A turn that reports no usage leaves the token distributions alone,
    /// while the counters still take its zeros without effect.
    #[test]
    fn empty_usage_adds_no_distribution_sample() {
        init(false).expect("metrics init");
        let labels = InferenceLabels::new(&AgentLabels::main("silent"), "openai", "gpt");
        record_token_usage(
            &labels,
            TokenMeasure {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            },
        );
        let text = render_prometheus().expect("render");
        assert!(
            !text.contains(
                r#"gen_ai_client_inference_operation_input_tokens_count{aura_agent="silent",gen_ai_agent_name="silent""#
            ),
            "no distribution sample for an empty turn in:\n{text}"
        );
    }

    #[test]
    fn declared_config_starts_its_counters_at_zero() {
        init(false).expect("metrics init");
        let config: aura_config::Config = toml::from_str(
            r#"
[agent]
name = "declared"
system_prompt = "declare"

[agent.llm]
provider = "bedrock"
model = "main-model"
region = "us-east-1"

[orchestration]
enabled = true

[orchestration.worker.scout]
description = "looks"
preamble = "look"

[orchestration.worker.scribe]
description = "writes"
preamble = "write"

[orchestration.worker.scribe.llm]
provider = "bedrock"
model = "small-model"
region = "us-east-1"
"#,
        )
        .expect("config");
        declare_config(&config);
        let text = render_prometheus().expect("render");
        for needle in [
            r#"gen_ai_client_inference_usage_input_tokens_total{aura_agent="declared",gen_ai_agent_name="declared",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="main-model",gen_ai_token_modality="text"} 0"#,
            r#"gen_ai_client_inference_usage_output_tokens_total{aura_agent="declared",gen_ai_agent_name="scout",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="main-model",gen_ai_token_modality="text"} 0"#,
            r#"gen_ai_client_inference_usage_input_tokens_total{aura_agent="declared",gen_ai_agent_name="scribe",gen_ai_operation_name="chat",gen_ai_provider_name="aws.bedrock",gen_ai_request_model="small-model",gen_ai_token_modality="text"} 0"#,
            r#"aura_orchestration_routing_total{aura_agent="declared",aura_decision="coordinator_error",gen_ai_agent_name="declared"} 0"#,
            r#"aura_orchestration_routing_total{aura_agent="declared",aura_decision="direct_answer",gen_ai_agent_name="declared"} 0"#,
        ] {
            assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
        }
        assert!(
            !text.contains(
                r#"gen_ai_client_inference_usage_cache_read_tokens_total{aura_agent="declared""#
            ),
            "cache counters wait for a provider that reports them"
        );
    }

    /// First calls from several threads must end on one registry: the
    /// instruments every later record uses have to feed the registry the
    /// endpoint renders, whichever caller built them.
    #[test]
    fn concurrent_first_calls_serve_one_registry() {
        let callers: Vec<_> = (0..8).map(|_| std::thread::spawn(|| init(false))).collect();
        for caller in callers {
            caller.join().expect("init thread").expect("metrics init");
        }
        record_routing_decision("init-race", "direct_answer");
        let text = render_prometheus().expect("render");
        assert!(
            text.contains(
                r#"aura_orchestration_routing_total{aura_agent="init-race",aura_decision="direct_answer",gen_ai_agent_name="init-race"} 1"#
            ),
            "a record after concurrent init must reach the served registry in:\n{text}"
        );
    }
}
