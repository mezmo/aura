// ---------------------------------------------------------------------------
// SSE Stream panel state and rendering
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::sync::atomic::Ordering;

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{Color, Stylize};
use crossterm::terminal;

use serde::{Deserialize, Serialize};

use super::state::{
    CURSOR_ROW, EXPANDED_OUTPUT, FRAME_LINES, STREAM_CONV_DIR, STREAM_PANEL, STREAM_PANEL_DIRTY,
    term_size,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSseEvent {
    pub index: usize,
    pub event_name: String,
    pub data: String,
    pub parsed_data: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub display_id: Option<String>,
}

pub(crate) struct StreamPanelState {
    pub(crate) events: Vec<RawSseEvent>,
    pub(crate) visible: bool,
    pub(crate) focused: bool,
    pub(crate) scroll_offset: usize,
    pub(crate) selected_index: usize,
    pub(crate) expanded_index: Option<usize>,
    pub(crate) next_seq: usize,
    pub(crate) show_all: bool,
}

impl StreamPanelState {
    pub(crate) const fn new() -> Self {
        Self {
            events: Vec::new(),
            visible: false,
            focused: false,
            scroll_offset: 0,
            selected_index: 0,
            expanded_index: None,
            next_seq: 0,
            show_all: false,
        }
    }
}

/// Maximum number of content rows displayed in the stream panel.
pub(crate) const AURA_EVENT_PANEL_MAX_SHOWN: usize = 20;

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Extract a display ID from an SSE event's raw JSON data based on the event name.
fn extract_display_id(event_name: &str, data: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(data).ok()?;

    let try_field = |v: &serde_json::Value, field: &str| -> Option<String> {
        v.get(field)
            .and_then(|f| f.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                v.get("data")
                    .and_then(|d| d.get(field))
                    .and_then(|f| f.as_str())
                    .map(|s| s.to_string())
            })
    };

    match event_name {
        "aura.tool_requested" | "aura.tool_start" | "aura.tool_complete" => {
            try_field(&val, "tool_id")
        }
        "aura.usage" | "aura.session_info" => try_field(&val, "session_id"),
        "message" => try_field(&val, "id"),
        "aura.orchestrator.tool_call_started" | "aura.orchestrator.tool_call_completed" => {
            try_field(&val, "tool_call_id")
        }
        "aura.orchestrator.task_started" | "aura.orchestrator.task_completed" => {
            try_field(&val, "task_id")
        }
        "aura.orchestrator.plan_created"
        | "aura.orchestrator.iteration_complete"
        | "aura.orchestrator.synthesizing" => try_field(&val, "session_id"),
        _ => try_field(&val, "id")
            .or_else(|| try_field(&val, "session_id"))
            .or_else(|| try_field(&val, "tool_id")),
    }
}

/// Return indices into `state.events` for events that pass the current filter.
fn visible_event_indices(state: &StreamPanelState) -> Vec<usize> {
    state
        .events
        .iter()
        .enumerate()
        .filter(|(_, evt)| state.show_all || evt.event_name.starts_with("aura."))
        .map(|(i, _)| i)
        .collect()
}

/// Truncate a string to at most `max` display characters.
fn truncate_to_width(s: &str, max: usize) -> String {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    // Truncate at the byte offset of the (max)th character.
    s[..chars[max].0].to_string()
}

/// One display line in the expanded event detail view.
struct DetailLine {
    prefix: String,
    key: String,
    value: Option<String>,
}

