// ---------------------------------------------------------------------------
// Orchestrator tool call tracking (duration display + blinking icons)
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::Stylize;
use crossterm::terminal;

use crate::theme::{AuraStyle, Themed};

use super::state::{
    ACTIVE_ORCH_TOOLS, AGENT_REASONING, AGENT_REASONING_SEQ, EXPANDED_OUTPUT, ORCH_LAST_TOOL_LINES,
    ORCH_SCROLLBACK_COUNTER, PROGRESS_TOKEN_TO_TOOL_ID, lock_term, term_size,
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
    /// `true` when content (fields tree at register, or result lines at
    /// finalize) was printed below the duration line in expanded mode. Used
    /// at finalize time so the in-place duration overwrite keeps the right
    /// tree connector (`├─` instead of `⎿`).
    pub has_content_below: bool,
    /// Latest `aura.progress.message` correlated to this tool by
    /// `progress_token`. When `Some`, the live duration line renders this
    /// message instead of `running Xms`. Reset to `None` on register; never
    /// cleared in flight (each new progress message overwrites it). The
    /// final `completed in Xms` line is written by `finalize_orch_tool` and
    /// does not consult this field.
    pub progress_message: Mutex<Option<String>>,
}

/// Info about the last tool printed for a task, used to update └─ → ├─.
pub struct OrchLastToolInfo {
    pub bullet_line_num: u32,
    pub duration_line_num: u32,
    pub tool_display: String,
    pub duration_text: String,
    /// Whether content (fields and/or result) was printed below the duration
    /// line. When true, the duration line uses `├─` instead of `⎿` so it
    /// visually connects to the rows below.
    pub has_content_below: bool,
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

/// Visible rows a `println!`'d line will consume in the current terminal,
/// accounting for line-wrap. `text` should be the *unstyled* line content
/// (no ANSI escapes) — pass the format-string substitution result built
/// from raw strings.
///
/// Returns `1` if the line is empty or terminal width is unknown — every
/// `println!` of an empty line still consumes one row.
pub(crate) fn visual_row_count(text: &str) -> u32 {
    let (term_w, _) = term_size();
    let chars = text.chars().count() as u32;
    if chars == 0 || term_w == 0 {
        1
    } else {
        chars.div_ceil(term_w as u32).max(1)
    }
}

/// Register an in-flight orchestrator tool call.
pub fn register_orch_tool(
    tool_id: &str,
    task_id: &str,
    tool_display: &str,
    start_time: Instant,
    fields: &BTreeMap<String, serde_json::Value>,
) {
    upgrade_last_tool_to_mid(task_id);

    // Count visual rows (not logical printlns) so the cursor math in
    // `finalize_orch_tool` and the live animation tick still locks onto the
    // right row when a long bullet/value wraps.
    let bullet_text = format!("{}● {}", TREE_END_BULLET, tool_display);
    let bullet_rows = visual_row_count(&bullet_text);
    let bullet_line = ORCH_SCROLLBACK_COUNTER.fetch_add(bullet_rows, Ordering::Relaxed);
    println!(
        "{}{} {}",
        TREE_END_BULLET.themed(AuraStyle::Connector),
        "●".themed(AuraStyle::Muted),
        tool_display.themed(AuraStyle::Primary),
    );
    let running_text = format_orch_running(start_time);
    let has_content_below = EXPANDED_OUTPUT.load(Ordering::Relaxed) && !fields.is_empty();
    let dur_connector = if has_content_below { "├─" } else { "⎿" };
    let duration_text = format!("{}{} {}", TREE_END_DURATION, dur_connector, running_text);
    let duration_rows = visual_row_count(&duration_text);
    let duration_line = ORCH_SCROLLBACK_COUNTER.fetch_add(duration_rows, Ordering::Relaxed);
    println!(
        "{}{} {}",
        TREE_END_DURATION,
        dur_connector.themed(AuraStyle::Connector),
        running_text.as_str().themed(AuraStyle::Muted),
    );

    // In expanded mode, print the fields tree below the duration line
    if has_content_below {
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
                has_content_below,
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
        has_content_below,
        progress_message: Mutex::new(None),
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
            has_content_below: info.has_content_below,
        })
    } else {
        None
    };
    let Some(prev) = prev else { return };
    let bullet_color = crate::ui::state::task_color_for(task_id);

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
        TREE_MID_BULLET.themed(AuraStyle::Connector),
        "●".with(bullet_color),
        prev.tool_display.as_str().themed(AuraStyle::Primary),
    );
    let _ = execute!(stdout, cursor::RestorePosition);

    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(
        stdout,
        cursor::MoveUp(duration_up as u16),
        cursor::MoveToColumn(0)
    );
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    let dur_connector = if prev.has_content_below {
        "├─"
    } else {
        "⎿"
    };
    print!(
        "{}{} {}",
        TREE_MID_DURATION.themed(AuraStyle::Connector),
        dur_connector.themed(AuraStyle::Connector),
        prev.duration_text.as_str().themed(AuraStyle::Muted),
    );
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Finalize an orchestrator tool call.
///
/// Updates the bullet/duration lines in place and, when `result` is non-empty
/// in expanded mode, appends the tool output as indented lines below the
/// existing fields tree (matching what `/expand` replay shows). Doing this
/// live keeps the cursor-math correct for the next tool by incrementing
/// `ORCH_SCROLLBACK_COUNTER` for each appended line.
pub fn finalize_orch_tool(tool_id: &str, duration_ms: Option<u64>, result: Option<&str>) {
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
    let bullet_color = crate::ui::state::task_color_for(&tool.task_id);

    let bullet_up = total_scrollback.saturating_sub(tool.bullet_line_num);
    let duration_up = total_scrollback.saturating_sub(tool.duration_line_num);

    let (_, th) = term_size();
    let on_screen = bullet_up < th as u32 && duration_up < th as u32;

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

    let expanded = EXPANDED_OUTPUT.load(Ordering::Relaxed);
    let result_text = result.filter(|t| !t.is_empty());
    let will_print_result = expanded && result_text.is_some();
    let has_content_below = tool.has_content_below || will_print_result;

    if is_last
        && let Ok(mut guard) = ORCH_LAST_TOOL_LINES.lock()
        && let Some(info) = guard.get_mut(&tool.task_id)
    {
        info.duration_text = completed_text.clone();
        info.has_content_below = has_content_below;
    }

    if on_screen {
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
            b_prefix.themed(AuraStyle::Connector),
            "●".with(bullet_color),
            tool.tool_display.as_str().themed(AuraStyle::Primary),
        );
        let _ = execute!(stdout, cursor::RestorePosition);

        let _ = execute!(stdout, cursor::SavePosition);
        let _ = execute!(
            stdout,
            cursor::MoveUp(duration_up as u16),
            cursor::MoveToColumn(0)
        );
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        let dur_connector = if has_content_below { "├─" } else { "⎿" };
        print!(
            "{}{} {}",
            d_prefix.themed(AuraStyle::Connector),
            dur_connector.themed(AuraStyle::Connector),
            completed_text.as_str().themed(AuraStyle::Muted),
        );
        let _ = execute!(stdout, cursor::RestorePosition);
        let _ = stdout.flush();
    }

    // Append the tool result as indented lines under the fields tree.
    // Indented with TREE_END_DURATION (`   `) to match the live fields-tree
    // indent emitted at register time. Tracks visual rows (not printlns)
    // so a wrapped result line doesn't desync the cursor math for the next
    // tool's `register_orch_tool`.
    if let Some(text) = result_text
        && expanded
    {
        let normalized = crate::tools::normalize_tool_result_text(text);
        println!();
        ORCH_SCROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        for line in normalized.lines() {
            let line_text = format!("{}  {}", TREE_END_DURATION, line);
            println!(
                "{}  {}",
                TREE_END_DURATION.themed(AuraStyle::Connector),
                line.themed(AuraStyle::Muted),
            );
            ORCH_SCROLLBACK_COUNTER.fetch_add(visual_row_count(&line_text), Ordering::Relaxed);
        }
        // Trailing blank line so the next tool call has visual separation.
        println!();
        ORCH_SCROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
}

