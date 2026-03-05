//! OpenInference Semantic Convention Exporter
//!
//! Wraps an OTLP `SpanExporter` to add [OpenInference] attributes before
//! export. This ensures Phoenix correctly classifies spans (LLM, TOOL,
//! AGENT, CHAIN) and displays token counts, model names, and
//! structured messages in its UI.
//!
//! ## Dual-source attribute design
//!
//! Aura-owned spans (e.g. `agent.stream`, `mcp.tool_call`) set OpenInference
//! `llm.*` / `output.*` attributes **directly** via the helpers in
//! [`crate::logging`]. Rig-owned spans (e.g. `chat`, `execute_tool`) arrive
//! with `gen_ai.*` attributes from the Rig framework. This exporter
//! **translates** `gen_ai.*` → OpenInference equivalents (`llm.*`, `tool.*`,
//! `agent.*`, `turn.*`, `input.*`, `output.*`) and then **strips** the
//! original `gen_ai.*` attributes so only the canonical OpenInference names
//! appear in exported spans. The shared attribute key constants live in
//! [`crate::logging`] to keep the two paths in sync.
//!
//! [OpenInference]: https://github.com/Arize-ai/openinference/tree/main/spec

use crate::logging::{
    ATTR_INPUT_VALUE, ATTR_LLM_MODEL_NAME, ATTR_LLM_SYSTEM, ATTR_LLM_TOKEN_COMPLETION,
    ATTR_LLM_TOKEN_PROMPT, ATTR_OUTPUT_VALUE, ATTR_TOOL_NAME, ATTR_TOOL_PARAMETERS,
};
use futures::future::BoxFuture;
use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};

/// A span exporter that adds OpenInference semantic convention attributes
/// to every span before delegating to an inner exporter.
#[derive(Debug)]
pub struct OpenInferenceExporter<E> {
    inner: E,
}

impl<E: SpanExporter> OpenInferenceExporter<E> {
    /// Create a new `OpenInferenceExporter` wrapping the given exporter.
    pub fn new(inner: E) -> Self {
        Self { inner }
    }
}

impl<E: SpanExporter + Send + Sync + 'static> SpanExporter for OpenInferenceExporter<E> {
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
        let transformed: Vec<SpanData> = batch.into_iter().map(transform_span).collect();
        self.inner.export(transformed)
    }

    fn shutdown(&mut self) {
        self.inner.shutdown();
    }

    fn force_flush(&mut self) -> BoxFuture<'static, ExportResult> {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

// ---------------------------------------------------------------------------
// Span kind mapping
// ---------------------------------------------------------------------------

/// Map a span name to an OpenInference span kind.
fn infer_span_kind(name: &str) -> &'static str {
    match name {
        // Rig LLM-level spans (agent.turn is LLM so Phoenix renders Output Messages)
        "chat" | "chat_streaming" | "agent.turn" => "LLM",
        // Tool execution spans
        "execute_tool" | "mcp.tool_call" => "TOOL",
        // Aura agent orchestration
        "agent.stream" | "agent.prompt" | "agent.chat" => "AGENT",
        // Aura chain / entry-point spans
        "chat_completions" | "streaming_completion" => "CHAIN",
        // Safe default
        _ => "CHAIN",
    }
}

// ---------------------------------------------------------------------------
// Span transformation
// ---------------------------------------------------------------------------

