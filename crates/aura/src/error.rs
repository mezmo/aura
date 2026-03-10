use thiserror::Error;

#[derive(Error, Debug)]
pub enum SseTransportError {
    #[error("SSE stream error: {0}")]
    SseStream(#[from] sse_stream::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Missing Content-Type header from SSE endpoint")]
    MissingContentType,

    #[error("Unexpected Content-Type: expected 'text/event-stream', got '{0}'")]
    UnexpectedContentType(String),

    #[error("URL parse error: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("SSE endpoint event not received")]
    MissingEndpointEvent,
}

#[derive(Error, Debug)]
pub enum BuilderError {
    #[error("Invalid provider: {0}")]
    InvalidProvider(String),

    #[error("Config error: {0}")]
    ConfigError(#[from] aura_config::ConfigError),

    #[error("MCP initialization error: {0}")]
    McpInitError(String),

    #[error("Vector store error: {0}")]
    VectorStoreError(String),

    #[error("Tool creation error: {0}")]
    ToolError(String),

    #[error("Agent creation error: {0}")]
    AgentError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("RMCP error: {0}")]
    RmcpError(#[from] rmcp::ErrorData),

    #[error("SSE transport error: {0}")]
    SseTransport(#[from] SseTransportError),

    #[error("Other error: {0}")]
    Other(String),
}

pub type BuilderResult<T> = Result<T, BuilderError>;
