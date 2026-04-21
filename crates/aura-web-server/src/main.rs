use actix_web::{App, HttpResponse, HttpServer, middleware, web};
use aura_config::load_config;
use clap::Parser;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

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
    #[arg(long, env = "TOOL_RESULT_MAX_LENGTH", default_value = "100")]
    tool_result_max_length: usize,

    /// Streaming buffer size - number of chunks to buffer before backpressure
    /// Higher values use more memory but reduce latency, lower values are safer for many connections
    #[arg(long, env = "STREAMING_BUFFER_SIZE", default_value = "400")]
    streaming_buffer_size: usize,

    /// Enable Aura custom SSE events (aura.tool_requested, aura.tool_start, aura.tool_complete, etc.)
    /// These are emitted alongside OpenAI-compatible chunks for enhanced client UX
    #[arg(long, env = "AURA_CUSTOM_EVENTS", default_value = "false", action = clap::ArgAction::Set)]
    aura_custom_events: bool,

    /// Enable reasoning event emission (aura.reasoning)
    /// Only effective when aura_custom_events is also enabled
    #[arg(long, env = "AURA_EMIT_REASONING", default_value = "false", action = clap::ArgAction::Set)]
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
    /// Default: 30 seconds (much shorter than the full streaming timeout).
    #[arg(long, env = "FIRST_CHUNK_TIMEOUT_SECS", default_value = "30")]
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
}

/// Middleware that rejects new requests with 503 when shutdown_token is cancelled.
async fn shutdown_guard(
    data: web::Data<AppState>,
    req: actix_web::dev::ServiceRequest,
    next: actix_web::middleware::Next<impl actix_web::body::MessageBody + 'static>,
) -> Result<actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>, actix_web::Error> {
    if data.shutdown_token.is_cancelled() {
        let response = HttpResponse::ServiceUnavailable().json(ErrorResponse {
            error: ErrorDetail {
                message: "Server is shutting down".to_string(),
                error_type: "service_unavailable".to_string(),
            },
        });
        return Ok(req.into_response(response).map_into_right_body());
    }
    next.call(req)
        .await
        .map(actix_web::dev::ServiceResponse::map_into_left_body)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let result = run().await;
    aura::logging::shutdown_tracer().await;
    result
}

async fn run() -> std::io::Result<()> {
    let args = Args::parse();

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
        let (provider, model) = config.llm.model_info();
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

    // Two-phase shutdown: gate (immediate 503) → grace period → stream drain ([DONE])
    let shutdown_token = CancellationToken::new();
    let stream_shutdown_token = CancellationToken::new();
    let active_requests = Arc::new(ActiveRequestTracker::new());

    let shutdown_timeout_secs = args.shutdown_timeout_secs;

    // Create app state
    let app_state = web::Data::new(AppState {
        configs: configs_arc,
        tool_result_mode: args.tool_result_mode,
        tool_result_max_length: args.tool_result_max_length,
        streaming_buffer_size: args.streaming_buffer_size,
        aura_custom_events: args.aura_custom_events,
        aura_emit_reasoning: args.aura_emit_reasoning,
        streaming_timeout_secs: args.streaming_timeout_secs,
        first_chunk_timeout_secs: args.first_chunk_timeout_secs,
        shutdown_token: shutdown_token.clone(),
        stream_shutdown_token: stream_shutdown_token.clone(),
        active_requests: active_requests.clone(),
        default_agent: args.default_agent.clone(),
        additional_tools: Arc::new(Vec::new),
    });

    info!(
        "Starting server on {}:{} (shutdown_timeout={}s)",
        args.host, args.port, shutdown_timeout_secs
    );

    // Custom signal handling: CancellationToken bridges Actix and SSE stream lifecycles
    let server = HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .wrap(middleware::from_fn(shutdown_guard))
            .wrap(middleware::Logger::default())
            .route("/health", web::get().to(handlers::health))
            .route("/v1/models", web::get().to(handlers::list_models))
            .route(
                "/v1/chat/completions",
                web::post().to(handlers::chat_completions),
            )
    })
    .bind((args.host.as_str(), args.port))?
    .disable_signals()
    // Buffer for Phase 2 cleanup ([DONE] send + MCP cancellation) after grace period
    .shutdown_timeout(10)
    .run();

    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    let server_handle = server.handle();
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

            server_handle.stop(true).await;
        }
    });

    server.await
}
