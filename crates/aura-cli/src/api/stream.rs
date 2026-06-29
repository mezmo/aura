use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::Response;

use aura_events::AuraStreamEvent;

use crate::api::types::{AccumulatedToolCall, ChatCompletionChunk};
use crate::event_names;

/// Result of processing a stream — either a text response or tool calls.
#[derive(Debug)]
pub enum StreamResult {
    /// Normal text response from the LLM.
    TextResponse(String),
    /// LLM wants to call tools. Contains any partial text and the tool calls.
    ToolCalls {
        text: String,
        tool_calls: Vec<AccumulatedToolCall>,
        /// Server-side tool results keyed by tool_call_id.
        /// In hybrid mode, server tools are executed server-side and their results
        /// arrive via `aura.tool_complete` events. The client should use these
        /// cached results instead of attempting local execution.
        server_results: HashMap<String, String>,
    },
}

/// Poll an `AtomicBool` until it becomes `true`.
async fn wait_for_cancel(flag: &AtomicBool) {
    while !flag.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Sink for the events produced while processing a stream.
///
/// Every method defaults to a no-op, so implementors override only the
/// events they care about; [`NoopHandler`] ignores all of them.
///
/// Correlate by `tool_id`, not `tool_name`: a single turn may invoke the
/// same tool more than once.
pub trait StreamHandler {
    /// Called for each content delta (standard chat completions).
    fn on_token(&mut self, _token: &str) {}

    /// Called when a tool is requested, with (tool_id, tool_name, arguments).
    fn on_tool_requested(
        &mut self,
        _tool_id: &str,
        _tool_name: &str,
        _arguments: &BTreeMap<String, serde_json::Value>,
    ) {
    }

    /// Called when a tool begins execution, with (tool_id, tool_name).
    fn on_tool_start(&mut self, _tool_id: &str, _tool_name: &str) {}

    /// Called when a tool finishes, with (tool_id, tool_name, duration, result).
    fn on_tool_complete(
        &mut self,
        _tool_id: &str,
        _tool_name: &str,
        _duration: Duration,
        _result: Option<&str>,
    ) {
    }

    /// Called with (prompt_tokens, completion_tokens) from `aura.usage` events.
    /// These are cumulative provider-billed totals for the request, not context
    /// size — use [`on_context_usage`](Self::on_context_usage) for the context
    /// window indicator.
    fn on_usage(&mut self, _prompt_tokens: u64, _completion_tokens: u64) {}

    /// Called with (context_tokens, response_tokens, context_window) from
    /// `aura.context_usage` events. `context_tokens` is the absolute size of the
    /// context carried into the agent's final turn — the value to display as
    /// context-window occupancy.
    fn on_context_usage(
        &mut self,
        _context_tokens: u64,
        _response_tokens: u64,
        _context_window: Option<u64>,
    ) {
    }

    /// Called for `aura.reasoning` events, with (content, agent_id, fields).
    fn on_reasoning(
        &mut self,
        _content: &str,
        _agent_id: &str,
        _fields: &BTreeMap<String, serde_json::Value>,
    ) {
    }

    /// Called for every named (`aura.*`) SSE event, with (event_name, raw_data).
    fn on_raw_event(&mut self, _event_name: &str, _data: &str) {}

    /// Called for `aura.orchestrator.*`, `aura.session_info`, `aura.progress`,
    /// `aura.worker_phase`, and any future events, with (event_name, value).
    fn on_orchestrator_event(&mut self, _event_name: &str, _value: &serde_json::Value) {}
}

/// A [`StreamHandler`] that ignores every event. Used by one-shot mode and
/// tests where only the final [`StreamResult`] matters.
pub struct NoopHandler;

impl StreamHandler for NoopHandler {}

/// Process an SSE streaming response from an HTTP reqwest::Response.
///
/// Thin wrapper around `process_sse_events` that extracts the byte stream
/// and converts it to an eventsource stream.
pub async fn process_stream(
    response: Response,
    cancel: Arc<AtomicBool>,
    handler: &mut impl StreamHandler,
) -> Result<StreamResult> {
    let stream = response.bytes_stream().eventsource();
    process_sse_events(stream, cancel, handler).await
}

/// Process SSE events from any eventsource-compatible stream.
///
/// Used by both HTTP mode (via `process_stream`) and standalone mode
/// (via `DirectBackend`) to ensure identical event handling.
///
/// - `cancel`: when set to `true`, the stream is abandoned early
/// - `handler`: a [`StreamHandler`] whose methods are invoked as events arrive
///
/// Returns a `StreamResult` — either `TextResponse` or `ToolCalls`.
pub async fn process_sse_events<S, E>(
    mut stream: S,
    cancel: Arc<AtomicBool>,
    handler: &mut impl StreamHandler,
) -> Result<StreamResult>
where
    S: futures_util::Stream<Item = Result<eventsource_stream::Event, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut full_response = String::new();

    let mut tool_timers: std::collections::HashMap<String, Instant> =
        std::collections::HashMap::new();

    // Accumulate server-side tool results keyed by tool_call_id (from aura.tool_complete events).
    // In hybrid mode, the server executes server tools and streams results as custom events.
    let mut server_results: HashMap<String, String> = HashMap::new();

    // Accumulate tool calls from deltas (index-based)
    let mut tool_call_accumulators: std::collections::HashMap<usize, (String, String, String)> =
        std::collections::HashMap::new(); // index -> (id, name, arguments)
    let mut finish_reason: Option<String> = None;

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let event = tokio::select! {
            biased;
            _ = wait_for_cancel(&cancel) => break,
            event = stream.next() => match event {
                Some(Ok(event)) => event,
                Some(Err(e)) => {
                    eprintln!("SSE stream error: {}", e);
                    break;
                }
                None => break,
            },
        };

        if event.data == "[DONE]" {
            break;
        }

        let event_name = &event.event;

        // Capture raw SSE events with non-empty event names (aura.* custom events).
        // Standard chat completion chunks have empty event names and would flood the panel.
        if !event_name.is_empty() {
            handler.on_raw_event(event_name, &event.data);
        }

        // Parse aura-specific events using shared types from aura-events crate.
        // The event name tells us which variant to expect; serde's untagged
        // deserialization handles the JSON → enum mapping.
        if event_name.starts_with("aura.") {
            match event_name.as_str() {
                event_names::TOOL_REQUESTED => {
                    if let Ok(AuraStreamEvent::ToolRequested {
                        tool_id,
                        tool_name,
                        arguments,
                        ..
                    }) = serde_json::from_str::<AuraStreamEvent>(&event.data)
                    {
                        let args = match &arguments {
                            serde_json::Value::Object(map) => {
                                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                            }
                            _ => BTreeMap::new(),
                        };
                        handler.on_tool_requested(&tool_id, &tool_name, &args);
                    }
                }
                event_names::TOOL_START => {
                    if let Ok(AuraStreamEvent::ToolStart {
                        tool_id, tool_name, ..
                    }) = serde_json::from_str::<AuraStreamEvent>(&event.data)
                    {
                        tool_timers.insert(tool_id.clone(), Instant::now());
                        handler.on_tool_start(&tool_id, &tool_name);
                    }
                }
                event_names::TOOL_COMPLETE => {
                    if let Ok(AuraStreamEvent::ToolComplete {
                        tool_id,
                        tool_name,
                        duration_ms,
                        result,
                        error,
                        ..
                    }) = serde_json::from_str::<AuraStreamEvent>(&event.data)
                    {
                        let elapsed = Duration::from_millis(duration_ms);
                        tool_timers.remove(&tool_id);
                        handler.on_tool_complete(&tool_id, &tool_name, elapsed, result.as_deref());

                        // Cache server-side result by tool_call_id for hybrid mode.
                        // For failures, cache the error message so the model sees the
                        // real failure instead of a bogus "executed successfully" fallback.
                        if let Some(res) = result {
                            server_results.insert(tool_id, res);
                        } else if let Some(err) = error {
                            server_results.insert(tool_id, format!("Error: {err}"));
                        }
                    }
                }
                event_names::USAGE => {
                    if let Ok(AuraStreamEvent::Usage {
                        prompt_tokens,
                        completion_tokens,
                        ..
                    }) = serde_json::from_str::<AuraStreamEvent>(&event.data)
                    {
                        handler.on_usage(prompt_tokens, completion_tokens);
                    }
                }
                event_names::CONTEXT_USAGE => {
                    if let Ok(AuraStreamEvent::ContextUsage {
                        context_tokens,
                        response_tokens,
                        context_window,
                        ..
                    }) = serde_json::from_str::<AuraStreamEvent>(&event.data)
                    {
                        handler.on_context_usage(context_tokens, response_tokens, context_window);
                    }
                }
                event_names::TOOL_USAGE => {
                    // Silently skip — usage is tracked via aura.usage at stream end
                }
                event_names::REASONING => {
                    // Parse the raw payload into a BTreeMap so the consumer
                    // can render every wire-level field (agent_id, content,
                    // parent_agent_id, session_id, trace_id, ...) in /expand
                    // without us having to enumerate them here. Separately
                    // extract `content` and `agent_id` as named args because
                    // every consumer wants those for the live block header
                    // and body.
                    if let Ok(serde_json::Value::Object(map)) =
                        serde_json::from_str::<serde_json::Value>(&event.data)
                    {
                        let content = map
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let agent_id = map
                            .get("agent_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let fields: BTreeMap<String, serde_json::Value> = map.into_iter().collect();
                        handler.on_reasoning(&content, &agent_id, &fields);
                    }
                }
                _ => {
                    // aura.orchestrator.*, aura.session_info, aura.progress,
                    // aura.worker_phase, aura.mcp_status, and any future events.
                    // Consumers (REPL status notices, one-shot stderr) handle
                    // these via the orchestrator-event callback; on_raw_event
                    // above also captures them into the stream panel.
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        handler.on_orchestrator_event(event_name, &val);
                    }
                }
            }
            continue;
        }

        // Standard chat completion chunk
        let chunk: ChatCompletionChunk = match serde_json::from_str(&event.data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for choice in &chunk.choices {
            // Accumulate text content
            if let Some(ref content) = choice.delta.content {
                handler.on_token(content);
                full_response.push_str(content);
            }

            // Accumulate tool call deltas
            if let Some(ref tool_calls) = choice.delta.tool_calls {
                for tc in tool_calls {
                    let entry = tool_call_accumulators
                        .entry(tc.index)
                        .or_insert_with(|| (String::new(), String::new(), String::new()));

                    if let Some(ref id) = tc.id {
                        entry.0 = id.clone();
                    }
                    if let Some(ref func) = tc.function {
                        if let Some(ref name) = func.name {
                            entry.1 = name.clone();
                        }
                        if let Some(ref args) = func.arguments {
                            entry.2.push_str(args);
                        }
                    }
                }
            }

            // Track finish_reason
            if let Some(ref reason) = choice.finish_reason {
                finish_reason = Some(reason.clone());
            }
        }
    }

    // Determine result based on finish_reason and accumulated tool calls
    if finish_reason.as_deref() == Some("tool_calls") && !tool_call_accumulators.is_empty() {
        let mut tool_calls: Vec<(usize, AccumulatedToolCall)> = tool_call_accumulators
            .into_iter()
            .map(|(idx, (id, name, args))| {
                (
                    idx,
                    AccumulatedToolCall {
                        id,
                        name,
                        arguments: args,
                    },
                )
            })
            .collect();
        tool_calls.sort_by_key(|(idx, _)| *idx);
        let tool_calls = tool_calls.into_iter().map(|(_, tc)| tc).collect();

        Ok(StreamResult::ToolCalls {
            text: full_response,
            tool_calls,
            server_results,
        })
    } else {
        Ok(StreamResult::TextResponse(full_response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::sync::atomic::AtomicBool;

    /// Build a synthetic SSE event.
    fn sse(event_name: &str, data: &str) -> eventsource_stream::Event {
        eventsource_stream::Event {
            event: event_name.to_string(),
            data: data.to_string(),
            ..Default::default()
        }
    }

    /// Build a standard chat completion chunk JSON with text content.
    fn text_chunk(content: &str, finish_reason: Option<&str>) -> String {
        serde_json::json!({
            "choices": [{
                "delta": { "content": content },
                "finish_reason": finish_reason
            }]
        })
        .to_string()
    }

    /// Build a tool call delta chunk JSON.
    fn tool_call_chunk(
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
        finish_reason: Option<&str>,
    ) -> String {
        let mut tc = serde_json::json!({ "index": index });
        if let Some(id) = id {
            tc["id"] = serde_json::json!(id);
        }
        let mut func = serde_json::Map::new();
        if let Some(n) = name {
            func.insert("name".to_string(), serde_json::json!(n));
        }
        if let Some(a) = args {
            func.insert("arguments".to_string(), serde_json::json!(a));
        }
        if !func.is_empty() {
            tc["function"] = serde_json::Value::Object(func);
        }
        serde_json::json!({
            "choices": [{
                "delta": { "tool_calls": [tc] },
                "finish_reason": finish_reason
            }]
        })
        .to_string()
    }

    fn no_cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// Collected callback invocations for assertions.
    #[derive(Default)]
    struct Captures {
        tokens: Vec<String>,
        tools_requested: Vec<(String, String, BTreeMap<String, serde_json::Value>)>,
        tools_started: Vec<(String, String)>,
        tools_completed: Vec<(String, String, Option<String>)>,
        usages: Vec<(u64, u64)>,
        reasoning: Vec<(String, String)>,
        raw_events: Vec<(String, String)>,
        orchestrator_events: Vec<(String, serde_json::Value)>,
    }

    impl StreamHandler for Captures {
        fn on_token(&mut self, token: &str) {
            self.tokens.push(token.to_string());
        }
        fn on_tool_requested(
            &mut self,
            tool_id: &str,
            tool_name: &str,
            arguments: &BTreeMap<String, serde_json::Value>,
        ) {
            self.tools_requested.push((
                tool_id.to_string(),
                tool_name.to_string(),
                arguments.clone(),
            ));
        }
        fn on_tool_start(&mut self, tool_id: &str, tool_name: &str) {
            self.tools_started
                .push((tool_id.to_string(), tool_name.to_string()));
        }
        fn on_tool_complete(
            &mut self,
            tool_id: &str,
            tool_name: &str,
            _duration: Duration,
            result: Option<&str>,
        ) {
            self.tools_completed.push((
                tool_id.to_string(),
                tool_name.to_string(),
                result.map(|s| s.to_string()),
            ));
        }
        fn on_usage(&mut self, prompt_tokens: u64, completion_tokens: u64) {
            self.usages.push((prompt_tokens, completion_tokens));
        }
        fn on_reasoning(
            &mut self,
            content: &str,
            agent_id: &str,
            _fields: &BTreeMap<String, serde_json::Value>,
        ) {
            self.reasoning
                .push((content.to_string(), agent_id.to_string()));
        }
        fn on_raw_event(&mut self, event_name: &str, data: &str) {
            self.raw_events
                .push((event_name.to_string(), data.to_string()));
        }
        fn on_orchestrator_event(&mut self, event_name: &str, value: &serde_json::Value) {
            self.orchestrator_events
                .push((event_name.to_string(), value.clone()));
        }
    }

    /// Drive process_sse_events with a vec of synthetic events and tracking callbacks.
    async fn run_stream(
        events: Vec<eventsource_stream::Event>,
    ) -> (Result<StreamResult>, Captures) {
        let event_stream = stream::iter(
            events
                .into_iter()
                .map(Ok::<_, std::io::Error>)
                .collect::<Vec<_>>(),
        );
        let mut caps = Captures::default();
        let result = process_sse_events(event_stream, no_cancel(), &mut caps).await;
        (result, caps)
    }

    // -----------------------------------------------------------------------
    // Text response tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn text_only_response() {
        let events = vec![
            sse("", &text_chunk("Hello", None)),
            sse("", &text_chunk(" world", None)),
            sse("", &text_chunk("!", Some("stop"))),
            sse("", "[DONE]"),
        ];
        let (result, caps) = run_stream(events).await;
        match result.unwrap() {
            StreamResult::TextResponse(text) => assert_eq!(text, "Hello world!"),
            other => panic!("expected TextResponse, got {:?}", other),
        }
        assert_eq!(caps.tokens, vec!["Hello", " world", "!"]);
    }

    #[tokio::test]
    async fn empty_stream_returns_empty_text() {
        let (result, _) = run_stream(vec![]).await;
        match result.unwrap() {
            StreamResult::TextResponse(text) => assert_eq!(text, ""),
            other => panic!("expected TextResponse, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn done_marker_ends_stream() {
        let events = vec![
            sse("", &text_chunk("before", None)),
            sse("", "[DONE]"),
            sse("", &text_chunk("after", None)),
        ];
        let (result, caps) = run_stream(events).await;
        match result.unwrap() {
            StreamResult::TextResponse(text) => assert_eq!(text, "before"),
            other => panic!("expected TextResponse, got {:?}", other),
        }
        assert_eq!(caps.tokens.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Tool call tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn single_tool_call() {
        let events = vec![
            sse(
                "",
                &tool_call_chunk(0, Some("call_1"), Some("Shell"), Some("{\"co"), None),
            ),
            sse(
                "",
                &tool_call_chunk(0, None, None, Some("mmand\":\"ls\"}"), Some("tool_calls")),
            ),
            sse("", "[DONE]"),
        ];
        let (result, _) = run_stream(events).await;
        match result.unwrap() {
            StreamResult::ToolCalls { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "call_1");
                assert_eq!(tool_calls[0].name, "Shell");
                assert_eq!(tool_calls[0].arguments, "{\"command\":\"ls\"}");
            }
            other => panic!("expected ToolCalls, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn multiple_tool_calls_by_index() {
        let events = vec![
            sse(
                "",
                &tool_call_chunk(0, Some("c0"), Some("Read"), Some("{\"file\":\"a\"}"), None),
            ),
            sse(
                "",
                &tool_call_chunk(
                    1,
                    Some("c1"),
                    Some("Shell"),
                    Some("{\"cmd\":\"ls\"}"),
                    Some("tool_calls"),
                ),
            ),
            sse("", "[DONE]"),
        ];
        let (result, _) = run_stream(events).await;
        match result.unwrap() {
            StreamResult::ToolCalls { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 2);
                assert_eq!(tool_calls[0].name, "Read");
                assert_eq!(tool_calls[1].name, "Shell");
            }
            other => panic!("expected ToolCalls, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn tool_calls_with_partial_text() {
        let events = vec![
            sse("", &text_chunk("I'll help:", None)),
            sse(
                "",
                &tool_call_chunk(0, Some("c0"), Some("Shell"), Some("{}"), Some("tool_calls")),
            ),
            sse("", "[DONE]"),
        ];
        let (result, _) = run_stream(events).await;
        match result.unwrap() {
            StreamResult::ToolCalls {
                text, tool_calls, ..
            } => {
                assert_eq!(text, "I'll help:");
                assert_eq!(tool_calls.len(), 1);
            }
            other => panic!("expected ToolCalls, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn finish_stop_ignores_tool_accumulators() {
        // Tool deltas present but finish_reason="stop" — should return TextResponse
        let events = vec![
            sse(
                "",
                &tool_call_chunk(0, Some("c0"), Some("Shell"), Some("{}"), Some("stop")),
            ),
            sse("", "[DONE]"),
        ];
        let (result, _) = run_stream(events).await;
        match result.unwrap() {
            StreamResult::TextResponse(_) => {}
            other => panic!("expected TextResponse for stop, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Aura custom event tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn aura_tool_requested_callback() {
        let data = serde_json::json!({
            "tool_id": "call_1",
            "tool_name": "Shell",
            "arguments": {"command": "ls"},
            "agent_id": "main",
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::TOOL_REQUESTED, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.tools_requested.len(), 1);
        assert_eq!(caps.tools_requested[0].0, "call_1");
        assert_eq!(caps.tools_requested[0].1, "Shell");
        assert!(caps.tools_requested[0].2.contains_key("command"));
        // raw_event should also be called for named events
        assert!(
            caps.raw_events
                .iter()
                .any(|(n, _)| n == event_names::TOOL_REQUESTED)
        );
    }

    #[tokio::test]
    async fn aura_tool_start_callback() {
        let data = serde_json::json!({
            "tool_id": "call_1",
            "tool_name": "Read",
            "agent_id": "main",
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::TOOL_START, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(
            caps.tools_started,
            vec![("call_1".to_string(), "Read".to_string())]
        );
    }

    #[tokio::test]
    async fn aura_tool_complete_with_server_duration() {
        let data = serde_json::json!({
            "tool_id": "call_1",
            "tool_name": "Shell",
            "duration_ms": 1500,
            "success": true,
            "agent_id": "main",
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::TOOL_COMPLETE, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.tools_completed.len(), 1);
        assert_eq!(caps.tools_completed[0].0, "call_1");
        assert_eq!(caps.tools_completed[0].1, "Shell");
    }

    #[tokio::test]
    async fn aura_tool_complete_caches_error_on_failure() {
        // ToolComplete with success=false and an error string should be cached
        // as "Error: ..." so the model sees the real failure rather than a bogus
        // "executed successfully" placeholder later.
        let data = serde_json::json!({
            "tool_id": "call_fail",
            "tool_name": "Shell",
            "duration_ms": 50,
            "success": false,
            "error": "command not found",
            "agent_id": "main",
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::TOOL_COMPLETE, &data.to_string()),
            sse(
                "",
                &tool_call_chunk(
                    0,
                    Some("call_fail"),
                    Some("Shell"),
                    Some("{}"),
                    Some("tool_calls"),
                ),
            ),
            sse("", "[DONE]"),
        ];
        let (result, _) = run_stream(events).await;
        match result.unwrap() {
            StreamResult::ToolCalls { server_results, .. } => {
                assert_eq!(
                    server_results.get("call_fail").unwrap(),
                    "Error: command not found"
                );
            }
            other => panic!("expected ToolCalls, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn aura_tool_complete_caches_server_result() {
        let data = serde_json::json!({
            "tool_id": "call_99",
            "tool_name": "Shell",
            "duration_ms": 100,
            "success": true,
            "result": "file_output",
            "agent_id": "main",
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::TOOL_COMPLETE, &data.to_string()),
            // Now add a tool call with the same id and finish
            sse(
                "",
                &tool_call_chunk(
                    0,
                    Some("call_99"),
                    Some("Shell"),
                    Some("{}"),
                    Some("tool_calls"),
                ),
            ),
            sse("", "[DONE]"),
        ];
        let (result, _) = run_stream(events).await;
        match result.unwrap() {
            StreamResult::ToolCalls { server_results, .. } => {
                assert_eq!(server_results.get("call_99").unwrap(), "file_output");
            }
            other => panic!("expected ToolCalls, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn aura_usage_callback() {
        let data = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150,
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::USAGE, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.usages, vec![(100, 50)]);
    }

    #[tokio::test]
    async fn aura_orchestrator_event_callback() {
        let data = serde_json::json!({"goal": "compute sum"});
        let events = vec![
            sse(event_names::PLAN_CREATED, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.orchestrator_events.len(), 1);
        assert_eq!(caps.orchestrator_events[0].0, event_names::PLAN_CREATED);
    }

    #[tokio::test]
    async fn aura_session_info_routed_to_orchestrator() {
        let data = serde_json::json!({"model": "gpt-4o"});
        let events = vec![
            sse(event_names::SESSION_INFO, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.orchestrator_events.len(), 1);
        assert_eq!(caps.orchestrator_events[0].0, event_names::SESSION_INFO);
    }

    #[tokio::test]
    async fn aura_reasoning_callback_fires() {
        let data = serde_json::json!({
            "content": "Let me think about this problem step by step.",
            "agent_id": "main",
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::REASONING, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.reasoning.len(), 1);
        assert_eq!(
            caps.reasoning[0].0,
            "Let me think about this problem step by step."
        );
        assert_eq!(caps.reasoning[0].1, "main");
        // Reasoning should NOT have been routed to the orchestrator fallback
        assert!(caps.orchestrator_events.is_empty());
        // But it IS a named event, so raw_event should have captured it
        assert!(
            caps.raw_events
                .iter()
                .any(|(n, _)| n == event_names::REASONING)
        );
    }

    #[tokio::test]
    async fn aura_reasoning_callback_carries_worker_agent_id() {
        // In orchestration mode the server emits aura.reasoning with the
        // worker's agent_id so the CLI can attribute reasoning to a worker.
        let data = serde_json::json!({
            "content": "Analyzing the logs.",
            "agent_id": "log_worker",
            "parent_agent_id": "coordinator",
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::REASONING, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.reasoning.len(), 1);
        assert_eq!(caps.reasoning[0].1, "log_worker");
    }

    #[tokio::test]
    async fn aura_reasoning_chunks_accumulate() {
        let chunk = |c: &str| {
            serde_json::json!({
                "content": c,
                "agent_id": "main",
                "session_id": "s1"
            })
            .to_string()
        };
        let events = vec![
            sse(event_names::REASONING, &chunk("First, ")),
            sse(event_names::REASONING, &chunk("I'll check ")),
            sse(event_names::REASONING, &chunk("the inputs.")),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.reasoning.len(), 3);
        let joined: String = caps.reasoning.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(joined, "First, I'll check the inputs.");
    }

    #[tokio::test]
    async fn aura_progress_routed_to_orchestrator() {
        let data = serde_json::json!({"message": "Discovering tools"});
        let events = vec![
            sse(event_names::PROGRESS, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.orchestrator_events.len(), 1);
        assert_eq!(caps.orchestrator_events[0].0, event_names::PROGRESS);
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn raw_event_only_for_named_events() {
        let events = vec![
            sse(
                event_names::USAGE,
                &serde_json::json!({"prompt_tokens":1,"completion_tokens":1}).to_string(),
            ),
            sse("", &text_chunk("hi", Some("stop"))), // empty event name
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        // Only the named event should appear in raw_events
        assert_eq!(caps.raw_events.len(), 1);
        assert_eq!(caps.raw_events[0].0, event_names::USAGE);
    }

    #[tokio::test]
    async fn invalid_json_chunk_skipped() {
        let events = vec![
            sse("", "this is not valid json"),
            sse("", &text_chunk("ok", Some("stop"))),
            sse("", "[DONE]"),
        ];
        let (result, caps) = run_stream(events).await;
        // Should not panic, should get the valid token
        match result.unwrap() {
            StreamResult::TextResponse(text) => assert_eq!(text, "ok"),
            other => panic!("expected TextResponse, got {:?}", other),
        }
        assert_eq!(caps.tokens, vec!["ok"]);
    }

    #[tokio::test]
    async fn cancel_flag_stops_stream() {
        let events = vec![
            sse("", &text_chunk("first", None)),
            sse("", &text_chunk("second", None)),
            sse("", "[DONE]"),
        ];
        let event_stream = stream::iter(
            events
                .into_iter()
                .map(Ok::<_, std::io::Error>)
                .collect::<Vec<_>>(),
        );
        let cancel = Arc::new(AtomicBool::new(true)); // pre-cancelled
        let result = process_sse_events(event_stream, cancel, &mut NoopHandler)
            .await
            .unwrap();
        match result {
            StreamResult::TextResponse(text) => assert_eq!(text, ""),
            other => panic!("expected empty TextResponse on cancel, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn aura_tool_usage_silently_skipped() {
        let data = serde_json::json!({"tool_ids": ["c1"], "prompt_tokens": 10});
        let events = vec![
            sse(event_names::TOOL_USAGE, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        // tool_usage is explicitly skipped — only raw_event should fire
        assert!(caps.usages.is_empty());
        assert!(caps.orchestrator_events.is_empty());
        // But it IS a named event, so raw_event should have captured it
        assert_eq!(caps.raw_events.len(), 1);
    }

    #[tokio::test]
    async fn usage_event_with_all_fields() {
        let data = serde_json::json!({
            "prompt_tokens": 200,
            "completion_tokens": 75,
            "total_tokens": 275,
            "session_id": "s1"
        });
        let events = vec![
            sse(event_names::USAGE, &data.to_string()),
            sse("", "[DONE]"),
        ];
        let (_, caps) = run_stream(events).await;
        assert_eq!(caps.usages, vec![(200, 75)]);
    }
}
