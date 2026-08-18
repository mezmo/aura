// ---------------------------------------------------------------------------
// Status bar rendering
// ---------------------------------------------------------------------------

use std::io::{self, Write};
use std::num::NonZeroU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::execute;
use crossterm::terminal;

use crate::api::mcp_status::McpCounts;
use crate::theme::{AuraStyle, Themed};

use super::animation::render_queued_wave;
use super::state::{
    CONTEXT_USED, CTRLC_HINT_VISIBLE, CTRLC_RESET_SKIP, CUMULATIVE_COMPLETION, CUMULATIVE_PROMPT,
    CUMULATIVE_SCRATCHPAD_EXTRACTED, CUMULATIVE_SCRATCHPAD_INTERCEPTED, CURSOR_ROW, CWD,
    FRAME_LINES, LAST_CTRLC, MCP_COUNTS, MODEL_CONTEXT_LIMIT, PROCESSING, QUEUED_INPUT,
    QUEUED_WAVE_POS, SESSION_MODEL, STATUS_HINT, STATUS_ROWS, STATUS_SEGMENTS, TURN_NOTICES,
    get_selected_model, lock_term, status_rows, term_size,
};
use super::status_line::{self, ContextUsage, DEFAULT_SEGMENTS, Segment, Snapshot};

/// Right-aligned on the status line while the REPL is idle.
const IDLE_RIGHT_TEXT: &str = "AURA, by Mezmo!";
/// Right-aligned on the status line while a request is in flight.
const BUSY_RIGHT_TEXT: &str = "esc to stop";

/// Install the segments the status line shows; only the first call takes
/// effect.
pub fn set_status_segments(segments: Vec<Segment>) {
    let _ = STATUS_SEGMENTS.set(segments);
}

/// Record the model and context window reported by `aura.session_info`.
pub fn set_session_info(model: String, context_limit: Option<u64>) {
    if let Ok(mut g) = SESSION_MODEL.lock() {
        *g = Some(model);
    }
    MODEL_CONTEXT_LIMIT.store(context_limit.unwrap_or(0), Ordering::Relaxed);
}

/// Record the tokens currently occupying the model's context.
pub fn set_context_used(tokens: u64) {
    CONTEXT_USED.store(tokens, Ordering::Relaxed);
}

/// Record the latest MCP server tally.
pub fn set_mcp_counts(counts: McpCounts) {
    if let Ok(mut g) = MCP_COUNTS.lock() {
        *g = Some(counts);
    }
}

fn capture_snapshot() -> Snapshot {
    let cwd = CWD.get_or_init(|| std::env::current_dir().ok()).as_deref();
    // No window, no meter: a bare token count would be a sum of many agent
    // contexts in orchestration mode (which never reports a window), so the
    // segment only appears when there is a real ceiling to measure against.
    let context =
        NonZeroU64::new(MODEL_CONTEXT_LIMIT.load(Ordering::Relaxed)).map(|limit| ContextUsage {
            used: CONTEXT_USED.load(Ordering::Relaxed),
            limit,
        });
    Snapshot {
        model: get_selected_model().or_else(|| SESSION_MODEL.lock().ok().and_then(|g| g.clone())),
        cwd: cwd.map(|p| status_line::abbreviate_home(p, dirs::home_dir().as_deref())),
        git_branch: cwd.and_then(status_line::git_branch),
        context,
        prompt_tokens: CUMULATIVE_PROMPT.lock().map(|g| *g).unwrap_or(0),
        completion_tokens: CUMULATIVE_COMPLETION.lock().map(|g| *g).unwrap_or(0),
        scratchpad_intercepted: CUMULATIVE_SCRATCHPAD_INTERCEPTED
            .lock()
            .map(|g| *g)
            .unwrap_or(0),
        scratchpad_extracted: CUMULATIVE_SCRATCHPAD_EXTRACTED
            .lock()
            .map(|g| *g)
            .unwrap_or(0),
        mcp: MCP_COUNTS.lock().ok().and_then(|g| *g),
    }
}

/// The status line for the current REPL state, styled and fitted to the
/// terminal width.
fn status_line_now() -> String {
    let (width, _) = term_size();
    let right = if PROCESSING.load(Ordering::Relaxed) {
        BUSY_RIGHT_TEXT
    } else {
        IDLE_RIGHT_TEXT
    };
    let segments = STATUS_SEGMENTS
        .get()
        .map(Vec::as_slice)
        .unwrap_or(DEFAULT_SEGMENTS);
    status_line::render(&capture_snapshot(), segments, width as usize, right)
}

