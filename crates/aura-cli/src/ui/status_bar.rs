// ---------------------------------------------------------------------------
// Status area below the input frame: row sizing, the hint overlay, per-turn
// notices, Ctrl-C handling, and the counters the status line reads. Row 0's
// text itself is rendered by `status_line`.
// ---------------------------------------------------------------------------

use std::io::{self, Write};
use std::num::NonZeroU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::execute;
use crossterm::terminal;

use crate::api::mcp_status::{McpCounts, counts_from_event};
use crate::event_names;
use crate::theme::{AuraStyle, Themed};

use super::animation::render_queued_wave;
use super::state::{
    AGENT_HOST, CONTEXT_USED, CONTEXT_USED_FRESH, CTRLC_HINT_VISIBLE, CTRLC_RESET_SKIP,
    CUMULATIVE_CACHE_READ, CUMULATIVE_COMPLETION, CUMULATIVE_PROMPT,
    CUMULATIVE_SCRATCHPAD_EXTRACTED, CUMULATIVE_SCRATCHPAD_INTERCEPTED, CURSOR_ROW, CWD,
    FRAME_LINES, LAST_CTRLC, MCP_COUNTS, MODEL_CONTEXT_LIMIT, ORCHESTRATED, PROCESSING,
    QUEUED_INPUT, QUEUED_WAVE_POS, SESSION_MODEL, STATUS_HINT, STATUS_ROWS, STATUS_SEGMENTS,
    TURN_NOTICES, get_selected_model, lock_term, status_rows, term_size,
};
use super::status_line::{self, ContextUsage, DEFAULT_SEGMENTS, Segment, Snapshot};
use super::text::strip_control_chars;

/// Right-aligned on the status line while the REPL is idle.
const IDLE_RIGHT_TEXT: &str = "AURA, by Mezmo!";
/// Right-aligned on the status line while a request is in flight.
const BUSY_RIGHT_TEXT: &str = "esc to stop";

/// Where the agent the status line describes is running.
#[derive(Debug)]
pub enum AgentHost {
    /// In this process.
    Local,
    /// On an aura-web-server reached over HTTP.
    Remote {
        /// Server address in display form (see `status_line::server_display`).
        server: String,
        /// Whether this process runs local tools on the server's behalf.
        client_tools: bool,
    },
}

/// Install the segments the status line shows; only the first call takes
/// effect.
pub fn set_status_segments(segments: Vec<Segment>) {
    let _ = STATUS_SEGMENTS.set(segments);
}

