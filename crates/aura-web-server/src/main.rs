use a2a_server::StaticAgentCard;
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
use clap::Parser;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing::{error, info};

use aura_web_server::a2a::{AuraAgentExecutor, AuraRequestHandler, SharedTaskStore};
use aura_web_server::handlers;
use aura_web_server::streaming;
use aura_web_server::types;

use streaming::ToolResultMode;
use types::{ActiveRequestTracker, AppState, ErrorDetail, ErrorResponse};

/// CLI arguments for the web server
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, env = "CONFIG_PATH", default_value = "config.toml")]
    config: String,

    /// Host to bind to
    #[arg(long, env = "HOST", default_value = "127.0.0.1")]
    host: String,

    /// Port to bind to
    #[arg(short, long, env = "PORT", default_value = "8080")]
    port: u16,

    /// Verbose output (enables INFO level logging)
    #[arg(short, long)]
    verbose: bool,

    /// Debug output (enables DEBUG level logging for all rig crates)
    #[arg(short, long)]
    debug: bool,

    /// Tool result streaming mode (default: none)
    /// - none: Spec-compliant, no streaming (results in LLM summary only)
    /// - open-web-ui: Stream via tool_calls for OpenWebUI "View Results" UI
    /// - aura: Stream via aura.tool_complete SSE events (requires aura_custom_events)
    #[arg(long, env = "TOOL_RESULT_MODE", default_value = "none")]
    tool_result_mode: ToolResultMode,

    /// Maximum length for tool results in streaming (0 = no truncation)
    /// Results exceeding this will be truncated with "... [truncated]" suffix
    #[arg(long, env = "TOOL_RESULT_MAX_LENGTH", default_value = "1000")]
    tool_result_max_length: usize,

    /// Streaming buffer size - number of chunks to buffer before backpressure
    /// Higher values use more memory but reduce latency, lower values are safer for many connections
    #[arg(long, env = "STREAMING_BUFFER_SIZE", default_value = "400")]
    streaming_buffer_size: usize,

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
    aura_custom_events: bool,

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
    aura_emit_reasoning: bool,

    /// SSE streaming request timeout in seconds.
    /// This is the maximum time a streaming request can run before being cancelled.
    /// Set higher for long-running tool operations (e.g., log analysis).
    /// Set to 0 to disable timeout (not recommended for production).
    #[arg(long, env = "STREAMING_TIMEOUT_SECS", default_value = "900")]
    streaming_timeout_secs: u64,

    /// First chunk timeout in seconds.
    /// Maximum time to wait for the first chunk from the LLM provider before
    /// treating the connection as hung. Protects against non-streaming error
    /// responses that leave the connection open. Set to 0 to disable.
    /// Default: 90 seconds. Allows for slower providers (Gemini, local
    /// models) and extended-thinking warm-up time.
    #[arg(long, env = "FIRST_CHUNK_TIMEOUT_SECS", default_value = "90")]
    first_chunk_timeout_secs: u64,

    /// Graceful shutdown timeout in seconds.
    /// On SIGTERM/SIGINT, new requests are rejected immediately (503), but in-flight
    /// streaming requests are given this long to finish naturally before being terminated.
    /// Default: 30 seconds
    #[arg(long, env = "SHUTDOWN_TIMEOUT_SECS", default_value = "30")]
    shutdown_timeout_secs: u64,

    /// Default agent name or alias, used when `model` is omitted from the request.
    /// Not required when only one configuration is loaded via CONFIG_PATH.
    #[arg(long, env = "DEFAULT_AGENT")]
    default_agent: Option<String>,

    /// Canonical, externally-reachable base URL of this server (e.g.
    /// `https://aura.example.com`). It is published in the A2A agent card's
    /// interface endpoints — A2A clients require absolute URLs and pass them
    /// straight to their HTTP layer. When unset, this is derived from
    /// --host/--port (0.0.0.0 / :: mapped to 127.0.0.1), which is fine for local
    /// use but should be set explicitly behind a proxy or in K8s. Integration
    /// tests reuse this same value to know where to reach the server.
    #[arg(long, env = "AURA_SERVER_URL")]
    server_url: Option<String>,

    /// Enable the A2A (Agent-to-Agent) server interface.
    /// Exposes JSON-RPC at /a2a/v1/rpc, REST at /a2a/v1/, and agent card at
    /// /.well-known/agent-card.json. Disabled by default.
    #[arg(long, env = "AURA_ENABLE_A2A", action = clap::ArgAction::SetTrue)]
    enable_a2a: bool,
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

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let result = run().await;
    aura::logging::shutdown_tracer().await;
    result
}

async fn run() -> std::io::Result<()> {
    let args = Args::parse();

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
        info!("Loaded agent '{}' ({}/{})", id, provider, model);
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

    // Derive config directory from config file's parent directory.
    // Path::parent() on a bare filename like "config.toml" returns Some("") (empty path),
    // so we filter that out and fall back to "." for the current directory.
    let config_dir = std::path::Path::new(&args.config)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // Two-phase shutdown: gate (immediate 503) → grace period → stream drain ([DONE])
    let shutdown_token = CancellationToken::new();
    let stream_shutdown_token = CancellationToken::new();
    let active_requests = Arc::new(ActiveRequestTracker::new());

    let shutdown_timeout_secs = args.shutdown_timeout_secs;

    let app_state = Arc::new(AppState {
        configs: configs_arc,
        tool_result_mode: args.tool_result_mode,
        tool_result_max_length: args.tool_result_max_length,
        streaming_buffer_size: args.streaming_buffer_size,
        aura_custom_events: args.aura_custom_events,
        aura_emit_reasoning: args.aura_emit_reasoning,
        streaming_timeout_secs: args.streaming_timeout_secs,
        first_chunk_timeout_secs: args.first_chunk_timeout_secs,
        config_dir,
        shutdown_token: shutdown_token.clone(),
        stream_shutdown_token: stream_shutdown_token.clone(),
        active_requests: active_requests.clone(),
        default_agent: args.default_agent.clone(),
        additional_tools: Arc::new(Vec::new),
        pending_approvals: aura::hitl::PendingApprovals::new(),
    });

    info!(
        "Starting server on {}:{} (shutdown_timeout={}s)",
        args.host, args.port, shutdown_timeout_secs
    );

    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/v1/models", get(handlers::list_models))
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route(
            "/v1/approvals/{decision_id}",
            post(handlers::resolve_approval),
        )
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
    // Agent card at /.well-known/agent-card.json
    let app = if args.enable_a2a {
        // forcing an in-memory store for now. TBD: a resilient location
        let task_store = SharedTaskStore::new();
        let executor = AuraAgentExecutor::new(app_state.clone(), task_store.clone());
        let base_url = advertised_base_url(args.server_url.as_deref(), &args.host, args.port);
        let agent_card = executor.build_agent_card(&base_url);
        let a2a_handler = Arc::new(AuraRequestHandler::new(executor, task_store));
        let card_producer = Arc::new(StaticAgentCard::new(agent_card));
        let a2a_router = Router::new()
            .nest(
                "/a2a/v1/rpc",
                a2a_server::jsonrpc::jsonrpc_router(a2a_handler.clone()),
            )
            .nest("/a2a/v1", a2a_server::rest::rest_router(a2a_handler))
            .merge(a2a_server::agent_card::agent_card_router(card_producer))
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