/// Transform a single `SpanData`, adding OpenInference attributes.
fn transform_span(mut span: SpanData) -> SpanData {
    let kind = infer_span_kind(&span.name);

    // 1. Add openinference.span.kind
    span.attributes
        .push(KeyValue::new("openinference.span.kind", kind));

    // 2. Translate gen_ai.* → llm.* / tool.* (additive)
    let mut extra_attrs: Vec<KeyValue> = Vec::new();
    let mut deferred_response: Option<String> = None;
    let mut deferred_reasoning: Option<String> = None;

    for kv in &span.attributes {
        let key = kv.key.as_str();
        match key {
            "gen_ai.system" | "gen_ai.provider.name" => {
                extra_attrs.push(KeyValue::new(ATTR_LLM_SYSTEM, kv.value.clone()));
            }
            "gen_ai.request.model" => {
                extra_attrs.push(KeyValue::new(ATTR_LLM_MODEL_NAME, kv.value.clone()));
            }
            "gen_ai.usage.input_tokens" => {
                extra_attrs.push(KeyValue::new(ATTR_LLM_TOKEN_PROMPT, kv.value.clone()));
            }
            "gen_ai.usage.output_tokens" => {
                extra_attrs.push(KeyValue::new(ATTR_LLM_TOKEN_COMPLETION, kv.value.clone()));
            }
            "gen_ai.tool.name" => {
                extra_attrs.push(KeyValue::new(ATTR_TOOL_NAME, kv.value.clone()));
            }
            "gen_ai.tool.call.arguments" => {
                extra_attrs.push(KeyValue::new(ATTR_TOOL_PARAMETERS, kv.value.clone()));
            }
            "gen_ai.tool.call.result" if kind == "TOOL" => {
                extra_attrs.push(KeyValue::new(ATTR_OUTPUT_VALUE, kv.value.clone()));
            }
            // Agent turn attributes → OpenInference input/output for CHAIN and LLM spans
            "gen_ai.turn.prompt" if kind == "CHAIN" || kind == "LLM" => {
                extra_attrs.push(KeyValue::new(ATTR_INPUT_VALUE, kv.value.clone()));
            }
            "gen_ai.turn.response" if kind == "CHAIN" || kind == "LLM" => {
                deferred_response = Some(kv.value.to_string());
            }
            // Expand structured messages for LLM spans
            "gen_ai.input.messages" if kind == "LLM" => {
                expand_messages(
                    &kv.value.to_string(),
                    "llm.input_messages",
                    &mut extra_attrs,
                );
            }
            "gen_ai.output.messages" if kind == "LLM" => {
                expand_messages(
                    &kv.value.to_string(),
                    "llm.output_messages",
                    &mut extra_attrs,
                );
            }
            // Agent metadata: strip gen_ai. prefix
            "gen_ai.agent.name" => {
                extra_attrs.push(KeyValue::new("agent.name", kv.value.clone()));
            }
            "gen_ai.agent.turn" => {
                extra_attrs.push(KeyValue::new("agent.turn", kv.value.clone()));
            }
            "gen_ai.agent.max_turns" => {
                extra_attrs.push(KeyValue::new("agent.max_turns", kv.value.clone()));
            }
            // Turn metadata: strip gen_ai. prefix
            "gen_ai.turn.history_len" => {
                extra_attrs.push(KeyValue::new("turn.history_len", kv.value.clone()));
            }
            "gen_ai.turn.tool_count" => {
                extra_attrs.push(KeyValue::new("turn.tool_count", kv.value.clone()));
            }
            "gen_ai.turn.has_tool_calls" => {
                extra_attrs.push(KeyValue::new("turn.has_tool_calls", kv.value.clone()));
            }
            "gen_ai.turn.reasoning" if kind == "CHAIN" || kind == "LLM" => {
                extra_attrs.push(KeyValue::new("turn.reasoning", kv.value.clone()));
                deferred_reasoning = Some(kv.value.to_string());
            }
            _ => {}
        }
    }

    // For LLM spans: emit structured llm.output_messages (Phoenix Output Messages tab)
    // For CHAIN spans: fall back to output.value (Phoenix SpanIO view)
    // Note: gen_ai.turn.response and gen_ai.output.messages are mutually exclusive
    // in practice (turn.* lives on agent.turn, output.messages lives on chat/chat_streaming),
    // so there is no collision risk on the llm.output_messages.* indices.
    if kind == "LLM" {
        let mut msg_index = 0;
        if let Some(response) = deferred_response {
            extra_attrs.push(KeyValue::new(
                format!("llm.output_messages.{msg_index}.message.role"),
                "assistant",
            ));
            extra_attrs.push(KeyValue::new(
                format!("llm.output_messages.{msg_index}.message.content"),
                response,
            ));
            msg_index += 1;
        }
        if let Some(reasoning) = deferred_reasoning {
            extra_attrs.push(KeyValue::new(
                format!("llm.output_messages.{msg_index}.message.role"),
                "reasoning",
            ));
            extra_attrs.push(KeyValue::new(
                format!("llm.output_messages.{msg_index}.message.content"),
                reasoning,
            ));
        }
    } else if let Some(response) = deferred_response {
        extra_attrs.push(KeyValue::new(ATTR_OUTPUT_VALUE, response));
    }

    span.attributes.extend(extra_attrs);

    // Remove original gen_ai.* attributes now that they've been translated
    span.attributes
        .retain(|kv| !kv.key.as_str().starts_with("gen_ai."));

    span
}

