//! Web-server entry point, shared by every binary that can launch the server.

use aura::instance_id::instance_id as compute_instance_id;
use aura_config::load_config;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{
    Router, middleware,
    routing::{get, post},
};
use clap::{CommandFactory, FromArgMatches, Parser};
use std::ffi::OsString;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing::{error, info};

use crate::a2a::{
    AuraAgentExecutor, AuraRequestHandler, BusBridgedExecutor, SharedTaskStore, agent_card_router,
    legacy_jsonrpc_router,
};
use crate::handlers;
use crate::session_store::{SessionStore, build_session_store};
use crate::streaming::ToolResultMode;
use crate::types::{ActiveRequestTracker, AppState, ErrorDetail, ErrorResponse};

/// Command-line and environment configuration for the web server.
#[derive(Parser, Debug)]
#[command(author, version, about = "AURA OpenAI-compatible web server", long_about = None)]
pub struct ServerArgs {
    /// Path to the configuration file
    #[arg(short, long, env = "CONFIG_PATH", default_value = "config.toml")]
    pub config: String,

    /// Host to bind to
    #[arg(long, env = "HOST", default_value = "127.0.0.1")]
    pub host: String,

    /// Port to bind to
    #[arg(short, long, env = "PORT", default_value = "8080")]
    pub port: u16,

    /// Verbose output (enables INFO level logging)
    #[arg(short, long)]
    pub verbose: bool,

    /// Debug output (enables DEBUG level logging for all rig crates)
    #[arg(short, long)]
    pub debug: bool,

    /// Tool result streaming mode (default: none)
    /// - none: Spec-compliant, no streaming (results in LLM summary only)
    /// - open-web-ui: Stream via tool_calls for OpenWebUI "View Results" UI
    /// - aura: Stream via aura.tool_complete SSE events (requires aura_custom_events)
    #[arg(long, env = "TOOL_RESULT_MODE", default_value = "none")]
    pub tool_result_mode: ToolResultMode,

    /// Maximum length for tool results in streaming (0 = no truncation)
    /// Results exceeding this will be truncated with "... [truncated]" suffix
    #[arg(long, env = "TOOL_RESULT_MAX_LENGTH", default_value = "1000")]
    pub tool_result_max_length: usize,

    /// Streaming buffer size - number of chunks to buffer before backpressure
    /// Higher values use more memory but reduce latency, lower values are safer for many connections
    #[arg(long, env = "STREAMING_BUFFER_SIZE", default_value = "400")]
    pub streaming_buffer_size: usize,

