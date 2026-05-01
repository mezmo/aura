// ---------------------------------------------------------------------------
// Input validation and hints
// ---------------------------------------------------------------------------

use std::fs;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crossterm::style::{Color, Stylize};

use crate::repl::conversations::ConversationStore;

use super::input_frame::resize_status_area;
use super::state::{
    COMMANDS, CTRLC_HINT_VISIBLE, LAST_HINT_LINE, MODEL_CACHE, MODEL_ERROR, MODEL_FETCH_CONFIG,
    MODEL_FETCH_IN_PROGRESS, MODEL_MATCHES, RESUME_MATCHES, STATUS_HINT, STATUS_ROWS,
    STREAM_CONV_DIR, get_tab_select_index, lock_term, random_bullet_color, status_rows, term_size,
};
use super::status_bar::update_status_bar;

/// Update the model cache from a successful fetch.
pub fn set_model_cache(models: Vec<String>) {
    if let Ok(mut g) = MODEL_CACHE.lock() {
        *g = models.clone();
    }
    if let Ok(mut g) = MODEL_ERROR.lock() {
        g.clear();
    }
    persist_model_cache(&models);
    refresh_model_hints();
}

/// Store an error message from a failed model fetch.
pub fn set_model_error(err: String) {
    if let Ok(mut g) = MODEL_ERROR.lock() {
        *g = err;
    }
    refresh_model_hints();
}

/// Trigger a background model fetch.
pub fn trigger_model_fetch(
    models_url: String,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
) {
    if MODEL_FETCH_IN_PROGRESS.swap(true, Ordering::Relaxed) {
        return;
    }
    thread::spawn(move || {
        let result = (|| -> Result<Vec<String>, String> {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| e.to_string())?;
            let mut req = client.get(&models_url);
            if let Some(ref key) = api_key {
                req = req.bearer_auth(key);
            }
            for (name, value) in &extra_headers {
                req = req.header(name, value);
            }
            let resp = req.send().map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("HTTP {}", resp.status()));
            }
            let list: crate::api::types::ModelList = resp.json().map_err(|e| e.to_string())?;
            Ok(list.data.into_iter().map(|m| m.id).collect())
        })();
        match result {
            Ok(models) => set_model_cache(models),
            Err(e) => set_model_error(e),
        }
        MODEL_FETCH_IN_PROGRESS.store(false, Ordering::Relaxed);
    });
}

/// Initialize the model fetch config (call once from the REPL loop).
pub fn set_model_fetch_config(
    models_url: String,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
) {
    if let Ok(mut g) = MODEL_FETCH_CONFIG.lock() {
        *g = Some((models_url, api_key, extra_headers));
    }
}

/// Trigger a model fetch using the stored config.
fn trigger_model_fetch_cached() {
    if let Ok(g) = MODEL_FETCH_CONFIG.lock()
        && let Some((url, key, headers)) = g.clone()
    {
        trigger_model_fetch(url, key, headers);
    }
}

/// Re-run `update_input_hint` if the user is currently in `/model` mode.
fn refresh_model_hints() {
    let line = LAST_HINT_LINE.lock().map(|g| g.clone()).unwrap_or_default();
    if line == "/model" || line.starts_with("/model ") {
        update_input_hint(&line);
    }
}

/// Persist the model list to the current conversation directory.
fn persist_model_cache(models: &[String]) {
    let dir = match STREAM_CONV_DIR.lock().ok().and_then(|g| g.clone()) {
        Some(d) => d,
        None => return,
    };
    let _ = fs::write(dir.join("models_cache"), models.join("\n"));
}

/// Seed the in-memory model cache.
pub fn seed_model_cache(models: Vec<String>) {
    if let Ok(mut g) = MODEL_CACHE.lock()
        && g.is_empty()
    {
        *g = models;
    }
}

