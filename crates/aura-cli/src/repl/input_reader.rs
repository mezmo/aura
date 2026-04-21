use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, ConditionalEventHandler, Context, Editor, Event, EventContext, EventHandler, Helper,
    KeyCode, KeyEvent, Modifiers, Movement, RepeatCount,
};
use std::borrow::Cow;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::ui::prompt::{
    at_stream_top, check_resize, clear_screen_preserve_frame, commit_cursor_row,
    enter_stream_focus, exit_stream_focus, handle_resize_frame, is_stream_panel_focused,
    is_stream_panel_visible, reset_ctrlc_state, scroll_stream_down, scroll_stream_page_down,
    scroll_stream_page_up, scroll_stream_up, toggle_stream_expand, update_input_geometry,
    update_input_hint, validate_command_input,
};
use crate::ui::state::{
    FORCE_REPAINT, RESUME_MATCHES, get_model_matches, get_tab_select_index, set_tab_select_index,
};

/// Tracks the current readline buffer so it can be saved on Ctrl+C exit.
pub(crate) static LAST_READLINE_INPUT: Mutex<String> = Mutex::new(String::new());

/// How many steps back in history the user has navigated (0 = at current input).
pub(crate) static HISTORY_DEPTH: AtomicUsize = AtomicUsize::new(0);
/// Total number of entries in rustyline's history for the current session.
pub(crate) static HISTORY_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct AuraHelper;

impl Completer for AuraHelper {
    type Candidate = String;
}

impl Hinter for AuraHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        // Any keystroke in readline resets the Ctrl-C double-press-to-quit state.
        reset_ctrlc_state();
        // Reset tab selection on any buffer-changing keystroke.
        set_tab_select_index(None);
        // Snapshot the current input so it's available if the user Ctrl+C exits.
        if let Ok(mut g) = LAST_READLINE_INPUT.lock() {
            g.clear();
            g.push_str(line);
        }
        // Unfocus stream panel when user types regular characters
        if is_stream_panel_focused() && !line.is_empty() {
            exit_stream_focus();
        }
        let new_cursor_row = update_input_geometry(line, pos);
        update_input_hint(line);
        // Commit after update_input_hint so that update_status_bar (called
        // inside update_input_hint) still sees the OLD cursor row, which
        // matches the actual terminal cursor position.
        commit_cursor_row(new_cursor_row);
        // Redraw frame borders + status if terminal was resized
        if let Some(old_width) = check_resize() {
            handle_resize_frame(old_width);
        }
        None
    }
}

impl Highlighter for AuraHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let w = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80);
        let total = 2 + line.len(); // PROMPT_COLS (2) + text length
        // When text wraps, append clear-to-EOL so rustyline's redraw
        // wipes stale border/status characters on the last partial row.
        if w > 0 && total > w {
            Cow::Owned(format!("{}\x1b[0K", line))
        } else {
            Cow::Borrowed(line)
        }
    }

    fn highlight_char(
        &self,
        line: &str,
        _pos: usize,
        _kind: rustyline::highlight::CmdKind,
    ) -> bool {
        // Force full repaint after a status-area resize so rustyline
        // recalculates cursor positioning from scratch.
        if FORCE_REPAINT.swap(false, Ordering::Relaxed) {
            return true;
        }
        // Force full repaint when text wraps past terminal width.
        let w = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80);
        w > 0 && 2 + line.len() > w
    }
}
impl Validator for AuraHelper {}
impl Helper for AuraHelper {}

/// Intercept "?" on an empty buffer: show help hints in the status bar
/// without inserting "?" into the input. Returns Noop so the buffer stays empty
/// and subsequent typing (e.g. "/") starts fresh.
struct QuestionHandler;

impl ConditionalEventHandler for QuestionHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        if ctx.line().is_empty() {
            update_input_hint("?");
            return Some(Cmd::Noop);
        }
        None
    }
}

/// Gate Enter: only submit when the input is valid (known command or non-command text).
/// For `/resume ` with an argument, only submit when exactly 1 conversation matches.
/// When the stream panel is focused, Enter toggles expand instead.
struct EnterHandler;