    /// Enable Aura custom SSE events (aura.tool_requested, aura.tool_start, aura.tool_complete, etc.)
    /// These are emitted alongside OpenAI-compatible chunks for enhanced client UX.
    /// Accepts the canonical boolean vocabulary (1/0, true/false, yes/no, on/off, t/f, y/n).
    #[arg(
        long,
        env = "AURA_CUSTOM_EVENTS",
        default_value = "false",
        action = clap::ArgAction::Set,
        value_parser = clap::builder::BoolishValueParser::new(),
    )]
    pub aura_custom_events: bool,

    /// Enable reasoning event emission (aura.reasoning).
    /// Only effective when aura_custom_events is also enabled.
    /// Accepts the canonical boolean vocabulary (1/0, true/false, yes/no, on/off, t/f, y/n).
    #[arg(
        long,
        env = "AURA_EMIT_REASONING",
        default_value = "false",
        action = clap::ArgAction::Set,
        value_parser = clap::builder::BoolishValueParser::new(),
    )]
    pub aura_emit_reasoning: bool,

    /// Dev-only: surface the raw upstream provider error to clients on failure.
    /// Leave OFF for public-facing deployments — provider error bodies can echo
    /// request content. When off, clients get a generic message; the raw error
    /// is always available in server logs/OTel regardless.
    /// Accepts the canonical boolean vocabulary (1/0, true/false, yes/no, on/off, t/f, y/n).
    #[arg(
        long,
        env = "AURA_DEBUG_PROVIDER_ERRORS",
        default_value = "false",
        action = clap::ArgAction::Set,
        value_parser = clap::builder::BoolishValueParser::new(),
    )]
    pub debug_provider_errors: bool,

    /// SSE streaming request timeout in seconds.
    /// This is the maximum time a streaming request can run before being cancelled.
    /// Set higher for long-running tool operations (e.g., log analysis).
    /// Set to 0 to disable timeout (not recommended for production).
    #[arg(long, env = "STREAMING_TIMEOUT_SECS", default_value = "900")]
    pub streaming_timeout_secs: u64,

    /// First chunk timeout in seconds.
    /// Maximum time to wait for the first chunk from the LLM provider before
    /// treating the connection as hung. Protects against non-streaming error
    /// responses that leave the connection open. Set to 0 to disable.
    /// Default: 90 seconds. Allows for slower providers (Gemini, local
    /// models) and extended-thinking warm-up time.
    #[arg(long, env = "FIRST_CHUNK_TIMEOUT_SECS", default_value = "90")]
    pub first_chunk_timeout_secs: u64,

    /// Inactivity timeout in seconds (streaming requests only).
    /// Maximum silence between stream items after the first chunk before the
    /// stream is failed. Single-agent tool execution is exempt; orchestrated
    /// worker tools are bounded by the TOML stream_inactivity_timeout_secs instead,
    /// so size this window above it for orchestrated configs. Set to 0 to
    /// disable.
    /// Note: some providers emit nothing during long mid-stream reasoning
    /// phases; the window must exceed the longest such gap, not just startup
    /// latency.
    #[arg(long, env = "STREAM_INACTIVITY_TIMEOUT_SECS", default_value = "0")]
    pub stream_inactivity_timeout_secs: u64,

    /// Graceful shutdown timeout in seconds.
    /// On SIGTERM/SIGINT, new requests are rejected immediately (503), but in-flight
    /// streaming requests are given this long to finish naturally before being terminated.
    /// Default: 30 seconds
    #[arg(long, env = "SHUTDOWN_TIMEOUT_SECS", default_value = "30")]
    pub shutdown_timeout_secs: u64,

    /// Default agent name or alias, used when `model` is omitted from the request.
    /// Not required when only one configuration is loaded via CONFIG_PATH.
    #[arg(long, env = "DEFAULT_AGENT")]
    pub default_agent: Option<String>,

    /// Canonical, externally-reachable base URL of this server (e.g.
    /// `https://aura.example.com`). It is published in the A2A agent card's
    /// interface endpoints — A2A clients require absolute URLs and pass them
    /// straight to their HTTP layer. When unset, this is derived from
    /// --host/--port (0.0.0.0 / :: mapped to 127.0.0.1), which is fine for local
    /// use but should be set explicitly behind a proxy or in K8s. Integration
    /// tests reuse this same value to know where to reach the server.
    #[arg(long, env = "AURA_SERVER_URL")]
    pub server_url: Option<String>,

    /// Enable the A2A (Agent-to-Agent) server interface.
    /// Exposes JSON-RPC at /a2a/v1/rpc, REST at /a2a/v1/, and agent card at
    /// /.well-known/agent-card.json. Disabled by default.
    #[arg(long, env = "AURA_ENABLE_A2A", action = clap::ArgAction::SetTrue)]
    pub enable_a2a: bool,
}

/// Parse `argv` (leading program path ignored), titling `--help`/`--version`
/// with `name` and usage lines with `bin_name`. Exits like [`Parser::parse`].
pub fn parse_args<I, T>(argv: I, name: &'static str, bin_name: &'static str) -> ServerArgs
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = ServerArgs::command()
        .name(name)
        .bin_name(bin_name)
        .get_matches_from(argv);
    ServerArgs::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// Resolve the externally-advertised base URL for the A2A agent card.