/// Build columnar hint lines from a list of display entries.
/// Each entry is rendered with per-item styling: the tab-highlighted entry (if any)
/// gets `Color::White`, others get `Color::DarkGrey`.
///
/// For large lists, shows a scrolling 5-line window with ▲/▼ indicators.
/// The window follows the tab selection to keep it visible.
fn build_columnar_hints(entries: &[String], tab_idx: Option<usize>) -> Vec<String> {
    if entries.is_empty() {
        return vec![];
    }
    let (width, _) = term_size();
    let max_w = width as usize;
    let col_w = entries.iter().map(|e| e.len()).max().unwrap_or(0);
    if col_w == 0 {
        return vec![];
    }

    const MAX_VISIBLE: usize = 5;

    // First pass: assign each entry index to a line number.
    let mut line_of: Vec<usize> = Vec::with_capacity(entries.len());
    let mut entries_per_line: Vec<Vec<usize>> = vec![vec![]];
    let mut current_raw_len: usize = 0;

    for (i, _entry) in entries.iter().enumerate() {
        if current_raw_len > 0 && current_raw_len + 2 + col_w > max_w {
            entries_per_line.push(vec![]);
            current_raw_len = 0;
        }
        if current_raw_len > 0 {
            current_raw_len += 2;
        }
        current_raw_len += col_w;
        let line_num = entries_per_line.len() - 1;
        line_of.push(line_num);
        entries_per_line.last_mut().unwrap().push(i);
    }

    let total_lines = entries_per_line.len();

    // Determine visible window
    let (window_start, window_end, has_above, has_below) = if total_lines <= MAX_VISIBLE {
        (0, total_lines, false, false)
    } else {
        let selected_line = tab_idx.map(|idx| line_of[idx]).unwrap_or(0);
        let start = selected_line.min(total_lines - MAX_VISIBLE);
        let end = start + MAX_VISIBLE;
        (start, end, start > 0, end < total_lines)
    };

    // Render visible lines
    let mut result = Vec::new();
    for (line_offset, line_entries) in entries_per_line[window_start..window_end]
        .iter()
        .enumerate()
    {
        let mut line_str = String::new();

        // Scroll indicators on first/last visible line
        let is_first = line_offset == 0;
        let is_last = line_offset == window_end - window_start - 1;
        if is_first && has_above {
            line_str.push_str(&format!("{} ", "▲".with(Color::DarkGrey)));
        }
        if is_last && has_below {
            line_str.push_str(&format!("{} ", "▼".with(Color::DarkGrey)));
        }

        for (pos, &idx) in line_entries.iter().enumerate() {
            if pos > 0 {
                line_str.push_str("  ");
            }
            let padded = format!("{:<width$}", entries[idx], width = col_w);
            if tab_idx == Some(idx) {
                line_str.push_str(&format!("{}", padded.with(Color::White)));
            } else {
                line_str.push_str(&format!("{}", padded.with(Color::DarkGrey)));
            }
        }
        result.push(line_str);
    }
    result
}

