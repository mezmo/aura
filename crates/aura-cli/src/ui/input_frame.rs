// ---------------------------------------------------------------------------
// Terminal setup, frame management, and input geometry
// ---------------------------------------------------------------------------

use std::io::{self, Write};
use std::sync::atomic::Ordering;

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{Color, Stylize};
use crossterm::terminal;

use super::animation::render_queued_wave;
use super::state::{
    CURSOR_ROW, FORCE_REPAINT, FRAME_LINES, INPUT_LINES, LAST_TERM_WIDTH, PROCESSING, QUEUED_INPUT,
    QUEUED_WAVE_POS, lock_term, status_rows, term_size,
};
use super::status_bar::{
    get_effective_status, is_hint_active, print_status_line, set_status_bar,
    set_status_with_right_text,
};
use super::stream_panel::{render_stream_panel_in_place, stream_panel_rows};

const PROMPT_COLS: usize = 2; // "❯ " occupies 2 display columns

/// Number of visual terminal lines that prompt + text occupies.
fn visual_line_count(text: &str, term_width: u16) -> u16 {
    let w = term_width as usize;
    if w == 0 {
        return 1;
    }
    let total = PROMPT_COLS + text.len();
    total.div_ceil(w).max(1) as u16
}

/// Which visual row (0-indexed) the cursor is on within the input area.
fn cursor_visual_row(pos: usize, term_width: u16) -> u16 {
    let w = term_width as usize;
    if w == 0 {
        return 0;
    }
    ((PROMPT_COLS + pos) / w) as u16
}

/// Update shared geometry atomics and reposition the frame if the line count changed.
pub fn update_input_geometry(line: &str, pos: usize) -> u16 {
    let (width, _) = term_size();
    if width == 0 {
        return 0;
    }

    let text_lines = visual_line_count(line, width);
    let new_cursor_row = cursor_visual_row(pos, width);
    let new_lines = text_lines.max(new_cursor_row + 1);

    let prev_text_lines = INPUT_LINES.swap(new_lines, Ordering::Relaxed);
    let old_cursor_row = CURSOR_ROW.load(Ordering::Relaxed);
    let frame_pos = FRAME_LINES.load(Ordering::Relaxed);

    if new_lines > frame_pos {
        // Growing past the current frame: adjust immediately (always safe).
        adjust_frame_for_line_change(frame_pos, new_lines, old_cursor_row);
        FRAME_LINES.store(new_lines, Ordering::Relaxed);
    } else if new_lines < frame_pos && new_lines == prev_text_lines {
        // Text is stable at a smaller size (second+ call after shrink).
        // Rustyline's old_rows now matches the current text size, so it
        // won't clear rows that overlap our frame. Safe to compact.
        #[allow(clippy::if_same_then_else)]
        {
            adjust_frame_for_line_change(frame_pos, new_lines, old_cursor_row);
            FRAME_LINES.store(new_lines, Ordering::Relaxed);
        }
    }

    new_cursor_row
}

/// Store the new cursor row.
pub fn commit_cursor_row(row: u16) {
    CURSOR_ROW.store(row, Ordering::Relaxed);
}

/// Reposition the bottom border and status rows when the input line count changes.
fn adjust_frame_for_line_change(old_lines: u16, new_lines: u16, old_cursor_row: u16) {
    let mut stdout = io::stdout();
    let (width, _) = term_size();
    let border: String = "─".repeat(width as usize);
    let styled_border = border.with(Color::DarkGrey);
    let status_lines = get_effective_status();

    let _ = execute!(stdout, cursor::SavePosition);

    // 1. Clear old border + status rows (stream panel is rendered in-place
    //    by the animation thread and is not part of the frame layout).
    let sr = status_rows();
    let to_old_border = (old_lines as i32) - (old_cursor_row as i32);
    if to_old_border > 0 {
        let _ = execute!(stdout, cursor::MoveDown(to_old_border as u16));
    } else if to_old_border < 0 {
        let _ = execute!(stdout, cursor::MoveUp((-to_old_border) as u16));
    }
    let _ = execute!(stdout, cursor::MoveToColumn(0));
    for _ in 0..(1 + sr) {
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    }
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));

    // 2. Navigate to new border position
    let total_cleared = 1 + sr;
    let diff = (new_lines as i32) - (old_cursor_row as i32) - (total_cleared as i32);
    if diff > 0 {
        let _ = execute!(stdout, cursor::MoveDown(diff as u16));
    } else if diff < 0 {
        let _ = execute!(stdout, cursor::MoveUp((-diff) as u16));
    }
    let _ = execute!(stdout, cursor::MoveToColumn(0));

    // 3. Draw new border + status rows
    let queued = QUEUED_INPUT.lock().map(|g| g.clone()).unwrap_or_default();
    let show_queued = PROCESSING.load(Ordering::Relaxed) && !queued.is_empty();

    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!("{styled_border}");

    let hint_active = is_hint_active();

    for i in 0..sr as usize {
        let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        if i == (sr as usize) - 1 && show_queued {
            let wave_pos = QUEUED_WAVE_POS.lock().map(|g| *g).unwrap_or(0.0);
            print!("{}", render_queued_wave(&queued, wave_pos));
        } else if let Some(line) = status_lines.get(i) {
            print_status_line(line, hint_active);
        }
    }

    // 4. If shrinking, clear orphan rows below
    if old_lines > new_lines {
        for _ in 0..(old_lines - new_lines) {
            let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
            let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        }
    }

    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Set up the terminal.
