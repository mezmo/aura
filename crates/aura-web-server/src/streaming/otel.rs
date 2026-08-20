//! OTel recording context for the `agent.stream` span inside `tokio::spawn`.
//!
//! This struct bundles the cloned values that exist *only* for tracing purposes,
//! keeping them out of the business-logic variables. The two entry points mirror
//! the span lifecycle:
//!
//! - [`StreamOtelContext::record_input`] — called at span start
//! - [`StreamOtelContext::record_output`] — called after the stream ends

use aura::ResponseContent;

use super::StreamTermination;

/// OTel recording context for the `agent.stream` span inside `tokio::spawn`.
///
/// Bundles the cloned values that exist *only* for tracing purposes, keeping
/// them out of the business-logic variables. The two entry points mirror the
/// span lifecycle:
///
/// - [`record_input`](Self::record_input) — called at span start (provider, model, query, IDs)
/// - [`record_output`](Self::record_output) — called after the stream ends (usage, content, status)
pub struct StreamOtelContext {
    pub provider: String,
    pub model: String,
    pub request_id: String,
    pub session_id: String,
    pub query: String,
    /// OpenAI-compatible `user` field from the request.
    pub user_id: Option<String>,
    /// Request `metadata` map serialized as a JSON object string.
    pub metadata_json: Option<String>,
    /// `llm.invocation_parameters` JSON for the effective LLM config.
    pub invocation_parameters: Option<String>,
    /// MCP tool schemas for `llm.tools.{i}.tool.json_schema`.
    pub tools_json: Vec<String>,
    pub message_count: usize,
    pub response_content: ResponseContent,
    /// Assembled system prompt sent to the provider.
    pub system_prompt: Option<String>,
    /// Whether the request is served by the multi-agent orchestrator.
    pub orchestration_enabled: bool,
}

impl StreamOtelContext {
    /// Record input-side OTel attributes on the current span.
    pub fn record_input(&self) {
        let span = tracing::Span::current();
        aura::logging::set_llm_identifiers(&span, &self.provider, &self.model);
        aura::logging::set_input_attributes(&span, &self.query);
        if let Some(system_prompt) = &self.system_prompt {
            aura::logging::set_system_prompt_attribute(&span, system_prompt);
        }
        aura::logging::set_span_attribute(&span, "http.request_id", self.request_id.clone());
        aura::logging::set_span_attribute(&span, "session.id", self.session_id.clone());
        aura::logging::set_span_attribute(
            &span,
            aura::logging::ATTR_AURA_VERSION,
            aura::logging::AURA_VERSION,
        );
        aura::logging::set_span_attribute(
            &span,
            aura::logging::ATTR_AURA_MODE,
            if self.orchestration_enabled {
                "orchestration"
            } else {
                "single-agent"
            },
        );
        if let Some(user_id) = &self.user_id {
            aura::logging::set_span_attribute(&span, aura::logging::ATTR_USER_ID, user_id.clone());
        }
        if let Some(metadata) = &self.metadata_json {
            aura::logging::set_span_attribute(
                &span,
                aura::logging::ATTR_METADATA,
                metadata.clone(),
            );
        }
        if let Some(params) = &self.invocation_parameters {
            aura::logging::set_llm_invocation_parameters(&span, params);
        }
        if !self.tools_json.is_empty() {
            aura::logging::set_llm_tools(&span, &self.tools_json);
        }
        aura::logging::set_span_attribute(&span, "message_count", self.message_count as i64);
    }

    /// Record output-side OTel attributes on the current span after the stream ends.
    ///
    /// Captures response content (if available) and termination status. Token
    /// usage is not recorded here: each `agent.turn` child span carries its
    /// own per-call usage (recorded by the Rig fork), and Phoenix's rollup
    /// sums those descendants — an aggregate on this span would double-count.
    pub fn record_output(&self, termination: &StreamTermination) {
        let span = tracing::Span::current();
        // Record response content for OpenInference/Phoenix visibility
        if let Some(content) = self.response_content.get() {
            aura::logging::set_output_attributes(&span, &content);
        }

        match termination {
            StreamTermination::Complete => {
                aura::logging::set_span_ok(&span);
            }
            StreamTermination::StreamError(err) => {
                aura::logging::set_span_error(&span, aura::logging::truncate_for_otel(err));
            }
            StreamTermination::Disconnected => {
                aura::logging::set_span_attribute(&span, "stream.termination", "disconnected");
            }
            StreamTermination::Timeout => {
                aura::logging::set_span_error(&span, "timeout");
            }
            StreamTermination::Shutdown => {
                aura::logging::set_span_attribute(&span, "stream.termination", "shutdown");
            }
        }
    }
}