/// Update the status bar hint based on the current input line.
pub fn update_input_hint(line: &str) {
    if CTRLC_HINT_VISIBLE.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut g) = LAST_HINT_LINE.lock() {
        *g = line.to_string();
    }
    let hint: Vec<String> = if line == "?" {
        vec![
            format!("{}", "/ for commands".with(Color::DarkGrey)),
            format!("{}", "ctrl+c twice to quit".with(Color::DarkGrey)),
        ]
    } else if line == "/resume" || line.starts_with("/resume ") {
        let filter = line.strip_prefix("/resume").unwrap_or("").trim_start();
        let matches = ConversationStore::find_matching(filter);
        if let Ok(mut guard) = RESUME_MATCHES.lock() {
            *guard = matches.clone();
        }
        if matches.is_empty() {
            vec![format!(
                "{}",
                "no matching conversations".with(Color::DarkGrey)
            )]
        } else if matches.len() == 1 {
            let (uuid, name) = &matches[0];
            let short = &uuid[..8.min(uuid.len())];
            let display_name = if name.is_empty() {
                "(untitled)"
            } else {
                name.trim()
            };
            let color = random_bullet_color();
            vec![format!(
                "{}  {}  {}  {}",
                "▸".with(Color::DarkGrey),
                short.with(Color::DarkGrey),
                display_name.with(Color::DarkGrey),
                "press enter to auto-complete".with(color),
            )]
        } else {
            let tab_idx = get_tab_select_index();
            let entries: Vec<String> = matches
                .iter()
                .map(|(uuid, name)| {
                    let short = &uuid[..8.min(uuid.len())];
                    let display_name = if name.is_empty() {
                        "(untitled)"
                    } else {
                        name.trim()
                    };
                    format!("{}  {}", short, display_name)
                })
                .collect();
            build_columnar_hints(&entries, tab_idx)
        }
    } else if line == "/model" || line.starts_with("/model ") {
        let filter = line.strip_prefix("/model").unwrap_or("").trim_start();
        trigger_model_fetch_cached();
        let err = MODEL_ERROR.lock().map(|g| g.clone()).unwrap_or_default();
        if !err.is_empty() {
            if let Ok(mut guard) = MODEL_MATCHES.lock() {
                guard.clear();
            }
            vec![format!(
                "{}",
                format!("error: {}", err).with(Color::DarkGrey)
            )]
        } else {
            let cached = MODEL_CACHE.lock().map(|g| g.clone()).unwrap_or_default();
            let filtered: Vec<String> = if filter.is_empty() {
                cached
            } else {
                let lower = filter.to_lowercase();
                cached
                    .into_iter()
                    .filter(|m| m.to_lowercase().contains(&lower))
                    .collect()
            };
            if let Ok(mut guard) = MODEL_MATCHES.lock() {
                *guard = filtered.clone();
            }
            if filtered.is_empty() {
                if MODEL_FETCH_IN_PROGRESS.load(Ordering::Relaxed) {
                    vec![format!("{}", "loading models...".with(Color::DarkGrey))]
                } else if !filter.is_empty() {
                    let color = random_bullet_color();
                    vec![format!(
                        "{}  {}",
                        "no matching models".with(Color::DarkGrey),
                        "press enter to use anyway".with(color),
                    )]
                } else {
                    vec![format!("{}", "no matching models".with(Color::DarkGrey))]
                }
            } else if filtered.len() == 1 {
                let color = random_bullet_color();
                vec![format!(
                    "{}  {}  {}",
                    "▸".with(Color::DarkGrey),
                    filtered[0].clone().with(Color::DarkGrey),
                    "press enter to auto-complete".with(color),
                )]
            } else {
                let tab_idx = get_tab_select_index();
                build_columnar_hints(&filtered, tab_idx)
            }
        }
    } else if let Some(prefix) = line.strip_prefix('/') {
        if let Ok(mut guard) = RESUME_MATCHES.lock() {
            guard.clear();
        }
        let matching: Vec<(&str, &str)> = COMMANDS
            .iter()
            .filter(|(name, _)| name[1..].starts_with(prefix))
            .copied()
            .collect();
        if matching.is_empty() {
            vec![]
        } else if matching.len() == 1 {
            vec![format!(
                "{}",
                format!("{} — {}", matching[0].0, matching[0].1).with(Color::DarkGrey)
            )]
        } else {
            vec![format!(
                "{}",
                matching
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join("  ")
                    .with(Color::DarkGrey)
            )]
        }
    } else {
        if let Ok(mut guard) = RESUME_MATCHES.lock() {
            guard.clear();
        }
        vec![]
    };
    // Compute new status row count and handle resizing
    let new_sr = if hint.is_empty() {
        3u16
    } else {
        (hint.len() as u16 + 1).max(3)
    };
    let old_sr = status_rows();

    let changed = if let Ok(mut guard) = STATUS_HINT.lock() {
        if *guard != hint {
            *guard = hint;
            true
        } else {
            false
        }
    } else {
        false
    };

    if new_sr != old_sr {
        let _term = lock_term();
        STATUS_ROWS.store(new_sr, Ordering::Relaxed);
        resize_status_area(old_sr, new_sr);
    } else if changed {
        update_status_bar();
    }
}

/// Clear the input hint overlay.
/// Note: does NOT reset TAB_SELECT_INDEX — command handlers consume it.
pub fn clear_input_hint() {
    CTRLC_HINT_VISIBLE.store(false, Ordering::Relaxed);
    let old_sr = status_rows();
    if let Ok(mut guard) = STATUS_HINT.lock() {
        guard.clear();
    }
    if old_sr != 3 {
        let _term = lock_term();
        STATUS_ROWS.store(3, Ordering::Relaxed);
        resize_status_area(old_sr, 3);
    }
}

/// Returns whether Enter should submit the current input line.
pub fn validate_command_input(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return true;
    }
    // Tab selection active -> allow Enter for /model and /resume
    let tab_active = get_tab_select_index().is_some();
    if trimmed == "/resume" || trimmed.starts_with("/resume ") {
        if tab_active {
            return true;
        }
        let count = RESUME_MATCHES.lock().map(|g| g.len()).unwrap_or(0);
        return count == 1;
    }
    if trimmed == "/model" || trimmed.starts_with("/model ") {
        if tab_active {
            return true;
        }
        let filter = trimmed.strip_prefix("/model").unwrap_or("").trim();
        if filter.is_empty() {
            let count = MODEL_MATCHES.lock().map(|g| g.len()).unwrap_or(0);
            return count == 1;
        }
        return true;
    }
    let cmd_word = trimmed.split_whitespace().next().unwrap_or(trimmed);
    let resolved = resolve_command_prefix(cmd_word);
    COMMANDS.iter().any(|(name, _)| *name == resolved)
}

/// Resolve a possibly-abbreviated slash command to its full form (used by validate).
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
