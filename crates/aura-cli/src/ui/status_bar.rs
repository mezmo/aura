// ---------------------------------------------------------------------------
// Status bar rendering
// ---------------------------------------------------------------------------

use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{Color, Stylize};
use crossterm::terminal;

use super::animation::render_queued_wave;
use super::state::{
    AUTO_COMPACT_CEILING, CTRLC_HINT_VISIBLE, CTRLC_RESET_SKIP, CUMULATIVE_COMPLETION,
    CUMULATIVE_PROMPT, CUMULATIVE_SCRATCHPAD_EXTRACTED, CUMULATIVE_SCRATCHPAD_INTERCEPTED,
    CURSOR_ROW, FRAME_LINES, LAST_CTRLC, PROCESSING, QUEUED_INPUT, QUEUED_WAVE_POS, STATUS_BAR,
    STATUS_HINT, lock_term, status_rows, term_size,
};

/// Format a number with comma separators (e.g. 1234 -> "1,234").
fn format_number_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Approximate token count from a byte count (~4 bytes per token).
fn bytes_to_tokens(bytes: u64) -> u64 {
    bytes / 4
}

/// Set the status bar text.
pub fn set_status_bar(text: String) {
    if let Ok(mut guard) = STATUS_BAR.lock() {
        *guard = text;
    }
}

fn get_status_bar() -> String {
    STATUS_BAR.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Whether the status area is currently showing a hint overlay.
pub(crate) fn is_hint_active() -> bool {
    STATUS_HINT.lock().map(|g| !g.is_empty()).unwrap_or(false)
}

/// Return the lines to display in the status area.
pub(crate) fn get_effective_status() -> Vec<String> {
    let hint = STATUS_HINT.lock().map(|g| g.clone()).unwrap_or_default();
    if !hint.is_empty() {
        return hint;
    }
    if PROCESSING.load(Ordering::Relaxed) {
        return vec!["esc to stop".to_string()];
    }
    let bar = get_status_bar();
    if !bar.is_empty() {
        return vec![bar];
    }
    vec!["? for help".to_string()]
}

/// Print a status line: hints are pre-styled, others get DarkGrey.
pub(crate) fn print_status_line(line: &str, hint_active: bool) {
    if hint_active {
        print!("{line}");
    } else {
        print!("{}", line.with(Color::DarkGrey));
    }
}

/// Update the status bar rows in place (dynamically sized).
pub fn update_status_bar() {
    let _term = lock_term();
    update_status_bar_unlocked();
}

/// Inner implementation — caller must already hold `TERM_WRITE`.
pub(crate) fn update_status_bar_unlocked() {
    let lines = get_effective_status();
    let hint_active = is_hint_active();
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
            print_status_line(line, hint_active);
        }
    }
    let _ = execute!(stdout, cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Accumulate token counts and update the status bar text.
pub fn set_status_bar_tokens(prompt_tokens: u64, completion_tokens: u64) {
    let cumulative_prompt = CUMULATIVE_PROMPT
        .lock()
        .map(|mut g| {
            *g += prompt_tokens;
            *g
        })
        .unwrap_or(prompt_tokens);
    let cumulative_completion = CUMULATIVE_COMPLETION
        .lock()
        .map(|mut g| {
            *g += completion_tokens;
            *g
        })
        .unwrap_or(completion_tokens);
    let total = cumulative_prompt + cumulative_completion;

    let left = build_status_left(cumulative_prompt, cumulative_completion, total);

    let ceiling = AUTO_COMPACT_CEILING.load(Ordering::Relaxed);
    let right = if ceiling > 0 && total < ceiling {
        let remaining_pct = ((ceiling - total) as f64 / ceiling as f64 * 100.0).round() as u64;
        format!("Context left: {remaining_pct}%")
    } else {
        "Aura, by Mezmo!".to_string()
    };
    set_status_bar(set_status_with_right_text(&left, &right));
}

/// Accumulate scratchpad savings and refresh the status bar.
pub fn add_scratchpad_usage(tokens_intercepted: u64, tokens_extracted: u64) {
    if let Ok(mut g) = CUMULATIVE_SCRATCHPAD_INTERCEPTED.lock() {
        *g += tokens_intercepted;
    }
    if let Ok(mut g) = CUMULATIVE_SCRATCHPAD_EXTRACTED.lock() {
        *g += tokens_extracted;
    }
    refresh_status_bar_from_counters();
}

/// Re-render the status bar from the current cumulative counters.
fn refresh_status_bar_from_counters() {
    let cumulative_prompt = CUMULATIVE_PROMPT.lock().map(|g| *g).unwrap_or(0);
    let cumulative_completion = CUMULATIVE_COMPLETION.lock().map(|g| *g).unwrap_or(0);
    let total = cumulative_prompt + cumulative_completion;

    let left = build_status_left(cumulative_prompt, cumulative_completion, total);

    let ceiling = AUTO_COMPACT_CEILING.load(Ordering::Relaxed);
    let right = if ceiling > 0 && total < ceiling {
        let remaining_pct = ((ceiling - total) as f64 / ceiling as f64 * 100.0).round() as u64;
        format!("Context left: {remaining_pct}%")
    } else {
        "Aura, by Mezmo!".to_string()
    };
    set_status_bar(set_status_with_right_text(&left, &right));
}

/// Build the left portion of the status bar text.
fn build_status_left(cumulative_prompt: u64, cumulative_completion: u64, total: u64) -> String {
    let base = format!(
        "prompt: {} | completion: {} | context: {} tokens",
        format_number_with_commas(cumulative_prompt),
        format_number_with_commas(cumulative_completion),
        format_number_with_commas(total),
    );
    let intercepted = CUMULATIVE_SCRATCHPAD_INTERCEPTED
        .lock()
        .map(|g| *g)
        .unwrap_or(0);
    let extracted = CUMULATIVE_SCRATCHPAD_EXTRACTED
        .lock()
        .map(|g| *g)
        .unwrap_or(0);
    if intercepted > 0 {
        format!(
            "{base} | scratchpad: intercepted ~{} tokens, extracted ~{} tokens",
            format_number_with_commas(bytes_to_tokens(intercepted)),
            format_number_with_commas(bytes_to_tokens(extracted)),
        )
    } else {
        base
    }
}

/// Combine left-aligned content with right-aligned text.
pub(crate) fn set_status_with_right_text(left: &str, right: &str) -> String {
    let (width, _) = term_size();
    let left_len = left.len();
    let right_len = right.len();
    if left_len + right_len + 2 <= width as usize {
        let gap = width as usize - left_len - right_len;
        format!("{left}{}{right}", " ".repeat(gap))
    } else {
        left.to_string()
    }
}

/// Set the token ceiling at which auto-compact will fire.
pub fn set_auto_compact_ceiling(ceiling: u64) {
    AUTO_COMPACT_CEILING.store(ceiling, Ordering::Relaxed);
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
    let total = prompt_tokens + completion_tokens;
    let left = build_status_left(prompt_tokens, completion_tokens, total);
    set_status_bar(set_status_with_right_text(&left, "Aura, by Mezmo!"));
}

/// Reset cumulative token counters to zero.
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
    set_status_bar(set_status_with_right_text("", "Aura, by Mezmo!"));
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
            "Press Ctrl+C again to quit".with(Color::DarkGrey)
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