/// Record where the agent runs; only the first call takes effect.
pub fn set_agent_host(host: AgentHost) {
    let _ = AGENT_HOST.set(host);
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

/// Record what the status line learns from a stream event: the model and
/// context window from `aura.session_info`, the server tally from
/// `aura.mcp_status`. Other events are ignored.
pub fn record_session_event(event_name: &str, val: &serde_json::Value) {
    if event_name == event_names::SESSION_INFO {
        if let Some(model) = val.get("model").and_then(|m| m.as_str()) {
            set_session_info(
                model.to_owned(),
                val.get("model_context_limit").and_then(|l| l.as_u64()),
            );
        }
    } else if event_name == event_names::MCP_STATUS
        && let Some(counts) = counts_from_event(val)
    {
        set_mcp_counts(counts);
    }
}

/// Record that this conversation is orchestrated.
pub fn mark_orchestrated() {
    ORCHESTRATED.store(true, Ordering::Relaxed);
}

/// Forget everything the status line learned from the previous
/// conversation's stream — reported model, context window, MCP tally,
/// context size, and whether it was orchestrated — so a fresh conversation
/// starts blank rather than showing the old session's metadata until its
/// first turn reports.
pub fn reset_session_status() {
    if let Ok(mut g) = SESSION_MODEL.lock() {
        *g = None;
    }
    MODEL_CONTEXT_LIMIT.store(0, Ordering::Relaxed);
    if let Ok(mut g) = MCP_COUNTS.lock() {
        *g = None;
    }
    ORCHESTRATED.store(false, Ordering::Relaxed);
    set_context_used(0);
    CONTEXT_USED_FRESH.store(false, Ordering::Relaxed);
}

fn capture_snapshot() -> Snapshot {
    // A remote agent's working tree is the server's, so the local directory
    // and branch say nothing about it — except that client tools still run
    // against the local directory, which keeps the cwd relevant.
    let (server, show_cwd, show_git) = match AGENT_HOST.get() {
        None | Some(AgentHost::Local) => (None, true, true),
        Some(AgentHost::Remote {
            server,
            client_tools,
        }) => (Some(server.clone()), *client_tools, false),
    };
    let cwd = if show_cwd {
        CWD.get_or_init(|| std::env::current_dir().ok()).as_deref()
    } else {
        None
    };
    // In an orchestrated conversation the mid-turn aura.tool_usage readings
    // come from every worker as well as the coordinator (the event carries no
    // agent id), so there is no single context to show. Otherwise show the
    // count once something has been reported, with the meter when the
    // model's window is known.
    let used = CONTEXT_USED.load(Ordering::Relaxed);
    let limit = NonZeroU64::new(MODEL_CONTEXT_LIMIT.load(Ordering::Relaxed));
    let context = if ORCHESTRATED.load(Ordering::Relaxed) || (used == 0 && limit.is_none()) {
        None
    } else {
        Some(ContextUsage { used, limit })
    };
    Snapshot {
        model: get_selected_model().or_else(|| SESSION_MODEL.lock().ok().and_then(|g| g.clone())),
        server,
        cwd: cwd.map(|p| status_line::abbreviate_home(p, dirs::home_dir().as_deref())),
        git_branch: cwd.filter(|_| show_git).and_then(status_line::git_branch),
        context,
        prompt_tokens: CUMULATIVE_PROMPT.lock().map(|g| *g).unwrap_or(0),
        completion_tokens: CUMULATIVE_COMPLETION.lock().map(|g| *g).unwrap_or(0),
        cached_prompt_tokens: CUMULATIVE_CACHE_READ.lock().map(|g| *g).unwrap_or(0),
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
    let styled = strip_control_chars(message.as_ref())
        .themed(style)
        .to_string();
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

/// Accumulate a turn's billed token usage.
pub fn set_status_bar_tokens(prompt_tokens: u64, completion_tokens: u64) {
    if let Ok(mut g) = CUMULATIVE_PROMPT.lock() {
        *g += prompt_tokens;
    }
    if let Ok(mut g) = CUMULATIVE_COMPLETION.lock() {
        *g += completion_tokens;
    }
}

/// Accumulate prompt tokens the provider served from its prompt cache
/// (a subset of the prompt tokens counted by `set_status_bar_tokens`).
pub fn add_status_bar_cached_tokens(cache_read_tokens: u64) {
    if let Ok(mut g) = CUMULATIVE_CACHE_READ.lock() {
        *g += cache_read_tokens;
    }
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

/// Return the current cumulative total *billed* tokens (left-side display).
pub fn get_cumulative_tokens() -> u64 {
    let prompt = CUMULATIVE_PROMPT.lock().map(|g| *g).unwrap_or(0);
    let completion = CUMULATIVE_COMPLETION.lock().map(|g| *g).unwrap_or(0);
    prompt + completion
}

/// Tokens used to gauge context-window pressure: the reported context
/// occupancy when available, otherwise the cumulative billed total as a
/// pre-`aura.context_usage` fallback.
fn context_pressure_tokens(cumulative_total: u64) -> u64 {
    let occupancy = CONTEXT_USED.load(Ordering::Relaxed);
    if occupancy > 0 {
        occupancy
    } else {
        cumulative_total
    }
}

/// Mark the occupancy reading as belonging to a previous turn.
///
/// The reading itself is kept: it is the closest estimate available while the
/// current turn streams, and dropping it would fall back to cumulative billed
/// tokens, which exceed the window and blank the indicator. Decisions that must
/// not act on a previous turn's context use
/// [`fresh_context_fill_ratio`] instead.
pub fn begin_turn_context_tracking() {
    CONTEXT_USED_FRESH.store(false, Ordering::Relaxed);
}

/// Occupied fraction of the model's context window.
///
/// `None` when either the occupancy or the window is unknown — no
/// `aura.context_usage` has arrived, or neither it nor `aura.session_info`
/// reported the model's window — which callers treat as "fall back to
/// token-count thresholds".
pub fn context_fill_ratio() -> Option<f64> {
    let occupancy = CONTEXT_USED.load(Ordering::Relaxed);
    let window = MODEL_CONTEXT_LIMIT.load(Ordering::Relaxed);
    (occupancy > 0 && window > 0).then(|| occupancy as f64 / window as f64)
}

/// [`context_fill_ratio`] restricted to a reading the current turn reported.
///
/// `None` once a turn passes without an `aura.context_usage` event, so callers
/// fall back to token-count thresholds rather than acting on a fill fraction
/// that describes an earlier turn's context.
pub fn fresh_context_fill_ratio() -> Option<f64> {
    if !CONTEXT_USED_FRESH.load(Ordering::Relaxed) {
        return None;
    }
    context_fill_ratio()
}

/// Context-window pressure in tokens — used for auto-compaction decisions.
/// Reflects actual context occupancy (not cumulative billed usage).
pub fn get_context_tokens() -> u64 {
    context_pressure_tokens(get_cumulative_tokens())
}

/// Record context-window occupancy from an `aura.context_usage` event.
///
/// Sets the absolute occupancy that drives the context segment and
/// auto-compaction, and, when the event carries the model's context window,
/// records it so the meter reflects the real window.
pub fn set_context_window_usage(
    context_tokens: u64,
    response_tokens: u64,
    context_window: Option<u64>,
) {
    set_context_used(context_tokens + response_tokens);
    CONTEXT_USED_FRESH.store(true, Ordering::Relaxed);
    if let Some(window) = context_window {
        MODEL_CONTEXT_LIMIT.store(window, Ordering::Relaxed);
    }
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

/// Reset the cumulative token and scratchpad counters to zero. Replaying an
/// event log rebuilds them, so repaint paths call this before a replay;
/// conversation boundaries also call [`reset_session_status`].
pub fn reset_status_bar_tokens() {
    if let Ok(mut g) = CUMULATIVE_PROMPT.lock() {
        *g = 0;
    }
    if let Ok(mut g) = CUMULATIVE_COMPLETION.lock() {
        *g = 0;
    }
    if let Ok(mut g) = CUMULATIVE_CACHE_READ.lock() {
        *g = 0;
    }
    if let Ok(mut g) = CUMULATIVE_SCRATCHPAD_INTERCEPTED.lock() {
        *g = 0;
    }
    if let Ok(mut g) = CUMULATIVE_SCRATCHPAD_EXTRACTED.lock() {
        *g = 0;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session_model() -> Option<String> {
        SESSION_MODEL.lock().unwrap().clone()
    }

    #[test]
    fn record_session_event_seeds_and_reset_clears() {
        record_session_event(
            event_names::SESSION_INFO,
            &json!({ "model": "gpt-4o", "model_context_limit": 128000 }),
        );
        record_session_event(
            event_names::MCP_STATUS,
            &json!({ "servers": [
                { "server_name": "a", "status": "connected" },
                { "server_name": "b", "status": "failed" }
            ] }),
        );
        record_session_event("aura.progress", &json!({ "model": "other" }));
        assert_eq!(session_model().as_deref(), Some("gpt-4o"));
        assert_eq!(MODEL_CONTEXT_LIMIT.load(Ordering::Relaxed), 128000);
        assert_eq!(
            *MCP_COUNTS.lock().unwrap(),
            Some(McpCounts {
                connected: 1,
                total: 2
            })
        );

        reset_session_status();
        assert_eq!(session_model(), None);
        assert_eq!(MODEL_CONTEXT_LIMIT.load(Ordering::Relaxed), 0);
        assert_eq!(*MCP_COUNTS.lock().unwrap(), None);
    }

    #[test]
    fn a_previous_turns_reading_still_displays_but_stops_driving_decisions() {
        set_context_window_usage(100_000, 5_000, Some(200_000));
        assert_eq!(fresh_context_fill_ratio(), Some(0.525));

        // A later turn starts without reporting: the gauge keeps showing the
        // last known occupancy rather than blanking...
        begin_turn_context_tracking();
        assert_eq!(CONTEXT_USED.load(Ordering::Relaxed), 105_000);
        assert_eq!(context_fill_ratio(), Some(0.525));

        // ...while compaction decisions see no usable reading and fall back.
        assert_eq!(fresh_context_fill_ratio(), None);

        // A reading from the current turn drives decisions again.
        set_context_window_usage(150_000, 5_000, Some(200_000));
        assert_eq!(fresh_context_fill_ratio(), Some(0.775));
    }
}