/// Parse a JSON array of `[{"role":"...","content":"..."},...]` and emit
/// flattened OpenInference message attributes.
///
/// Handles both raw JSON strings and strings that may arrive with surrounding
/// quotes from `Display`-formatted OTel values (e.g. `"[{...}]"`).
fn expand_messages(json_str: &str, prefix: &str, out: &mut Vec<KeyValue>) {
    // Try raw first, then fall back to stripping surrounding quotes
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(json_str).or_else(|_| {
        let trimmed = json_str.trim_matches('"');
        serde_json::from_str::<Vec<serde_json::Value>>(trimmed)
    }) else {
        return;
    };
    for (i, msg) in arr.iter().enumerate() {
        if let Some(role) = msg.get("role").and_then(|v| v.as_str()) {
            out.push(KeyValue::new(
                format!("{prefix}.{i}.message.role"),
                role.to_string(),
            ));
        }
        if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
            out.push(KeyValue::new(
                format!("{prefix}.{i}.message.content"),
                content.to_string(),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::InstrumentationScope;
    use opentelemetry::trace::{
        SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};
    use std::borrow::Cow;
    use std::time::SystemTime;

    /// Helper to build a minimal `SpanData` with the given name and attributes.
    fn make_span(name: &'static str, attrs: Vec<KeyValue>) -> SpanData {
        SpanData {
            span_context: SpanContext::new(
                TraceId::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
                SpanId::from_bytes([0, 0, 0, 0, 0, 0, 0, 1]),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            parent_span_id: SpanId::INVALID,
            span_kind: SpanKind::Internal,
            name: Cow::Borrowed(name),
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            attributes: attrs,
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: Status::Unset,
            instrumentation_scope: InstrumentationScope::builder("test").build(),
        }
    }

    fn find_attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    #[test]
    fn test_span_kind_mapping() {
        assert_eq!(infer_span_kind("chat"), "LLM");
        assert_eq!(infer_span_kind("chat_streaming"), "LLM");
        assert_eq!(infer_span_kind("execute_tool"), "TOOL");
        assert_eq!(infer_span_kind("mcp.tool_call"), "TOOL");
        assert_eq!(infer_span_kind("agent.stream"), "AGENT");
        assert_eq!(infer_span_kind("agent.prompt"), "AGENT");
        assert_eq!(infer_span_kind("agent.chat"), "AGENT");
        assert_eq!(infer_span_kind("agent.turn"), "LLM");
        assert_eq!(infer_span_kind("chat_completions"), "CHAIN");
        assert_eq!(infer_span_kind("streaming_completion"), "CHAIN");
        assert_eq!(infer_span_kind("unknown_span"), "CHAIN");
    }

    #[test]
    fn test_adds_span_kind() {
        let span = make_span("chat", vec![]);
        let result = transform_span(span);
        assert_eq!(
            find_attr(&result, "openinference.span.kind")
                .unwrap()
                .to_string(),
            "LLM"
        );
    }

    #[test]
    fn test_translates_gen_ai_system() {
        let span = make_span("chat", vec![KeyValue::new("gen_ai.system", "openai")]);
        let result = transform_span(span);
        assert_eq!(
            find_attr(&result, "llm.system").unwrap().to_string(),
            "openai"
        );
        // Original gen_ai.* attribute should be removed
        assert!(find_attr(&result, "gen_ai.system").is_none());
    }

    #[test]
    fn test_translates_gen_ai_request_model() {
        let span = make_span("chat", vec![KeyValue::new("gen_ai.request.model", "gpt-4")]);
        let result = transform_span(span);
        assert_eq!(
            find_attr(&result, "llm.model_name").unwrap().to_string(),
            "gpt-4"
        );
    }

    #[test]
    fn test_translates_token_counts() {
        let span = make_span(
            "agent.stream",
            vec![
                KeyValue::new("gen_ai.usage.input_tokens", 100i64),
                KeyValue::new("gen_ai.usage.output_tokens", 50i64),
            ],
        );
        let result = transform_span(span);
        assert!(find_attr(&result, "llm.token_count.prompt").is_some());
        assert!(find_attr(&result, "llm.token_count.completion").is_some());
    }

    /// Verify that transform_span output uses the same attribute names defined
    /// as constants in `logging.rs`. This catches drift where someone changes
    /// a constant value but forgets to update the exporter (or vice versa).
    #[test]
    fn test_tool_attributes_match_logging_constants() {
        use crate::logging::{ATTR_TOOL_NAME, ATTR_TOOL_PARAMETERS};

        let span = make_span(
            "execute_tool",
            vec![
                KeyValue::new("gen_ai.tool.name", "search"),
                KeyValue::new("gen_ai.tool.call.arguments", r#"{"q":"test"}"#),
            ],
        );
        let result = transform_span(span);

        // The translated attribute keys must exactly match the logging constants
        assert!(
            find_attr(&result, ATTR_TOOL_NAME).is_some(),
            "tool.name attribute must use ATTR_TOOL_NAME constant (\"{}\")",
            ATTR_TOOL_NAME
        );
        assert!(
            find_attr(&result, ATTR_TOOL_PARAMETERS).is_some(),
            "tool.parameters attribute must use ATTR_TOOL_PARAMETERS constant (\"{}\")",
            ATTR_TOOL_PARAMETERS
        );
    }

    /// Verify LLM attribute names in transform_span match logging.rs constants.
    #[test]
    fn test_llm_attributes_match_logging_constants() {
        use crate::logging::{
            ATTR_LLM_MODEL_NAME, ATTR_LLM_SYSTEM, ATTR_LLM_TOKEN_COMPLETION, ATTR_LLM_TOKEN_PROMPT,
        };

        let span = make_span(
            "chat",
            vec![
                KeyValue::new("gen_ai.system", "openai"),
                KeyValue::new("gen_ai.request.model", "gpt-4"),
                KeyValue::new("gen_ai.usage.input_tokens", 100i64),
                KeyValue::new("gen_ai.usage.output_tokens", 50i64),
            ],
        );
        let result = transform_span(span);

        assert!(
            find_attr(&result, ATTR_LLM_SYSTEM).is_some(),
            "must use ATTR_LLM_SYSTEM constant (\"{}\")",
            ATTR_LLM_SYSTEM
        );
        assert!(
            find_attr(&result, ATTR_LLM_MODEL_NAME).is_some(),
            "must use ATTR_LLM_MODEL_NAME constant (\"{}\")",
            ATTR_LLM_MODEL_NAME
        );
        assert!(
            find_attr(&result, ATTR_LLM_TOKEN_PROMPT).is_some(),
            "must use ATTR_LLM_TOKEN_PROMPT constant (\"{}\")",
            ATTR_LLM_TOKEN_PROMPT
        );
        assert!(
            find_attr(&result, ATTR_LLM_TOKEN_COMPLETION).is_some(),
            "must use ATTR_LLM_TOKEN_COMPLETION constant (\"{}\")",
            ATTR_LLM_TOKEN_COMPLETION
        );
    }

    /// Verify I/O attribute names in transform_span match logging.rs constants.
    #[test]
    fn test_io_attributes_match_logging_constants() {
        use crate::logging::{ATTR_INPUT_VALUE, ATTR_OUTPUT_VALUE};

        // LLM span (agent.turn): gen_ai.turn.prompt → input.value
        let span = make_span(
            "agent.turn",
            vec![KeyValue::new("gen_ai.turn.prompt", "hello")],
        );
        let result = transform_span(span);
        assert!(
            find_attr(&result, ATTR_INPUT_VALUE).is_some(),
            "must use ATTR_INPUT_VALUE constant (\"{}\")",
            ATTR_INPUT_VALUE
        );

        // CHAIN span: gen_ai.turn.response → output.value (CHAIN falls back to output.value)
        let span = make_span(
            "chat_completions",
            vec![KeyValue::new("gen_ai.turn.response", "world")],
        );
        let result = transform_span(span);
        assert!(
            find_attr(&result, ATTR_OUTPUT_VALUE).is_some(),
            "must use ATTR_OUTPUT_VALUE constant (\"{}\")",
            ATTR_OUTPUT_VALUE
        );
    }

    #[test]
    fn test_translates_tool_attributes() {
        let span = make_span(
            "execute_tool",
            vec![
                KeyValue::new("gen_ai.tool.name", "search"),
                KeyValue::new("gen_ai.tool.call.arguments", r#"{"q":"test"}"#),
                KeyValue::new("gen_ai.tool.call.result", "found it"),
            ],
        );
        let result = transform_span(span);
        assert_eq!(
            find_attr(&result, "tool.name").unwrap().to_string(),
            "search"
        );
        assert!(find_attr(&result, "tool.parameters").is_some());
        assert_eq!(
            find_attr(&result, "output.value").unwrap().to_string(),
            "found it"
        );
    }

    #[test]
    fn test_tool_result_only_on_tool_spans() {
        // On a non-TOOL span, gen_ai.tool.call.result should NOT produce output.value
        let span = make_span(
            "chat",
            vec![KeyValue::new("gen_ai.tool.call.result", "some result")],
        );
        let result = transform_span(span);
        assert!(find_attr(&result, "output.value").is_none());
    }

    #[test]
    fn test_expand_input_messages() {
        let messages = r#"[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"}]"#;
        let span = make_span(
            "chat",
            vec![KeyValue::new("gen_ai.input.messages", messages)],
        );
        let result = transform_span(span);
        assert_eq!(
            find_attr(&result, "llm.input_messages.0.message.role")
                .unwrap()
                .to_string(),
            "user"
        );
        assert_eq!(
            find_attr(&result, "llm.input_messages.0.message.content")
                .unwrap()
                .to_string(),
            "hello"
        );
        assert_eq!(
            find_attr(&result, "llm.input_messages.1.message.role")
                .unwrap()
                .to_string(),
            "assistant"
        );
    }

    #[test]
    fn test_expand_messages_invalid_json() {
        let span = make_span(
            "chat",
            vec![KeyValue::new("gen_ai.input.messages", "not json")],
        );
        // Should not panic, just skip
        let result = transform_span(span);
        assert!(find_attr(&result, "llm.input_messages.0.message.role").is_none());
    }

    #[test]
    fn test_expand_messages_quoted_json() {
        // Some OTel value Display impls wrap strings in quotes
        let messages = r#""[{"role":"user","content":"hello"}]""#;
        let span = make_span(
            "chat",
            vec![KeyValue::new("gen_ai.input.messages", messages)],
        );
        let result = transform_span(span);
        assert_eq!(
            find_attr(&result, "llm.input_messages.0.message.role")
                .unwrap()
                .to_string(),
            "user"
        );
    }

    #[test]
    fn test_messages_only_expanded_on_llm_spans() {
        let messages = r#"[{"role":"user","content":"hello"}]"#;
        let span = make_span(
            "agent.stream",
            vec![KeyValue::new("gen_ai.input.messages", messages)],
        );
        let result = transform_span(span);
        // AGENT span — messages should NOT be expanded
        assert!(find_attr(&result, "llm.input_messages.0.message.role").is_none());
    }

    #[test]
    fn test_translates_agent_turn_prompt_to_input_value() {
        let span = make_span(
            "agent.turn",
            vec![KeyValue::new("gen_ai.turn.prompt", "What is Rust?")],
        );
        let result = transform_span(span);
        assert_eq!(
            find_attr(&result, "openinference.span.kind")
                .unwrap()
                .to_string(),
            "LLM"
        );
        assert_eq!(
            find_attr(&result, "input.value").unwrap().to_string(),
            "What is Rust?"
        );
    }

    #[test]
    fn test_agent_turn_response_goes_to_output_messages_not_output_value() {
        let span = make_span(
            "agent.turn",
            vec![KeyValue::new(
                "gen_ai.turn.response",
                "Rust is a systems programming language.",
            )],
        );
        let result = transform_span(span);
        // LLM span: response goes to llm.output_messages, NOT output.value
        assert!(find_attr(&result, "output.value").is_none());
        assert_eq!(
            find_attr(&result, "llm.output_messages.0.message.content")
                .unwrap()
                .to_string(),
            "Rust is a systems programming language."
        );
    }

    #[test]
    fn test_agent_turn_prompt_not_translated_on_tool_span() {
        let span = make_span(
            "execute_tool",
            vec![KeyValue::new("gen_ai.turn.prompt", "hello")],
        );
        let result = transform_span(span);
        // TOOL span — turn.prompt should NOT become input.value
        assert!(find_attr(&result, "input.value").is_none());
    }

    #[test]
    fn test_translates_agent_metadata() {
        let span = make_span(
            "agent.turn",
            vec![
                KeyValue::new("gen_ai.agent.name", "Unnamed Agent"),
                KeyValue::new("gen_ai.agent.turn", 4i64),
                KeyValue::new("gen_ai.agent.max_turns", 100i64),
            ],
        );
        let result = transform_span(span);
        assert_eq!(
            find_attr(&result, "agent.name").unwrap().to_string(),
            "Unnamed Agent"
        );
        assert!(find_attr(&result, "agent.turn").is_some());
        assert!(find_attr(&result, "agent.max_turns").is_some());
        // Originals removed
        assert!(find_attr(&result, "gen_ai.agent.name").is_none());
        assert!(find_attr(&result, "gen_ai.agent.turn").is_none());
        assert!(find_attr(&result, "gen_ai.agent.max_turns").is_none());
    }

    #[test]
    fn test_translates_turn_metadata() {
        let span = make_span(
            "agent.turn",
            vec![
                KeyValue::new("gen_ai.turn.history_len", 9i64),
                KeyValue::new("gen_ai.turn.tool_count", 1i64),
                KeyValue::new("gen_ai.turn.has_tool_calls", true),
            ],
        );
        let result = transform_span(span);
        assert!(find_attr(&result, "turn.history_len").is_some());
        assert!(find_attr(&result, "turn.tool_count").is_some());
        assert!(find_attr(&result, "turn.has_tool_calls").is_some());
        // Originals removed
        assert!(find_attr(&result, "gen_ai.turn.history_len").is_none());
        assert!(find_attr(&result, "gen_ai.turn.tool_count").is_none());
        assert!(find_attr(&result, "gen_ai.turn.has_tool_calls").is_none());
    }

    #[test]
    fn test_all_gen_ai_attributes_stripped() {
        let span = make_span(
            "agent.turn",
            vec![
                KeyValue::new("gen_ai.system", "openai"),
                KeyValue::new("gen_ai.agent.name", "Test"),
                KeyValue::new("gen_ai.turn.prompt", "hello"),
                KeyValue::new("gen_ai.turn.response", "world"),
                KeyValue::new("gen_ai.turn.history_len", 5i64),
            ],
        );
        let result = transform_span(span);
        // No gen_ai.* attributes should remain
        let gen_ai_attrs: Vec<_> = result
            .attributes
            .iter()
            .filter(|kv| kv.key.as_str().starts_with("gen_ai."))
            .collect();
        assert!(
            gen_ai_attrs.is_empty(),
            "gen_ai.* attributes should be stripped, found: {:?}",
            gen_ai_attrs
                .iter()
                .map(|kv| kv.key.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_llm_span_output_messages_response_and_reasoning() {
        let span = make_span(
            "agent.turn",
            vec![
                KeyValue::new("gen_ai.turn.response", "The answer is 42."),
                KeyValue::new("gen_ai.turn.reasoning", "I computed 6 * 7."),
            ],
        );
        let result = transform_span(span);
        // Assistant message at index 0
        assert_eq!(
            find_attr(&result, "llm.output_messages.0.message.role")
                .unwrap()
                .to_string(),
            "assistant"
        );
        assert_eq!(
            find_attr(&result, "llm.output_messages.0.message.content")
                .unwrap()
                .to_string(),
            "The answer is 42."
        );
        // Reasoning message at index 1
        assert_eq!(
            find_attr(&result, "llm.output_messages.1.message.role")
                .unwrap()
                .to_string(),
            "reasoning"
        );
        assert_eq!(
            find_attr(&result, "llm.output_messages.1.message.content")
                .unwrap()
                .to_string(),
            "I computed 6 * 7."
        );
    }

    #[test]
    fn test_llm_span_output_messages_response_only() {
        let span = make_span(
            "agent.turn",
            vec![KeyValue::new(
                "gen_ai.turn.response",
                "Just a plain response.",
            )],
        );
        let result = transform_span(span);
        // Assistant message at index 0
        assert_eq!(
            find_attr(&result, "llm.output_messages.0.message.role")
                .unwrap()
                .to_string(),
            "assistant"
        );
        assert_eq!(
            find_attr(&result, "llm.output_messages.0.message.content")
                .unwrap()
                .to_string(),
            "Just a plain response."
        );
        // No reasoning message
        assert!(find_attr(&result, "llm.output_messages.1.message.role").is_none());
    }

    #[test]
    fn test_non_llm_span_no_output_messages() {
        // TOOL span with gen_ai.turn.reasoning should NOT produce llm.output_messages
        let span = make_span(
            "execute_tool",
            vec![KeyValue::new(
                "gen_ai.turn.reasoning",
                "some reasoning text",
            )],
        );
        let result = transform_span(span);
        assert!(find_attr(&result, "llm.output_messages.0.message.role").is_none());
        // Also verify turn.reasoning is NOT emitted for TOOL spans
        assert!(find_attr(&result, "turn.reasoning").is_none());
    }
}

// ---------------------------------------------------------------------------
// Pipeline integration tests
//
// These exercise the full path: tracing::info_span!() → tracing-opentelemetry
// layer → OpenInferenceExporter → captured SpanData.  They catch breakages
// in how tracing-opentelemetry maps field names, handles Empty/.record(),
// or changes StringValue quoting — none of which the unit tests above cover.
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "otel"))]
mod pipeline_tests {
    use super::*;
    use futures::future::BoxFuture;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
    use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
    use std::sync::{Arc, Mutex};

    // -- InMemoryExporter: collects exported spans for assertions -----------

    #[derive(Clone, Debug)]
    struct InMemoryExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
    }

    impl InMemoryExporter {
        fn new() -> Self {
            Self {
                spans: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn spans(&self) -> Vec<SpanData> {
            self.spans.lock().unwrap().clone()
        }
    }

    impl SpanExporter for InMemoryExporter {
        fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
            self.spans.lock().unwrap().extend(batch);
            Box::pin(std::future::ready(Ok(())))
        }

        fn shutdown(&mut self) {}

        fn force_flush(&mut self) -> BoxFuture<'static, ExportResult> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn set_resource(&mut self, _resource: &Resource) {}
    }

    // -- Test helper: build a subscriber with OI exporter → in-memory ------

    /// Build a tracing subscriber wired to our OpenInferenceExporter backed
    /// by an InMemoryExporter.  Returns the subscriber and the memory handle
    /// for reading captured spans after flush.
    fn build_pipeline() -> (impl tracing::Subscriber + Send + Sync, InMemoryExporter) {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::layer::SubscriberExt;

        let memory = InMemoryExporter::new();
        let oi_exporter = OpenInferenceExporter::new(memory.clone());

        // SimpleSpanProcessor exports synchronously on span-close — no
        // background runtime needed, so tests don't hang.
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(oi_exporter)))
            .build();

        let otel_layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));

        let subscriber = tracing_subscriber::registry().with(otel_layer);

        // Stash provider so we can shut it down later, ensuring all spans
        // are flushed to the InMemoryExporter.
        PROVIDER.with(|cell: &std::cell::RefCell<Option<TracerProvider>>| {
            *cell.borrow_mut() = Some(provider);
        });

        (subscriber, memory)
    }

    thread_local! {
        static PROVIDER: std::cell::RefCell<Option<TracerProvider>> = const { std::cell::RefCell::new(None) };
    }

    /// Collect all spans captured by the in-memory exporter.
    /// With SimpleSpanProcessor, spans are exported synchronously on close,
    /// so no flush is needed — just read the memory.
    fn collect_spans(memory: &InMemoryExporter) -> Vec<SpanData> {
        memory.spans()
    }

    fn find_attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    fn find_span_by_name<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
        spans
            .iter()
            .find(|s| s.name.as_ref() == name)
            .unwrap_or_else(|| {
                panic!(
                    "span '{}' not found in: {:?}",
                    name,
                    spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
                )
            })
    }

    // -- Pipeline test: LLM `chat` span -----------------------------------

    #[test]
    fn test_pipeline_chat_span() {
        let (subscriber, memory) = build_pipeline();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "chat",
                "gen_ai.system" = "openai",
                "gen_ai.request.model" = "gpt-4",
                "gen_ai.usage.input_tokens" = tracing::field::Empty,
                "gen_ai.usage.output_tokens" = tracing::field::Empty,
                "gen_ai.input.messages" = tracing::field::Empty,
                "gen_ai.output.messages" = tracing::field::Empty,
            );
            let _guard = span.enter();

            // Simulate Rig recording values after the LLM call
            span.record("gen_ai.usage.input_tokens", 150_i64);
            span.record("gen_ai.usage.output_tokens", 42_i64);
            span.record(
                "gen_ai.input.messages",
                r#"[{"role":"user","content":"hello"}]"#,
            );
            span.record(
                "gen_ai.output.messages",
                r#"[{"role":"assistant","content":"hi there"}]"#,
            );
        });

        let spans = collect_spans(&memory);
        let chat = find_span_by_name(&spans, "chat");

        // OpenInference span kind
        assert_eq!(
            find_attr(chat, "openinference.span.kind")
                .unwrap()
                .to_string(),
            "LLM"
        );

        // Translated LLM attributes
        assert_eq!(find_attr(chat, "llm.system").unwrap().to_string(), "openai");
        assert_eq!(
            find_attr(chat, "llm.model_name").unwrap().to_string(),
            "gpt-4"
        );
        assert_eq!(
            find_attr(chat, "llm.token_count.prompt")
                .unwrap()
                .to_string(),
            "150"
        );
        assert_eq!(
            find_attr(chat, "llm.token_count.completion")
                .unwrap()
                .to_string(),
            "42"
        );

        // Expanded input messages
        assert_eq!(
            find_attr(chat, "llm.input_messages.0.message.role")
                .unwrap()
                .to_string(),
            "user"
        );
        assert_eq!(
            find_attr(chat, "llm.input_messages.0.message.content")
                .unwrap()
                .to_string(),
            "hello"
        );

        // Expanded output messages
        assert_eq!(
            find_attr(chat, "llm.output_messages.0.message.role")
                .unwrap()
                .to_string(),
            "assistant"
        );
        assert_eq!(
            find_attr(chat, "llm.output_messages.0.message.content")
                .unwrap()
                .to_string(),
            "hi there"
        );

        // No gen_ai.* attributes should remain
        assert!(
            chat.attributes
                .iter()
                .all(|kv| !kv.key.as_str().starts_with("gen_ai.")),
            "gen_ai.* attributes should be stripped"
        );
    }

    // -- Pipeline test: tool `execute_tool` span --------------------------

    #[test]
    fn test_pipeline_execute_tool_span() {
        let (subscriber, memory) = build_pipeline();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "execute_tool",
                "gen_ai.tool.name" = "web_search",
                "gen_ai.tool.call.arguments" = r#"{"query":"rust lang"}"#,
                "gen_ai.tool.call.result" = tracing::field::Empty,
            );
            let _guard = span.enter();
            span.record(
                "gen_ai.tool.call.result",
                "Rust is a systems programming language",
            );
        });

        let spans = collect_spans(&memory);
        let tool = find_span_by_name(&spans, "execute_tool");

        assert_eq!(
            find_attr(tool, "openinference.span.kind")
                .unwrap()
                .to_string(),
            "TOOL"
        );
        assert_eq!(
            find_attr(tool, "tool.name").unwrap().to_string(),
            "web_search"
        );
        assert!(
            find_attr(tool, "tool.parameters").is_some(),
            "tool.parameters should be present"
        );
        assert_eq!(
            find_attr(tool, "output.value").unwrap().to_string(),
            "Rust is a systems programming language"
        );

        // No gen_ai.* remain
        assert!(
            tool.attributes
                .iter()
                .all(|kv| !kv.key.as_str().starts_with("gen_ai.")),
            "gen_ai.* attributes should be stripped"
        );
    }

    // -- Pipeline test: agent.turn span (LLM) -----------------------------

    #[test]
    fn test_pipeline_agent_turn_span() {
        let (subscriber, memory) = build_pipeline();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "agent.turn",
                "gen_ai.agent.name" = "SRE Agent",
                "gen_ai.agent.turn" = 2_i64,
                "gen_ai.turn.prompt" = "diagnose the outage",
                "gen_ai.turn.response" = tracing::field::Empty,
                "gen_ai.turn.history_len" = 5_i64,
                "gen_ai.turn.tool_count" = 1_i64,
                "gen_ai.turn.has_tool_calls" = true,
            );
            let _guard = span.enter();
            span.record("gen_ai.turn.response", "The root cause is a memory leak");
        });

        let spans = collect_spans(&memory);
        let turn = find_span_by_name(&spans, "agent.turn");

        assert_eq!(
            find_attr(turn, "openinference.span.kind")
                .unwrap()
                .to_string(),
            "LLM"
        );
        assert_eq!(
            find_attr(turn, "agent.name").unwrap().to_string(),
            "SRE Agent"
        );
        assert_eq!(find_attr(turn, "agent.turn").unwrap().to_string(), "2");
        assert_eq!(
            find_attr(turn, "input.value").unwrap().to_string(),
            "diagnose the outage"
        );
        // LLM span: response goes to llm.output_messages, not output.value
        assert!(find_attr(turn, "output.value").is_none());
        assert_eq!(
            find_attr(turn, "llm.output_messages.0.message.content")
                .unwrap()
                .to_string(),
            "The root cause is a memory leak"
        );
        assert!(find_attr(turn, "turn.history_len").is_some());
        assert!(find_attr(turn, "turn.tool_count").is_some());
        assert!(find_attr(turn, "turn.has_tool_calls").is_some());

        // No gen_ai.* remain
        assert!(
            turn.attributes
                .iter()
                .all(|kv| !kv.key.as_str().starts_with("gen_ai.")),
            "gen_ai.* attributes should be stripped"
        );
    }

    // -- Pipeline test: agent.turn with reasoning → llm.output_messages ---

    #[test]
    fn test_pipeline_agent_turn_with_reasoning() {
        let (subscriber, memory) = build_pipeline();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "agent.turn",
                "gen_ai.turn.prompt" = "explain quantum computing",
                "gen_ai.turn.response" = tracing::field::Empty,
                "gen_ai.turn.reasoning" = tracing::field::Empty,
            );
            let _guard = span.enter();
            span.record("gen_ai.turn.response", "Quantum computing uses qubits.");
            span.record(
                "gen_ai.turn.reasoning",
                "The user wants a simple explanation.",
            );
        });

        let spans = collect_spans(&memory);
        let turn = find_span_by_name(&spans, "agent.turn");

        // LLM span: no output.value, response is in llm.output_messages
        assert!(find_attr(turn, "output.value").is_none());
        assert_eq!(
            find_attr(turn, "turn.reasoning").unwrap().to_string(),
            "The user wants a simple explanation."
        );

        // Verify structured llm.output_messages
        assert_eq!(
            find_attr(turn, "llm.output_messages.0.message.role")
                .unwrap()
                .to_string(),
            "assistant"
        );
        assert_eq!(
            find_attr(turn, "llm.output_messages.0.message.content")
                .unwrap()
                .to_string(),
            "Quantum computing uses qubits."
        );
        assert_eq!(
            find_attr(turn, "llm.output_messages.1.message.role")
                .unwrap()
                .to_string(),
            "reasoning"
        );
        assert_eq!(
            find_attr(turn, "llm.output_messages.1.message.content")
                .unwrap()
                .to_string(),
            "The user wants a simple explanation."
        );
    }

    // -- Pipeline test: Empty fields that are never recorded are omitted ---

    #[test]
    fn test_pipeline_empty_fields_not_exported() {
        let (subscriber, memory) = build_pipeline();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "chat",
                "gen_ai.system" = "openai",
                "gen_ai.usage.input_tokens" = tracing::field::Empty,
                "gen_ai.usage.output_tokens" = tracing::field::Empty,
            );
            let _guard = span.enter();
            // Deliberately do NOT record the Empty fields — they should not appear
        });

        let spans = collect_spans(&memory);
        let chat = find_span_by_name(&spans, "chat");

        // The system field was set inline, so it should exist (translated)
        assert!(
            find_attr(chat, "llm.system").is_some(),
            "llm.system should be present (was set inline)"
        );

        // The Empty fields were never recorded, so their translated versions
        // should NOT appear.  This validates that tracing-opentelemetry does
        // not export Empty fields as zero/null values.
        assert!(
            find_attr(chat, "llm.token_count.prompt").is_none(),
            "llm.token_count.prompt should NOT be present (field was Empty and never recorded)"
        );
        assert!(
            find_attr(chat, "llm.token_count.completion").is_none(),
            "llm.token_count.completion should NOT be present (field was Empty and never recorded)"
        );
    }
}