/// Whether the status area is currently showing a hint overlay.
pub(crate) fn is_hint_active() -> bool {
    STATUS_HINT.lock().map(|g| !g.is_empty()).unwrap_or(false)
}

/// Whether per-turn notices should currently be shown. Notices are hidden
/// while a request is processing and while a hint overlay is active.
fn notices_visible() -> bool {
    !PROCESSING.load(Ordering::Relaxed)
        && !is_hint_active()
        && TURN_NOTICES.lock().map(|g| !g.is_empty()).unwrap_or(false)
}

/// Number of status rows the notices require when idle: status line + a blank
/// separator + one row per notice + a trailing (reserved) row. Returns the
/// legacy default of 3 when no notices are visible.
pub(crate) fn notice_status_rows() -> u16 {
    let n = if !PROCESSING.load(Ordering::Relaxed) {
        TURN_NOTICES.lock().map(|g| g.len()).unwrap_or(0)
    } else {
        0
    };
    if n == 0 { 3 } else { n as u16 + 3 }
}

/// Desired status-row count given the current hint and notice state. The hint
/// overlay takes precedence over notices (it replaces the status area).
pub(crate) fn desired_status_rows() -> u16 {
    let hint_len = STATUS_HINT.lock().map(|g| g.len()).unwrap_or(0);
    if hint_len > 0 {
        return (hint_len as u16 + 1).max(3);
    }
    notice_status_rows()
}

/// Return the styled lines to display in the status area.
pub(crate) fn get_effective_status() -> Vec<String> {
    let hint = STATUS_HINT.lock().map(|g| g.clone()).unwrap_or_default();
    if !hint.is_empty() {
        return hint;
    }

    let first = status_line_now();
    if !notices_visible() {
        return vec![first];
    }

    let notices = TURN_NOTICES.lock().map(|g| g.clone()).unwrap_or_default();
    let mut lines = Vec::with_capacity(notices.len() + 2);
    lines.push(first);
    lines.push(String::new());
    lines.extend(notices);
    lines
}

/// Print one already-styled status line.
pub(crate) fn print_status_line(line: &str) {
    print!("{line}");
}

/// Append a per-turn status notice (error/warning) shown below the status
/// line while idle. The `style` colours the whole line. The notice is
/// data-only here — it becomes visible the next time the input frame is
/// redrawn (i.e. once the in-flight request finishes), so this is safe to
/// call mid-stream.
pub fn add_turn_notice(style: AuraStyle, message: impl AsRef<str>) {
    let styled = message.as_ref().themed(style).to_string();
    if let Ok(mut g) = TURN_NOTICES.lock() {
        g.push(styled);
    }
}

/// Clear all per-turn notices and reflow the status area back to its baseline
/// size. Called at the start of each request, after the previous frame has
/// already been erased, so a plain store (no in-place reflow) is sufficient.
pub fn clear_turn_notices() {
    if let Ok(mut g) = TURN_NOTICES.lock() {
        g.clear();
    }
    STATUS_ROWS.store(notice_status_rows(), Ordering::Relaxed);
}

/// Update the status bar rows in place (dynamically sized).
pub fn update_status_bar() {
    let _term = lock_term();
    update_status_bar_unlocked();
}

