use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;

use crate::api::client::ChatClient;
pub use crate::api::resume::ResumeOutcome;
use crate::api::stream::{StreamHandler, StreamOutcome, process_stream};
use crate::api::types::{Message, ToolDefinition};
use crate::config::AppConfig;

/// HTTP/SSE backend — connects to an aura-web-server via HTTP.
///
/// This wraps the existing `ChatClient` + `process_stream` pattern.
/// Zero behavioral change from the original code.
pub struct HttpBackend {
    client: Box<ChatClient>,
}

impl HttpBackend {
    pub fn new(config: AppConfig) -> Self {
        Self {
            client: Box::new(ChatClient::new(config)),
        }
    }

    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        session_id: &str,
        cancel: Arc<AtomicBool>,
        handler: &mut impl StreamHandler,
    ) -> Result<StreamOutcome> {
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

    /// POST to the resume endpoint and, on a 200, stream the resumed
    /// run's SSE through `handler` with its termination classified. A
    /// refusal status decodes to the typed [`ResumeOutcome`] rows so the
    /// caller branches on the variant, never on strings.
    pub async fn stream_resume(
        &self,
        session_id: &str,
        run_id: &str,
        cancel: Arc<AtomicBool>,
        handler: &mut impl StreamHandler,
    ) -> Result<ResumeOutcome> {
        let response = self.client.send_resume(session_id, run_id).await?;
        if response.status() == reqwest::StatusCode::OK {
            let outcome: StreamOutcome = process_stream(response, cancel, handler).await?;
            Ok(ResumeOutcome::Streamed(Box::new(outcome)))
        } else {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            Ok(ResumeOutcome::from_refusal(status, &body))
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        self.client.list_models().await
    }

    /// Describe the connected server for the startup banner (base URL plus
    /// version when the server reports one).
    pub async fn connection_summary(&self) -> String {
        self.client.connection_summary().await
    }

    /// Fetch the selected agent overview from `GET /aura/info`.
    /// Returns `None` if the server is unreachable, doesn't support the
    /// endpoint, or the selected model cannot be resolved.
    pub async fn startup_agent_overview(&self) -> Option<aura_events::AgentInfo> {
        let info = self.client.server_info().await?;
        super::select_agent(info, crate::ui::prompt::get_selected_model().as_deref())
    }
}