/// Build the flattened list of detail lines for an expanded event.
fn build_detail_lines(
    map: &BTreeMap<String, serde_json::Value>,
    max_lines: usize,
    width: usize,
) -> Vec<DetailLine> {
    let mut lines: Vec<DetailLine> = Vec::new();
    let entries: Vec<_> = map.iter().collect();
    let total_l1 = entries.len();
    for (l1_idx, (key, val)) in entries.iter().enumerate() {
        if lines.len() >= max_lines {
            break;
        }
        let is_last_l1 = l1_idx == total_l1 - 1;
        let l1_connector = if is_last_l1 { "  └─" } else { "  ├─" };
        let l1_child_cont = if is_last_l1 { "     " } else { "  │  " };

        let obj_to_expand: Option<serde_json::Map<String, serde_json::Value>> = match val {
            serde_json::Value::Object(obj) => Some(obj.clone()),
            serde_json::Value::String(s) => {
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(s).ok()
            }
            _ => None,
        };
        if let Some(obj) = obj_to_expand {
            lines.push(DetailLine {
                prefix: l1_connector.to_string(),
                key: (*key).clone(),
                value: None,
            });
            let sub_entries: Vec<_> = obj.iter().collect();
            let total_l2 = sub_entries.len();
            for (l2_idx, (sub_key, sub_val)) in sub_entries.iter().enumerate() {
                if lines.len() >= max_lines {
                    break;
                }
                let is_last_l2 = l2_idx == total_l2 - 1;
                let l2_connector = if is_last_l2 { "└─" } else { "├─" };
                let sub_str = sub_val.to_string().replace('\n', " ");
                let max_val = width.saturating_sub(sub_key.len() + 8);
                let display = if sub_str.len() > max_val {
                    format!("{}…", &sub_str[..max_val.saturating_sub(1)])
                } else {
                    sub_str
                };
                lines.push(DetailLine {
                    prefix: format!("{}{}", l1_child_cont, l2_connector),
                    key: (*sub_key).clone(),
                    value: Some(display),
                });
            }
        } else {
            let val_str = match val {
                serde_json::Value::String(s) => s.replace('\n', " "),
                other => other.to_string(),
            };
            let max_val = width.saturating_sub(key.len() + 6);
            let display = if val_str.len() > max_val {
                format!("{}…", &val_str[..max_val.saturating_sub(1)])
            } else {
                val_str
            };
            lines.push(DetailLine {
                prefix: l1_connector.to_string(),
                key: (*key).clone(),
                value: Some(display),
            });
        }
    }
    lines
}

