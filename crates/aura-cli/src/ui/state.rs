// ---------------------------------------------------------------------------
// Global statics and accessor functions ("state store")
// ---------------------------------------------------------------------------
//
// Every `static` / `Mutex` / `AtomicXxx` that was previously declared at the
// top of `prompt.rs` now lives here.  Other `ui::*` modules import from this
// module instead of reaching into prompt.rs directly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::api::types::DisplayEvent;
use crate::ui::welcome::WelcomeState;

use super::orchestrator::{ActiveOrchTool, OrchLastToolInfo};
use super::stream_panel::StreamPanelState;

// ---------------------------------------------------------------------------
// Terminal helpers
// ---------------------------------------------------------------------------

/// Global mutex that serializes all cursor-positioned terminal I/O.
/// Any code that does cursor save/move/write/restore or erase_input_frame
/// must hold this lock for the duration of its terminal write sequence.
pub(crate) static TERM_WRITE: Mutex<()> = Mutex::new(());

/// Acquire the terminal write lock.  Returns a `MutexGuard` that must be held
/// for the entire cursor-manipulation sequence.
pub(crate) fn lock_term() -> std::sync::MutexGuard<'static, ()> {
    TERM_WRITE.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn term_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

// ---------------------------------------------------------------------------
// Status bar state
// ---------------------------------------------------------------------------

pub(crate) static STATUS_BAR: Mutex<String> = Mutex::new(String::new());
pub(crate) static STATUS_HINT: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub(crate) static CUMULATIVE_PROMPT: Mutex<u64> = Mutex::new(0);
pub(crate) static CUMULATIVE_COMPLETION: Mutex<u64> = Mutex::new(0);
pub(crate) static CUMULATIVE_SCRATCHPAD_INTERCEPTED: Mutex<u64> = Mutex::new(0);
pub(crate) static CUMULATIVE_SCRATCHPAD_EXTRACTED: Mutex<u64> = Mutex::new(0);
pub(crate) static PROCESSING: AtomicBool = AtomicBool::new(false);
pub(crate) static QUEUED_INPUT: Mutex<String> = Mutex::new(String::new());
pub(crate) static QUEUED_WAVE_POS: Mutex<f32> = Mutex::new(0.0);
pub(crate) static QUEUED_WAVE_DIR: Mutex<f32> = Mutex::new(0.5);
/// Token ceiling at which auto-compact fires. 0 means no warning active yet.
pub(crate) static AUTO_COMPACT_CEILING: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Mid-stream input history (for up/down arrow during streaming)
// ---------------------------------------------------------------------------

/// Copy of per-conversation input history for mid-stream browsing.
pub(crate) static MID_STREAM_HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Current browse position: `len()` = at current typed input (not browsing).
pub(crate) static MID_STREAM_HISTORY_POS: Mutex<usize> = Mutex::new(0);
/// The buffer contents before the user started pressing up.
pub(crate) static MID_STREAM_SAVED_INPUT: Mutex<String> = Mutex::new(String::new());

// ---------------------------------------------------------------------------
// Shared REPL state (promoted from loop-local for mid-stream command access)
// ---------------------------------------------------------------------------

pub(crate) static EXPANDED_OUTPUT: AtomicBool = AtomicBool::new(false);
pub(crate) static EVENT_LOG: Mutex<Vec<DisplayEvent>> = Mutex::new(Vec::new());
pub(crate) static WELCOME_STATE: Mutex<Option<WelcomeState>> = Mutex::new(None);

/// Cached last-rendered animation lines (so replay can reprint them).
pub(crate) static LAST_ANIM_LINES: Mutex<(String, String)> =
    Mutex::new((String::new(), String::new()));

/// Pending command from mid-stream input that needs main-loop execution.
pub(crate) static PENDING_COMMAND: Mutex<Option<PendingCommand>> = Mutex::new(None);

