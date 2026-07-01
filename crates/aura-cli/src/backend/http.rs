use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;

use crate::api::client::ChatClient;
use crate::api::stream::{StreamHandler, StreamResult, process_stream};
use crate::api::types::{Message, ToolDefinition};
use crate::config::AppConfig;

/// HTTP/SSE backend — connects to an aura-web-server via HTTP.
///
/// This wraps the existing `ChatClient` + `process_stream` pattern.
/// Zero behavioral change from the original code.
pub struct HttpBackend {
    client: ChatClient,
}

impl HttpBackend {
    pub fn new(config: AppConfig) -> Self {
        Self {
            client: ChatClient::new(config),
        }
    }

    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        session_id: &str,
        cancel: Arc<AtomicBool>,
        handler: &mut impl StreamHandler,
    ) -> Result<StreamResult> {
        let response = self
            .client
            .send_streaming(messages, tools, session_id)
            .await?;
        process_stream(response, cancel, handler).await
    }

    pub async fn summarize(
        &self,
        text: &str,
        session_id: &str,
    ) -> Result<(String, Option<(u64, u64)>)> {
        self.client.summarize(text, session_id).await
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        self.client.list_models().await
    }

    /// Describe the connected server for the startup banner (base URL plus
    /// version when the server reports one).
    pub async fn connection_summary(&self) -> String {
        self.client.connection_summary().await
    }

    pub async fn worker_overviews(&self) -> crate::worker::WorkerOverviews {
        self.client.list_worker_overviews().await
    }
}