/// Inner implementation — caller must already hold `TERM_WRITE`.
pub(crate) fn update_status_bar_unlocked() {
    let lines = get_effective_status();
    let queued = QUEUED_INPUT.lock().map(|g| g.clone()).unwrap_or_default();
    let show_queued = PROCESSING.load(Ordering::Relaxed) && !queued.is_empty();
    let sr = status_rows() as usize;

    let mut stdout = io::stdout();
    let n = FRAME_LINES.load(Ordering::Relaxed) as i32;
    let r = CURSOR_ROW.load(Ordering::Relaxed) as i32;
    let _ = execute!(stdout, cursor::SavePosition);
    let down1 = n - r + 1;
    if down1 > 0 {
        let _ = execute!(
            stdout,
            cursor::MoveDown(down1 as u16),
            cursor::MoveToColumn(0)
        );
    } else if down1 < 0 {
        let _ = execute!(
            stdout,
            cursor::MoveUp((-down1) as u16),
            cursor::MoveToColumn(0)
        );
    } else {
        let _ = execute!(stdout, cursor::MoveToColumn(0));
    }
    for i in 0..sr {
        if i > 0 {
            let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
        }
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        if i == sr - 1 && show_queued {
            let wave_pos = QUEUED_WAVE_POS.lock().map(|g| *g).unwrap_or(0.0);
            print!("{}", render_queued_wave(&queued, wave_pos));
        } else if let Some(line) = lines.get(i) {
            print_status_line(line);
        }
    }
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Accumulate a turn's token usage and record the resulting context size.
pub fn set_status_bar_tokens(prompt_tokens: u64, completion_tokens: u64) {
    if let Ok(mut g) = CUMULATIVE_PROMPT.lock() {
        *g += prompt_tokens;
    }
    if let Ok(mut g) = CUMULATIVE_COMPLETION.lock() {
        *g += completion_tokens;
    }
    // Per the `aura.usage` contract, prompt + completion is the context
    // window position for the next request.
    set_context_used(prompt_tokens + completion_tokens);
}

/// Accumulate scratchpad savings.
pub fn add_scratchpad_usage(tokens_intercepted: u64, tokens_extracted: u64) {
    if let Ok(mut g) = CUMULATIVE_SCRATCHPAD_INTERCEPTED.lock() {
        *g += tokens_intercepted;
    }
    if let Ok(mut g) = CUMULATIVE_SCRATCHPAD_EXTRACTED.lock() {
        *g += tokens_extracted;
    }
}

/// Return the current cumulative total tokens.
pub fn get_cumulative_tokens() -> u64 {
    let prompt = CUMULATIVE_PROMPT.lock().map(|g| *g).unwrap_or(0);
    let completion = CUMULATIVE_COMPLETION.lock().map(|g| *g).unwrap_or(0);
    prompt + completion
}

/// Seed cumulative token counters (used when resuming).
pub fn seed_status_bar_tokens(prompt_tokens: u64, completion_tokens: u64) {
    if let Ok(mut g) = CUMULATIVE_PROMPT.lock() {
        *g = prompt_tokens;
    }
    if let Ok(mut g) = CUMULATIVE_COMPLETION.lock() {
        *g = completion_tokens;
    }
}

/// Reset cumulative token counters and context usage to zero.
pub fn reset_status_bar_tokens() {
    if let Ok(mut g) = CUMULATIVE_PROMPT.lock() {
        *g = 0;
    }
    if let Ok(mut g) = CUMULATIVE_COMPLETION.lock() {
        *g = 0;
    }
    if let Ok(mut g) = CUMULATIVE_SCRATCHPAD_INTERCEPTED.lock() {
        *g = 0;
    }
    if let Ok(mut g) = CUMULATIVE_SCRATCHPAD_EXTRACTED.lock() {
        *g = 0;
    }
    set_context_used(0);
}

// ---------------------------------------------------------------------------
// Ctrl-C double-press-to-quit logic
// ---------------------------------------------------------------------------

/// Handle a Ctrl-C press with double-press-to-quit logic.
/// Returns `true` if the user should actually quit (second press within 5 s).
pub fn handle_ctrlc() -> bool {
    let now = Instant::now();
    if let Ok(mut guard) = LAST_CTRLC.lock() {
        if let Some(last) = *guard
            && now.duration_since(last) < Duration::from_secs(5)
        {
            *guard = None;
            CTRLC_HINT_VISIBLE.store(false, Ordering::Relaxed);
            if let Ok(mut h) = STATUS_HINT.lock() {
                h.clear();
            }
            return true;
        }
        *guard = Some(now);
    }
    CTRLC_HINT_VISIBLE.store(true, Ordering::Relaxed);
    CTRLC_RESET_SKIP.store(true, Ordering::Relaxed);
    if let Ok(mut h) = STATUS_HINT.lock() {
        *h = vec![format!(
            "{}",
            "Press Ctrl+C again to quit".themed(AuraStyle::Muted)
        )];
    }
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(5));
        if let Ok(mut guard) = LAST_CTRLC.lock()
            && let Some(last) = *guard
            && last == now
        {
            *guard = None;
            CTRLC_HINT_VISIBLE.store(false, Ordering::Relaxed);
            if let Ok(mut h) = STATUS_HINT.lock() {
                h.clear();
            }
            update_status_bar();
        }
    });
    false
}

/// Reset Ctrl-C double-press state.
pub fn reset_ctrlc_state() {
    if !CTRLC_HINT_VISIBLE.load(Ordering::Relaxed) {
        return;
    }
    if CTRLC_RESET_SKIP.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Ok(mut guard) = LAST_CTRLC.lock() {
        *guard = None;
    }
    CTRLC_HINT_VISIBLE.store(false, Ordering::Relaxed);
    if let Ok(mut h) = STATUS_HINT.lock() {
        h.clear();
    }
    update_status_bar();
}