/// Flag set by a SIGINT handler so drain_stdin detects Ctrl-C even when ISIG
/// is (unexpectedly) still enabled and the byte never reaches stdin.
pub(crate) static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Tracks when the first Ctrl-C was pressed for double-press-to-quit logic.
pub(crate) static LAST_CTRLC: Mutex<Option<Instant>> = Mutex::new(None);
/// Whether the "press Ctrl-C again to quit" hint is currently visible.
pub(crate) static CTRLC_HINT_VISIBLE: AtomicBool = AtomicBool::new(false);
/// Skip one `reset_ctrlc_state` call.
pub(crate) static CTRLC_RESET_SKIP: AtomicBool = AtomicBool::new(false);

/// Shared agent reasoning text.
pub(crate) static AGENT_REASONING: Mutex<String> = Mutex::new(String::new());
/// Sequence counter bumped each time the reasoning text changes.
pub(crate) static AGENT_REASONING_SEQ: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Orchestrator tool call tracking
// ---------------------------------------------------------------------------

/// Cumulative scrollback line counter for orchestrator output.
pub static ORCH_SCROLLBACK_COUNTER: AtomicU32 = AtomicU32::new(0);
/// Active orchestrator tool calls being tracked for live updates.
pub static ACTIVE_ORCH_TOOLS: Mutex<Vec<Arc<ActiveOrchTool>>> = Mutex::new(Vec::new());

/// Per-task tracking of the last tool's line numbers for tree-connector updates.
pub(crate) static ORCH_LAST_TOOL_LINES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<String, OrchLastToolInfo>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

// ---------------------------------------------------------------------------
// SSE Stream panel state
// ---------------------------------------------------------------------------

pub(crate) static STREAM_PANEL: Mutex<StreamPanelState> = Mutex::new(StreamPanelState::new());
pub(crate) static STREAM_PANEL_DIRTY: AtomicBool = AtomicBool::new(false);

/// Conversation directory used for persisting SSE events to `events.jsonl`.
pub(crate) static STREAM_CONV_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Input geometry tracking
// ---------------------------------------------------------------------------

/// How many visual terminal lines the input text currently occupies.
pub(crate) static INPUT_LINES: AtomicU16 = AtomicU16::new(1);
/// Where the frame (border + status) is actually drawn.
pub(crate) static FRAME_LINES: AtomicU16 = AtomicU16::new(1);
/// The cursor's row within the input area (0-indexed).
pub(crate) static CURSOR_ROW: AtomicU16 = AtomicU16::new(0);
/// Set by resize_status_area (growing) to force rustyline to do a full
/// repaint, which fixes cursor positioning after terminal scrolling.
pub(crate) static FORCE_REPAINT: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Terminal resize detection
// ---------------------------------------------------------------------------

/// Last known terminal width.
pub(crate) static LAST_TERM_WIDTH: AtomicU16 = AtomicU16::new(0);

// ---------------------------------------------------------------------------
// Bullet ("●") color helpers
// ---------------------------------------------------------------------------

pub(crate) const BULLET_PALETTE: &[(u8, u8, u8)] = &[
    (0, 255, 255),   // Cyan
    (255, 0, 255),   // Magenta
    (255, 255, 0),   // Yellow
    (0, 255, 0),     // Green
    (100, 149, 237), // Cornflower blue
    (255, 165, 0),   // Orange
    (147, 112, 219), // Purple
    (0, 255, 127),   // Spring green
    (255, 105, 180), // Hot pink
    (64, 224, 208),  // Turquoise
];

pub(crate) static BULLET_COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Command list & input hint state
// ---------------------------------------------------------------------------

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("/quit", "exit the REPL"),
    ("/exit", "exit the REPL"),
    ("/clear", "start a new conversation"),
    ("/help", "show available commands"),
    ("/expand", "toggle expanded/compact tool call view"),
    ("/stream", "toggle SSE event stream panel"),
    ("/conversations", "list saved conversations"),
    ("/resume", "resume a saved conversation"),
    ("/rename", "rename the current conversation"),
    ("/model", "select a model"),
];

// Cached matches from the last /resume autocomplete lookup.
pub(crate) static RESUME_MATCHES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

