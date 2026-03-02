use thiserror::Error;

#[derive(Error, Debug)]
pub enum BuilderError {
    #[error("Invalid provider: {0}")]
    InvalidProvider(String),

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

    #[error("Other error: {0}")]
    Other(String),
}

pub type BuilderResult<T> = Result<T, BuilderError>;
