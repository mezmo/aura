// ---------------------------------------------------------------------------
// Input validation and hints
// ---------------------------------------------------------------------------

use std::fs;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crossterm::style::Stylize;

use crate::repl::conversations::ConversationStore;
use crate::repl::registry::{COMMANDS, Command, lookup, split_command};

use super::input_frame::resize_status_area;
use super::state::{
    CTRLC_HINT_VISIBLE, LAST_HINT_LINE, MODEL_CACHE, MODEL_ERROR, MODEL_FETCH_CONFIG,
    MODEL_FETCH_IN_PROGRESS, MODEL_MATCHES, RESUME_MATCHES, STATUS_HINT, STATUS_ROWS,
    STREAM_CONV_DIR, STYLE_MATCHES, get_tab_select_index, lock_term, random_bullet_color,
    status_rows, term_size,
};
use super::status_bar::{notice_status_rows, update_status_bar};
use crate::theme::{AuraStyle, STYLE_NAMES, Themed, theme};

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

/// Seed the in-memory model cache. No-op when the cache is already
/// populated; use [`refresh_model_cache`] to overwrite.
pub fn seed_model_cache(models: Vec<String>) {
    if let Ok(mut g) = MODEL_CACHE.lock()
        && g.is_empty()
    {
        *g = models;
    }
}

/// Overwrite the in-memory model cache with a fresh roster.
pub fn refresh_model_cache(models: Vec<String>) {
    if let Ok(mut g) = MODEL_CACHE.lock() {
        *g = models;
    }
}