/// Overwrite an orchestrator task header line in-place. `status` is the
/// suffix appended after `Worker: {worker_id}` (e.g. `Planning`,
/// `Executing`, `Analyzing`, `done`).
pub fn overwrite_orch_task_header(
    header_line_num: u32,
    task_id: &str,
    worker_id: &str,
    status: &str,
) {
    let _term = lock_term();
    overwrite_orch_task_header_unlocked(header_line_num, task_id, worker_id, status);
}

/// Inner implementation — caller must already hold `TERM_WRITE`.
pub(crate) fn overwrite_orch_task_header_unlocked(
    header_line_num: u32,
    task_id: &str,
    worker_id: &str,
    status: &str,
) {
    let total_scrollback = ORCH_SCROLLBACK_COUNTER.load(Ordering::Relaxed);
    let up = total_scrollback.saturating_sub(header_line_num);
    let (_, th) = term_size();
    if up >= th as u32 {
        return;
    }
    let bullet_color = crate::ui::state::task_color_for(task_id);
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(stdout, cursor::MoveUp(up as u16), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!(
        "{} {} {} {} {} {}",
        "●"
            .with(bullet_color)
            .attribute(crossterm::style::Attribute::Bold),
        format!("Task {task_id}").attribute(crossterm::style::Attribute::Bold),
        "-".themed(AuraStyle::Muted),
        format!("Worker: {worker_id}").themed(AuraStyle::Muted),
        "-".themed(AuraStyle::Muted),
        status.themed(AuraStyle::Muted),
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
    if let Ok(mut guard) = PROGRESS_TOKEN_TO_TOOL_ID.lock() {
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

/// Record the `progress_token → tool_id` mapping so a later `aura.progress`
/// can be steered onto the matching active orchestrator tool's running line
/// instead of the global agent-reasoning sub-line. Both arguments are passed
/// through verbatim (canonical JSON form for the token, OpenAI tool call id
/// for `tool_id`); the producer in `aura.tool_start` and the consumer in
/// `aura.progress` use the same encoding so direct string comparison works.
pub fn record_tool_progress_token(tool_id: &str, token: &str) {
    if tool_id.is_empty() || token.is_empty() {
        return;
    }
    if let Ok(mut map) = PROGRESS_TOKEN_TO_TOOL_ID.lock() {
        map.insert(token.to_string(), tool_id.to_string());
    }
}

/// Try to attach a progress message to the active orchestrator tool that
/// owns this `progress_token`. Returns `true` when a matching tool exists
/// (and its `progress_message` was updated); the caller should fall back to
/// `set_agent_reasoning` when this returns `false`.
pub fn set_orch_tool_progress_by_token(token: &str, message: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let tool_id = match PROGRESS_TOKEN_TO_TOOL_ID.lock() {
        Ok(map) => match map.get(token) {
            Some(id) => id.clone(),
            None => return false,
        },
        Err(_) => return false,
    };
    let tools = match ACTIVE_ORCH_TOOLS.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let Some(tool) = tools.iter().find(|t| t.tool_id == tool_id).cloned() else {
        return false;
    };
    drop(tools);
    if let Ok(mut slot) = tool.progress_message.lock() {
        *slot = Some(message.to_string());
    }
    true
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
                    base.themed(AuraStyle::Connector),
                    connector.themed(AuraStyle::Connector),
                    key.as_str().themed(AuraStyle::Muted),
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
                        child_cont.as_str().themed(AuraStyle::Connector),
                        sub_connector.themed(AuraStyle::Connector),
                        sub_key.as_str().themed(AuraStyle::Muted),
                        val_str.as_str().themed(AuraStyle::Muted),
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
                    base.themed(AuraStyle::Connector),
                    connector.themed(AuraStyle::Connector),
                    key.as_str().themed(AuraStyle::Muted),
                    val_str.as_str().themed(AuraStyle::Muted),
                );
            }
        }
    }
}