// Model selection state
pub(crate) static MODEL_CACHE: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub(crate) static MODEL_MATCHES: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub(crate) static MODEL_ERROR: Mutex<String> = Mutex::new(String::new());
pub(crate) static SELECTED_MODEL: Mutex<Option<String>> = Mutex::new(None);
/// Whether a model fetch is currently in progress.
pub(crate) static MODEL_FETCH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
/// Last input line from the hinter.
pub(crate) static LAST_HINT_LINE: Mutex<String> = Mutex::new(String::new());

/// Number of rows currently reserved for the status/hint area below the frame border.
/// Default is 3 (the legacy fixed size). Updated whenever STATUS_HINT changes.
pub(crate) static STATUS_ROWS: AtomicU16 = AtomicU16::new(3);

/// Tab-cycling index into the current match list (models or conversations).
/// None = no tab selection active.
pub(crate) static TAB_SELECT_INDEX: Mutex<Option<usize>> = Mutex::new(None);

/// Store config needed for model fetching (set once at REPL start).
#[allow(clippy::type_complexity)]
pub(crate) static MODEL_FETCH_CONFIG: Mutex<
    Option<(String, Option<String>, Vec<(String, String)>)>,
> = Mutex::new(None);

// ---------------------------------------------------------------------------
// PendingCommand enum
// ---------------------------------------------------------------------------

pub enum PendingCommand {
    Quit,
    Clear,
    Resume(String),
}

// ---------------------------------------------------------------------------
// Basic accessor functions
// ---------------------------------------------------------------------------

pub fn set_expanded_output(val: bool) {
    EXPANDED_OUTPUT.store(val, Ordering::Relaxed);
}

pub fn is_expanded_output() -> bool {
    EXPANDED_OUTPUT.load(Ordering::Relaxed)
}

pub fn push_display_event(event: DisplayEvent) {
    EVENT_LOG.lock().unwrap().push(event);
}

pub fn extend_display_events(events: Vec<DisplayEvent>) {
    EVENT_LOG.lock().unwrap().extend(events);
}

pub fn clear_display_events() {
    EVENT_LOG.lock().unwrap().clear();
}

pub fn with_event_log<R>(f: impl FnOnce(&[DisplayEvent]) -> R) -> R {
    let log = EVENT_LOG.lock().unwrap();
    f(&log)
}

pub fn with_event_log_mut<R>(f: impl FnOnce(&mut Vec<DisplayEvent>) -> R) -> R {
    let mut log = EVENT_LOG.lock().unwrap();
    f(&mut log)
}

pub fn set_welcome_state(w: Option<WelcomeState>) {
    *WELCOME_STATE.lock().unwrap() = w;
}

pub fn print_welcome_state() {
    let w = WELCOME_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref ws) = *w {
        ws.print_static();
    }
}

/// Like `print_welcome_state` but plays the fade-in animation.
pub fn print_welcome_state_animated() {
    let w = WELCOME_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref ws) = *w {
        ws.print();
    }
}

pub fn cache_anim_lines(top: &str, bottom: &str) {
    if let Ok(mut lines) = LAST_ANIM_LINES.lock() {
        lines.0 = top.to_string();
        lines.1 = bottom.to_string();
    }
}

pub fn set_pending_command(cmd: PendingCommand) {
    *PENDING_COMMAND.lock().unwrap() = Some(cmd);
}

pub fn take_pending_command() -> Option<PendingCommand> {
    PENDING_COMMAND.lock().unwrap().take()
}

/// Mark whether the app is actively processing a request.
pub fn set_processing(active: bool) {
    PROCESSING.store(active, Ordering::Relaxed);
}

/// Store text as the queued next input (replaces any previous value).
pub fn set_queued_input(text: String) {
    if let Ok(mut g) = QUEUED_INPUT.lock() {
        *g = text;
    }
    if let Ok(mut pos) = QUEUED_WAVE_POS.lock() {
        *pos = 0.0;
    }
    if let Ok(mut dir) = QUEUED_WAVE_DIR.lock() {
        *dir = 0.5;
    }
}