impl ConditionalEventHandler for EnterHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        if is_stream_panel_focused() {
            toggle_stream_expand();
            return Some(Cmd::Noop);
        }
        if validate_command_input(ctx.line()) {
            None // default Enter behavior (submit)
        } else {
            Some(Cmd::Noop) // block Enter
        }
    }
}

/// Down arrow: navigate forward in history first; only enter/scroll the stream
/// panel once the user is back at the current (newest) input.
struct StreamDownHandler;

impl ConditionalEventHandler for StreamDownHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext,
    ) -> Option<Cmd> {
        if is_stream_panel_focused() {
            scroll_stream_down();
            Some(Cmd::Noop)
        } else if HISTORY_DEPTH.load(Ordering::Relaxed) > 0 {
            // Still navigating back through history — let rustyline go forward.
            HISTORY_DEPTH.fetch_sub(1, Ordering::Relaxed);
            None // default rustyline behavior (next history)
        } else if is_stream_panel_visible() {
            enter_stream_focus();
            Some(Cmd::Noop)
        } else {
            None // no stream panel, no history to navigate
        }
    }
}

/// Up arrow: scroll up or exit the stream panel when focused.
/// When not focused, track history depth so down-arrow knows when to enter the stream panel.
struct StreamUpHandler;

impl ConditionalEventHandler for StreamUpHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext,
    ) -> Option<Cmd> {
        if is_stream_panel_focused() {
            if at_stream_top() {
                exit_stream_focus();
            } else {
                scroll_stream_up();
            }
            Some(Cmd::Noop)
        } else {
            // Track how deep into history the user has gone (capped at actual count).
            let count = HISTORY_COUNT.load(Ordering::Relaxed);
            let _ = HISTORY_DEPTH.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| {
                if d < count { Some(d + 1) } else { None }
            });
            None // default rustyline behavior (history)
        }
    }
}

/// PageDown: page-jump through stream panel or input history.
struct PageDownHandler;

impl ConditionalEventHandler for PageDownHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext,
    ) -> Option<Cmd> {
        if is_stream_panel_focused() {
            scroll_stream_page_down();
            Some(Cmd::Noop)
        } else {
            // Jump forward 10 entries in input history
            let depth = HISTORY_DEPTH.load(Ordering::Relaxed);
            if depth > 0 {
                let jump = depth.min(10);
                HISTORY_DEPTH.fetch_sub(jump, Ordering::Relaxed);
                // Emit individual history-next commands so rustyline updates the line
                Some(Cmd::LineDownOrNextHistory(10))
            } else if is_stream_panel_visible() {
                enter_stream_focus();
                Some(Cmd::Noop)
            } else {
                None
            }
        }
    }
}

/// PageUp: page-jump through stream panel or input history.
struct PageUpHandler;

impl ConditionalEventHandler for PageUpHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext,
    ) -> Option<Cmd> {
        if is_stream_panel_focused() {
            if at_stream_top() {
                exit_stream_focus();
            } else {
                scroll_stream_page_up();
            }
            Some(Cmd::Noop)
        } else {
            // Jump back 10 entries in input history (capped at count)
            let count = HISTORY_COUNT.load(Ordering::Relaxed);
            let _ = HISTORY_DEPTH.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| {
                let new = (d + 10).min(count);
                if new != d { Some(new) } else { None }
            });
            Some(Cmd::LineUpOrPreviousHistory(10))
        }
    }
}

/// Tab: cycle through model/conversation matches with visual highlighting.
struct TabHandler;

impl ConditionalEventHandler for TabHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        let line = ctx.line();
        let match_count = if line == "/model" || line.starts_with("/model ") {
            get_model_matches().len()
        } else if line == "/resume" || line.starts_with("/resume ") {
            RESUME_MATCHES.lock().map(|g| g.len()).unwrap_or(0)
        } else {
            return None; // not in a list context, pass through
        };
        if match_count == 0 {
            return Some(Cmd::Noop);
        }
        let current = get_tab_select_index();
        let next = match current {
            Some(idx) => (idx + 1) % match_count,
            None => 0,
        };
        set_tab_select_index(Some(next));
        update_input_hint(line);
        Some(Cmd::Noop)
    }
}

/// Shift+Tab: cycle backward through model/conversation matches.
struct ShiftTabHandler;

