// ---------------------------------------------------------------------------
// Mid-stream input handling (drain_stdin, render_input_line, poll_input)
// ---------------------------------------------------------------------------

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{Attribute, Color, Stylize};
use crossterm::terminal;
use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::event_replay::{list_conversations, print_help, replay_event_log_global};
use super::input_frame::{erase_input_frame, redraw_input_frame};
use super::input_hint::update_input_hint;
use super::state::{COMMANDS, PendingCommand};
use super::state::{
    CTRLC_HINT_VISIBLE, EXPANDED_OUTPUT, LAST_ANIM_LINES, MID_STREAM_HISTORY,
    MID_STREAM_HISTORY_POS, MID_STREAM_SAVED_INPUT, PROCESSING, QUEUED_INPUT, SIGINT_RECEIVED,
    get_selected_model, lock_term, set_pending_command, set_queued_input, term_size,
};
use super::status_bar::{handle_ctrlc, reset_ctrlc_state, update_status_bar};
use super::stream_panel::{
    at_stream_top, clear_stream_panel_in_place, enter_stream_focus, exit_stream_focus,
    is_stream_panel_focused, is_stream_panel_visible, scroll_stream_down, scroll_stream_page_down,
    scroll_stream_page_up, scroll_stream_up, set_stream_show_all, toggle_stream_expand,
    toggle_stream_panel,
};

const PROMPT_COLS: usize = 2; // "❯ " occupies 2 display columns

/// Return the styled prompt string for rustyline.
pub fn styled_prompt() -> String {
    format!("{} ", "❯".with(Color::Green).attribute(Attribute::Bold))
}

/// Reprint cached animation lines above the frame after a replay.
fn reprint_cached_anim_lines() {
    if let Ok(lines) = LAST_ANIM_LINES.lock()
        && !lines.0.is_empty()
    {
        println!("{}", lines.0);
        println!("{}", lines.1);
    }
}

// ---------------------------------------------------------------------------
// Mid-stream immediate command execution
// ---------------------------------------------------------------------------

enum ImmediateResult {
    Handled,
    HandledCancel,
    NotHandled,
}

/// Resolve a possibly-abbreviated slash command to its full form.
fn resolve_command_prefix(input: &str) -> String {
    if COMMANDS.iter().any(|(name, _)| *name == input) {
        return input.to_string();
    }
    let (cmd_part, args_part) = match input.find(' ') {
        Some(pos) => (&input[..pos], Some(&input[pos..])),
        None => (input, None),
    };
    let matches: Vec<&str> = COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(cmd_part))
        .map(|(name, _)| *name)
        .collect();
    if matches.len() == 1 {
        match args_part {
            Some(args) => format!("{}{}", matches[0], args),
            None => matches[0].to_string(),
        }
    } else {
        input.to_string()
    }
}

/// Execute a command immediately during processing.
fn execute_immediate_command(input: &str) -> ImmediateResult {
    let resolved = resolve_command_prefix(input);
    match resolved.as_str() {
        "/stream" => {
            clear_stream_panel_in_place();
            toggle_stream_panel();
            erase_input_frame();
            redraw_input_frame();
            ImmediateResult::Handled
        }
        "/expand" => {
            let expanded = !EXPANDED_OUTPUT.load(Ordering::Relaxed);
            EXPANDED_OUTPUT.store(expanded, Ordering::Relaxed);
            set_stream_show_all(expanded);
            replay_event_log_global();
            reprint_cached_anim_lines();
            redraw_input_frame();
            ImmediateResult::Handled
        }
        "/help" => {
            replay_event_log_global();
            print_help();
            reprint_cached_anim_lines();
            redraw_input_frame();
            ImmediateResult::Handled
        }
        "/conversations" => {
            replay_event_log_global();
            list_conversations();
            reprint_cached_anim_lines();
            redraw_input_frame();
            ImmediateResult::Handled
        }
        "/quit" | "/exit" => {
            set_pending_command(PendingCommand::Quit);
            ImmediateResult::HandledCancel
        }
        "/clear" => {
            set_pending_command(PendingCommand::Clear);
            ImmediateResult::HandledCancel
        }
        _ if input.starts_with("/resume") => {
            let arg = input
                .strip_prefix("/resume")
                .unwrap_or("")
                .trim()
                .to_string();
            set_pending_command(PendingCommand::Resume(arg));
            ImmediateResult::HandledCancel
        }
        _ if input == "/model" || input.starts_with("/model ") => {
            replay_event_log_global();
            let current = get_selected_model();
            match current {
                Some(m) => println!("Current model: {}", m),
                None => println!("No model selected (using server default)"),
            }
            reprint_cached_anim_lines();
            redraw_input_frame();
            ImmediateResult::Handled
        }
        _ => ImmediateResult::NotHandled,
    }
}