///
/// A2A clients reject relative interface URLs, so the card must carry an absolute
/// origin. Prefer an explicit `--server-url`; otherwise derive one from the bind
/// host/port, mapping wildcard binds to a loopback address since `0.0.0.0` is not
/// a routable destination.
fn advertised_base_url(server_url: Option<&str>, host: &str, port: u16) -> String {
    if let Some(url) = server_url {
        return url.trim_end_matches('/').to_string();
    }
    let host = match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };
    format!("http://{host}:{port}")
}

/// Middleware that rejects new requests with 503 when shutdown_token is cancelled.
async fn shutdown_guard(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if state.shutdown_token.is_cancelled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: ErrorDetail {
                    message: "Server is shutting down".to_string(),
                    error_type: "service_unavailable".to_string(),
                },
            }),
        )
            .into_response();
    }
    next.run(request).await
}

/// Startup warnings relating the per-request budgets (CLI/env) to an agent's
/// orchestration timeouts (TOML). `aura-config` validation sees only the TOML
/// layer, so cross-layer checks live here.
fn warn_timeout_relationships(agent_id: &str, config: &aura_config::Config, args: &ServerArgs) {
    let Some(orch) = config.orchestration.as_ref().filter(|o| o.enabled) else {
        return;
    };
    let per_call = orch.timeouts.per_call_timeout_secs;
    let toml_inactivity = orch.timeouts.stream_inactivity_timeout_secs;
    let streaming = args.streaming_timeout_secs;
    let server_inactivity = args.stream_inactivity_timeout_secs;

    if let Some(msg) = per_call_vs_streaming_warning(per_call, streaming) {
        tracing::warn!("agent '{agent_id}': {msg}");
    }
    if let Some(msg) = server_window_shadows_tools_warning(server_inactivity, toml_inactivity) {
        tracing::warn!("agent '{agent_id}': {msg}");
    }
    if let Some(msg) = server_inactivity_vs_orchestration_warning(server_inactivity) {
        tracing::warn!("agent '{agent_id}': {msg}");
    }
    if let Some(hitl) = config.hitl.as_ref() {
        let route_timeout = match &hitl.route {
            aura_config::DecisionRouteConfig::Webhook { timeout_secs, .. } => *timeout_secs,
            aura_config::DecisionRouteConfig::Conversational { timeout_secs, .. } => *timeout_secs,
        };
        if let Some(msg) = hitl_route_vs_server_window_warning(route_timeout, server_inactivity) {
            tracing::warn!("agent '{agent_id}': {msg}");
        }
    }
}

/// Warn when a single worker task cannot finish inside the request budget.
fn per_call_vs_streaming_warning(per_call: u64, streaming: u64) -> Option<String> {
    if streaming > 0 && per_call >= streaming {
        return Some(format!(
            "per_call_timeout_secs ({per_call}s) >= streaming timeout ({streaming}s); a single worker task cannot finish inside the request budget"
        ));
    }
    None
}

/// Warn when the server inactivity window fires before (or instead of) the
/// TOML window that exempts orchestrated worker tools.
fn server_window_shadows_tools_warning(
    server_inactivity: u64,
    toml_inactivity: u64,
) -> Option<String> {
    if server_inactivity > 0 && (toml_inactivity == 0 || server_inactivity <= toml_inactivity) {
        return Some(format!(
            "the server inactivity window ({server_inactivity}s) does not exempt orchestrated worker tools; set [orchestration.timeouts].stream_inactivity_timeout_secs below it (currently {toml_inactivity}s) so the inner window, which does exempt tools, fires first"
        ));
    }
    None
}

/// Warn when the server-level inactivity timeout is set for an orchestration agent.
///
/// The server-layer deadline re-arms on tool and progress events but cannot
/// suspend during tool execution — only stream items carry the suspend signal.
/// A long MCP tool call will trip the server deadline even when the TOML
/// deadline is correctly suspended.
fn server_inactivity_vs_orchestration_warning(server_inactivity: u64) -> Option<String> {
    if server_inactivity > 0 {
        return Some(format!(
            "STREAM_INACTIVITY_TIMEOUT_SECS ({server_inactivity}s) is set for an orchestrated agent; \
             the server-level inactivity deadline does not suspend during MCP tool calls — a long \
             tool call can trip it. Use [orchestration.timeouts].stream_inactivity_timeout_secs instead, \
             which suspends correctly. Set STREAM_INACTIVITY_TIMEOUT_SECS=0 for orchestration deployments."
        ));
    }
    None
}

