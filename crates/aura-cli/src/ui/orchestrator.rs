// ---------------------------------------------------------------------------
// Orchestrator tool call tracking (duration display + blinking icons)
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::time::Instant;

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{Color, Stylize};
use crossterm::terminal;

use super::state::{
    ACTIVE_ORCH_TOOLS, AGENT_REASONING, AGENT_REASONING_SEQ, EXPANDED_OUTPUT, ORCH_LAST_TOOL_LINES,
    ORCH_SCROLLBACK_COUNTER, lock_term, term_size,
};

// Tree connector prefixes for tool calls under tasks
pub(crate) const TREE_MID_BULLET: &str = "├─ ";
pub(crate) const TREE_MID_DURATION: &str = "│  ";
pub(crate) const TREE_END_BULLET: &str = "└─ ";
pub(crate) const TREE_END_DURATION: &str = "   ";

/// Tracks an in-flight orchestrator tool call for live duration display.
pub struct ActiveOrchTool {
    pub tool_id: String,
    pub task_id: String,
    pub tool_display: String,
    pub start_time: Instant,
    pub bullet_line_num: u32,
    pub duration_line_num: u32,
}

/// Info about the last tool printed for a task, used to update └─ → ├─.
pub struct OrchLastToolInfo {
    pub bullet_line_num: u32,
    pub duration_line_num: u32,
    pub tool_display: String,
    pub duration_text: String,
    pub bullet_color: Color,
}

/// Format a duration in ms for orchestrator tool display.
pub(crate) fn format_orch_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

pub(crate) fn format_orch_running(start: Instant) -> String {
    let ms = start.elapsed().as_millis() as u64;
    format!("running {}", format_orch_duration_ms(ms))
}

/// Register an in-flight orchestrator tool call.
pub fn register_orch_tool(
    tool_id: &str,
    task_id: &str,
    tool_display: &str,
    start_time: Instant,
    _task_color: (u8, u8, u8),
    fields: &BTreeMap<String, serde_json::Value>,
) {
    upgrade_last_tool_to_mid(task_id);

    let bullet_line = ORCH_SCROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
    println!(
        "{}{} {}",
        TREE_END_BULLET.with(Color::DarkGrey),
        "●".with(Color::DarkGrey),
        tool_display.with(Color::White),
    );
    let running_text = format_orch_running(start_time);
    let duration_line = ORCH_SCROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
    println!(
        "{}{} {}",
        TREE_END_DURATION,
        "⎿".with(Color::DarkGrey),
        running_text.as_str().with(Color::DarkGrey),
    );

    // In expanded mode, print the fields tree below the duration line
    if EXPANDED_OUTPUT.load(Ordering::Relaxed) && !fields.is_empty() {
        print_fields_tree_indented_live(fields, TREE_END_DURATION);
    }

    if let Ok(mut guard) = ORCH_LAST_TOOL_LINES.lock() {
        guard.insert(
            task_id.to_string(),
            OrchLastToolInfo {
                bullet_line_num: bullet_line,
                duration_line_num: duration_line,
                tool_display: tool_display.to_string(),
                duration_text: running_text,
                bullet_color: Color::DarkGrey,
            },
        );
    }
    let tool = std::sync::Arc::new(ActiveOrchTool {
        tool_id: tool_id.to_string(),
        task_id: task_id.to_string(),
        tool_display: tool_display.to_string(),
        start_time,
        bullet_line_num: bullet_line,
        duration_line_num: duration_line,
    });
    if let Ok(mut guard) = ACTIVE_ORCH_TOOLS.lock() {
        guard.push(tool);
    }
}