// ---------------------------------------------------------------------------
// Mid-stream history navigation
// ---------------------------------------------------------------------------

/// Navigate backward (older) in mid-stream input history.
fn mid_stream_history_up(buf: &mut String) {
    if buf.is_empty()
        && let Ok(mut queued) = QUEUED_INPUT.lock()
        && !queued.is_empty()
    {
        buf.clear();
        buf.push_str(&queued);
        queued.clear();
        render_input_line(buf);
        return;
    }

    let history = match MID_STREAM_HISTORY.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if history.is_empty() {
        return;
    }
    let mut pos = MID_STREAM_HISTORY_POS.lock().unwrap();
    if *pos == 0 {
        return;
    }
    if *pos == history.len()
        && let Ok(mut saved) = MID_STREAM_SAVED_INPUT.lock()
    {
        *saved = buf.clone();
    }
    *pos -= 1;
    buf.clear();
    buf.push_str(&history[*pos]);
    render_input_line(buf);
}

/// Navigate forward (newer) in mid-stream input history.
fn mid_stream_history_down(buf: &mut String) {
    let history = match MID_STREAM_HISTORY.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut pos = MID_STREAM_HISTORY_POS.lock().unwrap();
    if *pos < history.len() {
        *pos += 1;
        buf.clear();
        if *pos == history.len() {
            if let Ok(saved) = MID_STREAM_SAVED_INPUT.lock() {
                buf.push_str(&saved);
            }
        } else {
            buf.push_str(&history[*pos]);
        }
        render_input_line(buf);
    } else if is_stream_panel_visible() {
        enter_stream_focus();
    }
}

// ---------------------------------------------------------------------------
// drain_stdin
// ---------------------------------------------------------------------------