impl ConditionalEventHandler for ShiftTabHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        let line = ctx.line();
        let match_count = if line == "/model" || line.starts_with("/model ") {
            get_model_matches().len()
        } else if line == "/resume" || line.starts_with("/resume ") {
            RESUME_MATCHES.lock().map(|g| g.len()).unwrap_or(0)
        } else {
            return None;
        };
        if match_count == 0 {
            return Some(Cmd::Noop);
        }
        let current = get_tab_select_index();
        let prev = match current {
            Some(0) | None => match_count - 1,
            Some(idx) => idx - 1,
        };
        set_tab_select_index(Some(prev));
        update_input_hint(line);
        Some(Cmd::Noop)
    }
}

/// Escape: cancel tab completion if active.
struct EscHandler;

impl ConditionalEventHandler for EscHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        if get_tab_select_index().is_some() {
            set_tab_select_index(None);
            update_input_hint(ctx.line());
            return Some(Cmd::Noop);
        }
        None // fall through to default Esc behavior
    }
}

/// Ctrl+L: clear input line if non-empty, or clear the terminal screen if empty.
struct ClearLineHandler;

impl ConditionalEventHandler for ClearLineHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        if ctx.line().is_empty() {
            clear_screen_preserve_frame();
            Some(Cmd::Repaint)
        } else {
            Some(Cmd::Kill(Movement::WholeLine))
        }
    }
}

/// Create and configure the input reader with all keybindings.
pub(crate) fn create_input_reader() -> rustyline::Result<Editor<AuraHelper, DefaultHistory>> {
    let mut input_reader = Editor::new()?;
    input_reader.set_helper(Some(AuraHelper));
    // Intercept "?" on empty buffer to show help hints without inserting "?"
    input_reader.bind_sequence(
        KeyEvent::from('?'),
        EventHandler::Conditional(Box::new(QuestionHandler)),
    );
    // Gate Enter: block submission for partial/unknown commands and ambiguous /resume
    input_reader.bind_sequence(
        KeyEvent::from('\r'),
        EventHandler::Conditional(Box::new(EnterHandler)),
    );
    // Stream panel: Down arrow to enter/scroll
    input_reader.bind_sequence(
        KeyEvent(KeyCode::Down, Modifiers::NONE),
        EventHandler::Conditional(Box::new(StreamDownHandler)),
    );
    // Stream panel: Up arrow to scroll/exit
    input_reader.bind_sequence(
        KeyEvent(KeyCode::Up, Modifiers::NONE),
        EventHandler::Conditional(Box::new(StreamUpHandler)),
    );
    // PageDown: page-jump through stream panel or input history
    input_reader.bind_sequence(
        KeyEvent(KeyCode::PageDown, Modifiers::NONE),
        EventHandler::Conditional(Box::new(PageDownHandler)),
    );
    // PageUp: page-jump through stream panel or input history
    input_reader.bind_sequence(
        KeyEvent(KeyCode::PageUp, Modifiers::NONE),
        EventHandler::Conditional(Box::new(PageUpHandler)),
    );
    // Tab: cycle through model/conversation matches
    input_reader.bind_sequence(
        KeyEvent(KeyCode::Tab, Modifiers::NONE),
        EventHandler::Conditional(Box::new(TabHandler)),
    );
    // Shift+Tab: cycle backward through matches
    input_reader.bind_sequence(
        KeyEvent(KeyCode::BackTab, Modifiers::NONE),
        EventHandler::Conditional(Box::new(ShiftTabHandler)),
    );
    // Escape: cancel tab completion
    input_reader.bind_sequence(
        KeyEvent(KeyCode::Esc, Modifiers::NONE),
        EventHandler::Conditional(Box::new(EscHandler)),
    );
    // Ctrl+L: clear input line only (override default clear-screen)
    input_reader.bind_sequence(
        KeyEvent::ctrl('L'),
        EventHandler::Conditional(Box::new(ClearLineHandler)),
    );
    Ok(input_reader)
}

pub(crate) fn get_history_path() -> Option<String> {
    let data_dir = dirs::data_local_dir()?;
    let path = data_dir.join("aura-cli").join("history.txt");
    path.to_str().map(|s| s.to_string())
}