/// Build the stream panel top-border label string.
fn stream_panel_label(state: &StreamPanelState, visible: &[usize]) -> String {
    let total = state.events.len();
    let vis = visible.len();
    if state.show_all || vis == total {
        format!(" SSE Stream ({total} events) ")
    } else {
        format!(" SSE Stream ({vis}/{total} events) ")
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Write stream panel rows sequentially. Caller must position cursor one row
/// above where the panel starts.
pub(crate) fn write_stream_panel_rows_locked(
    stdout: &mut io::Stdout,
    state: &StreamPanelState,
    width: usize,
) {
    let visible = visible_event_indices(state);
    let visible_count = visible.len();

    // Top border
    let label = stream_panel_label(state, &visible);
    let side_len = width.saturating_sub(label.len()) / 2;
    let top_border = format!(
        "{}{}{}",
        "─".repeat(side_len),
        label,
        "─".repeat(width.saturating_sub(side_len + label.len()))
    );
    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!("{}", top_border.as_str().with(Color::DarkGrey));

    // Content rows
    if let Some(vis_idx) = state.expanded_index {
        let raw_idx = visible.get(vis_idx).copied();
        if let Some(evt) = raw_idx.and_then(|ri| state.events.get(ri)) {
            let detail_color = if evt.event_name.starts_with("aura.") {
                Color::Rgb {
                    r: 200,
                    g: 180,
                    b: 60,
                }
            } else if evt.event_name == "message" {
                Color::Rgb {
                    r: 110,
                    g: 140,
                    b: 220,
                }
            } else {
                Color::DarkGrey
            };
            let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
            let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
            let id_label = match &evt.display_id {
                Some(id) => id.clone(),
                None => format!("#{}", evt.index),
            };
            let header =
                truncate_to_width(&format!("  {} ({}) ▾", evt.event_name, id_label), width);
            print!("{}", header.as_str().with(detail_color));
            if let Some(ref map) = evt.parsed_data {
                let detail = build_detail_lines(map, AURA_EVENT_PANEL_MAX_SHOWN - 1, width);
                let lines_printed = detail.len();
                for line in &detail {
                    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
                    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
                    let text = match &line.value {
                        Some(v) => format!("{} {}: {}", line.prefix, line.key, v),
                        None => format!("{} {}:", line.prefix, line.key),
                    };
                    let text = truncate_to_width(&text, width);
                    print!("{}", text.as_str().with(detail_color));
                }
                for _ in lines_printed..(AURA_EVENT_PANEL_MAX_SHOWN - 1) {
                    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
                    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
                }
            } else {
                let max_data = width.saturating_sub(6);
                let display = if evt.data.len() > max_data {
                    format!("{}…", &evt.data[..max_data.saturating_sub(1)])
                } else {
                    evt.data.clone()
                };
                let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
                let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
                print!(
                    "{} {}",
                    "  └─".with(detail_color),
                    display.as_str().with(detail_color),
                );
                for _ in 0..(AURA_EVENT_PANEL_MAX_SHOWN - 2) {
                    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
                    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
                }
            }
        } else {
            for _ in 0..AURA_EVENT_PANEL_MAX_SHOWN {
                let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
                let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
            }
        }
    } else {
        let max_name_len = visible
            .iter()
            .filter_map(|&ri| {
                let evt = &state.events[ri];
                evt.display_id.as_ref().map(|_| evt.event_name.len())
            })
            .max()
            .unwrap_or(0);
        for row in 0..AURA_EVENT_PANEL_MAX_SHOWN {
            let vis_pos = state.scroll_offset + row;
            let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
            let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
            if vis_pos < visible_count {
                let raw_idx = visible[vis_pos];
                let evt = &state.events[raw_idx];
                let is_selected = state.focused && vis_pos == state.selected_index;
                let marker = if is_selected { "▸" } else { " " };
                let line = if let Some(ref id) = evt.display_id {
                    format!(
                        " {} {:<width$} - {}",
                        marker,
                        evt.event_name,
                        id,
                        width = max_name_len
                    )
                } else {
                    format!(" {} {}", marker, evt.event_name)
                };
                let line = truncate_to_width(&line, width);
                if is_selected {
                    print!("{}", line.as_str().with(Color::White));
                } else if evt.event_name.starts_with("aura.") {
                    print!(
                        "{}",
                        line.as_str().with(Color::Rgb {
                            r: 200,
                            g: 180,
                            b: 60
                        })
                    );
                } else if evt.event_name == "message" {
                    print!(
                        "{}",
                        line.as_str().with(Color::Rgb {
                            r: 110,
                            g: 140,
                            b: 220
                        })
                    );
                } else {
                    print!("{}", line.as_str().with(Color::DarkGrey));
                }
            }
        }
    }

    // Bottom border
    let bottom_border = "─".repeat(width);
    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!("{}", bottom_border.as_str().with(Color::DarkGrey));
}

/// Render the stream panel content lines via println (for non-streaming contexts).
#[allow(dead_code)]
pub(crate) fn render_stream_panel() {
    if let Ok(state) = STREAM_PANEL.lock() {
        if !state.visible {
            return;
        }
        let (width, _) = term_size();
        let visible = visible_event_indices(&state);
        let visible_count = visible.len();

        // Top border
        let label = stream_panel_label(&state, &visible);
        let side_len = (width as usize).saturating_sub(label.len()) / 2;
        let top_border = format!(
            "{}{}{}",
            "─".repeat(side_len),
            label,
            "─".repeat((width as usize).saturating_sub(side_len + label.len()))
        );
        println!("{}", top_border.as_str().with(Color::DarkGrey));

        if let Some(vis_idx) = state.expanded_index {
            let raw_idx = visible.get(vis_idx).copied();
            if let Some(evt) = raw_idx.and_then(|ri| state.events.get(ri)) {
                let detail_color = if evt.event_name.starts_with("aura.") {
                    Color::Rgb {
                        r: 200,
                        g: 180,
                        b: 60,
                    }
                } else if evt.event_name == "message" {
                    Color::Rgb {
                        r: 110,
                        g: 140,
                        b: 220,
                    }
                } else {
                    Color::DarkGrey
                };
                let id_label = match &evt.display_id {
                    Some(id) => id.clone(),
                    None => format!("#{}", evt.index),
                };
                let header = truncate_to_width(
                    &format!("  {} ({}) ▾", evt.event_name, id_label),
                    width as usize,
                );
                println!("{}", header.as_str().with(detail_color));
                if let Some(ref map) = evt.parsed_data {
                    let detail =
                        build_detail_lines(map, AURA_EVENT_PANEL_MAX_SHOWN - 1, width as usize);
                    let lines_printed = detail.len();
                    for line in &detail {
                        let text = match &line.value {
                            Some(v) => format!("{} {}: {}", line.prefix, line.key, v),
                            None => format!("{} {}:", line.prefix, line.key),
                        };
                        let text = truncate_to_width(&text, width as usize);
                        println!("{}", text.as_str().with(detail_color));
                    }
                    for _ in lines_printed..(AURA_EVENT_PANEL_MAX_SHOWN - 1) {
                        println!();
                    }
                } else {
                    let max_data = (width as usize).saturating_sub(6);
                    let display = if evt.data.len() > max_data {
                        format!("{}…", &evt.data[..max_data.saturating_sub(1)])
                    } else {
                        evt.data.clone()
                    };
                    println!(
                        "{} {}",
                        "└─".with(detail_color),
                        display.as_str().with(detail_color),
                    );
                    for _ in 0..(AURA_EVENT_PANEL_MAX_SHOWN - 2) {
                        println!();
                    }
                }
            } else {
                for _ in 0..AURA_EVENT_PANEL_MAX_SHOWN {
                    println!();
                }
            }
        } else {
            let max_name_len = visible
                .iter()
                .filter_map(|&ri| {
                    let evt = &state.events[ri];
                    evt.display_id.as_ref().map(|_| evt.event_name.len())
                })
                .max()
                .unwrap_or(0);
            for row in 0..AURA_EVENT_PANEL_MAX_SHOWN {
                let vis_pos = state.scroll_offset + row;
                if vis_pos < visible_count {
                    let raw_idx = visible[vis_pos];
                    let evt = &state.events[raw_idx];
                    let is_selected = state.focused && vis_pos == state.selected_index;
                    let marker = if is_selected { "▸" } else { " " };
                    let line = if let Some(ref id) = evt.display_id {
                        format!(
                            " {} {:<width$} - {}",
                            marker,
                            evt.event_name,
                            id,
                            width = max_name_len
                        )
                    } else {
                        format!(" {} {}", marker, evt.event_name)
                    };
                    let line = truncate_to_width(&line, width as usize);
                    if is_selected {
                        println!("{}", line.as_str().with(Color::White));
                    } else if evt.event_name.starts_with("aura.") {
                        println!(
                            "{}",
                            line.as_str().with(Color::Rgb {
                                r: 200,
                                g: 180,
                                b: 60
                            })
                        );
                    } else if evt.event_name == "message" {
                        println!(
                            "{}",
                            line.as_str().with(Color::Rgb {
                                r: 110,
                                g: 140,
                                b: 220
                            })
                        );
                    } else {
                        println!("{}", line.as_str().with(Color::DarkGrey));
                    }
                } else {
                    println!();
                }
            }
        }

        // Bottom border
        let bottom_border = "─".repeat(width as usize);
        print!("{}", bottom_border.as_str().with(Color::DarkGrey));
    }
}

/// Render the stream panel in-place without moving the cursor permanently.
///
/// When the cursor is near the bottom of the terminal there may not be
/// enough room below the frame for the panel's rows.  In that case we
/// scroll the terminal upward to create space, render the panel, and
/// then compensate the saved cursor position so the prompt stays put.
pub(crate) fn render_stream_panel_in_place() {
    if !is_stream_panel_visible() {
        return;
    }
    let mut stdout = io::stdout();

    let fl = FRAME_LINES.load(Ordering::Relaxed) as i32;
    let cr = CURSOR_ROW.load(Ordering::Relaxed) as i32;
    let sr = super::state::status_rows() as i32;
    let sp = stream_panel_rows() as i32; // rows the panel needs (content + borders)

    // How many rows from the cursor to the bottom of where the panel ends:
    // frame lines below cursor (border) + status rows + panel rows
    let rows_below_cursor = (fl - cr) + sr + sp;

    // Check available space
    let (_, term_h) = term_size();
    let _ = stdout.flush();
    let cur_row = cursor::position().map(|(_, r)| r as i32).unwrap_or(0);
    let available_below = (term_h as i32) - cur_row; // rows from cursor to bottom
    let need_scroll = (rows_below_cursor - available_below).max(0) as u16;

    if need_scroll > 0 {
        // Scroll terminal up to make room, then move cursor up to compensate
        let _ = execute!(stdout, terminal::ScrollUp(need_scroll));
        let _ = execute!(stdout, cursor::MoveUp(need_scroll));
    }

    let _ = execute!(stdout, cursor::SavePosition);

    // Navigate to the last status row; write_stream_panel_rows_locked's
    // initial MoveDown(1) will advance to the first panel row.
    let down = (fl - cr) + sr;
    if down > 0 {
        let _ = execute!(stdout, cursor::MoveDown(down as u16));
    }
    let _ = execute!(stdout, cursor::MoveToColumn(0));

    if let Ok(state) = STREAM_PANEL.lock() {
        if !state.visible {
            let _ = execute!(stdout, cursor::RestorePosition);
            let _ = stdout.flush();
            return;
        }
        let (width, _) = term_size();
        write_stream_panel_rows_locked(&mut stdout, &state, width as usize);
    }

    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

// ---------------------------------------------------------------------------
// Disk persistence
// ---------------------------------------------------------------------------

/// Set the conversation directory used for SSE event persistence.
pub fn set_stream_conv_dir(dir: Option<std::path::PathBuf>) {
    if let Ok(mut g) = STREAM_CONV_DIR.lock() {
        *g = dir;
    }
}

/// Append a single SSE event line to `events.jsonl`.
fn append_sse_event_to_disk(event_name: &str, data: &str) {
    let dir = match STREAM_CONV_DIR.lock().ok().and_then(|g| g.clone()) {
        Some(d) => d,
        None => return,
    };
    let path = dir.join("events.jsonl");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let line = serde_json::json!({ "event_name": event_name, "data": data });
        let _ = writeln!(f, "{}", line);
    }
}

/// Load SSE events from `events.jsonl` and restore them into the stream panel.
pub fn load_and_restore_sse_events(dir: &std::path::Path) {
    let path = dir.join("events.jsonl");
    let data = match fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut events = Vec::new();
    for line in data.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && let (Some(event_name), Some(evt_data)) =
                (val["event_name"].as_str(), val["data"].as_str())
        {
            let display_id = extract_display_id(event_name, evt_data);
            events.push(RawSseEvent {
                index: events.len(),
                event_name: event_name.to_string(),
                data: evt_data.to_string(),
                parsed_data: None,
                display_id,
            });
        }
    }
    if events.is_empty() {
        return;
    }
    if let Ok(mut state) = STREAM_PANEL.lock() {
        let next_seq = events.len();
        state.events = events;
        state.next_seq = next_seq;
        state.scroll_offset = 0;
        state.selected_index = 0;
        state.expanded_index = None;
        state.focused = false;
        // Don't force the panel visible on resume; let the user toggle it with /stream
        state.visible = false;
        state.show_all = EXPANDED_OUTPUT.load(Ordering::Relaxed);
    }
    STREAM_PANEL_DIRTY.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Append an SSE event to the stream panel and persist to disk.
pub fn push_sse_event(event_name: &str, data: &str) {
    if let Ok(mut state) = STREAM_PANEL.lock() {
        let index = state.next_seq;
        state.next_seq += 1;
        let display_id = extract_display_id(event_name, data);
        state.events.push(RawSseEvent {
            index,
            event_name: event_name.to_string(),
            data: data.to_string(),
            parsed_data: None,
            display_id,
        });
        if !state.focused {
            state.scroll_offset = 0;
            state.selected_index = 0;
        }
    }
    append_sse_event_to_disk(event_name, data);
    STREAM_PANEL_DIRTY.store(true, Ordering::Relaxed);
}

/// Toggle the stream panel visibility.
pub fn toggle_stream_panel() {
    if let Ok(mut state) = STREAM_PANEL.lock() {
        state.visible = !state.visible;
        state.focused = false;
        state.expanded_index = None;
    }
}

/// Clear the stream panel area on screen (used when toggling the panel off).
pub(crate) fn clear_stream_panel_in_place() {
    let mut stdout = io::stdout();
    let _ = stdout.flush();

    // Record actual cursor row before navigating
    let start_row = cursor::position().map(|(_, r)| r).unwrap_or(0);

    let fl = FRAME_LINES.load(Ordering::Relaxed) as i32;
    let cr = CURSOR_ROW.load(Ordering::Relaxed) as i32;
    let sr = super::state::status_rows() as i32;
    // Use the full panel height so we clear all rows that were rendered.
    let sp = (AURA_EVENT_PANEL_MAX_SHOWN as i32) + 2;

    let down = (fl - cr) + sr;
    if down > 0 {
        let _ = execute!(stdout, cursor::MoveDown(down as u16));
    }
    let _ = execute!(stdout, cursor::MoveToColumn(0));

    // Clear each row the panel occupied
    for _ in 0..sp {
        let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    }

    // Return to the exact original row (MoveDown may have been clamped
    // at the terminal bottom, so we can't just MoveUp by the same amount).
    let _ = stdout.flush();
    let end_row = cursor::position().map(|(_, r)| r).unwrap_or(0);
    let actual_down = end_row.saturating_sub(start_row);
    if actual_down > 0 {
        let _ = execute!(stdout, cursor::MoveUp(actual_down), cursor::MoveToColumn(0));
    }
    let _ = stdout.flush();
}

/// Clear all stream events (called on /clear).
pub fn clear_stream_events() {
    if let Ok(mut state) = STREAM_PANEL.lock() {
        state.events.clear();
        state.scroll_offset = 0;
        state.selected_index = 0;
        state.expanded_index = None;
        state.focused = false;
        state.next_seq = 0;
    }
    if let Some(dir) = STREAM_CONV_DIR.lock().ok().and_then(|g| g.clone()) {
        let _ = fs::remove_file(dir.join("events.jsonl"));
    }
}

/// Set whether the stream panel shows all events or only aura.* events.
pub fn set_stream_show_all(show_all: bool) {
    if let Ok(mut state) = STREAM_PANEL.lock() {
        state.show_all = show_all;
        let visible = visible_event_indices(&state);
        let vis_len = visible.len();
        if state.selected_index >= vis_len {
            state.selected_index = vis_len.saturating_sub(1);
        }
        if state.scroll_offset + AURA_EVENT_PANEL_MAX_SHOWN > vis_len && vis_len > 0 {
            state.scroll_offset = vis_len.saturating_sub(AURA_EVENT_PANEL_MAX_SHOWN);
        }
        state.expanded_index = None;
    }
    STREAM_PANEL_DIRTY.store(true, Ordering::Relaxed);
}

/// Whether the stream panel is currently visible.
pub fn is_stream_panel_visible() -> bool {
    STREAM_PANEL.lock().map(|s| s.visible).unwrap_or(false)
}

/// Whether the stream panel is currently focused.
pub fn is_stream_panel_focused() -> bool {
    STREAM_PANEL.lock().map(|s| s.focused).unwrap_or(false)
}

/// Number of terminal rows the stream panel occupies.
pub(crate) fn stream_panel_rows() -> u16 {
    if is_stream_panel_visible() {
        AURA_EVENT_PANEL_MAX_SHOWN as u16 + 2
    } else {
        0
    }
}

/// Enter focus mode on the stream panel.
pub fn enter_stream_focus() -> bool {
    if let Ok(mut state) = STREAM_PANEL.lock() {
        if !state.visible {
            return false;
        }
        let visible = visible_event_indices(&state);
        if visible.is_empty() {
            return false;
        }
        state.focused = true;
        state.selected_index = 0;
        state.scroll_offset = 0;
        state.expanded_index = None;
    }
    STREAM_PANEL_DIRTY.store(true, Ordering::Relaxed);
    render_stream_panel_in_place();
    true
}

/// Exit focus mode on the stream panel.
pub fn exit_stream_focus() {
    if let Ok(mut state) = STREAM_PANEL.lock() {
        state.focused = false;
        state.expanded_index = None;
    }
    STREAM_PANEL_DIRTY.store(true, Ordering::Relaxed);
    render_stream_panel_in_place();
}

/// Scroll stream panel selection down.
pub fn scroll_stream_down() -> bool {
    let changed = if let Ok(mut state) = STREAM_PANEL.lock() {
        let visible = visible_event_indices(&state);
        let vis_len = visible.len();
        if vis_len == 0 || state.selected_index >= vis_len - 1 {
            false
        } else {
            state.selected_index += 1;
            if state.selected_index >= state.scroll_offset + AURA_EVENT_PANEL_MAX_SHOWN {
                state.scroll_offset = state.selected_index - (AURA_EVENT_PANEL_MAX_SHOWN - 1);
            }
            state.expanded_index = None;
            true
        }
    } else {
        false
    };
    if changed {
        render_stream_panel_in_place();
    }
    changed
}

/// Scroll stream panel selection up.
pub fn scroll_stream_up() -> bool {
    let changed = if let Ok(mut state) = STREAM_PANEL.lock() {
        if state.selected_index == 0 {
            false
        } else {
            state.selected_index -= 1;
            if state.selected_index < state.scroll_offset {
                state.scroll_offset = state.selected_index;
            }
            state.expanded_index = None;
            true
        }
    } else {
        false
    };
    if changed {
        render_stream_panel_in_place();
    }
    changed
}

/// Scroll stream panel selection down by a page.
pub fn scroll_stream_page_down() -> bool {
    let changed = if let Ok(mut state) = STREAM_PANEL.lock() {
        let visible = visible_event_indices(&state);
        let vis_len = visible.len();
        if vis_len == 0 || state.selected_index >= vis_len - 1 {
            false
        } else {
            state.selected_index = (state.selected_index + 10).min(vis_len - 1);
            if state.selected_index >= state.scroll_offset + AURA_EVENT_PANEL_MAX_SHOWN {
                state.scroll_offset = state.selected_index - (AURA_EVENT_PANEL_MAX_SHOWN - 1);
            }
            state.expanded_index = None;
            true
        }
    } else {
        false
    };
    if changed {
        render_stream_panel_in_place();
    }
    changed
}

/// Scroll stream panel selection up by a page.
pub fn scroll_stream_page_up() -> bool {
    let changed = if let Ok(mut state) = STREAM_PANEL.lock() {
        if state.selected_index == 0 {
            false
        } else {
            state.selected_index = state.selected_index.saturating_sub(10);
            if state.selected_index < state.scroll_offset {
                state.scroll_offset = state.selected_index;
            }
            state.expanded_index = None;
            true
        }
    } else {
        false
    };
    if changed {
        render_stream_panel_in_place();
    }
    changed
}

/// Check if the stream panel is focused and selected_index is at the top.
pub fn at_stream_top() -> bool {
    STREAM_PANEL
        .lock()
        .map(|s| s.focused && s.selected_index == 0)
        .unwrap_or(false)
}

/// Toggle expand/collapse of the selected event.
pub fn toggle_stream_expand() {
    if let Ok(mut state) = STREAM_PANEL.lock() {
        let visible = visible_event_indices(&state);
        if visible.is_empty() {
            return;
        }
        let vis_idx = state.selected_index;
        if state.expanded_index == Some(vis_idx) {
            state.expanded_index = None;
        } else {
            if let Some(&raw_idx) = visible.get(vis_idx)
                && state.events[raw_idx].parsed_data.is_none()
                && let Ok(map) = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(
                    &state.events[raw_idx].data,
                )
            {
                state.events[raw_idx].parsed_data = Some(map);
            }
            state.expanded_index = Some(vis_idx);
        }
    }
    render_stream_panel_in_place();
}