/// Drain all available bytes from stdin into the buffer.
/// Returns `true` if a standalone ESC key press was detected.
pub fn drain_stdin(buf: &mut String) -> bool {
    let mut esc_pressed = false;
    if SIGINT_RECEIVED.swap(false, Ordering::Relaxed) && handle_ctrlc() {
        set_pending_command(PendingCommand::Quit);
        esc_pressed = true;
    }
    #[cfg(unix)]
    {
        let prev_len = buf.len();
        let mut tmp = [0u8; 256];
        loop {
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    tmp.as_mut_ptr() as *mut libc::c_void,
                    tmp.len(),
                )
            };
            if n <= 0 {
                break;
            }
            let bytes = &tmp[..n as usize];
            let mut i = 0;
            while i < bytes.len() {
                let b = bytes[i];
                if b == 0x03 {
                    if handle_ctrlc() {
                        set_pending_command(PendingCommand::Quit);
                        esc_pressed = true;
                    }
                    i += 1;
                    continue;
                }
                if CTRLC_HINT_VISIBLE.load(Ordering::Relaxed) {
                    reset_ctrlc_state();
                }
                if b == 0x1B {
                    if i + 1 < bytes.len() && (bytes[i + 1] == b'[' || bytes[i + 1] == b'O') {
                        let seq_start = i;
                        i += 2;
                        if bytes.get(seq_start + 1) == Some(&b'[') {
                            while i < bytes.len() && !(bytes[i] >= 0x40 && bytes[i] <= 0x7E) {
                                i += 1;
                            }
                            if i < bytes.len() && bytes[i] == b'B' && (i - seq_start) == 2 {
                                if is_stream_panel_focused() {
                                    scroll_stream_down();
                                } else {
                                    mid_stream_history_down(buf);
                                }
                            }
                            if i < bytes.len() && bytes[i] == b'A' && (i - seq_start) == 2 {
                                if is_stream_panel_focused() {
                                    if at_stream_top() {
                                        exit_stream_focus();
                                    } else {
                                        scroll_stream_up();
                                    }
                                } else {
                                    mid_stream_history_up(buf);
                                }
                            }
                            if i < bytes.len()
                                && bytes[i] == b'~'
                                && bytes.get(seq_start + 2) == Some(&b'6')
                                && is_stream_panel_focused()
                            {
                                scroll_stream_page_down();
                            }
                            if i < bytes.len()
                                && bytes[i] == b'~'
                                && bytes.get(seq_start + 2) == Some(&b'5')
                                && is_stream_panel_focused()
                            {
                                scroll_stream_page_up();
                            }
                            if i < bytes.len() {
                                i += 1;
                            }
                        } else if i < bytes.len() {
                            i += 1;
                        }
                    } else {
                        esc_pressed = true;
                        i += 1;
                    }
                    continue;
                }
                if b == 0x7F || b == 0x08 {
                    buf.pop();
                } else if b == b'?' && buf.is_empty() {
                    update_input_hint("?");
                } else if b == 0x0A || b == 0x0D {
                    if is_stream_panel_focused() {
                        toggle_stream_expand();
                    } else if !buf.is_empty() {
                        match execute_immediate_command(buf.trim()) {
                            ImmediateResult::Handled => {
                                buf.clear();
                            }
                            ImmediateResult::HandledCancel => {
                                buf.clear();
                                esc_pressed = true;
                            }
                            ImmediateResult::NotHandled => {
                                set_queued_input(buf.clone());
                                buf.clear();
                                update_status_bar();
                            }
                        }
                    }
                } else if (0x20..0x7F).contains(&b) {
                    buf.push(b as char);
                }
                i += 1;
            }
        }
        if buf.len() != prev_len {
            update_input_hint(buf);
        }
    }
    esc_pressed
}

/// Render the prompt and input buffer on the current line.
pub fn render_input_line(buf: &str) {
    let mut stdout = io::stdout();
    let (width, _) = term_size();
    let prompt = styled_prompt();
    let available = (width as usize).saturating_sub(PROMPT_COLS);

    let _ = execute!(stdout, cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));

    if CTRLC_HINT_VISIBLE.load(Ordering::Relaxed) {
        print!(
            "{}{}",
            prompt,
            "Press Ctrl+C again to quit".with(Color::DarkGrey)
        );
        let _ = execute!(stdout, cursor::MoveToColumn(PROMPT_COLS as u16));
    } else if available > 1 && buf.len() > available {
        let start = buf.len() - (available - 1);
        print!("{}…{}", prompt, &buf[start..]);
    } else if buf.is_empty()
        && PROCESSING.load(Ordering::Relaxed)
        && QUEUED_INPUT.lock().map(|g| !g.is_empty()).unwrap_or(false)
    {
        let hint = "Press up to edit queued message";
        print!("{}{}", prompt, hint.with(Color::DarkGrey));
        let _ = execute!(stdout, cursor::MoveToColumn(PROMPT_COLS as u16));
    } else {
        print!("{}{}", prompt, buf);
    }
    let _ = stdout.flush();
}

/// Make the input line interactive during processing.
pub fn prepare_input_line(input_buf: &Mutex<String>, cancel: Option<&AtomicBool>) {
    if let Ok(mut buf) = input_buf.lock() {
        if drain_stdin(&mut buf)
            && let Some(flag) = cancel
        {
            flag.store(true, Ordering::Relaxed);
        }
        let _term = lock_term();
        render_input_line(&buf);
    }
    let _ = execute!(io::stdout(), cursor::Show);
}

/// Poll stdin for new keystrokes and re-render the input line only if new
/// characters arrived.
#[allow(dead_code)]
pub fn poll_input(input_buf: &Mutex<String>, cancel: Option<&AtomicBool>) {
    if let Ok(mut buf) = input_buf.lock() {
        let prev_len = buf.len();
        if drain_stdin(&mut buf)
            && let Some(flag) = cancel
        {
            flag.store(true, Ordering::Relaxed);
        }
        if buf.len() != prev_len {
            render_input_line(&buf);
        }
    }
}