/// Warn when a parked HITL approval can outlive the server inactivity window.
fn hitl_route_vs_server_window_warning(
    route_timeout: u64,
    server_inactivity: u64,
) -> Option<String> {
    if server_inactivity > 0 && route_timeout >= server_inactivity {
        return Some(format!(
            "hitl route timeout ({route_timeout}s) >= server inactivity window ({server_inactivity}s); approvals parked during orchestrated runs may be killed as stalls"
        ));
    }
    None
}

/// Serve until SIGINT/SIGTERM, then drain in-flight streams and flush spans.
pub async fn serve(args: ServerArgs) -> std::io::Result<()> {
    let result = run(args).await;
    aura::logging::shutdown_tracer().await;
    result
}

async fn run(args: ServerArgs) -> std::io::Result<()> {
    // Load .env from the working directory before resolving config templates, so
    // {{ env.* }} references work without manual exporting (parity with the
    // Docker quickstart's `env_file: .env`). Shell exports take precedence; an
    // absent .env is not an error.
    dotenvy::dotenv().ok();

    // Initialize logging using shared module
    aura::logging::init_logging(args.debug, args.verbose, "aura_web_server");

    info!("Starting Aura Web Server v{}", env!("CARGO_PKG_VERSION"));
    info!("Loading configuration from: {}", args.config);

    let configs = match load_config(&args.config) {
        Ok(cfgs) => cfgs,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Configuration error: {e}"),
            ));
        }
    };

    for config in &configs {
        let id = config.agent.alias.as_deref().unwrap_or(&config.agent.name);
        let (provider, model) = config.agent.llm.model_info();
        let iid = compute_instance_id(&config.agent);
        info!(
            "Loaded agent '{}' ({}/{}) [instance_id={}]",
            id, provider, model, iid
        );
        warn_timeout_relationships(id, config, &args);
    }

    // Validate DEFAULT_AGENT matches a loaded config
    if let Some(ref default_agent) = args.default_agent {
        let exists = configs
            .iter()
            .any(|c| c.agent.alias.as_deref().unwrap_or(&c.agent.name) == default_agent);
        if !exists {
            error!(
                "DEFAULT_AGENT '{}' does not match any loaded agent name or alias",
                default_agent
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "DEFAULT_AGENT '{}' does not match any loaded agent name or alias",
                    default_agent
                ),
            ));
        }
        info!("Default agent: '{}'", default_agent);
    }

    let configs_arc = Arc::new(configs);

    // Two-phase shutdown: gate (immediate 503) → grace period → stream drain ([DONE])
    let shutdown_token = CancellationToken::new();
    let stream_shutdown_token = CancellationToken::new();
    let active_requests = Arc::new(ActiveRequestTracker::new());

    let shutdown_timeout_secs = args.shutdown_timeout_secs;

    // Deployment-scoped, env-only configuration (AURA_SESSION_STORE*).
    let session_store_config = aura_config::SessionStoreConfig::from_env().map_err(|e| {
        error!("Invalid session store configuration: {e}");
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("session store configuration error: {e}"),
        )
    })?;
    let session_store: Arc<dyn SessionStore> = build_session_store(&session_store_config)
        .await
        .map_err(|e| {
            error!("Failed to build session store: {e}");
            std::io::Error::other(format!("session store error: {e}"))
        })?;
    // Fail fast at startup if the session-state backend is unreachable.
    if let Err(e) = session_store.ping().await {
        error!("Session store unreachable: {e}");
        return Err(std::io::Error::other(format!(
            "session store unreachable: {e}"
        )));
    }
    info!("Session store backend: {}", session_store.backend());

    // HITL webhook HMAC (AURA_HITL_WEBHOOK_SECRET*): fail startup loud on a
    // misconfiguration instead of silently serving unsigned, unverified
    // traffic. One load serves both legs: egress signing via AppState,
    // ingress verification via the IngressHmac extension.
    let ingress_hmac = aura::hitl::WebhookHmac::load_from_env().map_err(|e| {
        error!("Invalid HITL webhook HMAC configuration: {e}");
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("HITL webhook HMAC configuration error: {e}"),
        )
    })?;
    // With a secret configured, a plaintext webhook URL fails at boot rather
    // than on the first approval request. Cleartext response-header capture
    // (a usable, unsigned configuration) warns here too, once per config —
    // not from `WebhookClient` construction, which runs fresh on every
    // request that builds an agent.
    for config in configs_arc.iter() {
        if let Some(hitl) = &config.hitl {
            aura::hitl::validate_webhook_signing_config(hitl, ingress_hmac.as_ref()).map_err(
                |e| {
                    error!("Invalid HITL webhook configuration: {e}");
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("HITL webhook configuration error: {e}"),
                    )
                },
            )?;
            aura::hitl::warn_on_cleartext_capture(hitl);
        }
    }

    let app_state = Arc::new(AppState {
        configs: configs_arc,
        tool_result_mode: args.tool_result_mode,
        tool_result_max_length: args.tool_result_max_length,
        streaming_buffer_size: args.streaming_buffer_size,
        aura_custom_events: args.aura_custom_events,
        aura_emit_reasoning: args.aura_emit_reasoning,
        debug_provider_errors: args.debug_provider_errors,
        streaming_timeout_secs: args.streaming_timeout_secs,
        first_chunk_timeout_secs: args.first_chunk_timeout_secs,
        stream_inactivity_timeout_secs: args.stream_inactivity_timeout_secs,
        shutdown_token: shutdown_token.clone(),
        stream_shutdown_token: stream_shutdown_token.clone(),
        active_requests: active_requests.clone(),
        default_agent: args.default_agent.clone(),
        additional_tools: Arc::new(Vec::new),
        pending_approvals: aura::hitl::PendingApprovals::with_backend(
            session_store.approvals(),
            session_store.bus(),
        ),
        hitl_webhook_hmac: ingress_hmac.clone(),
        session_store: session_store.clone(),
    });

    info!(
        "Starting server on {}:{} (shutdown_timeout={}s)",
        args.host, args.port, shutdown_timeout_secs
    );

    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/aura/info", get(handlers::info))
        .route("/v1/models", get(handlers::list_models))
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route(
            "/v1/approvals/{decision_id}",
            post(handlers::resolve_approval),
        )
        .layer(axum::extract::Extension(handlers::IngressHmac(
            ingress_hmac,
        )))
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            shutdown_guard,
        ))
        .with_state(app_state.clone());

    // Build the A2A router only when explicitly enabled.
    // A2A server:
    // JSON-RPC at /a2a/v1/rpc
    // REST at /a2a/v1/message:send, /a2a/v1/tasks/
    // v0.3 JSON-RPC at /
    // Agent card at /.well-known/agent-card.json
    let app = if args.enable_a2a {
        let task_store = SharedTaskStore::from_store(session_store.tasks());
        let executor = AuraAgentExecutor::new(app_state.clone(), task_store.clone());
        let base_url = advertised_base_url(args.server_url.as_deref(), &args.host, args.port);
        let agent_card = executor.build_agent_card(&base_url);
        // Bridge execution events and cancels over the session-store bus so
        // subscribe/cancel work across instances.
        let executor = BusBridgedExecutor::new(executor, session_store.bus());
        let a2a_handler = Arc::new(AuraRequestHandler::new(
            executor,
            task_store,
            session_store.bus(),
        ));
        let a2a_router = Router::new()
            .nest(
                "/a2a/v1/rpc",
                a2a_server::jsonrpc::jsonrpc_router(a2a_handler.clone()),
            )
            .merge(legacy_jsonrpc_router(a2a_handler.clone()))
            .nest("/a2a/v1", a2a_server::rest::rest_router(a2a_handler))
            .merge(agent_card_router(agent_card))
            .layer(tower_http::timeout::TimeoutLayer::with_status_code(
                axum::http::StatusCode::REQUEST_TIMEOUT,
                std::time::Duration::from_secs(120),
            ));

        app.merge(a2a_router)
    } else {
        app
    };

    let listener = tokio::net::TcpListener::bind(format!("{}:{}", args.host, args.port)).await?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    tokio::spawn({
        let shutdown_token = shutdown_token.clone();
        let stream_shutdown_token = stream_shutdown_token.clone();
        let active_requests = active_requests.clone();
        async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received SIGINT, initiating graceful shutdown");
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, initiating graceful shutdown");
                }
            }

            // Phase 1: reject new requests (middleware returns 503)
            shutdown_token.cancel();

            info!(
                "Allowing {}s for in-flight requests to complete",
                shutdown_timeout_secs
            );
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(shutdown_timeout_secs)) => {
                    info!("Grace period expired, terminating remaining streams");
                }
                _ = active_requests.wait_for_drain() => {
                    info!("All in-flight requests completed, shutting down early");
                }
            }

            // Phase 2: terminate remaining streams ([DONE] → MCP cleanup)
            stream_shutdown_token.cancel();

            let _ = shutdown_tx.send(());
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            shutdown_rx.await.ok();
        })
        .await
}