/// Consume and return the queued input, clearing it.
pub fn take_queued_input() -> String {
    QUEUED_INPUT
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

/// Clear the queued input without returning it.
#[allow(dead_code)]
pub fn clear_queued_input() {
    if let Ok(mut g) = QUEUED_INPUT.lock() {
        g.clear();
    }
}

/// Replace the mid-stream input history.
pub fn set_mid_stream_history(entries: Vec<String>) {
    if let Ok(mut g) = MID_STREAM_HISTORY.lock() {
        let len = entries.len();
        *g = entries;
        if let Ok(mut pos) = MID_STREAM_HISTORY_POS.lock() {
            *pos = len;
        }
    }
    if let Ok(mut g) = MID_STREAM_SAVED_INPUT.lock() {
        g.clear();
    }
}

/// Append a single entry to mid-stream history.
pub fn push_mid_stream_history(entry: String) {
    if let Ok(mut g) = MID_STREAM_HISTORY.lock() {
        if g.last() != Some(&entry) {
            g.push(entry);
        }
        if let Ok(mut pos) = MID_STREAM_HISTORY_POS.lock() {
            *pos = g.len();
        }
    }
}

/// Returns the most recent mid-stream history entry, if any.
pub fn last_mid_stream_history_entry() -> Option<String> {
    MID_STREAM_HISTORY
        .lock()
        .ok()
        .and_then(|g| g.last().cloned())
}

/// Get the currently selected model (None = let server decide).
pub fn get_selected_model() -> Option<String> {
    SELECTED_MODEL.lock().ok().and_then(|g| g.clone())
}

/// Set the selected model.
pub fn set_selected_model(model: Option<String>) {
    if let Ok(mut g) = SELECTED_MODEL.lock() {
        *g = model;
    }
}

/// Get the cached model matches.
pub fn get_model_matches() -> Vec<String> {
    MODEL_MATCHES.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Current number of status/hint rows below the frame border.
pub fn status_rows() -> u16 {
    STATUS_ROWS.load(Ordering::Relaxed)
}

/// Get the current tab-selection index.
pub fn get_tab_select_index() -> Option<usize> {
    TAB_SELECT_INDEX.lock().ok().and_then(|g| *g)
}

/// Set the tab-selection index.
pub fn set_tab_select_index(idx: Option<usize>) {
    if let Ok(mut g) = TAB_SELECT_INDEX.lock() {
        *g = idx;
    }
}

/// Register a SIGINT handler that sets [`SIGINT_RECEIVED`].
pub fn install_sigint_handler() {
    #[cfg(unix)]
    {
        unsafe {
            let _ = signal_hook::low_level::register(signal_hook::consts::SIGINT, || {
                SIGINT_RECEIVED.store(true, Ordering::Relaxed);
            });
        }
    }
}

/// Pick a random colour from the palette for the "●" bullet.
pub fn random_bullet_color() -> crossterm::style::Color {
    let count = BULLET_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let idx = ((count.wrapping_mul(7) ^ nanos) as usize) % BULLET_PALETTE.len();
    let (r, g, b) = BULLET_PALETTE[idx];
    crossterm::style::Color::Rgb { r, g, b }
}

/// Reset input geometry to defaults.
pub fn reset_input_geometry() {
    INPUT_LINES.store(1, Ordering::Relaxed);
    FRAME_LINES.store(1, Ordering::Relaxed);
    CURSOR_ROW.store(0, Ordering::Relaxed);
}

/// How many visual lines the frame currently occupies.
pub fn frame_lines() -> u16 {
    FRAME_LINES.load(Ordering::Relaxed)
}

/// How many visual lines the text currently occupies.
pub fn text_lines() -> u16 {
    INPUT_LINES.load(Ordering::Relaxed)
}

/// Check if the terminal width changed since the last check.
pub fn check_resize() -> Option<u16> {
    let (w, _) = term_size();
    let prev = LAST_TERM_WIDTH.swap(w, Ordering::Relaxed);
    if prev != 0 && prev != w {
        Some(prev)
    } else {
        None
    }
}