/// Build columnar hint lines from a list of display entries.
/// Each entry is rendered with per-item styling: the tab-highlighted entry (if any)
/// gets `AuraStyle::Selected`, others get `AuraStyle::Muted`.
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
            line_str.push_str(&format!("{} ", "▲".themed(AuraStyle::Connector)));
        }
        if is_last && has_below {
            line_str.push_str(&format!("{} ", "▼".themed(AuraStyle::Connector)));
        }

        for (pos, &idx) in line_entries.iter().enumerate() {
            if pos > 0 {
                line_str.push_str("  ");
            }
            let padded = format!("{:<width$}", entries[idx], width = col_w);
            if tab_idx == Some(idx) {
                line_str.push_str(&format!("{}", padded.themed(AuraStyle::Selected)));
            } else {
                line_str.push_str(&format!("{}", padded.themed(AuraStyle::Muted)));
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
            format!("{}", "/ for commands".themed(AuraStyle::Muted)),
            format!("{}", "ctrl+c twice to quit".themed(AuraStyle::Muted)),
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
                "no matching conversations".themed(AuraStyle::Muted)
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
                "▸".themed(AuraStyle::Connector),
                short.themed(AuraStyle::Muted),
                display_name.themed(AuraStyle::Muted),
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
                format!("error: {}", err).themed(AuraStyle::Muted)
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
                    vec![format!("{}", "loading models...".themed(AuraStyle::Muted))]
                } else if !filter.is_empty() {
                    let color = random_bullet_color();
                    vec![format!(
                        "{}  {}",
                        "no matching models".themed(AuraStyle::Muted),
                        "press enter to use anyway".with(color),
                    )]
                } else {
                    vec![format!("{}", "no matching models".themed(AuraStyle::Muted))]
                }
            } else if filtered.len() == 1 {
                let color = random_bullet_color();
                vec![format!(
                    "{}  {}  {}",
                    "▸".themed(AuraStyle::Connector),
                    filtered[0].clone().themed(AuraStyle::Muted),
                    "press enter to auto-complete".with(color),
                )]
            } else {
                let tab_idx = get_tab_select_index();
                build_columnar_hints(&filtered, tab_idx)
            }
        }
    } else if line == "/style" || line.starts_with("/style ") {
        let filter = line.strip_prefix("/style").unwrap_or("").trim_start();
        let lower = filter.to_ascii_lowercase();
        let filtered: Vec<String> = STYLE_NAMES
            .iter()
            .filter(|name| lower.is_empty() || name.starts_with(&lower))
            .map(|s| (*s).to_string())
            .collect();
        if let Ok(mut guard) = STYLE_MATCHES.lock() {
            *guard = filtered.clone();
        }
        let current = theme().name;
        // Mark the active style with a leading "* "; pad others with two
        // spaces so column widths stay aligned. The asterisk follows the
        // active theme — Tab live-preview moves it as the user cycles.
        let mark = |name: &str| -> String {
            if name == current {
                format!("* {name}")
            } else {
                format!("  {name}")
            }
        };
        if filtered.is_empty() {
            vec![format!("{}", "no matching styles".themed(AuraStyle::Muted),)]
        } else if filtered.len() == 1 {
            let color = random_bullet_color();
            vec![format!(
                "{}  {}  {}",
                "▸".themed(AuraStyle::Connector),
                mark(&filtered[0]).themed(AuraStyle::Muted),
                "press enter to apply".with(color),
            )]
        } else {
            let tab_idx = get_tab_select_index();
            let entries: Vec<String> = filtered.iter().map(|n| mark(n)).collect();
            build_columnar_hints(&entries, tab_idx)
        }
    } else if let Some(prefix) = line.strip_prefix('/') {
        if let Ok(mut guard) = RESUME_MATCHES.lock() {
            guard.clear();
        }
        let matching: Vec<&Command> = COMMANDS
            .iter()
            .filter(|c| {
                c.name
                    .strip_prefix('/')
                    .unwrap_or(c.name)
                    .starts_with(prefix)
            })
            .collect();
        if matching.is_empty() {
            vec![]
        } else if matching.len() == 1 {
            vec![format!(
                "{}",
                format!("{} — {}", matching[0].name, matching[0].description)
                    .themed(AuraStyle::Muted)
            )]
        } else {
            vec![format!(
                "{}",
                matching
                    .iter()
                    .map(|c| c.name)
                    .collect::<Vec<_>>()
                    .join("  ")
                    .themed(AuraStyle::Muted)
            )]
        }
    } else {
        if let Ok(mut guard) = RESUME_MATCHES.lock() {
            guard.clear();
        }
        vec![]
    };
    // Compute new status row count and handle resizing. When no hint is
    // showing, fall back to the notice-aware baseline so any per-turn notices
    // stay visible.
    let new_sr = if hint.is_empty() {
        notice_status_rows()
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
    // Shrink back to the notice-aware baseline (3 when no notices), so notices
    // collected this turn reappear once the command hint is dismissed.
    let target = notice_status_rows();
    if old_sr != target {
        let _term = lock_term();
        STATUS_ROWS.store(target, Ordering::Relaxed);
        resize_status_area(old_sr, target);
    }
}

/// Returns whether Enter should submit the current input line.
///
/// Enter always submits, except for a known command whose submission gate
/// holds it back (e.g. an ambiguous `/resume` argument). Unknown or partial
/// commands submit too, so dispatch can report them as unknown rather than
/// Enter doing nothing.
pub fn validate_command_input(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return true;
    }
    let (word, _) = split_command(trimmed);
    match lookup(word) {
        Some(cmd) => cmd.validate.is_none_or(|gate| gate(trimmed)),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::validate_command_input;

    #[test]
    fn plain_text_submits() {
        assert!(validate_command_input(""));
        assert!(validate_command_input("   "));
        assert!(validate_command_input("hello there"));
        assert!(validate_command_input("what is /help"));
    }

    #[test]
    fn known_commands_submit() {
        assert!(validate_command_input("/help"));
        assert!(validate_command_input("/clear"));
    }

    #[test]
    fn unknown_commands_submit() {
        assert!(validate_command_input("/zzz"));
        assert!(validate_command_input("/he"));
        assert!(validate_command_input("/conv"));
        assert!(validate_command_input("/e"));
    }
}