#[cfg(test)]
mod timeout_warning_tests {
    use super::*;

    #[test]
    fn per_call_vs_streaming_boundaries() {
        assert!(per_call_vs_streaming_warning(900, 900).is_some());
        assert!(per_call_vs_streaming_warning(901, 900).is_some());
        assert!(per_call_vs_streaming_warning(899, 900).is_none());
        assert!(per_call_vs_streaming_warning(900, 0).is_none());
    }

    #[test]
    fn server_window_shadows_tools_boundaries() {
        assert!(server_window_shadows_tools_warning(30, 0).is_some());
        assert!(server_window_shadows_tools_warning(30, 30).is_some());
        assert!(server_window_shadows_tools_warning(30, 60).is_some());
        assert!(server_window_shadows_tools_warning(30, 20).is_none());
        assert!(server_window_shadows_tools_warning(0, 0).is_none());
        assert!(server_window_shadows_tools_warning(0, 60).is_none());
    }

    #[test]
    fn hitl_route_vs_server_window_boundaries() {
        assert!(hitl_route_vs_server_window_warning(300, 300).is_some());
        assert!(hitl_route_vs_server_window_warning(600, 300).is_some());
        assert!(hitl_route_vs_server_window_warning(299, 300).is_none());
        assert!(hitl_route_vs_server_window_warning(600, 0).is_none());
    }

    #[test]
    fn server_inactivity_vs_orchestration_boundaries() {
        assert!(server_inactivity_vs_orchestration_warning(30).is_some());
        assert!(server_inactivity_vs_orchestration_warning(1).is_some());
        assert!(server_inactivity_vs_orchestration_warning(0).is_none());
    }
}

#[cfg(test)]
mod parse_args_tests {
    use super::*;

    #[test]
    fn parses_explicit_argv() {
        let args = parse_args(
            ["aura", "--config", "agents.toml", "--port", "9999"],
            "aura",
            "aura webserver",
        );
        assert_eq!(args.config, "agents.toml");
        assert_eq!(args.port, 9999);
    }

    #[test]
    fn version_matches_package_version() {
        let rendered = ServerArgs::command()
            .name("aura")
            .render_version()
            .to_string();
        assert_eq!(
            rendered.trim(),
            format!("aura {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn usage_reflects_bin_name() {
        let usage = ServerArgs::command()
            .name("aura")
            .bin_name("aura webserver")
            .render_usage()
            .to_string();
        assert!(usage.contains("aura webserver"), "usage was: {usage}");
    }
}