pub fn setup_terminal() {
    set_status_bar(set_status_with_right_text("", "Aura, by Mezmo!"));
    let (w, _) = term_size();
    LAST_TERM_WIDTH.store(w, Ordering::Relaxed);
    println!();
    let _ = execute!(io::stdout(), cursor::Show);
    redraw_input_frame();
}

/// Reset terminal to normal state before exit.
pub fn cleanup_terminal() {
    let mut stdout = io::stdout();
    restore_terminal_mode();
    let _ = execute!(stdout, cursor::Show);
}

/// Draw the input frame inline.
/// Caller must hold `TERM_WRITE` (via `lock_term()`) when called during streaming.
pub fn redraw_input_frame() {
    INPUT_LINES.store(1, Ordering::Relaxed);
    FRAME_LINES.store(1, Ordering::Relaxed);
    CURSOR_ROW.store(0, Ordering::Relaxed);
    let mut stdout = io::stdout();
    let (width, _) = term_size();
    LAST_TERM_WIDTH.store(width, Ordering::Relaxed);
    let border: String = "─".repeat(width as usize);
    let styled_border = border.with(Color::DarkGrey);
    let status_lines = get_effective_status();
    let hint_active = is_hint_active();
    let queued = QUEUED_INPUT.lock().map(|g| g.clone()).unwrap_or_default();
    let show_queued = PROCESSING.load(Ordering::Relaxed) && !queued.is_empty();

    let sr = status_rows();
    println!("{styled_border}");
    println!();
    println!("{styled_border}");
    // Print status rows. The last row is reserved for the queued wave and uses
    // print! (no newline) to keep cursor on the same line. All earlier rows use
    // println! which advances the cursor.
    for i in 0..sr as usize {
        if i == (sr as usize) - 1 {
            // Last row: only used for queued wave (print!, not println!)
            if show_queued {
                let wave_pos = QUEUED_WAVE_POS.lock().map(|g| *g).unwrap_or(0.0);
                print!("{}", render_queued_wave(&queued, wave_pos));
            }
        } else if let Some(line) = status_lines.get(i) {
            if !line.is_empty() {
                if hint_active {
                    println!("{line}");
                } else {
                    println!("{}", line.as_str().with(Color::DarkGrey));
                }
            } else {
                println!();
            }
        } else {
            println!();
        }
    }
    // Pre-allocate space for the stream panel by printing blank lines.
    // This uses natural terminal scrolling (which preserves scrollback)
    // rather than the explicit ScrollUp in render_stream_panel_in_place
    // which can push output above the viewport.
    let sp = stream_panel_rows();
    for _ in 0..sp {
        println!();
    }
    let _ = stdout.flush();
    let _ = execute!(stdout, cursor::MoveUp(1 + sr + sp), cursor::MoveToColumn(0));
    // Render the stream panel in-place (cursor save/restore) into the
    // space we just allocated above.
    render_stream_panel_in_place();
}

/// Clear the visible terminal, then redraw the input frame at the top.
pub fn clear_screen_preserve_frame() {
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0),
    );
    redraw_input_frame();
}

