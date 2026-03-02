//! Shared SSE (Server-Sent Events) parsing utilities for integration tests.

/// A parsed SSE event with optional event type.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event_type: Option<String>,
    pub data: String,
}

/// Parse a single SSE data line into an `SseEvent`.
///
/// Returns `None` for `[DONE]` markers and non-data lines.
pub fn parse_data_line(line: &str) -> Option<SseEvent> {
    let data = line.strip_prefix("data: ")?;
    if data == "[DONE]" {
        return None;
    }
    Some(SseEvent {
        event_type: None,
        data: data.to_string(),
    })
}

/// Parse a full SSE response body into a `Vec<SseEvent>` and a boolean indicating if `[DONE]` was seen.
///
/// Handles `event:` type lines, filters `[DONE]` markers (returning true in the bool), and skips blank lines.
pub fn parse_sse_stream(body: &str) -> (Vec<SseEvent>, bool) {
    let mut events = Vec::new();
    let mut found_done = false;
    let mut current_event_type: Option<String> = None;

    for line in body.lines() {
        if line.is_empty() {
            continue;
        }

        if let Some(event) = line.strip_prefix("event: ") {
            current_event_type = Some(event.to_string());
            continue;
        }

        if let Some(data) = line.strip_prefix("data: ") {
            if data == "[DONE]" {
                found_done = true;
                continue;
            }
            events.push(SseEvent {
                event_type: current_event_type.take(),
                data: data.to_string(),
            });
        }
    }

    (events, found_done)
}

/// Filter events by exact event type match.
pub fn events_by_type<'a>(events: &'a [SseEvent], event_type: &str) -> Vec<&'a SseEvent> {
    events
        .iter()
        .filter(|e| e.event_type.as_deref() == Some(event_type))
        .collect()
}

/// Extract OpenAI-compatible JSON chunks from an SSE stream.
///
/// Filters out custom `aura.*` events and parses the data payload as JSON.
/// Returns the list of parsed JSON values and a boolean indicating if `[DONE]` was seen.
pub fn extract_openai_chunks(body: &str) -> (Vec<serde_json::Value>, bool) {
    let (events, found_done) = parse_sse_stream(body);
    let chunks = events
        .into_iter()
        .filter(|e| {
            // Keep only events that are NOT aura.* events
            !e.event_type
                .as_deref()
                .map(|t| t.starts_with("aura."))
                .unwrap_or(false)
        })
        .filter_map(|e| serde_json::from_str(&e.data).ok())
        .collect();
    (chunks, found_done)
}