/// Update the previous last tool under a task from └─ to ├─.
fn upgrade_last_tool_to_mid(task_id: &str) {
    let prev = if let Ok(guard) = ORCH_LAST_TOOL_LINES.lock() {
        guard.get(task_id).map(|info| OrchLastToolInfo {
            bullet_line_num: info.bullet_line_num,
            duration_line_num: info.duration_line_num,
            tool_display: info.tool_display.clone(),
            duration_text: info.duration_text.clone(),
            bullet_color: info.bullet_color,
        })
    } else {
        None
    };
    let Some(prev) = prev else { return };

    let total_sb = ORCH_SCROLLBACK_COUNTER.load(Ordering::Relaxed);
    let (_, th) = term_size();
    let bullet_up = total_sb.saturating_sub(prev.bullet_line_num);
    let duration_up = total_sb.saturating_sub(prev.duration_line_num);
    if bullet_up >= th as u32 || duration_up >= th as u32 {
        return;
    }

    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(
        stdout,
        cursor::MoveUp(bullet_up as u16),
        cursor::MoveToColumn(0)
    );
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!(
        "{}{} {}",
        TREE_MID_BULLET.with(Color::DarkGrey),
        "●".with(prev.bullet_color),
        prev.tool_display.as_str().with(Color::White),
    );
    let _ = execute!(stdout, cursor::RestorePosition);

    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(
        stdout,
        cursor::MoveUp(duration_up as u16),
        cursor::MoveToColumn(0)
    );
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!(
        "{}{} {}",
        TREE_MID_DURATION.with(Color::DarkGrey),
        "⎿".with(Color::DarkGrey),
        prev.duration_text.as_str().with(Color::DarkGrey),
    );
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Finalize an orchestrator tool call.
pub fn finalize_orch_tool(tool_id: &str, duration_ms: Option<u64>, task_color: (u8, u8, u8)) {
    let tool = if let Ok(mut guard) = ACTIVE_ORCH_TOOLS.lock() {
        let idx = guard.iter().position(|t| t.tool_id == tool_id);
        idx.map(|i| guard.remove(i))
    } else {
        None
    };

    let Some(tool) = tool else { return };

    let ms = duration_ms.unwrap_or_else(|| tool.start_time.elapsed().as_millis() as u64);
    let dur_str = format_orch_duration_ms(ms);
    let total_scrollback = ORCH_SCROLLBACK_COUNTER.load(Ordering::Relaxed);
    let bc = Color::Rgb {
        r: task_color.0,
        g: task_color.1,
        b: task_color.2,
    };

    let bullet_up = total_scrollback.saturating_sub(tool.bullet_line_num);
    let duration_up = total_scrollback.saturating_sub(tool.duration_line_num);

    let (_, th) = term_size();
    if bullet_up >= th as u32 || duration_up >= th as u32 {
        return;
    }

    let is_last = if let Ok(guard) = ORCH_LAST_TOOL_LINES.lock() {
        guard
            .get(&tool.task_id)
            .map(|info| info.bullet_line_num == tool.bullet_line_num)
            .unwrap_or(false)
    } else {
        false
    };
    let (b_prefix, d_prefix) = if is_last {
        (TREE_END_BULLET, TREE_END_DURATION)
    } else {
        (TREE_MID_BULLET, TREE_MID_DURATION)
    };

    let completed_text = format!("completed in {dur_str}");

    if is_last
        && let Ok(mut guard) = ORCH_LAST_TOOL_LINES.lock()
        && let Some(info) = guard.get_mut(&tool.task_id)
    {
        info.duration_text = completed_text.clone();
        info.bullet_color = bc;
    }

    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(
        stdout,
        cursor::MoveUp(bullet_up as u16),
        cursor::MoveToColumn(0)
    );
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!(
        "{}{} {}",
        b_prefix.with(Color::DarkGrey),
        "●".with(bc),
        tool.tool_display.as_str().with(Color::White),
    );
    let _ = execute!(stdout, cursor::RestorePosition);

    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(
        stdout,
        cursor::MoveUp(duration_up as u16),
        cursor::MoveToColumn(0)
    );
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!(
        "{}{} {}",
        d_prefix.with(Color::DarkGrey),
        "⎿".with(Color::DarkGrey),
        completed_text.as_str().with(Color::DarkGrey),
    );
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Overwrite an orchestrator task header line in-place.
pub fn overwrite_orch_task_header(
    header_line_num: u32,
    task_id: &str,
    worker_id: &str,
    task_color: (u8, u8, u8),
) {
    let _term = lock_term();
    overwrite_orch_task_header_unlocked(header_line_num, task_id, worker_id, task_color);
}

/// Inner implementation — caller must already hold `TERM_WRITE`.
pub(crate) fn overwrite_orch_task_header_unlocked(
    header_line_num: u32,
    task_id: &str,
    worker_id: &str,
    task_color: (u8, u8, u8),
) {
    let total_scrollback = ORCH_SCROLLBACK_COUNTER.load(Ordering::Relaxed);
    let up = total_scrollback.saturating_sub(header_line_num);
    let (_, th) = term_size();
    if up >= th as u32 {
        return;
    }
    let bc = Color::Rgb {
        r: task_color.0,
        g: task_color.1,
        b: task_color.2,
    };
    let grey = Color::DarkGrey;
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(stdout, cursor::MoveUp(up as u16), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!(
        "{} {} {} {} {} {}",
        "●".with(bc).attribute(crossterm::style::Attribute::Bold),
        format!("Task {task_id}").attribute(crossterm::style::Attribute::Bold),
        "-".with(grey),
        format!("Worker: {worker_id}").with(grey),
        "-".with(grey),
        "done".with(grey),
    );
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Return the current orchestrator scrollback counter.
pub fn current_orch_scrollback() -> u32 {
    ORCH_SCROLLBACK_COUNTER.load(Ordering::Relaxed)
}

/// Increment the orchestrator scrollback counter.
pub fn increment_orch_scrollback() {
    ORCH_SCROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
}

/// Reset orchestrator tool tracking.
pub fn reset_orch_tools() {
    if let Ok(mut guard) = ACTIVE_ORCH_TOOLS.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = ORCH_LAST_TOOL_LINES.lock() {
        guard.clear();
    }
    ORCH_SCROLLBACK_COUNTER.store(0, Ordering::Relaxed);
}

/// Clean up last-tool tracking for a completed task.
pub fn clear_orch_task_tools(task_id: &str) {
    if let Ok(mut guard) = ORCH_LAST_TOOL_LINES.lock() {
        guard.remove(task_id);
    }
}

/// Set the agent reasoning text.
pub fn set_agent_reasoning(text: &str) {
    if let Ok(mut guard) = AGENT_REASONING.lock() {
        *guard = text.to_string();
    }
    AGENT_REASONING_SEQ.fetch_add(1, Ordering::Relaxed);
}

/// Clear the agent reasoning text.
pub fn clear_agent_reasoning() {
    if let Ok(mut guard) = AGENT_REASONING.lock() {
        guard.clear();
    }
    AGENT_REASONING_SEQ.fetch_add(1, Ordering::Relaxed);
}

/// Print a fields tree indented under a tool call (for replay).
pub(crate) fn print_fields_tree_indented(
    fields: &BTreeMap<String, serde_json::Value>,
    base: &str,
    _has_prior_siblings: bool,
) {
    if fields.is_empty() {
        return;
    }
    let total = fields.len();
    for (idx, (key, val)) in fields.iter().enumerate() {
        let is_last = idx == total - 1;
        let connector = if is_last { "└─" } else { "├─" };
        let child_cont = if is_last {
            format!("{}   ", base)
        } else {
            format!("{}│  ", base)
        };
        match val {
            serde_json::Value::Object(obj) => {
                println!(
                    "{}{} {}:",
                    base.with(Color::DarkGrey),
                    connector.with(Color::DarkGrey),
                    key.as_str().with(Color::DarkGrey),
                );
                let sub_total = obj.len();
                for (sub_idx, (sub_key, sub_val)) in obj.iter().enumerate() {
                    let sub_is_last = sub_idx == sub_total - 1;
                    let sub_connector = if sub_is_last { "└─" } else { "├─" };
                    let val_str = match sub_val {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    println!(
                        "{}{} {}: {}",
                        child_cont.as_str().with(Color::DarkGrey),
                        sub_connector.with(Color::DarkGrey),
                        sub_key.as_str().with(Color::DarkGrey),
                        val_str.as_str().with(Color::DarkGrey),
                    );
                }
            }
            _ => {
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                println!(
                    "{}{} {}: {}",
                    base.with(Color::DarkGrey),
                    connector.with(Color::DarkGrey),
                    key.as_str().with(Color::DarkGrey),
                    val_str.as_str().with(Color::DarkGrey),
                );
            }
        }
    }
}

/// Print a fields tree indented under a tool call during live streaming.
/// Same rendering as `print_fields_tree_indented` but also increments
/// `ORCH_SCROLLBACK_COUNTER` for each line to keep cursor math correct.
fn print_fields_tree_indented_live(fields: &BTreeMap<String, serde_json::Value>, base: &str) {
    if fields.is_empty() {
        return;
    }
    let total = fields.len();
    for (idx, (key, val)) in fields.iter().enumerate() {
        let is_last = idx == total - 1;
        let connector = if is_last { "└─" } else { "├─" };
        let child_cont = if is_last {
            format!("{}   ", base)
        } else {
            format!("{}│  ", base)
        };
        match val {
            serde_json::Value::Object(obj) => {
                println!(
                    "{}{} {}:",
                    base.with(Color::DarkGrey),
                    connector.with(Color::DarkGrey),
                    key.as_str().with(Color::DarkGrey),
                );
                ORCH_SCROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
                let sub_total = obj.len();
                for (sub_idx, (sub_key, sub_val)) in obj.iter().enumerate() {
                    let sub_is_last = sub_idx == sub_total - 1;
                    let sub_connector = if sub_is_last { "└─" } else { "├─" };
                    let val_str = match sub_val {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    println!(
                        "{}{} {}: {}",
                        child_cont.as_str().with(Color::DarkGrey),
                        sub_connector.with(Color::DarkGrey),
                        sub_key.as_str().with(Color::DarkGrey),
                        val_str.as_str().with(Color::DarkGrey),
                    );
                    ORCH_SCROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                println!(
                    "{}{} {}: {}",
                    base.with(Color::DarkGrey),
                    connector.with(Color::DarkGrey),
                    key.as_str().with(Color::DarkGrey),
                    val_str.as_str().with(Color::DarkGrey),
                );
                ORCH_SCROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
