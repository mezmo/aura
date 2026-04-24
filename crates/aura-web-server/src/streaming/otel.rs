//! OTel recording context for the `agent.stream` span inside `tokio::spawn`.
//!
//! This struct bundles the cloned values that exist *only* for tracing purposes,
//! keeping them out of the business-logic variables. The two entry points mirror
//! the span lifecycle:
//!
//! - [`StreamOtelContext::record_input`] — called at span start
//! - [`StreamOtelContext::record_output`] — called after the stream ends

use aura::{ResponseContent, UsageState};

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
    /// Identity from request headers (currently unused).
    pub identity_id: String,
    pub message_count: usize,
    pub usage_state: UsageState,
    pub response_content: ResponseContent,
    /// True when the streaming agent is an orchestrator. Orchestration emits
    /// per-phase LLM spans (`orchestration.planning`, `orchestration.worker`,
    /// `orchestration.synthesis`, `orchestration.evaluation`) that each carry
    /// their own `llm.token_count.*` attributes. Recording the aggregate on
    /// the parent `agent.stream` span on top of that double-counts in
    /// Phoenix's rollup, so `record_output` skips the token write here and
    /// lets Phoenix sum the descendants.
    pub is_orchestration: bool,
}

impl StreamOtelContext {
    /// Record input-side OTel attributes on the current span.
    pub fn record_input(&self) {
        let span = tracing::Span::current();
        aura::logging::set_llm_identifiers(&span, &self.provider, &self.model);
        aura::logging::set_input_attributes(&span, &self.query);
        aura::logging::set_span_attribute(&span, "http.request_id", self.request_id.clone());
        aura::logging::set_span_attribute(&span, "session.id", self.session_id.clone());
        if !self.identity_id.is_empty() {
            aura::logging::set_span_attribute(&span, "identity.id", self.identity_id.clone());
        }
        aura::logging::set_span_attribute(&span, "message_count", self.message_count as i64);
    }

    /// Record output-side OTel attributes on the current span after the stream ends.
    ///
    /// Captures token usage, response content (if available), and termination status.
    pub fn record_output(&self, termination: &StreamTermination) {
        let span = tracing::Span::current();
        // In orchestration mode, per-phase spans already carry their own
        // `llm.token_count.*` attributes; recording the aggregate on the parent
        // `agent.stream` span would double-count under Phoenix's rollup.
        if !self.is_orchestration {
            let (prompt_tokens, completion_tokens, total_tokens) =
                self.usage_state.get_final_usage();
            let tool_completion_tokens = self.usage_state.get_tool_completion_tokens();
            aura::logging::set_token_usage(
                &span,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                tool_completion_tokens,
            );
        }

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