/// Erase the input frame.
/// Caller must hold `TERM_WRITE` (via `lock_term()`) when called during streaming.
pub fn erase_input_frame() {
    let mut stdout = io::stdout();
    let n = FRAME_LINES.load(Ordering::Relaxed);
    let _ = execute!(stdout, cursor::MoveUp(n), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    for _ in 0..n {
        let _ = execute!(stdout, cursor::MoveDown(1));
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    }
    let sr = status_rows();
    let sp = stream_panel_rows();
    // Clear border + status rows + any in-place rendered stream panel rows.
    for _ in 0..(1 + sr + sp) {
        let _ = execute!(stdout, cursor::MoveDown(1));
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    }
    // Return cursor to the top of where the frame was (stream panel is
    // NOT part of the frame layout, so we skip it in the return offset).
    let _ = execute!(
        stdout,
        cursor::MoveUp(n + 1 + sr + sp),
        cursor::MoveToColumn(0)
    );
    let _ = stdout.flush();
}

/// Put terminal in non-canonical mode with no echo and non-blocking reads.
pub fn set_noncanonical_noecho() {
    #[cfg(unix)]
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        libc::tcgetattr(libc::STDIN_FILENO, &mut termios);
        termios.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        termios.c_cc[libc::VMIN] = 0;
        termios.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios);
    }
}

/// Restore terminal to canonical mode with echo.
pub fn restore_terminal_mode() {
    #[cfg(unix)]
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        libc::tcgetattr(libc::STDIN_FILENO, &mut termios);
        termios.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios);
    }
}

/// Redraw the input frame borders and status rows after a terminal resize.
pub fn handle_resize_frame(old_width: u16) {
    let mut stdout = io::stdout();
    let (new_width, _) = term_size();
    let border = "─".repeat(new_width as usize);
    let styled_border = border.with(Color::DarkGrey);

    let n = FRAME_LINES.load(Ordering::Relaxed);
    let r = CURSOR_ROW.load(Ordering::Relaxed);

    let extra_per_border = if new_width > 0 && old_width > new_width {
        (old_width as u32).div_ceil(new_width as u32) as u16 - 1
    } else {
        0
    };

    let _ = execute!(stdout, cursor::SavePosition);

    let clear_above = r + 1 + extra_per_border;
    if clear_above > 0 {
        let _ = execute!(stdout, cursor::MoveUp(clear_above), cursor::MoveToColumn(0));
    }
    let sr = status_rows();
    let total_clear = clear_above + n + 1 + extra_per_border + sr + extra_per_border;
    for _ in 0..=total_clear {
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    }
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));

    let _ = execute!(stdout, cursor::RestorePosition);

    let _ = execute!(stdout, cursor::MoveUp(r + 1), cursor::MoveToColumn(0));
    print!("{styled_border}");

    let _ = execute!(stdout, cursor::MoveDown(n + 1), cursor::MoveToColumn(0));
    print!("{styled_border}");

    let status_lines = get_effective_status();
    let hint_active = is_hint_active();
    let queued = QUEUED_INPUT.lock().map(|g| g.clone()).unwrap_or_default();
    let show_queued = PROCESSING.load(Ordering::Relaxed) && !queued.is_empty();

    for i in 0..sr as usize {
        let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
        if i == (sr as usize) - 1 && show_queued {
            let wave_pos = QUEUED_WAVE_POS.lock().map(|g| *g).unwrap_or(0.0);
            print!("{}", render_queued_wave(&queued, wave_pos));
        } else if let Some(line) = status_lines.get(i) {
            print_status_line(line, hint_active);
        }
    }

    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

// ---------------------------------------------------------------------------
// Helpers: update or collapse lines above the input frame
// ---------------------------------------------------------------------------