/// Print a fields tree indented under a tool call during live streaming.
/// Same rendering as `print_fields_tree_indented` but also increments
/// `ORCH_SCROLLBACK_COUNTER` for each line to keep cursor math correct —
/// counting **visual rows** (not logical printlns) so wrapped long values
/// don't desync the counter from the cursor's terminal row.
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
                let header_text = format!("{}{} {}:", base, connector, key);
                println!(
                    "{}{} {}:",
                    base.themed(AuraStyle::Connector),
                    connector.themed(AuraStyle::Connector),
                    key.as_str().themed(AuraStyle::Muted),
                );
                ORCH_SCROLLBACK_COUNTER
                    .fetch_add(visual_row_count(&header_text), Ordering::Relaxed);
                let sub_total = obj.len();
                for (sub_idx, (sub_key, sub_val)) in obj.iter().enumerate() {
                    let sub_is_last = sub_idx == sub_total - 1;
                    let sub_connector = if sub_is_last { "└─" } else { "├─" };
                    let val_str = match sub_val {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let line_text =
                        format!("{}{} {}: {}", child_cont, sub_connector, sub_key, val_str);
                    println!(
                        "{}{} {}: {}",
                        child_cont.as_str().themed(AuraStyle::Connector),
                        sub_connector.themed(AuraStyle::Connector),
                        sub_key.as_str().themed(AuraStyle::Muted),
                        val_str.as_str().themed(AuraStyle::Muted),
                    );
                    ORCH_SCROLLBACK_COUNTER
                        .fetch_add(visual_row_count(&line_text), Ordering::Relaxed);
                }
            }
            _ => {
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let line_text = format!("{}{} {}: {}", base, connector, key, val_str);
                println!(
                    "{}{} {}: {}",
                    base.themed(AuraStyle::Connector),
                    connector.themed(AuraStyle::Connector),
                    key.as_str().themed(AuraStyle::Muted),
                    val_str.as_str().themed(AuraStyle::Muted),
                );
                ORCH_SCROLLBACK_COUNTER.fetch_add(visual_row_count(&line_text), Ordering::Relaxed);
            }
        }
    }
}
