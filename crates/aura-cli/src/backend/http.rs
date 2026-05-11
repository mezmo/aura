use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Result;

use crate::api::client::ChatClient;
use crate::api::stream::{StreamResult, process_stream};
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

    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        session_id: &str,
        cancel: Arc<AtomicBool>,
        on_token: impl FnMut(&str),
        on_tool_requested: impl FnMut(&str, &str, &BTreeMap<String, serde_json::Value>),
        on_tool_start: impl FnMut(&str, &str),
        on_tool_complete: impl FnMut(&str, &str, Duration, Option<&str>),
        on_usage: impl FnMut(u64, u64),
        on_raw_event: impl FnMut(&str, &str),
        on_orchestrator_event: impl FnMut(&str, &serde_json::Value),
    ) -> Result<StreamResult> {
        let response = self
            .client
            .send_streaming(messages, tools, session_id)
            .await?;
        process_stream(
            response,
            cancel,
            on_token,
            on_tool_requested,
            on_tool_start,
            on_tool_complete,
            on_usage,
            on_raw_event,
            on_orchestrator_event,
        )
        .await
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
}