/// Resize the status/hint area when the number of hint lines changes.
/// Erases old rows, prints new rows, and repositions the cursor.
pub fn resize_status_area(old_sr: u16, new_sr: u16) {
    let mut stdout = io::stdout();
    let (width, _) = term_size();
    let border = "─".repeat(width as usize);
    let styled_border = border.with(Color::DarkGrey);
    let n = FRAME_LINES.load(Ordering::Relaxed);
    let r = CURSOR_ROW.load(Ordering::Relaxed);
    let status_lines = get_effective_status();
    let hint_active = is_hint_active();
    let queued = QUEUED_INPUT.lock().map(|g| g.clone()).unwrap_or_default();
    let show_queued = PROCESSING.load(Ordering::Relaxed) && !queued.is_empty();

    let cursor_to_border = n - r;

    if new_sr > old_sr {
        // GROWING: use only relative cursor movements so that terminal
        // scrolling (which invalidates SavePosition's absolute row) cannot
        // corrupt the cursor position.  The relative distance from the last
        // status row back to the input line is always
        //   new_sr + cursor_to_border
        // regardless of how many rows the terminal scrolled.

        // Navigate from cursor to the border row.
        if cursor_to_border > 0 {
            let _ = execute!(stdout, cursor::MoveDown(cursor_to_border));
        }
        let _ = execute!(stdout, cursor::MoveToColumn(0));

        // Clear from border through the end of the visible area.
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::FromCursorDown));

        // Redraw border.
        print!("{styled_border}");

        // Draw new status rows. Use println!() to advance rows — unlike
        // MoveDown, println causes the terminal to scroll when at the
        // bottom, which is exactly what we need to create room.
        for i in 0..new_sr as usize {
            println!();
            let _ = execute!(stdout, cursor::MoveToColumn(0));
            if i == (new_sr as usize) - 1 && show_queued {
                let wave_pos = QUEUED_WAVE_POS.lock().map(|g| *g).unwrap_or(0.0);
                print!("{}", render_queued_wave(&queued, wave_pos));
            } else if let Some(line) = status_lines.get(i) {
                print_status_line(line, hint_active);
            }
        }

        // Navigate back to the input cursor row.
        let _ = execute!(stdout, cursor::MoveUp(new_sr + cursor_to_border),);
        // Force rustyline to do a full repaint on the next refresh cycle.
        // This fixes cursor column positioning — rustyline's incremental
        // refresh would otherwise assume the cursor hasn't moved during
        // the hint callback and render at the wrong column.
        FORCE_REPAINT.store(true, Ordering::Relaxed);
        let _ = stdout.flush();
        return;
    }

    // SHRINKING (or equal): no scrolling occurs, so SavePosition /
    // RestorePosition is safe.
    let _ = execute!(stdout, cursor::SavePosition);

    if cursor_to_border > 0 {
        let _ = execute!(stdout, cursor::MoveDown(cursor_to_border));
    }
    let _ = execute!(stdout, cursor::MoveToColumn(0));

    // Clear from border through the end of the visible area.
    // Using ClearFromCursorDown avoids a MoveDown loop that can clamp
    // at the terminal bottom and cause MoveUp(clear_rows) to overshoot.
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::FromCursorDown));

    // Redraw border + new (smaller) status rows in place.
    print!("{styled_border}");
    for i in 0..new_sr as usize {
        let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
        if i == (new_sr as usize) - 1 && show_queued {
            let wave_pos = QUEUED_WAVE_POS.lock().map(|g| *g).unwrap_or(0.0);
            print!("{}", render_queued_wave(&queued, wave_pos));
        } else if let Some(line) = status_lines.get(i) {
            print_status_line(line, hint_active);
        }
    }

    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Overwrite just the bullet character on the line 3 above the cursor.
#[allow(dead_code)]
pub(crate) fn update_bullet_above_tool_line(bullet: &str) {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(stdout, cursor::MoveUp(3), cursor::MoveToColumn(0));
    print!("{bullet}");
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Overwrite the line 2 above the cursor.
#[allow(dead_code)]
pub(crate) fn update_line_above_frame(text: &str) {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(stdout, cursor::MoveUp(2), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!("{text}");
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Overwrite the two lines above the frame.
pub(crate) fn update_two_lines_above_frame(top: &str, bottom: &str) {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::SavePosition);
    let _ = execute!(stdout, cursor::MoveUp(3), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!("{top}");
    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!("{bottom}");
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Collapse the two active lines above the frame.
pub(crate) fn collapse_two_lines_above_frame() {
    let _term = lock_term();
    let mut stdout = io::stdout();
    let (width, _) = term_size();
    let border: String = "─".repeat(width as usize);
    let styled_border = border.with(Color::DarkGrey);
    let status_lines = get_effective_status();
    let hint_active = is_hint_active();
    let queued = QUEUED_INPUT.lock().map(|g| g.clone()).unwrap_or_default();
    let show_queued = PROCESSING.load(Ordering::Relaxed) && !queued.is_empty();

    let sr = status_rows();
    let _ = execute!(stdout, cursor::MoveUp(3), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!("{styled_border}");
    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    print!("{styled_border}");
    for i in 0..sr as usize {
        let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        if i == (sr as usize) - 1 && show_queued {
            let wave_pos = QUEUED_WAVE_POS.lock().map(|g| *g).unwrap_or(0.0);
            print!("{}", render_queued_wave(&queued, wave_pos));
        } else if let Some(line) = status_lines.get(i) {
            print_status_line(line, hint_active);
        }
    }
    // Clear the 2 orphan rows left by collapsing the animation lines.
    // Don't render the stream panel here — the animation thread handles it
    // entirely via render_stream_panel_in_place().
    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    let _ = execute!(stdout, cursor::MoveDown(1), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    // Total MoveDown from P-3: 2 + sr + 2 = 4 + sr
    // Target: P-2 (new prompt after collapsing 2 animation lines)
    // MoveUp = (P-3 + 4 + sr) - (P-2) = 3 + sr
    let _ = execute!(stdout, cursor::MoveUp(3 + sr), cursor::MoveToColumn(0));
    let _ = stdout.flush();
}
