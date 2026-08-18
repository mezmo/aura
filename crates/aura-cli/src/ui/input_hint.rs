// ---------------------------------------------------------------------------
// Input validation and hints
// ---------------------------------------------------------------------------

use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crossterm::style::Stylize;
use unicode_width::UnicodeWidthStr;

use crate::api::types::ModelEntry;
use crate::repl::conversations::ConversationStore;
use crate::repl::registry::{lookup, matching_commands, split_command};

use super::input_frame::resize_status_area;
use super::state::{
    CTRLC_HINT_VISIBLE, LAST_HINT_LINE, MODEL_CACHE, MODEL_ERROR, MODEL_FETCH_CONFIG,
    MODEL_FETCH_IN_PROGRESS, MODEL_MATCHES, RESUME_MATCHES, STATUS_HINT, STATUS_ROWS,
    STREAM_CONV_DIR, STYLE_MATCHES, get_selected_model, get_tab_select_index, lock_term,
    random_bullet_color, status_rows, term_size,
};
use super::status_bar::{notice_status_rows, update_status_bar};
use super::text::{collapse_whitespace, strip_control_chars, truncate_with_ellipsis};
use crate::theme::{AuraStyle, STYLE_NAMES, Themed, theme};

/// Update the model cache from a successful fetch.
pub fn set_model_cache(models: Vec<ModelEntry>) {
    persist_model_cache(&models);
    if let Ok(mut g) = MODEL_CACHE.lock() {
        *g = models;
    }
    if let Ok(mut g) = MODEL_ERROR.lock() {
        g.clear();
    }
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
        let result = (|| -> Result<Vec<ModelEntry>, String> {
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
            Ok(list.data)
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
fn persist_model_cache(models: &[ModelEntry]) {
    if let Some(dir) = STREAM_CONV_DIR.lock().ok().and_then(|g| g.clone()) {
        ConversationStore::write_models_cache(&dir, models);
    }
}

/// Seed the in-memory model cache.
pub fn seed_model_cache(models: Vec<ModelEntry>) {
    if let Ok(mut g) = MODEL_CACHE.lock()
        && g.is_empty()
    {
        *g = models;
    }
}

/// Maximum rows a scrolling hint list shows at once.
const MAX_VISIBLE: usize = 5;

/// Narrowest column worth truncating a model description into; below it a
/// description that does not fit whole is dropped rather than shown as a stub.
const MIN_DESC_WIDTH: usize = 12;

/// Render `lines` (each the entry indices it holds) through `render`, showing
/// at most [`MAX_VISIBLE`] of them. `render` also receives the styled
/// two-column `▲ `/`▼ ` scroll cue on the window's edge lines when more lines
/// lie beyond them. The window keeps the tab selection in its middle row where
/// it can, so the selected line is never also an edge line carrying a cue —
/// layouts may share one gutter for both.
fn windowed_hint_lines(
    lines: &[Vec<usize>],
    tab_idx: Option<usize>,
    render: impl Fn(&[usize], Option<&str>) -> String,
) -> Vec<String> {
    let total_lines = lines.len();
    let (window_start, window_end) = if total_lines <= MAX_VISIBLE {
        (0, total_lines)
    } else {
        let selected_line = tab_idx
            .and_then(|idx| lines.iter().position(|line| line.contains(&idx)))
            .unwrap_or(0);
        let start = selected_line
            .saturating_sub(MAX_VISIBLE / 2)
            .min(total_lines - MAX_VISIBLE);
        (start, start + MAX_VISIBLE)
    };

    lines[window_start..window_end]
        .iter()
        .enumerate()
        .map(|(line_offset, line_entries)| {
            let glyph = if line_offset == 0 && window_start > 0 {
                Some("▲")
            } else if line_offset == window_end - window_start - 1 && window_end < total_lines {
                Some("▼")
            } else {
                None
            };
            let cue = glyph.map(|g| format!("{} ", g.themed(AuraStyle::Connector)));
            render(line_entries, cue.as_deref())
        })
        .collect()
}

/// Build columnar hint lines from a list of display entries.
/// Each entry is rendered with per-item styling: the tab-highlighted entry (if any)
/// gets `AuraStyle::Selected`, others get `AuraStyle::Muted`.
///
/// For large lists, shows a scrolling window with ▲/▼ indicators that follows
/// the tab selection (see [`windowed_hint_lines`]).
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

    // Pack entries into lines of equal-width columns.
    let mut entries_per_line: Vec<Vec<usize>> = vec![vec![]];
    let mut current_raw_len: usize = 0;
    for i in 0..entries.len() {
        if current_raw_len > 0 && current_raw_len + 2 + col_w > max_w {
            entries_per_line.push(vec![]);
            current_raw_len = 0;
        }
        if current_raw_len > 0 {
            current_raw_len += 2;
        }
        current_raw_len += col_w;
        entries_per_line.last_mut().unwrap().push(i);
    }

    windowed_hint_lines(&entries_per_line, tab_idx, |line_entries, cue| {
        let mut line_str = cue.unwrap_or_default().to_string();
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
        line_str
    })
}

/// A model's description flattened to one line, stripped of control
/// characters, and truncated to `room` columns. `None` when the model has no
/// description, or when it would need truncating and `room` is too narrow to
/// say anything useful.
fn description_column(model: &ModelEntry, room: usize) -> Option<String> {
    let desc = strip_control_chars(&collapse_whitespace(model.description.as_deref()?));
    if desc.is_empty() {
        return None;
    }
    let fits = UnicodeWidthStr::width(desc.as_str()) <= room;
    (fits || room >= MIN_DESC_WIDTH).then(|| truncate_with_ellipsis(&desc, room))
}

/// The single-row hint for a `/model` filter that leaves exactly one match:
/// `▸  id  [description  ][press enter to auto-complete]`. As `width` shrinks
/// the description goes first, then the call to action, and finally the id
/// is truncated, so the row fits any terminal for any id the server hands out.
fn unique_model_hint(model: &ModelEntry, width: usize) -> String {
    let marker = "▸  ";
    let marker_w = UnicodeWidthStr::width(marker);
    if width < marker_w {
        let partial: String = marker.chars().take(width).collect();
        return format!("{}", partial.themed(AuraStyle::Connector));
    }
    let cta = "press enter to auto-complete";
    let name = strip_control_chars(&model.id);
    let with_cta = marker_w + UnicodeWidthStr::width(name.as_str()) + 2 + cta.len();
    let mut line = format!("{}", marker.themed(AuraStyle::Connector));
    if with_cta <= width {
        line.push_str(&format!("{}  ", name.themed(AuraStyle::Muted)));
        if let Some(desc) = description_column(model, width.saturating_sub(with_cta + 2)) {
            line.push_str(&format!("{}  ", desc.themed(AuraStyle::Muted)));
        }
        line.push_str(&format!("{}", cta.with(random_bullet_color())));
    } else {
        let name = truncate_with_ellipsis(&name, width - marker_w);
        line.push_str(&format!("{}", name.themed(AuraStyle::Muted)));
    }
    line
}

/// Hint lines for `/model <filter>` over the models `filtered` leaves. A typed
/// filter that narrows to one model gets the auto-complete row; everything
/// else — including a bare `/model` with a single model — is the numbered
/// table.
fn model_hint_lines(
    filtered: &[ModelEntry],
    filter: &str,
    tab_idx: Option<usize>,
    current: Option<&str>,
    width: usize,
) -> Vec<String> {
    match filtered {
        [only] if !filter.is_empty() => vec![unique_model_hint(only, width)],
        _ => build_model_hints(filtered, tab_idx, current, width),
    }
}

/// Build hint lines for the `/model` picker at `width` columns, one model per
/// numbered row:
///
/// ```text
///      Name                Description
///   1. sre/openai          OpenAI backed SRE agent
/// ❯ 2. sre/anthropic ✓     Anthropic backed SRE agent
/// ```
///
/// The two-column gutter holds the `❯` cursor on the tab-selected row and the
/// ▲/▼ scroll cue on the window's edge rows; `✓` marks `current`. The
/// header appears once any model has a description and the header fits.
/// Ids and descriptions are stripped of control characters and truncated so
/// no row exceeds `width` display columns. Numbers count within `models` as
/// given, so a filtered list renumbers from 1.
fn build_model_hints(
    models: &[ModelEntry],
    tab_idx: Option<usize>,
    current: Option<&str>,
    width: usize,
) -> Vec<String> {
    const CHECK: &str = " ✓";
    let check_w = UnicodeWidthStr::width(CHECK);
    let is_current = |m: &ModelEntry| current.is_some_and(|c| c.eq_ignore_ascii_case(&m.id));
    let has_descriptions = models.iter().any(|m| m.description.is_some());
    let num_w = models.len().to_string().len();
    // "❯ " + "NN. " precede the name column; two spaces separate the columns.
    let prefix_w = 2 + num_w + 2;
    let lines: Vec<Vec<usize>> = (0..models.len()).map(|i| vec![i]).collect();
    // A terminal too narrow for the prefix plus a few name columns gets bare
    // truncated names, so rows fit any width at all.
    if width < prefix_w + 4 {
        return windowed_hint_lines(&lines, tab_idx, |line_entries, _| {
            let idx = line_entries[0];
            let name = truncate_with_ellipsis(&strip_control_chars(&models[idx].id), width);
            let style = if tab_idx == Some(idx) {
                AuraStyle::Selected
            } else {
                AuraStyle::Muted
            };
            format!("{}", name.themed(style))
        });
    }
    // Every row must hold its prefix, name, and check without wrapping.
    let max_name_w = width - prefix_w;
    let names: Vec<String> = models
        .iter()
        .map(|m| {
            let room = max_name_w.saturating_sub(if is_current(m) { check_w } else { 0 });
            truncate_with_ellipsis(&strip_control_chars(&m.id), room)
        })
        .collect();
    // Columns the name (plus its check, when current) occupies in the name column.
    let name_cols = |i: usize| {
        UnicodeWidthStr::width(names[i].as_str()) + if is_current(&models[i]) { check_w } else { 0 }
    };
    let name_w = (0..models.len())
        .map(name_cols)
        .chain(has_descriptions.then_some("Name".len()))
        .max()
        .unwrap_or(0);
    let desc_room = width.saturating_sub(prefix_w + name_w + 2);

    let header = (has_descriptions && desc_room >= "Description".len()).then(|| {
        format!(
            "{}{}{}  {}",
            " ".repeat(prefix_w),
            "Name".themed(AuraStyle::KeyLabel),
            " ".repeat(name_w - "Name".len()),
            "Description".themed(AuraStyle::KeyLabel),
        )
    });
    let rows = windowed_hint_lines(&lines, tab_idx, |line_entries, cue| {
        let idx = line_entries[0];
        let model = &models[idx];
        let selected = tab_idx == Some(idx);
        let gutter = if selected {
            format!("{} ", "❯".themed(AuraStyle::Prompt))
        } else {
            cue.unwrap_or("  ").to_string()
        };
        let number = format!("{:>num_w$}.", idx + 1);
        let name_style = if selected {
            AuraStyle::Selected
        } else {
            AuraStyle::Muted
        };
        let mut row = format!(
            "{gutter}{} {}",
            number.themed(AuraStyle::Muted),
            names[idx].as_str().themed(name_style)
        );
        if is_current(model) {
            row.push_str(&format!("{}", CHECK.themed(AuraStyle::Success)));
        }
        if let Some(desc) = description_column(model, desc_room) {
            row.push_str(&" ".repeat(name_w - name_cols(idx) + 2));
            row.push_str(&format!("{}", desc.themed(AuraStyle::Muted)));
        }
        row
    });
    header.into_iter().chain(rows).collect()
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
            let filtered: Vec<ModelEntry> = if filter.is_empty() {
                cached
            } else {
                let lower = filter.to_lowercase();
                cached
                    .into_iter()
                    .filter(|m| m.id.to_lowercase().contains(&lower))
                    .collect()
            };
            if let Ok(mut guard) = MODEL_MATCHES.lock() {
                *guard = filtered.iter().map(|m| m.id.clone()).collect();
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
            } else {
                let tab_idx = get_tab_select_index();
                let current = get_selected_model();
                let (width, _) = term_size();
                model_hint_lines(
                    &filtered,
                    filter,
                    tab_idx,
                    current.as_deref(),
                    width as usize,
                )
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
    } else if line.starts_with('/') {
        if let Ok(mut guard) = RESUME_MATCHES.lock() {
            guard.clear();
        }
        let matching = matching_commands(line);
        let tab_idx = get_tab_select_index();
        if matching.is_empty() {
            vec![]
        } else if matching.len() == 1 && tab_idx.is_none() {
            vec![format!(
                "{}",
                format!("{} — {}", matching[0].name, matching[0].description)
                    .themed(AuraStyle::Muted)
            )]
        } else {
            let entries: Vec<String> = matching
                .iter()
                .map(|command| command.name.to_owned())
                .collect();
            build_columnar_hints(&entries, tab_idx)
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
    use super::{
        MAX_VISIBLE, ModelEntry, build_model_hints, description_column, model_hint_lines,
        unique_model_hint, validate_command_input,
    };
    use crate::test_fixtures::{plain, strip_sgr};

    fn model(id: &str, description: Option<&str>) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            description: description.map(str::to_string),
        }
    }

    #[test]
    fn model_list_numbers_rows_and_omits_header_without_descriptions() {
        let models = [model("gpt-4o", None), model("gpt-4o-mini", None)];
        assert_eq!(
            plain(&build_model_hints(&models, None, None, 80)),
            ["  1. gpt-4o", "  2. gpt-4o-mini"]
        );
    }

    #[test]
    fn model_list_has_header_and_aligned_columns_with_descriptions() {
        let models = [
            model(
                "Mezmo Anthropic SRE Agent",
                Some("Anthropic-backed SRE agent"),
            ),
            model("Mezmo OpenAI SRE Agent", Some("OpenAI-backed SRE agent")),
            model("plain", None),
        ];
        assert_eq!(
            plain(&build_model_hints(&models, None, None, 80)),
            [
                "     Name                       Description",
                "  1. Mezmo Anthropic SRE Agent  Anthropic-backed SRE agent",
                "  2. Mezmo OpenAI SRE Agent     OpenAI-backed SRE agent",
                "  3. plain",
            ]
        );
    }

    #[test]
    fn model_list_marks_cursor_and_current_model() {
        let models = [
            model("sre/openai", Some("OpenAI")),
            model("sre/anthropic", Some("Anthropic")),
        ];
        // The check widens the name column; the cursor sits in the gutter.
        assert_eq!(
            plain(&build_model_hints(
                &models,
                Some(1),
                Some("SRE/Anthropic"),
                80
            )),
            [
                "     Name             Description",
                "  1. sre/openai       OpenAI",
                "❯ 2. sre/anthropic ✓  Anthropic",
            ]
        );
    }

    #[test]
    fn model_list_right_aligns_numbers_past_nine() {
        let models: Vec<ModelEntry> = (0..10).map(|i| model(&format!("m{i}"), None)).collect();
        let lines = plain(&build_model_hints(&models, Some(9), None, 80));
        assert_eq!(lines[MAX_VISIBLE - 1], "❯ 10. m9");
        assert!(lines[MAX_VISIBLE - 2].ends_with("  9. m8"), "{lines:?}");
    }

    #[test]
    fn model_list_truncates_descriptions_to_the_terminal_width() {
        let models = [
            model(
                "sre",
                Some("A very long description that will not fit here"),
            ),
            model("ops", Some("short")),
        ];
        let lines = plain(&build_model_hints(&models, None, None, 36));
        assert_eq!(
            lines,
            [
                "     Name  Description",
                "  1. sre   A very long descriptio...",
                "  2. ops   short",
            ]
        );
        assert!(lines.iter().all(|l| l.chars().count() <= 36));
    }

    #[test]
    fn model_list_drops_only_descriptions_that_cannot_fit_when_narrow() {
        let models = [
            model("agent-one", Some("a description")),
            model("agent-two", Some("ok")),
        ];
        // 5 prefix + 9-char name + 2 leaves 4 columns at width 20: below
        // MIN_DESC_WIDTH, so a description that needs truncating is dropped
        // while one that fits whole still shows; the header no longer fits.
        assert_eq!(
            plain(&build_model_hints(&models, None, None, 20)),
            ["  1. agent-one", "  2. agent-two  ok"]
        );
    }

    #[test]
    fn model_list_strips_control_characters_from_remote_text() {
        let models = [
            model("sre\x1b]0;pwned\x07", Some("desc\x1b[31m red\u{9b}2J")),
            model("ok", None),
        ];
        // Checked before theming, since `plain` would itself swallow an SGR.
        assert_eq!(
            description_column(&models[0], 80).as_deref(),
            Some("desc[31m red2J")
        );
        let lines = plain(&build_model_hints(&models, None, None, 80));
        assert!(
            lines.iter().all(|l| l.chars().all(|c| !c.is_control())),
            "{lines:?}"
        );
        assert_eq!(lines[1], "  1. sre]0;pwned  desc[31m red2J");
    }

    #[test]
    fn model_list_measures_wide_glyphs_in_columns() {
        let models = [
            model("漢字モデル", Some("wide id")),
            model("ascii", Some("narrow id")),
        ];
        // "漢字モデル" is 5 scalars but 10 columns, so "ascii" pads to 10.
        assert_eq!(
            plain(&build_model_hints(&models, None, None, 80)),
            [
                "     Name        Description",
                "  1. 漢字モデル  wide id",
                "  2. ascii       narrow id",
            ]
        );
    }

    #[test]
    fn model_list_never_exceeds_the_terminal_width() {
        let long = "a-model-id-that-is-far-longer-than-any-sane-terminal-would-be";
        let models: Vec<ModelEntry> = std::iter::once(model(long, Some("and a description")))
            .chain((0..12).map(|i| model(&format!("m{i}"), Some("漢字 wide"))))
            .collect();
        for width in (0..=60).chain([80, 120, 200]) {
            for (tab_idx, current) in [(None, None), (Some(0), Some(long)), (Some(12), Some("m11"))]
            {
                let lines = build_model_hints(&models, tab_idx, current, width);
                assert!(!lines.is_empty());
                for line in plain(&lines) {
                    let cols = unicode_width::UnicodeWidthStr::width(line.as_str());
                    assert!(cols <= width, "width {width}: {cols} cols in {line:?}");
                }
            }
        }
        // At 20 columns the long id is truncated to fit its row alone.
        let lines = plain(&build_model_hints(&models[..2], None, None, 20));
        assert_eq!(lines[0], "  1. a-model-id-t...");
        assert_eq!(lines[1], "  2. m0");
        // Narrower than the prefix allows: bare names, still cut to fit.
        let lines = plain(&build_model_hints(&models[..2], Some(1), None, 6));
        assert_eq!(lines, ["a-m...", "m0"]);
    }

    #[test]
    fn unique_match_hint_fits_the_terminal_for_any_id_and_width() {
        let long = "a-model-id-that-is-far-longer-than-any-sane-terminal-would-be";
        let model = model(long, Some("with a description as well"));
        for width in [0usize, 1, 2, 3, 5, 8, 20, 36, 50, 80, 120, 200] {
            let line = strip_sgr(&unique_model_hint(&model, width));
            let cols = unicode_width::UnicodeWidthStr::width(line.as_str());
            assert!(cols <= width, "width {width}: {cols} cols in {line:?}");
        }
        // Wide: id whole, description truncated to what is left, then the cta.
        let line = strip_sgr(&unique_model_hint(&model, 120));
        assert_eq!(
            line,
            format!("▸  {long}  with a description as...  press enter to auto-complete")
        );
        // Room for id and cta but not the description: description goes.
        let line = strip_sgr(&unique_model_hint(&model, 100));
        assert_eq!(line, format!("▸  {long}  press enter to auto-complete"));
        // Room for the id alone: the cta goes before the id is cut.
        let line = strip_sgr(&unique_model_hint(&model, 70));
        assert_eq!(line, format!("▸  {long}"));
        // Narrower still: the id itself is truncated.
        let line = strip_sgr(&unique_model_hint(&model, 20));
        assert_eq!(line, "▸  a-model-id-tha...");
    }

    #[test]
    fn bare_model_command_lists_even_a_single_model_but_a_filter_autocompletes() {
        let only = [model("Mezmo SRE Agent", Some("Testing the description"))];
        assert_eq!(
            plain(&model_hint_lines(&only, "", None, None, 80)),
            [
                "     Name             Description",
                "  1. Mezmo SRE Agent  Testing the description",
            ]
        );
        assert_eq!(
            plain(&model_hint_lines(&only, "mez", None, None, 80)),
            ["▸  Mezmo SRE Agent  Testing the description  press enter to auto-complete"]
        );
    }

    #[test]
    fn model_list_windows_around_the_cursor_and_marks_overflow() {
        let models: Vec<ModelEntry> = (0..MAX_VISIBLE + 3)
            .map(|i| model(&format!("m{i}"), Some(&format!("model {i}"))))
            .collect();

        // No selection: top of the list, ▼ on the last visible row.
        let top = plain(&build_model_hints(&models, None, None, 80));
        assert_eq!(top.len(), MAX_VISIBLE + 1, "{top:?}");
        assert!(top[1].starts_with("  1. m0  "), "{top:?}");
        assert!(top[MAX_VISIBLE].starts_with("▼ 5. m4"), "{top:?}");

        // A mid-list selection is centred: ▲ above, ▼ below, ❯ in between.
        let mid = plain(&build_model_hints(&models, Some(4), None, 80));
        assert!(mid[1].starts_with("▲ 3. m2"), "{mid:?}");
        assert!(mid[3].starts_with("❯ 5. m4"), "{mid:?}");
        assert!(mid[MAX_VISIBLE].starts_with("▼ 7. m6"), "{mid:?}");

        // Selecting the last entry pins the window to the bottom.
        let bottom = plain(&build_model_hints(
            &models,
            Some(models.len() - 1),
            None,
            80,
        ));
        assert!(bottom[1].starts_with("▲ 4. m3"), "{bottom:?}");
        assert!(bottom[MAX_VISIBLE].starts_with("❯ 8. m7"), "{bottom:?}");
        assert!(bottom.iter().all(|l| l.chars().count() <= 80));
    }

    #[test]
    fn description_column_flattens_and_respects_room() {
        let multi = model("m", Some("  first\nsecond   line  "));
        assert_eq!(
            description_column(&multi, 40).as_deref(),
            Some("first second line")
        );
        assert_eq!(
            description_column(&multi, 12).as_deref(),
            Some("first sec...")
        );
        assert_eq!(description_column(&multi, 11), None);
        // A short description that fits whole shows even in a narrow column.
        assert_eq!(
            description_column(&model("m", Some("SRE")), 3).as_deref(),
            Some("SRE")
        );
        assert_eq!(description_column(&model("m", Some("SRE")), 2), None);
        assert_eq!(description_column(&model("m", Some("   ")), 40), None);
        assert_eq!(description_column(&model("m", None), 40), None);
    }

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
